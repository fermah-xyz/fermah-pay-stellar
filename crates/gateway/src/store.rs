//! PostgreSQL access for the API process. Every buyer query binds the full
//! scope (product, seller deployment, network), so a row created under one
//! scope is invisible under any other.

use fermah_pay_stellar_domain::{ChainAddress, ExternalRef, Network};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::scope::Scope;

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) pool: PgPool,
    pub(crate) quotas: Quotas,
}

/// Admission limits against dust: requests that each cost the operator
/// fees, or contract state it pays rent for, whatever their amount. Each
/// count covers the last 24 hours and is checked in the transaction that
/// creates the row, under a lock, so concurrent requests cannot pass it
/// together. A repeated request (same idempotency key or buyer) is answered
/// from its row and counts nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quotas {
    /// New buyers per seller deployment. A buyer's first deposit creates
    /// its account on the contract.
    pub buyers_per_deployment: u32,
    /// Deposits prepared per buyer.
    pub deposits_per_buyer: u32,
    /// Withdrawals prepared per buyer.
    pub withdrawals_per_buyer: u32,
    /// Smallest withdrawal, in USDC base units; the contract has none.
    pub min_withdrawal: i64,
    /// Mandates and revocations prepared per buyer.
    pub mandate_changes_per_buyer: u32,
    /// Deposits prepared across a seller deployment. Each one the worker
    /// sends costs the operator a fee whatever its amount; the per-buyer
    /// quota alone lets the deployment's new buyers multiply it.
    pub deposits_per_deployment: u32,
    /// Mandates and revocations prepared across a seller deployment.
    pub mandate_changes_per_deployment: u32,
}

