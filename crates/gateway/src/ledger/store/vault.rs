//! A vault buyer's limit changes and exit requests, under the API role.

use fermah_pay_stellar_domain::{ChainAddress, IdempotencyKey};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{Insertion, address, is_violation_of, query};
use crate::scope::Scope;
use crate::store::{Store, StoreError};

const VAULT_REQUEST_KEY: &str = "vault_requests_idempotency_key";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultRequestState {
    AwaitingSignature,
    Signed,
    Submitted,
    Confirmed,
    Failed,
    Expired,
}

impl VaultRequestState {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        Ok(match raw {
            "awaiting_signature" => Self::AwaitingSignature,
            "signed" => Self::Signed,
            "submitted" => Self::Submitted,
            "confirmed" => Self::Confirmed,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => {
                return Err(StoreError::Corrupt(
                    "vault request state outside the CHECK constraint",
                ));
            }
        })
    }
}

/// What a vault request asks the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VaultRequestKind {
    /// A new daily spending limit.
    SetCap { cap: i64 },
    /// An exit of `amount` to `destination`.
    RequestExit { amount: i64, destination: ChainAddress },
}

impl VaultRequestKind {
    const fn token(&self) -> &'static str {
        match self {
            Self::SetCap { .. } => "set_cap",
            Self::RequestExit { .. } => "request_exit",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultRequestRecord {
    pub id: Uuid,
    pub buyer_id: Uuid,
    pub wallet: ChainAddress,
    pub kind: VaultRequestKind,
    pub state: VaultRequestState,
    pub authorization_xdr: String,
    pub signed_authorization_xdr: Option<String>,
    pub expiration_ledger: i64,
    pub transaction_hash: Option<Vec<u8>>,
    pub ledger: Option<i32>,
    pub created_at: OffsetDateTime,
}

pub struct NewVaultRequest<'a> {
    pub buyer_id: Uuid,
    pub key: &'a IdempotencyKey,
    pub kind: VaultRequestKind,
    pub authorization_xdr: &'a str,
    pub expiration_ledger: u32,
}

impl Store {
    /// Inserts a prepared limit change or exit request, unless the buyer or
    /// the deployment used up its quota of changes to a standing
    /// authorization, which it shares with mandates and revocations.
    pub async fn insert_vault_request(
        &self,
        scope: &Scope,
        request: &NewVaultRequest<'_>,
    ) -> Result<Insertion, StoreError> {
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await.map_err(query("begin vault request"))?;
        let quota = self.quotas.mandate_changes_per_buyer;
        if !self.within_quota(&mut tx, scope, request.buyer_id, "mandates", quota).await? {
            return Ok(Insertion::QuotaExceeded);
        }
        let deployment_quota = self.quotas.mandate_changes_per_deployment;
        if !self.within_deployment_quota(&mut tx, scope, "mandates", deployment_quota).await? {
            return Ok(Insertion::DeploymentQuotaExceeded);
        }
        let (cap, amount, destination) = match &request.kind {
            VaultRequestKind::SetCap { cap } => (Some(*cap), None, None),
            VaultRequestKind::RequestExit { amount, destination } => {
                (None, Some(*amount), Some(destination.to_string()))
            }
        };
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.vault_requests
                (id, buyer_id, seller_deployment_id, network, idempotency_key, kind, cap, amount,
                 destination, authorization_xdr, expiration_ledger)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
            id,
            request.buyer_id,
            scope.seller_deployment_id(),
            scope.network().caip2(),
            request.key.as_str(),
            request.kind.token(),
            cap,
            amount,
            destination,
            request.authorization_xdr,
            i64::from(request.expiration_ledger),
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(error) if is_violation_of(&error, VAULT_REQUEST_KEY) => {
                return Ok(Insertion::KeyTaken);
            }
            Err(source) => {
                return Err(StoreError::Query { operation: "insert vault request", source });
            }
        }
        tx.commit().await.map_err(query("commit vault request"))?;
        Ok(Insertion::Created(id))
    }

    /// The caller's vault request by id or idempotency key.
    pub async fn vault_request(
        &self,
        scope: &Scope,
        id: Option<Uuid>,
        key: Option<&IdempotencyKey>,
    ) -> Result<Option<VaultRequestRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT r.id, r.buyer_id, b.wallet_address, r.kind, r.cap, r.amount, r.destination,
                   r.state, r.authorization_xdr, r.signed_authorization_xdr, r.expiration_ledger,
                   r.created_at, s.outer_hash AS "outer_hash?", s.ledger AS "ledger?"
            FROM pay_stellar.vault_requests r
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
        .map_err(query("read vault request"))?;
        row.map(|row| {
            let kind = match (row.kind.as_str(), row.cap, row.amount, row.destination.as_deref()) {
                ("set_cap", Some(cap), None, None) => VaultRequestKind::SetCap { cap },
                ("request_exit", None, Some(amount), Some(destination)) => {
                    VaultRequestKind::RequestExit { amount, destination: address(destination)? }
                }
                _ => return Err(StoreError::Corrupt("vault request outside the CHECK constraint")),
            };
            Ok(VaultRequestRecord {
                id: row.id,
                buyer_id: row.buyer_id,
                wallet: address(&row.wallet_address)?,
                kind,
                state: VaultRequestState::parse(&row.state)?,
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

    /// Stores the verified signed entry if the request still awaits one;
    /// from then on it counts towards admission (see
    /// [`super::reservations`]). Returns whether this call stored it.
    pub async fn sign_vault_request(
        &self,
        scope: &Scope,
        id: Uuid,
        signed_authorization_xdr: &str,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query!(
            r#"
            UPDATE pay_stellar.vault_requests
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
        .map_err(query("sign vault request"))?;
        Ok(updated.rows_affected() == 1)
    }
}
