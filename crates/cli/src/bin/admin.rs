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
use fermah_pay_stellar_chain::prepaid::{Custody, PrepaidDeployment, contract_config};
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::quarantine::{self, QuarantinedCharge};
use sqlx::PgPool;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../db/migrations");

#[derive(Parser)]
#[command(
    name = "fermah-pay-stellar-admin",
    version,
    about = "Operator administration: migrations, products, seller deployments, ledger bindings, API keys and quarantine resolution"
)]
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
        /// The prepaid ledger's treasury account.
        #[arg(long, required_unless_present = "vault", conflicts_with = "vault")]
        treasury: Option<AccountAddress>,
        /// The contract is a prepaid vault, which holds its own USDC.
        #[arg(long)]
        vault: bool,
        #[arg(long)]
        operator: AccountAddress,
    },
    /// Revoke an API key.
    RevokeApiKey {
        #[arg(long)]
        key_id: Uuid,
    },
    /// Move a deployment's binding to the operator and treasury its ledger
    /// contract now names, after a role rotation on the contract. The
    /// accounts are read from the contract's `get_config`; the contract and
    /// its USDC must still match the binding.
    SyncLedger {
        #[arg(long)]
        deployment_id: Uuid,
        #[arg(long, env = "PAY_STELLAR_RPC_URL")]
        rpc_url: String,
    },
    /// Acknowledge a deployment's books as they stand, so the chain
    /// observer's reconciliation compares only what changes from now on
    /// (operator role). For after events were lost to the RPC node's
    /// retention, or observation began late; refused while any charge or
    /// deposit is in flight.
    ObserverBaseline {
        #[arg(long)]
        deployment_id: Uuid,
        /// Why the books are acknowledged, kept with the baseline.
        #[arg(long)]
        note: String,
        #[arg(long)]
        network: Network,
        #[arg(long, env = "PAY_STELLAR_RPC_URL")]
        rpc_url: String,
    },
    /// List quarantined charges (operator role).
    QuarantinedCharges,
    /// Resolve a quarantined charge (operator role). The resolution is read
    /// from the network, never taken from the operator: with `--record`, from
    /// the charge's record on the contract (its outcome; or, if there is none,
    /// readmission while the charge is within its last ledger and expiry
    /// shortly after); with `--transaction`, from the contract's settlement
    /// of the charge in that transaction, once the record has lapsed; with
    /// `--events`, from every `charges` event of the contract between the
    /// batch's authorization and the charge's last ledger (its settlement, or
    /// expiry if there is none), once the record has lapsed.
    ResolveCharge {
        #[arg(long)]
        charge_id: Uuid,
        /// Hex hash of the transaction in which the contract settled the
        /// charge.
        #[arg(
            long,
            conflicts_with_all = ["record", "events"],
            required_unless_present_any = ["record", "events"]
        )]
        transaction: Option<String>,
        #[arg(long, conflicts_with = "events")]
        record: bool,
        #[arg(long)]
        events: bool,
        #[arg(long)]
        network: Network,
        #[arg(long, env = "PAY_STELLAR_RPC_URL")]
        rpc_url: String,
    },
    /// List quarantined recurring charges (operator role).
    QuarantinedRecurringCharges,
    /// Resolve a quarantined recurring charge (operator role), from the
    /// network: the attempt's record while it can exist, then every
    /// `recurring` event of the contract between the batch's authorization
    /// and the attempt's last ledger.
    ResolveRecurringCharge {
        #[arg(long)]
        recurring_charge_id: Uuid,
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
        Command::BindLedger { deployment_id, contract, treasury, vault: _, operator } => {
            let binding = LedgerBinding { contract, treasury, operator };
            issuance::bind_ledger_contract(&pool, deployment_id, &binding).await?;
            serde_json::json!({
                "deployment_id": deployment_id.to_string(),
                "contract": binding.contract,
            })
        }
        Command::ObserverBaseline { deployment_id, note, network, rpc_url } => {
            use fermah_pay_stellar_gateway::observer::{Observer, Settings, StartPosition};
            let rpc = RpcClient::new(&rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(network).await.context("checking the RPC network")?;
            // Only the chain reading and the deployment list are used.
            let observer = Observer::new(
                pool.clone(),
                rpc,
                fermah_pay_stellar_gateway::submission::SystemClock,
                network,
                Settings {
                    start: StartPosition::Latest,
                    page_size: 100,
                    settle_within: Duration::from_secs(7200),
                    confirmations: 3,
                    max_pages_per_round: 1,
                },
            );
            let ledger = observer.record_baseline(&pool, deployment_id, &note).await?;
            serde_json::json!({
                "deployment_id": deployment_id.to_string(),
                "baseline_ledger": ledger,
            })
        }
        Command::SyncLedger { deployment_id, rpc_url } => {
            let bound = issuance::ledger_binding(&pool, deployment_id).await?;
            let rpc = RpcClient::new(&rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(bound.network).await.context("checking the RPC network")?;
            let contract = contract_id(&bound.contract)?;
            let usdc = contract_id(&bound.usdc)?;
            let custody = bound.treasury.clone().map_or(Custody::Vault, Custody::Treasury);
            let deployment = PrepaidDeployment { contract, usdc, custody };
            let value = rpc
                .read_contract(&bound.operator, deployment.get_config_call())
                .await
                .context("reading the contract's configuration")?
                .context("get_config returned nothing")?;
            let config = contract_config(&value).context("get_config returned another shape")?;
            if config.usdc != usdc {
                bail!("the contract's USDC is not the bound one");
            }
            let treasury = config
                .treasury
                .context("the contract has no treasury: it is not a prepaid ledger")?;
            let evidence = format!(
                "get_config of {}: operator {}, treasury {}",
                bound.contract, config.operator, treasury
            );
            let changed = issuance::sync_ledger_binding(
                &pool,
                deployment_id,
                &config.operator,
                &treasury,
                &evidence,
            )
            .await?;
            serde_json::json!({
                "deployment_id": deployment_id.to_string(),
                "changed": changed,
                "operator": config.operator.to_string(),
                "treasury": treasury.to_string(),
            })
        }
        Command::QuarantinedCharges => {
            let charges = quarantine::quarantined_charges(&pool, None).await?;
            serde_json::Value::Array(charges.iter().map(describe).collect())
        }
        Command::ResolveCharge { charge_id, transaction, record, events, network, rpc_url } => {
            let rpc = RpcClient::new(&rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(network).await.context("checking the RPC network")?;
            let charge = quarantine::quarantined_charges(&pool, Some(charge_id))
                .await?
                .pop()
                .with_context(|| format!("no quarantined charge {charge_id}"))?;
            let (resolution, evidence) = match (transaction, record, events) {
                (_, true, _) => quarantine::prove_from_record(&rpc, &charge).await?,
                (_, _, true) => {
                    let from = quarantine::authorization_ledger(&pool, charge_id).await?;
                    quarantine::prove_from_events(&rpc, &charge, from).await?
                }
                (Some(hash), false, false) => {
                    let hash = parse_hash(&hash)?;
                    quarantine::prove_from_transaction(&rpc, &charge, &hash).await?
                }
                (None, false, false) => bail!("give --transaction, --record or --events"),
            };
            quarantine::resolve(&pool, charge_id, resolution, &evidence).await?;
            serde_json::json!({
                "charge_id": charge_id.to_string(),
                "resolution": format!("{resolution:?}"),
                "evidence": evidence,
            })
        }
        Command::QuarantinedRecurringCharges => {
            let charges = quarantine::recurring::quarantined_recurring(&pool, None).await?;
            serde_json::Value::Array(
                charges
                    .iter()
                    .map(|charge| {
                        serde_json::json!({
                            "recurring_charge_id": charge.id.to_string(),
                            "seller_deployment_id": charge.seller_deployment_id.to_string(),
                            "owner": charge.owner.to_string(),
                            "contract_charge_id": fermah_pay_stellar_chain::rpc::hex_lower(&charge.charge_id),
                            "cycle": charge.cycle,
                            "amount": charge.amount,
                            "last_ledger": charge.last_ledger,
                            "reason": charge.reason,
                            "transaction_hash": charge.transaction_hash,
                        })
                    })
                    .collect(),
            )
        }
        Command::ResolveRecurringCharge { recurring_charge_id, network, rpc_url } => {
            let rpc = RpcClient::new(&rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(network).await.context("checking the RPC network")?;
            let charge =
                quarantine::recurring::quarantined_recurring(&pool, Some(recurring_charge_id))
                    .await?
                    .pop()
                    .with_context(|| {
                        format!("no quarantined recurring charge {recurring_charge_id}")
                    })?;
            let (outcome, evidence) = quarantine::recurring::prove_recurring(&rpc, &charge).await?;
            quarantine::recurring::resolve_recurring(
                &pool,
                recurring_charge_id,
                outcome,
                &evidence,
            )
            .await?;
            serde_json::json!({
                "recurring_charge_id": recurring_charge_id.to_string(),
                "outcome": outcome.token(),
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
        "contract_charge_id": fermah_pay_stellar_chain::rpc::hex_lower(&charge.charge_id),
        "last_ledger": charge.last_ledger,
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

fn contract_id(strkey: &str) -> anyhow::Result<[u8; 32]> {
    stellar_strkey::Contract::from_string(strkey)
        .map(|contract| contract.0)
        .map_err(|_| anyhow::anyhow!("{strkey} is not a contract address"))
}
