//! PostgreSQL access for the API process. Every buyer query binds the full
//! scope (product, seller deployment, network), so a row created under one
//! scope is invisible under any other.

use fermah_pay_stellar_domain::{AccountAddress, ExternalRef, Network};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::scope::Scope;

#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
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
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
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
        wallet: &AccountAddress,
    ) -> Result<CreateBuyerOutcome, StoreError> {
        // ON CONFLICT without a target covers both the reference and the
        // wallet uniqueness constraints; a concurrent identical insert waits
        // for the first and then reads its committed row below.
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
            wallet.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Query { operation: "insert buyer", source })?;

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
            Some(existing) if existing.wallet_address == wallet.as_str() => {
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
