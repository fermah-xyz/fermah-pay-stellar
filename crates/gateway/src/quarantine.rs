//! Resolving quarantined charges, run by an operator under the
//! `pay_stellar_operator` role.
//!
//! The database accepts any resolution from that role; what keeps a
//! resolution honest is that it is derived here from the network, not from
//! the operator's judgement: from the charge's record on the contract while it
//! lives, or from the contract's `charges` event in the transaction that
//! settled the charge.

use fermah_pay_stellar_chain::prepaid::{
    CHARGE_RECORD_GRACE, Outcome, PrepaidDeployment, charge_record, settled_in,
};
use fermah_pay_stellar_chain::rpc::{RpcError, TransactionStatus, hex_lower};
use fermah_pay_stellar_domain::AccountAddress;
use sqlx::PgPool;
use uuid::Uuid;

use crate::submission::{Chain, stored_authorization_horizon};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedCharge {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub seller_deployment_id: Uuid,
    pub owner: AccountAddress,
    pub charge_id: [u8; 32],
    pub last_ledger: u32,
    /// The last ledger in which the authorization of the batch that carried
    /// the charge could still be included.
    pub authorization_horizon: u32,
    pub amount: i64,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub transaction_hash: Option<String>,
    pub deployment: PrepaidDeployment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The contract settled the charge with this outcome.
    Settled(Outcome),
    /// The contract has no record of the charge and it is past its last
    /// ledger: nothing was debited and nothing can be.
    Expired,
    /// The contract has no record of the charge and it is still within its
    /// last ledger; it goes back to the next batch.
    Readmitted,
}

