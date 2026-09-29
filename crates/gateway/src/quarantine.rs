//! Resolving quarantined charges, run by an operator under the
//! `pay_stellar_operator` role.
//!
//! The database accepts any of the four resolutions from that role; what
//! keeps a resolution honest is that it is derived here from the network, not
//! from the operator's judgement. A settled outcome is read from the
//! contract's own `charges` event in the transaction that consumed the
//! charge's sequence; a readmission requires the account's consumed sequence,
//! read now, to be below the charge's.

use fermah_pay_stellar_chain::prepaid::{Outcome, PrepaidDeployment, account_state, settled_in};
use fermah_pay_stellar_chain::rpc::{RpcError, TransactionStatus, hex_lower};
use fermah_pay_stellar_domain::AccountAddress;
use sqlx::PgPool;
use uuid::Uuid;

use crate::submission::Chain;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedCharge {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub seller_deployment_id: Uuid,
    pub owner: AccountAddress,
    pub sequence: u64,
    pub amount: i64,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub transaction_hash: Option<String>,
    pub deployment: PrepaidDeployment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The contract settled the sequence with this consuming outcome.
    Settled(Outcome),
    /// The sequence is still free; the charge goes back to the next batch.
    Readmitted,
}

impl Resolution {
    const fn token(self) -> &'static str {
        match self {
            Self::Settled(outcome) => outcome.token(),
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
    #[error("transaction {hash} settles no charge of {owner} with sequence {sequence}")]
    NotInTransaction { hash: String, owner: AccountAddress, sequence: u64 },
    #[error("transaction {hash} settled sequence {sequence} for {found}, not {expected}")]
    AmountMismatch { hash: String, sequence: u64, found: i128, expected: i64 },
    #[error(
        "transaction {hash} answered {outcome} for sequence {sequence}, which consumes nothing"
    )]
    NotConsumed { hash: String, sequence: u64, outcome: &'static str },
    #[error(
        "the account has consumed sequence {consumed} at ledger {ledger}; sequence {sequence} cannot be sent again"
    )]
    SequenceConsumed { consumed: u64, sequence: u64, ledger: u32 },
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
        SELECT c.id, c.buyer_id, c.seller_deployment_id, c.sequence, c.amount, c.outcome,
               c.last_error, b.wallet_address, l.contract_address, l.usdc_address,
               l.treasury_address, s.outer_hash AS "outer_hash?"
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
                sequence: u64::try_from(row.sequence)
                    .map_err(|_| QuarantineError::Corrupt("negative charge sequence"))?,
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
    let entry = settled_in(meta, &charge.deployment.contract)
        .into_iter()
        .find(|entry| entry.owner == charge.owner && entry.seq == charge.sequence)
        .ok_or_else(|| QuarantineError::NotInTransaction {
            hash: shown.clone(),
            owner: charge.owner.clone(),
            sequence: charge.sequence,
        })?;
    if entry.amount != i128::from(charge.amount) {
        return Err(QuarantineError::AmountMismatch {
            hash: shown,
            sequence: charge.sequence,
            found: entry.amount,
            expected: charge.amount,
        });
    }
    match entry.outcome {
        Outcome::Charged | Outcome::InsufficientBalance | Outcome::AboveLimit => Ok((
            Resolution::Settled(entry.outcome),
            format!(
                "transaction {shown} (ledger {}) settled sequence {} as {}",
                tx.ledger,
                charge.sequence,
                entry.outcome.token()
            ),
        )),
        other => Err(QuarantineError::NotConsumed {
            hash: shown,
            sequence: charge.sequence,
            outcome: other.token(),
        }),
    }
}

/// A readmission, if the account's consumed sequence read now is below the
/// charge's, and the evidence text to record.
pub async fn prove_readmission<C: Chain>(
    chain: &C,
    charge: &QuarantinedCharge,
) -> Result<(Resolution, String), QuarantineError> {
    let key = charge.deployment.account_key(&charge.owner);
    let read = chain.ledger_entries(&[key]).await.map_err(QuarantineError::Chain)?;
    let consumed = match read.entries.first() {
        None => 0,
        Some(record) => account_state(&record.data)
            .map(|(_, seq)| seq)
            .ok_or(QuarantineError::Corrupt("account entry does not decode"))?,
    };
    if consumed >= charge.sequence {
        return Err(QuarantineError::SequenceConsumed {
            consumed,
            sequence: charge.sequence,
            ledger: read.latest_ledger,
        });
    }
    Ok((
        Resolution::Readmitted,
        format!("account consumed sequence {consumed} at ledger {}", read.latest_ledger),
    ))
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
