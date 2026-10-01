//! Mandates for recurring charges and their revocations, under the API role.

use fermah_pay_stellar_domain::IdempotencyKey;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{Insertion, address, is_violation_of, query};
use crate::scope::Scope;
use crate::store::{Store, StoreError};

const MANDATE_KEY: &str = "mandates_idempotency_key";
const REVOCATION_KEY: &str = "revocations_idempotency_key";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MandateState {
    AwaitingSignature,
    Signed,
    Submitted,
    Active,
    Failed,
    Expired,
    Replaced,
    Revoked,
    Ended,
}

impl MandateState {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "awaiting_signature" => Self::AwaitingSignature,
            "signed" => Self::Signed,
            "submitted" => Self::Submitted,
            "active" => Self::Active,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            "replaced" => Self::Replaced,
            "revoked" => Self::Revoked,
            "ended" => Self::Ended,
            _ => return Err(StoreError::Corrupt("mandate state outside the CHECK constraint")),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MandateRecord {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub wallet: fermah_pay_stellar_domain::AccountAddress,
    pub mandate_id: [u8; 32],
    pub amount: i64,
    pub period_secs: i64,
    pub cycles: i32,
    pub live_until: i64,
    pub state: MandateState,
    pub authorization_xdr: String,
    pub signed_authorization_xdr: Option<String>,
    pub expiration_ledger: i64,
    pub starts_at: Option<i64>,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

pub struct NewMandate<'a> {
    pub buyer_id: Uuid,
    pub key: &'a IdempotencyKey,
    pub mandate_id: [u8; 32],
    pub amount: i64,
    pub period_secs: u64,
    pub cycles: u32,
    pub live_until: u32,
    pub authorization_xdr: &'a str,
    pub expiration_ledger: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevocationState {
    AwaitingSignature,
    Signed,
    Submitted,
    Confirmed,
    Failed,
    Expired,
}

impl RevocationState {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "awaiting_signature" => Self::AwaitingSignature,
            "signed" => Self::Signed,
            "submitted" => Self::Submitted,
            "confirmed" => Self::Confirmed,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => return Err(StoreError::Corrupt("revocation state outside the CHECK constraint")),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationRecord {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub wallet: fermah_pay_stellar_domain::AccountAddress,
    pub state: RevocationState,
    pub authorization_xdr: String,
    pub signed_authorization_xdr: Option<String>,
    pub expiration_ledger: i64,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

pub struct NewRevocation<'a> {
    pub buyer_id: Uuid,
    pub key: &'a IdempotencyKey,
    pub authorization_xdr: &'a str,
    pub expiration_ledger: u32,
}

fn hash32(bytes: Vec<u8>) -> Result<[u8; 32], StoreError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| StoreError::Corrupt("identifier is not 32 bytes"))
}

impl Store {
    /// Inserts a mandate awaiting the buyer's signature, within the buyer's
    /// daily quota of mandate changes: each one the worker sends costs the
    /// operator a fee, and the approval's rent.
    pub async fn insert_mandate(
        &self,
        scope: &Scope,
        mandate: &NewMandate<'_>,
    ) -> Result<Insertion, StoreError> {
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await.map_err(query("begin mandate"))?;
        let quota = self.quotas.mandate_changes_per_buyer;
        if !self.within_quota(&mut tx, scope, mandate.buyer_id, "mandates", quota).await? {
            return Ok(Insertion::QuotaExceeded);
        }
        let deployment_quota = self.quotas.mandate_changes_per_deployment;
        if !self.within_deployment_quota(&mut tx, scope, "mandates", deployment_quota).await? {
            return Ok(Insertion::DeploymentQuotaExceeded);
        }
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.mandates
                (id, buyer_id, seller_deployment_id, network, idempotency_key, mandate_id, amount,
                 period_secs, cycles, live_until, authorization_xdr, expiration_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
            id,
            mandate.buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            mandate.key.as_str(),
            mandate.mandate_id.as_slice(),
            mandate.amount,
            i64::try_from(mandate.period_secs)
                .map_err(|_| StoreError::Corrupt("period beyond the column's range"))?,
            i32::try_from(mandate.cycles)
                .map_err(|_| StoreError::Corrupt("cycles beyond the column's range"))?,
            i64::from(mandate.live_until),
            mandate.authorization_xdr,
            i64::from(mandate.expiration_ledger),
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(error) if is_violation_of(&error, MANDATE_KEY) => return Ok(Insertion::KeyTaken),
            Err(source) => return Err(StoreError::Query { operation: "insert mandate", source }),
        }
        tx.commit().await.map_err(query("commit mandate"))?;
        Ok(Insertion::Created(id))
    }