impl Resolution {
    const fn token(self) -> &'static str {
        match self {
            Self::Settled(outcome) => outcome.token(),
            Self::Expired => "expired",
            Self::Readmitted => "readmitted",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QuarantineError {
    #[error("database operation `{operation}` failed")]
    Store {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("network read failed")]
    Chain(#[source] RpcError),
    #[error("no quarantined charge {0}")]
    NotQuarantined(Uuid),
    #[error("stored row violates an invariant: {0}")]
    Corrupt(&'static str),
    #[error("transaction {0} is not a successful transaction known to the node")]
    TransactionNotSucceeded(String),
    #[error("transaction {hash} settles no charge {charge} of {owner}")]
    NotInTransaction { hash: String, owner: AccountAddress, charge: String },
    #[error("transaction {hash} settled charge {charge} for {found}, not {expected}")]
    AmountMismatch { hash: String, charge: String, found: i128, expected: i64 },
    #[error("transaction {hash} answered {outcome} for charge {charge}, which settles nothing")]
    NotSettled { hash: String, charge: String, outcome: &'static str },
    #[error("the record of charge {charge} holds {outcome}, which resolves nothing")]
    RecordUnresolvable { charge: String, outcome: &'static str },
    #[error(
        "no record of charge {charge} at ledger {ledger}, past the ledger a record would live to; resolve it from the transaction that settled it"
    )]
    RecordGone { charge: String, ledger: u32 },
    #[error(
        "the node is at ledger {ledger}, not past ledger {horizon} up to which the batch carrying charge {charge} could still land; retry against a node that is"
    )]
    ReadBeforeHorizon { charge: String, ledger: u32, horizon: u32 },
}

fn store(operation: &'static str) -> impl FnOnce(sqlx::Error) -> QuarantineError {
    move |source| QuarantineError::Store { operation, source }
}

fn contract_id(raw: &str) -> Result<[u8; 32], QuarantineError> {
    stellar_strkey::Contract::from_string(raw)
        .map(|contract| contract.0)
        .map_err(|_| QuarantineError::Corrupt("contract address outside the CHECK constraint"))
}

pub async fn quarantined_charges(
    pool: &PgPool,
    only: Option<Uuid>,
) -> Result<Vec<QuarantinedCharge>, QuarantineError> {
    let rows = sqlx::query!(
        r#"
        SELECT c.id, c.buyer_id, c.seller_deployment_id, c.charge_id, c.last_ledger, c.amount,
               c.outcome,
               c.last_error, b.wallet_address, l.contract_address, l.usdc_address,
               l.treasury_address, s.outer_hash AS "outer_hash?", s.envelope_xdr AS "envelope_xdr?"
        FROM pay_stellar.charges c
        JOIN pay_stellar.buyers b ON b.id = c.buyer_id
        JOIN pay_stellar.ledger_contracts l
          ON l.seller_deployment_id = c.seller_deployment_id AND l.network = c.network
        LEFT JOIN pay_stellar.submissions s ON s.id = c.submission_id
        WHERE c.state = 'quarantined' AND ($1::uuid IS NULL OR c.id = $1)
        ORDER BY c.settled_at
        "#,
        only,
    )
    .fetch_all(pool)
    .await
    .map_err(store("read quarantined charges"))?;
    rows.into_iter()
        .map(|row| {
            Ok(QuarantinedCharge {
                id: row.id,
                buyer_id: row.buyer_id,
                seller_deployment_id: row.seller_deployment_id,
                owner: row.wallet_address.parse().map_err(|_| {
                    QuarantineError::Corrupt("address outside the CHECK constraint")
                })?,
                charge_id: <[u8; 32]>::try_from(row.charge_id)
                    .map_err(|_| QuarantineError::Corrupt("charge identifier is not 32 bytes"))?,
                last_ledger: u32::try_from(row.last_ledger)
                    .map_err(|_| QuarantineError::Corrupt("charge last ledger out of range"))?,
                // Every quarantined charge names the submission that carried
                // it (`charges_linked`).
                authorization_horizon: row
                    .envelope_xdr
                    .as_deref()
                    .and_then(stored_authorization_horizon)
                    .ok_or(QuarantineError::Corrupt(
                        "quarantined charge without a readable envelope",
                    ))?,
                amount: row.amount,
                outcome: row.outcome,
                reason: row.last_error,
                transaction_hash: row.outer_hash.as_deref().map(hex_lower),
                deployment: PrepaidDeployment {
                    contract: contract_id(&row.contract_address)?,
                    usdc: contract_id(&row.usdc_address)?,
                    treasury: row.treasury_address.parse().map_err(|_| {
                        QuarantineError::Corrupt("address outside the CHECK constraint")
                    })?,
                },
            })
        })
        .collect()
}

/// The resolution the contract's `charges` event in transaction `hash`
/// establishes for `charge`, and the evidence text to record.
pub async fn prove_from_transaction<C: Chain>(
    chain: &C,
    charge: &QuarantinedCharge,
    hash: &[u8; 32],
) -> Result<(Resolution, String), QuarantineError> {
    let shown = hex_lower(hash);
    let TransactionStatus::Success(tx) =
        chain.transaction(hash).await.map_err(QuarantineError::Chain)?
    else {
        return Err(QuarantineError::TransactionNotSucceeded(shown));
    };
    let meta =
        tx.meta.as_ref().ok_or_else(|| QuarantineError::TransactionNotSucceeded(shown.clone()))?;
    let shown_charge = hex_lower(&charge.charge_id);
    let entry = settled_in(meta, &charge.deployment.contract)
        .into_iter()
        .find(|entry| entry.owner == charge.owner && entry.charge_id == charge.charge_id)
        .ok_or_else(|| QuarantineError::NotInTransaction {
            hash: shown.clone(),
            owner: charge.owner.clone(),
            charge: shown_charge.clone(),
        })?;
    if entry.amount != i128::from(charge.amount) {
        return Err(QuarantineError::AmountMismatch {
            hash: shown,
            charge: shown_charge,
            found: entry.amount,
            expected: charge.amount,
        });
    }
    match entry.outcome {
        Outcome::Charged | Outcome::InsufficientBalance | Outcome::AboveLimit => Ok((
            Resolution::Settled(entry.outcome),
            format!(
                "transaction {shown} (ledger {}) settled charge {shown_charge} as {}",
                tx.ledger,
                entry.outcome.token()
            ),
        )),
        other => Err(QuarantineError::NotSettled {
            hash: shown,
            charge: shown_charge,
            outcome: other.token(),
        }),
    }
}

/// The resolution the charge's record on the contract establishes, read
/// now: its outcome if the record exists; if not, a readmission while the
/// charge is within its last ledger, or expiry while a record, had there
/// been one, would still live.
pub async fn prove_from_record<C: Chain>(
    chain: &C,
    charge: &QuarantinedCharge,
) -> Result<(Resolution, String), QuarantineError> {
    let shown_charge = hex_lower(&charge.charge_id);
    let key = charge.deployment.charge_record_key(&charge.owner, &charge.charge_id);
    let read = chain.ledger_entries(&[key]).await.map_err(QuarantineError::Chain)?;
    let ledger = read.latest_ledger;
    // A node behind the batch's inclusion, or the authorization window
    // itself, cannot show that the record is absent: the batch may still
    // land, or may have landed after the node's view.
    if ledger <= charge.authorization_horizon {
        return Err(QuarantineError::ReadBeforeHorizon {
            charge: shown_charge,
            ledger,
            horizon: charge.authorization_horizon,
        });
    }
    let resolution = match read.entries.first() {
        Some(record) => {
            let outcome = charge_record(&record.data)
                .ok_or(QuarantineError::Corrupt("charge record does not decode"))?;
            match outcome {
                Outcome::Charged | Outcome::InsufficientBalance | Outcome::AboveLimit => {
                    Resolution::Settled(outcome)
                }
                other => {
                    return Err(QuarantineError::RecordUnresolvable {
                        charge: shown_charge,
                        outcome: other.token(),
                    });
                }
            }
        }
        None if ledger <= charge.last_ledger => Resolution::Readmitted,
        None if ledger <= charge.last_ledger.saturating_add(CHARGE_RECORD_GRACE) => {
            Resolution::Expired
        }
        None => return Err(QuarantineError::RecordGone { charge: shown_charge, ledger }),
    };
    let evidence = match resolution {
        Resolution::Settled(outcome) => {
            format!("record of charge {shown_charge} holds {} at ledger {ledger}", outcome.token())
        }
        _ => format!(
            "no record of charge {shown_charge} at ledger {ledger}; its last ledger is {}",
            charge.last_ledger
        ),
    };
    Ok((resolution, evidence))
}

/// Records the resolution and applies it, through the database function
/// that is the only way out of quarantine.
pub async fn resolve(
    pool: &PgPool,
    charge: Uuid,
    resolution: Resolution,
    evidence: &str,
) -> Result<(), QuarantineError> {
    sqlx::query_scalar!(
        "SELECT pay_stellar.resolve_quarantined_charge($1, $2, $3)",
        charge,
        resolution.token(),
        evidence,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| match &source {
        sqlx::Error::Database(db) if db.message().contains("not quarantined") => {
            QuarantineError::NotQuarantined(charge)
        }
        _ => QuarantineError::Store { operation: "resolve quarantined charge", source },
    })?;
    Ok(())
}