impl Default for Quotas {
    fn default() -> Self {
        Self {
            buyers_per_deployment: 1_000,
            deposits_per_buyer: 10,
            withdrawals_per_buyer: 5,
            min_withdrawal: 100_000,
            mandate_changes_per_buyer: 5,
            deposits_per_deployment: 2_000,
            mandate_changes_per_deployment: 2_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database operation `{operation}` failed")]
    Query {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("stored row violates a schema invariant: {0}")]
    Corrupt(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuyerRecord {
    pub id: Uuid,
    pub external_ref: String,
    pub wallet_address: String,
    pub network: Network,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateBuyerOutcome {
    Created(BuyerRecord),
    /// The same reference and wallet were registered before.
    Existing(BuyerRecord),
    /// The reference or the wallet is already bound differently.
    Conflict,
    /// The deployment registered its quota of new buyers in the last day.
    QuotaExceeded,
}

#[derive(Clone, Copy, Debug)]
pub enum BuyerLookup<'a> {
    Id(Uuid),
    ExternalRef(&'a ExternalRef),
}

fn parse_network(raw: &str) -> Result<Network, StoreError> {
    raw.parse().map_err(|_| StoreError::Corrupt("network outside the CHECK constraint"))
}

impl Store {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool, quotas: Quotas::default() }
    }

    #[must_use]
    pub const fn with_quotas(mut self, quotas: Quotas) -> Self {
        self.quotas = quotas;
        self
    }

    /// The scope of a live key whose deployment is on `network`.
    pub async fn scope_for_token(
        &self,
        digest: &[u8; 32],
        network: Network,
    ) -> Result<Option<Scope>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT d.id AS seller_deployment_id, d.product_id, d.network
            FROM pay_stellar.api_keys k
            JOIN pay_stellar.seller_deployments d
              ON d.id = k.seller_deployment_id AND d.network = k.network
            WHERE k.token_sha256 = $1
              AND k.revoked_at IS NULL
              AND k.network = $2
            "#,
            digest.as_slice(),
            network.caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Query { operation: "authenticate api key", source })?;
        row.map(|row| {
            Ok(Scope::new(row.product_id, row.seller_deployment_id, parse_network(&row.network)?))
        })
        .transpose()
    }

    /// Registers a buyer, idempotently on (deployment, external reference).
    ///
    /// Replaying an identical registration is decided purely by the stored
    /// row, so a retry returns the original buyer whatever happened in
    /// between.
    pub async fn create_buyer(
        &self,
        scope: &Scope,
        external_ref: &ExternalRef,
        wallet: &ChainAddress,
    ) -> Result<CreateBuyerOutcome, StoreError> {
        let query = |operation| move |source| StoreError::Query { operation, source };
        let mut tx = self.pool.begin().await.map_err(query("begin buyer registration"))?;
        // New buyers of one deployment are counted and inserted one at a
        // time. The key is the deployment's; the first number keeps it apart
        // from other advisory locks.
        sqlx::query!(
            "SELECT pg_advisory_xact_lock(1, hashtext($1::text))",
            scope.seller_deployment_id().to_string(),
        )
        .execute(&mut *tx)
        .await
        .map_err(query("lock deployment registrations"))?;
        let taken = sqlx::query_scalar!(
            r#"
            SELECT EXISTS (SELECT 1 FROM pay_stellar.buyers
                           WHERE seller_deployment_id = $1
                             AND (external_ref = $2 OR wallet_address = $3)) AS "taken!"
            "#,
            scope.seller_deployment_id(),
            external_ref.as_str(),
            wallet.to_string(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(query("check buyer"))?;
        if !taken {
            let recent = sqlx::query_scalar!(
                r#"
                SELECT count(*) AS "recent!" FROM pay_stellar.buyers
                WHERE seller_deployment_id = $1 AND created_at > now() - interval '1 day'
                "#,
                scope.seller_deployment_id(),
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(query("count new buyers"))?;
            if recent >= i64::from(self.quotas.buyers_per_deployment) {
                return Ok(CreateBuyerOutcome::QuotaExceeded);
            }
        }
        // ON CONFLICT without a target covers both the reference and the
        // wallet uniqueness constraints; the existing row is read below.
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.buyers
                (id, product_id, seller_deployment_id, network, external_ref, wallet_address)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT DO NOTHING
            RETURNING id, external_ref, wallet_address, network, created_at
            "#,
            Uuid::now_v7(),
            scope.product_id(),
            scope.seller_deployment_id(),
            scope.network().caip2(),
            external_ref.as_str(),
            wallet.to_string(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(query("insert buyer"))?;
        tx.commit().await.map_err(query("commit buyer registration"))?;

        if let Some(row) = inserted {
            return Ok(CreateBuyerOutcome::Created(BuyerRecord {
                id: row.id,
                external_ref: row.external_ref,
                wallet_address: row.wallet_address,
                network: parse_network(&row.network)?,
                created_at: row.created_at,
            }));
        }
        match self.buyer(scope, BuyerLookup::ExternalRef(external_ref)).await? {
            Some(existing) if existing.wallet_address == *wallet.to_string() => {
                Ok(CreateBuyerOutcome::Existing(existing))
            }
            _ => Ok(CreateBuyerOutcome::Conflict),
        }
    }

    pub async fn buyer(
        &self,
        scope: &Scope,
        lookup: BuyerLookup<'_>,
    ) -> Result<Option<BuyerRecord>, StoreError> {
        let (id, external_ref) = match lookup {
            BuyerLookup::Id(id) => (Some(id), None),
            BuyerLookup::ExternalRef(external_ref) => (None, Some(external_ref.as_str())),
        };
        let row = sqlx::query!(
            r#"
            SELECT id, external_ref, wallet_address, network, created_at
            FROM pay_stellar.buyers
            WHERE product_id = $1
              AND seller_deployment_id = $2
              AND network = $3
              AND (id = $4 OR external_ref = $5)
            "#,
            scope.product_id(),
            scope.seller_deployment_id(),
            scope.network().caip2(),
            id,
            external_ref,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Query { operation: "read buyer", source })?;
        row.map(|row| {
            Ok(BuyerRecord {
                id: row.id,
                external_ref: row.external_ref,
                wallet_address: row.wallet_address,
                network: parse_network(&row.network)?,
                created_at: row.created_at,
            })
        })
        .transpose()
    }
}
