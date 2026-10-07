//! Quarantined recurring charges. No balance is held for them, so a
//! quarantine only keeps the period taken until an operator records, from
//! the network's evidence, what the contract did: the attempt's record while
//! it lives, then the contract's `recurring` events.

use fermah_pay_stellar_chain::prepaid::{
    CHARGE_RECORD_GRACE, Custody, PrepaidDeployment, RecurringOutcome, recurring_record,
};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_domain::AccountAddress;
use sqlx::PgPool;
use uuid::Uuid;

use super::{QuarantineError, contract_id, store};
use crate::events::{EventLog, RecurringSearch, search_recurring};
use crate::submission::{Chain, stored_authorization_horizon};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedRecurring {
    pub id: Uuid,
    pub seller_deployment_id: Uuid,
    pub owner: AccountAddress,
    pub charge_id: [u8; 32],
    pub cycle: i32,
    pub amount: i64,
    pub last_ledger: u32,
    /// The last ledger in which the batch that carried the attempt could
    /// still be included.
    pub authorization_horizon: u32,
    /// The ledger at which that batch was authorized, if recorded.
    pub authorized_from: Option<u32>,
    pub reason: Option<String>,
    pub transaction_hash: Option<String>,
    pub deployment: PrepaidDeployment,
}

pub async fn quarantined_recurring(
    pool: &PgPool,
    only: Option<Uuid>,
) -> Result<Vec<QuarantinedRecurring>, QuarantineError> {
    let rows = sqlx::query!(
        r#"
        SELECT r.id, r.seller_deployment_id, r.charge_id, r.cycle, r.amount, r.last_ledger,
               r.last_error, b.wallet_address, l.contract_address, l.usdc_address,
               l.treasury_address, s.outer_hash AS "outer_hash?",
               s.envelope_xdr AS "envelope_xdr?", s.authorized_from_ledger
        FROM pay_stellar.recurring_charges r
        JOIN pay_stellar.buyers b ON b.id = r.buyer_id
        JOIN pay_stellar.ledger_contracts l
          ON l.seller_deployment_id = r.seller_deployment_id AND l.network = r.network
        LEFT JOIN pay_stellar.submissions s ON s.id = r.submission_id
        WHERE r.state = 'quarantined' AND ($1::uuid IS NULL OR r.id = $1)
        ORDER BY r.settled_at
        "#,
        only,
    )
    .fetch_all(pool)
    .await
    .map_err(store("read quarantined recurring charges"))?;
    rows.into_iter()
        .map(|row| {
            let address = |raw: &str| {
                raw.parse::<AccountAddress>()
                    .map_err(|_| QuarantineError::Corrupt("address outside the CHECK constraint"))
            };
            Ok(QuarantinedRecurring {
                id: row.id,
                seller_deployment_id: row.seller_deployment_id,
                owner: address(&row.wallet_address)?,
                charge_id: <[u8; 32]>::try_from(row.charge_id)
                    .map_err(|_| QuarantineError::Corrupt("charge identifier is not 32 bytes"))?,
                cycle: row.cycle,
                amount: row.amount,
                last_ledger: u32::try_from(row.last_ledger)
                    .map_err(|_| QuarantineError::Corrupt("charge last ledger out of range"))?,
                authorization_horizon: row
                    .envelope_xdr
                    .as_deref()
                    .and_then(stored_authorization_horizon)
                    .ok_or(QuarantineError::Corrupt(
                        "quarantined recurring charge without a readable envelope",
                    ))?,
                authorized_from: row.authorized_from_ledger.and_then(|l| u32::try_from(l).ok()),
                reason: row.last_error,
                transaction_hash: row.outer_hash.as_deref().map(hex_lower),
                deployment: PrepaidDeployment {
                    contract: contract_id(&row.contract_address)?,
                    usdc: contract_id(&row.usdc_address)?,
                    custody: Custody::Treasury(address(&row.treasury_address)?),
                },
            })
        })
        .collect()
}

