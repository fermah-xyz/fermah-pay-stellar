//! Tenant provisioning, run by operators under the `pay_stellar_issuer`
//! role. Kept out of the API process: a request-serving credential cannot
//! mint keys.

use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, contract_strkey};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::auth::{generate_token, token_digest};

#[derive(Debug, thiserror::Error)]
pub enum IssuanceError {
    #[error("database operation `{operation}` failed")]
    Query {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("operating system randomness unavailable")]
    Randomness(#[source] getrandom::Error),
    #[error("no such seller deployment")]
    UnknownDeployment,
    #[error("not a contract address")]
    InvalidContract,
}

/// A freshly issued key. The token exists only in this value: the database
/// stores its digest, so it cannot be shown again.
pub struct IssuedKey {
    pub id: Uuid,
    pub token: Zeroizing<String>,
}

impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedKey").field("id", &self.id).field("token", &"[REDACTED]").finish()
    }
}

pub async fn create_product(pool: &PgPool, name: &str) -> Result<Uuid, IssuanceError> {
    sqlx::query_scalar!(
        "INSERT INTO pay_stellar.products (id, name) VALUES ($1, $2) RETURNING id",
        Uuid::now_v7(),
        name,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "insert product", source })
}

pub async fn create_seller_deployment(
    pool: &PgPool,
    product_id: Uuid,
    name: &str,
    network: Network,
) -> Result<Uuid, IssuanceError> {
    sqlx::query_scalar!(
        r#"
        INSERT INTO pay_stellar.seller_deployments (id, product_id, name, network)
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
        Uuid::now_v7(),
        product_id,
        name,
        network.caip2(),
    )
    .fetch_one(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "insert seller deployment", source })
}

/// Issues a key for `seller_deployment_id`. The key's network prefix is taken
/// from the deployment row, never from the caller.
pub async fn issue_api_key(
    pool: &PgPool,
    seller_deployment_id: Uuid,
    label: &str,
) -> Result<IssuedKey, IssuanceError> {
    let network = deployment_network(pool, seller_deployment_id).await?;
    let token = generate_token(network).map_err(IssuanceError::Randomness)?;
    let digest = token_digest(&token);
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO pay_stellar.api_keys (id, seller_deployment_id, network, token_sha256, label)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
        Uuid::now_v7(),
        seller_deployment_id,
        network.caip2(),
        digest.as_slice(),
        label,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "insert api key", source })?;
    Ok(IssuedKey { id, token })
}

async fn deployment_network(
    pool: &PgPool,
    seller_deployment_id: Uuid,
) -> Result<Network, IssuanceError> {
    let network = sqlx::query_scalar!(
        "SELECT network FROM pay_stellar.seller_deployments WHERE id = $1",
        seller_deployment_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "read deployment network", source })?
    .ok_or(IssuanceError::UnknownDeployment)?;
    network.parse().map_err(|_| IssuanceError::UnknownDeployment)
}

/// Revokes a key; returns whether a live key was revoked.
pub async fn revoke_api_key(pool: &PgPool, key_id: Uuid) -> Result<bool, IssuanceError> {
    let result = sqlx::query!(
        "UPDATE pay_stellar.api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        key_id,
    )
    .execute(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "revoke api key", source })?;
    Ok(result.rows_affected() == 1)
}

/// The on-chain accounts a deployment's ledger contract was constructed with.
#[derive(Clone, Debug)]
pub struct LedgerBinding {
    /// `C...` address of the ledger contract.
    pub contract: String,
    /// The prepaid ledger's treasury; `None` binds a vault, which holds its
    /// own USDC.
    pub treasury: Option<AccountAddress>,
    pub operator: AccountAddress,
}

/// Binds a deployment to its ledger contract, once. The USDC contract is not
/// an input: it is derived from Circle's USDC on the deployment's network, so
/// a deployment cannot be pointed at another token that calls itself USDC.
pub async fn bind_ledger_contract(
    pool: &PgPool,
    seller_deployment_id: Uuid,
    binding: &LedgerBinding,
) -> Result<(), IssuanceError> {
    stellar_strkey::Contract::from_string(&binding.contract)
        .map_err(|_| IssuanceError::InvalidContract)?;
    let network = deployment_network(pool, seller_deployment_id).await?;
    let usdc = contract_strkey(asset_contract_id(&circle_usdc(network), network));
    sqlx::query!(
        r#"
        INSERT INTO pay_stellar.ledger_contracts
            (seller_deployment_id, network, contract_address, usdc_address, treasury_address,
             operator_address, custody)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        seller_deployment_id,
        network.caip2(),
        binding.contract,
        usdc,
        binding.treasury.as_ref().map(AccountAddress::as_str),
        binding.operator.as_str(),
        if binding.treasury.is_some() { "treasury" } else { "vault" },
    )
    .execute(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "insert ledger binding", source })?;
    Ok(())
}

/// What a deployment is bound to, as the issuer role reads it.
#[derive(Clone, Debug)]
pub struct BoundLedger {
    pub network: Network,
    pub contract: String,
    pub usdc: String,
    /// `None` for a vault, which holds its own USDC.
    pub treasury: Option<AccountAddress>,
    pub operator: AccountAddress,
}

pub async fn ledger_binding(
    pool: &PgPool,
    seller_deployment_id: Uuid,
) -> Result<BoundLedger, IssuanceError> {
    let row = sqlx::query!(
        r#"
        SELECT network, contract_address, usdc_address, treasury_address, operator_address
        FROM pay_stellar.ledger_contracts WHERE seller_deployment_id = $1
        "#,
        seller_deployment_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "read ledger binding", source })?
    .ok_or(IssuanceError::UnknownDeployment)?;
    let account = |raw: &str| raw.parse().map_err(|_| IssuanceError::UnknownDeployment);
    Ok(BoundLedger {
        network: row.network.parse().map_err(|_| IssuanceError::UnknownDeployment)?,
        contract: row.contract_address,
        usdc: row.usdc_address,
        treasury: row.treasury_address.as_deref().map(account).transpose()?,
        operator: account(&row.operator_address)?,
    })
}

/// Moves the binding to the operator and treasury the contract now names,
/// through the database function that records every change. Returns whether
/// anything changed. The caller reads `operator` and `treasury` from the
/// contract; they are never typed in.
pub async fn sync_ledger_binding(
    pool: &PgPool,
    seller_deployment_id: Uuid,
    operator: &AccountAddress,
    treasury: &AccountAddress,
    evidence: &str,
) -> Result<bool, IssuanceError> {
    sqlx::query_scalar!(
        r#"SELECT pay_stellar.sync_ledger_binding($1, $2, $3, $4) AS "changed!""#,
        seller_deployment_id,
        operator.as_str(),
        treasury.as_str(),
        evidence,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| IssuanceError::Query { operation: "sync ledger binding", source })
}
