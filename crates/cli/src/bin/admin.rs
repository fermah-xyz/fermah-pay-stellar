//! Operator tool for schema migration, tenant provisioning and resolving
//! quarantined charges.
//!
//! `migrate` needs the database owner; `quarantined-charges` and
//! `resolve-charge` need a login role that is a member of
//! `pay_stellar_operator`; the other commands need a member of
//! `pay_stellar_issuer`.

use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::quarantine::{self, QuarantinedCharge};
use sqlx::PgPool;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../db/migrations");

#[derive(Parser)]
#[command(name = "fermah-pay-stellar-admin", version, about)]
struct Cli {
    /// PostgreSQL URL for the role the command needs.
    #[arg(long, env = "PAY_STELLAR_ADMIN_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply pending schema migrations.
    Migrate,
    /// Register a product.
    CreateProduct {
        #[arg(long)]
        name: String,
    },
    /// Register a seller deployment of a product, pinned to one network.
    CreateDeployment {
        #[arg(long)]
        product_id: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        network: Network,
    },
    /// Issue an API key for a deployment. The token is printed once and
    /// cannot be recovered afterwards.
    IssueApiKey {
        #[arg(long)]
        deployment_id: Uuid,
        #[arg(long)]
        label: String,
    },
    /// Bind a deployment to the prepaid ledger contract it settles against,
    /// with the treasury and operator accounts the contract was constructed
    /// with. A binding is permanent.
    BindLedger {
        #[arg(long)]
        deployment_id: Uuid,
        /// `C...` address of the deployed contract.
        #[arg(long)]
        contract: String,
        #[arg(long)]
        treasury: AccountAddress,
        #[arg(long)]
        operator: AccountAddress,
    },
    /// Revoke an API key.
    RevokeApiKey {
        #[arg(long)]
        key_id: Uuid,
    },
    /// List quarantined charges (operator role).
    QuarantinedCharges,
    /// Resolve a quarantined charge (operator role). The resolution is read
    /// from the network, never taken from the operator: either from the
    /// contract's settlement of the charge's sequence in `--transaction`, or,
    /// with `--readmit`, from the account's consumed sequence being below the
    /// charge's, which sends the charge again.
    ResolveCharge {
        #[arg(long)]
        charge_id: Uuid,
        /// Hex hash of the transaction in which the contract settled the
        /// charge's sequence.
        #[arg(long, conflicts_with = "readmit", required_unless_present = "readmit")]
        transaction: Option<String>,
        #[arg(long)]
        readmit: bool,
        #[arg(long)]
        network: Network,
        #[arg(long, env = "PAY_STELLAR_RPC_URL")]
        rpc_url: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let pool = PgPool::connect(&cli.database_url).await.context("connecting to PostgreSQL")?;
    let output = match cli.command {
        Command::Migrate => {
            MIGRATOR.run(&pool).await.context("applying migrations")?;
            serde_json::json!({ "migrated": true })
        }
        Command::CreateProduct { name } => {
            let id = issuance::create_product(&pool, &name).await?;
            serde_json::json!({ "product_id": id.to_string() })
        }
        Command::CreateDeployment { product_id, name, network } => {
            let id = issuance::create_seller_deployment(&pool, product_id, &name, network).await?;
            serde_json::json!({ "deployment_id": id.to_string(), "network": network.caip2() })
        }
        Command::IssueApiKey { deployment_id, label } => {
            let key = issuance::issue_api_key(&pool, deployment_id, &label).await?;
            serde_json::json!({ "key_id": key.id.to_string(), "token": key.token.as_str() })
        }
        Command::BindLedger { deployment_id, contract, treasury, operator } => {
            let binding = LedgerBinding { contract, treasury, operator };
            issuance::bind_ledger_contract(&pool, deployment_id, &binding).await?;
            serde_json::json!({
                "deployment_id": deployment_id.to_string(),
                "contract": binding.contract,
            })
        }
        Command::QuarantinedCharges => {
            let charges = quarantine::quarantined_charges(&pool, None).await?;
            serde_json::Value::Array(charges.iter().map(describe).collect())
        }
        Command::ResolveCharge { charge_id, transaction, readmit, network, rpc_url } => {
            let rpc = RpcClient::new(&rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(network).await.context("checking the RPC network")?;
            let charge = quarantine::quarantined_charges(&pool, Some(charge_id))
                .await?
                .pop()
                .with_context(|| format!("no quarantined charge {charge_id}"))?;
            let (resolution, evidence) = match (transaction, readmit) {
                (_, true) => quarantine::prove_readmission(&rpc, &charge).await?,
                (Some(hash), false) => {
                    let hash = parse_hash(&hash)?;
                    quarantine::prove_from_transaction(&rpc, &charge, &hash).await?
                }
                (None, false) => bail!("give --transaction or --readmit"),
            };
            quarantine::resolve(&pool, charge_id, resolution, &evidence).await?;
            serde_json::json!({
                "charge_id": charge_id.to_string(),
                "resolution": format!("{resolution:?}"),
                "evidence": evidence,
            })
        }
        Command::RevokeApiKey { key_id } => {
            let revoked = issuance::revoke_api_key(&pool, key_id).await?;
            serde_json::json!({ "key_id": key_id.to_string(), "revoked": revoked })
        }
    };
    println!("{output}");
    Ok(())
}

fn describe(charge: &QuarantinedCharge) -> serde_json::Value {
    serde_json::json!({
        "charge_id": charge.id.to_string(),
        "buyer_id": charge.buyer_id.to_string(),
        "seller_deployment_id": charge.seller_deployment_id.to_string(),
        "owner": charge.owner.to_string(),
        "sequence": charge.sequence,
        "amount": charge.amount,
        "contract_outcome": charge.outcome,
        "reason": charge.reason,
        "transaction_hash": charge.transaction_hash,
    })
}

fn parse_hash(hex: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| hex.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()))
        .collect::<Option<Vec<u8>>>()
        .context("transaction hash is not hex")?;
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow::anyhow!("transaction hash is not 32 bytes"))
}