    pub async fn mandate(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<MandateRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT m.id, m.buyer_id, b.wallet_address, m.mandate_id, m.amount, m.period_secs,
                   m.cycles, m.live_until, m.state, m.authorization_xdr,
                   m.signed_authorization_xdr, m.expiration_ledger, m.starts_at, m.created_at,
                   s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.mandates m
            JOIN pay_stellar.buyers b
              ON b.id = m.buyer_id AND b.seller_deployment_id = m.seller_deployment_id
             AND b.network = m.network
            LEFT JOIN pay_stellar.submissions s ON s.id = m.submission_id
            WHERE m.seller_deployment_id = $1 AND m.network = $2
              AND (m.id = $3 OR m.idempotency_key = $4)
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            key.map(IdempotencyKey::as_str),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read mandate"))?;
        row.map(|row| {
            Ok(MandateRecord {
                id: row.id,
                buyer_id: row.buyer_id,
                wallet: address(&row.wallet_address)?,
                mandate_id: hash32(row.mandate_id)?,
                amount: row.amount,
                period_secs: row.period_secs,
                cycles: row.cycles,
                live_until: row.live_until,
                state: MandateState::parse(&row.state)?,
                authorization_xdr: row.authorization_xdr,
                signed_authorization_xdr: row.signed_authorization_xdr,
                expiration_ledger: row.expiration_ledger,
                starts_at: row.starts_at,
                transaction_hash: row.outer_hash,
                ledger: row.ledger,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    /// Stores the verified signed entry if the mandate still awaits one.
    /// Returns whether it did; a concurrent submission may have won.
    pub async fn sign_mandate(
        &self,
        scope: &Scope,
        id: Uuid,
        signed_authorization_xdr: &str,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query!(
            r#"
            UPDATE pay_stellar.mandates
            SET state = 'signed', signed_authorization_xdr = $4, signed_at = now()
            WHERE id = $1 AND seller_deployment_id = $2 AND network = $3
              AND state = 'awaiting_signature'
            "#,
            id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            signed_authorization_xdr,
        )
        .execute(&self.pool)
        .await
        .map_err(query("sign mandate"))?;
        Ok(updated.rows_affected() == 1)
    }

    /// Inserts a revocation awaiting the buyer's signature, within the same
    /// daily quota as mandates.
    pub async fn insert_revocation(
        &self,
        scope: &Scope,
        revocation: &NewRevocation<'_>,
    ) -> Result<Insertion, StoreError> {
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await.map_err(query("begin revocation"))?;
        let quota = self.quotas.mandate_changes_per_buyer;
        if !self.within_quota(&mut tx, scope, revocation.buyer_id, "mandates", quota).await? {
            return Ok(Insertion::QuotaExceeded);
        }
        let deployment_quota = self.quotas.mandate_changes_per_deployment;
        if !self.within_deployment_quota(&mut tx, scope, "mandates", deployment_quota).await? {
            return Ok(Insertion::DeploymentQuotaExceeded);
        }
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.revocations
                (id, buyer_id, seller_deployment_id, network, idempotency_key, authorization_xdr,
                 expiration_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            id,
            revocation.buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            revocation.key.as_str(),
            revocation.authorization_xdr,
            i64::from(revocation.expiration_ledger),
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(error) if is_violation_of(&error, REVOCATION_KEY) => {
                return Ok(Insertion::KeyTaken);
            }
            Err(source) => {
                return Err(StoreError::Query { operation: "insert revocation", source });
            }
        }
        tx.commit().await.map_err(query("commit revocation"))?;
        Ok(Insertion::Created(id))
    }

    pub async fn revocation(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<RevocationRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT r.id, r.buyer_id, b.wallet_address, r.state, r.authorization_xdr,
                   r.signed_authorization_xdr, r.expiration_ledger, r.created_at,
                   s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.revocations r
            JOIN pay_stellar.buyers b
              ON b.id = r.buyer_id AND b.seller_deployment_id = r.seller_deployment_id
             AND b.network = r.network
            LEFT JOIN pay_stellar.submissions s ON s.id = r.submission_id
            WHERE r.seller_deployment_id = $1 AND r.network = $2
              AND (r.id = $3 OR r.idempotency_key = $4)
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            key.map(IdempotencyKey::as_str),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read revocation"))?;
        row.map(|row| {
            Ok(RevocationRecord {
                id: row.id,
                buyer_id: row.buyer_id,
                wallet: address(&row.wallet_address)?,
                state: RevocationState::parse(&row.state)?,
                authorization_xdr: row.authorization_xdr,
                signed_authorization_xdr: row.signed_authorization_xdr,
                expiration_ledger: row.expiration_ledger,
                transaction_hash: row.outer_hash,
                ledger: row.ledger,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    pub async fn sign_revocation(
        &self,
        scope: &Scope,
        id: Uuid,
        signed_authorization_xdr: &str,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query!(
            r#"
            UPDATE pay_stellar.revocations
            SET state = 'signed', signed_authorization_xdr = $4, signed_at = now()
            WHERE id = $1 AND seller_deployment_id = $2 AND network = $3
              AND state = 'awaiting_signature'
            "#,
            id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            signed_authorization_xdr,
        )
        .execute(&self.pool)
        .await
        .map_err(query("sign revocation"))?;
        Ok(updated.rows_affected() == 1)
    }
}

const RECURRING_KEY: &str = "recurring_charges_idempotency_key";
const ONE_PER_PERIOD: &str = "recurring_charges_one_per_period";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurringChargeState {
    Admitted,
    Submitted,
    Charged,
    Refused,
    Quarantined,
}

impl RecurringChargeState {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "admitted" => Self::Admitted,
            "submitted" => Self::Submitted,
            "charged" => Self::Charged,
            "refused" => Self::Refused,
            "quarantined" => Self::Quarantined,
            _ => {
                return Err(StoreError::Corrupt(
                    "recurring charge state outside the CHECK constraint",
                ));
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecurringChargeRecord {
    pub id: Uuid,
    /// The mandate row's id, which the API names the mandate by.
    pub mandate: Uuid,
    pub buyer_id: Uuid,
    pub cycle: i32,
    pub amount: i64,
    pub state: RecurringChargeState,
    pub outcome: Option<String>,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

pub struct NewRecurringCharge<'a> {
    pub mandate: Uuid,
    pub buyer_id: Uuid,
    pub key: &'a IdempotencyKey,
    pub cycle: u32,
    pub charge_id: [u8; 32],
    pub amount: i64,
    pub last_ledger: u32,
}

/// What admitting a recurring charge did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurringAdmission {
    Created(Uuid),
    /// Another row holds the idempotency key.
    KeyTaken,
    /// The period already has an attempt in flight, charged or quarantined.
    PeriodTaken,
}

impl Store {
    /// Admits a charge of one period. The partial unique index on the
    /// mandate and period refuses a second live or charged attempt, also
    /// against a concurrent request.
    pub async fn admit_recurring_charge(
        &self,
        scope: &Scope,
        charge: &NewRecurringCharge<'_>,
    ) -> Result<RecurringAdmission, StoreError> {
        let id = Uuid::now_v7();
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.recurring_charges
                (id, mandate_row_id, buyer_id, seller_deployment_id, network, idempotency_key,
                 cycle, charge_id, amount, last_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
            id,
            charge.mandate,
            charge.buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            charge.key.as_str(),
            i32::try_from(charge.cycle)
                .map_err(|_| StoreError::Corrupt("period beyond the column's range"))?,
            charge.charge_id.as_slice(),
            charge.amount,
            i64::from(charge.last_ledger),
        )
        .execute(&self.pool)
        .await;
        match inserted {
            Ok(_) => Ok(RecurringAdmission::Created(id)),
            Err(error) if is_violation_of(&error, RECURRING_KEY) => {
                Ok(RecurringAdmission::KeyTaken)
            }
            Err(error) if is_violation_of(&error, ONE_PER_PERIOD) => {
                Ok(RecurringAdmission::PeriodTaken)
            }
            Err(source) => Err(StoreError::Query { operation: "admit recurring charge", source }),
        }
    }

    pub async fn recurring_charge(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<RecurringChargeRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT r.id, r.mandate_row_id, r.buyer_id, r.cycle, r.amount, r.state, r.outcome,
                   r.created_at, s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.recurring_charges r
            LEFT JOIN pay_stellar.submissions s ON s.id = r.submission_id
            WHERE r.seller_deployment_id = $1 AND r.network = $2
              AND (r.id = $3 OR r.idempotency_key = $4)
            "#,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            key.map(IdempotencyKey::as_str),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(query("read recurring charge"))?;
        row.map(|row| {
            Ok(RecurringChargeRecord {
                id: row.id,
                mandate: row.mandate_row_id,
                buyer_id: row.buyer_id,
                cycle: row.cycle,
                amount: row.amount,
                state: RecurringChargeState::parse(&row.state)?,
                outcome: row.outcome,
                transaction_hash: row.outer_hash,
                ledger: row.ledger,
                created_at: row.created_at,
            })
        })
        .transpose()
    }
}