/// What the network establishes for `charge`, and the evidence to record:
/// its record while one can exist, the contract's events after that.
pub async fn prove_recurring<C: Chain + EventLog>(
    chain: &C,
    charge: &QuarantinedRecurring,
) -> Result<(RecurringOutcome, String), QuarantineError> {
    let shown = hex_lower(&charge.charge_id);
    let key = charge.deployment.recurring_record_key(&charge.owner, &charge.charge_id);
    let read = chain.ledger_entries(&[key]).await.map_err(QuarantineError::Chain)?;
    let ledger = read.latest_ledger;
    if ledger <= charge.authorization_horizon {
        return Err(QuarantineError::ReadBeforeHorizon {
            charge: shown,
            ledger,
            horizon: charge.authorization_horizon,
        });
    }
    if let Some(record) = read.entries.first() {
        let outcome = recurring_record(&record.data)
            .ok_or(QuarantineError::Corrupt("recurring record does not decode"))?;
        if matches!(outcome, RecurringOutcome::Duplicate | RecurringOutcome::Expired) {
            return Err(QuarantineError::RecordUnresolvable {
                charge: shown,
                outcome: outcome.token(),
            });
        }
        let evidence =
            format!("record of attempt {shown} holds {} at ledger {ledger}", outcome.token());
        return Ok((outcome, evidence));
    }
    if ledger <= charge.last_ledger {
        // Past the batch's horizon and with no record, nothing applied it,
        // but another batch could still settle it until its last ledger.
        return Err(QuarantineError::EventsBeforeLastLedger {
            charge: shown,
            latest: ledger,
            last_ledger: charge.last_ledger,
        });
    }
    if ledger <= charge.last_ledger.saturating_add(CHARGE_RECORD_GRACE) {
        let evidence = format!(
            "no record of attempt {shown} at ledger {ledger}; its last ledger is {}",
            charge.last_ledger
        );
        return Ok((RecurringOutcome::Expired, evidence));
    }
    let from = charge
        .authorized_from
        .ok_or(QuarantineError::NoAuthorizationLedger { charge: shown.clone() })?;
    let search = search_recurring(
        chain,
        &charge.deployment.contract,
        &charge.owner,
        &charge.charge_id,
        from,
        charge.last_ledger,
    )
    .await
    .map_err(QuarantineError::Chain)?;
    let entries = match search {
        RecurringSearch::Complete { entries, .. } => entries,
        RecurringSearch::Pruned { oldest } => {
            return Err(QuarantineError::EventsPruned { charge: shown, from, oldest });
        }
        RecurringSearch::Behind { latest } => {
            return Err(QuarantineError::EventsBeforeLastLedger {
                charge: shown,
                latest,
                last_ledger: charge.last_ledger,
            });
        }
        RecurringSearch::Unreadable { event } => {
            return Err(QuarantineError::EventUnreadable {
                charge: shown,
                event: event.to_string(),
            });
        }
    };
    match entries.iter().find(|found| found.settles()) {
        Some(found) if found.entry.amount != i128::from(charge.amount) => {
            Err(QuarantineError::AmountMismatch {
                hash: hex_lower(&found.transaction_hash),
                charge: shown,
                found: found.entry.amount,
                expected: charge.amount,
            })
        }
        Some(found) => Ok((
            found.entry.outcome,
            format!(
                "recurring event {} in transaction {} settled attempt {shown} as {}",
                found.event,
                hex_lower(&found.transaction_hash),
                found.entry.outcome.token()
            ),
        )),
        None if entries.iter().any(|found| found.is_duplicate()) => {
            Err(QuarantineError::RecordUnresolvable { charge: shown, outcome: "duplicate" })
        }
        None => Ok((
            RecurringOutcome::Expired,
            format!(
                "no recurring event settles attempt {shown} in ledgers {from} to {}",
                charge.last_ledger
            ),
        )),
    }
}

/// Records `outcome` for the quarantined attempt, with `evidence`, through
/// the only function that may take it out of quarantine.
pub async fn resolve_recurring(
    pool: &PgPool,
    charge: Uuid,
    outcome: RecurringOutcome,
    evidence: &str,
) -> Result<(), QuarantineError> {
    sqlx::query_scalar!(
        "SELECT pay_stellar.resolve_quarantined_recurring_charge($1, $2, $3)",
        charge,
        outcome.token(),
        evidence,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| match &source {
        sqlx::Error::Database(db) if db.message().contains("not quarantined") => {
            QuarantineError::NotQuarantined(charge)
        }
        _ => QuarantineError::Store { operation: "resolve quarantined recurring charge", source },
    })?;
    Ok(())
}
