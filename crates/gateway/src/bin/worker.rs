//! Settlement worker: sends signed deposits and charge batches to Stellar and
//! applies their outcomes. Run one process per source account.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::prepaid::MAX_BATCH;
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::submission::{Engine, Keys, Policy, SystemClock};
use fermah_pay_stellar_gateway::worker::{Settings, Worker};
use fermah_pay_stellar_gateway::{shutdown, startup};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

#[derive(Debug, Parser)]
#[command(name = "fermah-pay-stellar-worker", version, about)]
struct Config {
    /// PostgreSQL URL of a login role that is a member of `pay_stellar_worker`.
    #[arg(long, env = "PAY_STELLAR_WORKER_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long, env = "PAY_STELLAR_NETWORK")]
    network: Network,
    #[arg(long, env = "PAY_STELLAR_RPC_URL")]
    rpc_url: String,
    #[arg(long, env = "PAY_STELLAR_RPC_TIMEOUT_SECS", default_value = "10")]
    rpc_timeout_secs: u64,
    /// File holding the `S...` seed of the account that sequences every
    /// transaction. No other process may submit from this account.
    #[arg(long, env = "PAY_STELLAR_SOURCE_KEY_FILE")]
    source_key_file: PathBuf,
    /// File holding the seed of the account that pays fees.
    #[arg(long, env = "PAY_STELLAR_FEE_SOURCE_KEY_FILE")]
    fee_source_key_file: PathBuf,
    /// File holding the seed of the ledger contracts' operator. The worker
    /// serves exactly the deployments bound with this operator.
    #[arg(long, env = "PAY_STELLAR_OPERATOR_KEY_FILE")]
    operator_key_file: PathBuf,
    /// Inclusion bid per operation, in stroops.
    #[arg(long, env = "PAY_STELLAR_INCLUSION_FEE", default_value = "10000")]
    inclusion_fee: u32,
    #[arg(long, env = "PAY_STELLAR_RESOURCE_FEE_MARGIN_PERCENT", default_value = "20")]
    resource_fee_margin_percent: u8,
    /// Seconds a transaction may be included after it is built.
    #[arg(long, env = "PAY_STELLAR_TRANSACTION_VALIDITY_SECS", default_value = "60")]
    transaction_validity_secs: u64,
    /// Ledgers the operator's authorization of a batch stays valid.
    #[arg(long, env = "PAY_STELLAR_OPERATOR_AUTHORIZATION_LEDGERS", default_value = "24")]
    operator_authorization_ledgers: u32,
    /// Charges per batch, at most 98, the contract's limit.
    #[arg(long, env = "PAY_STELLAR_MAX_BATCH", default_value = "98")]
    max_batch: usize,
    #[arg(long, env = "PAY_STELLAR_RETRY_AFTER_SECS", default_value = "30")]
    retry_after_secs: u64,
    #[arg(long, env = "PAY_STELLAR_BUSY_POLL_MILLIS", default_value = "1000")]
    busy_poll_millis: u64,
    #[arg(long, env = "PAY_STELLAR_IDLE_POLL_MILLIS", default_value = "2000")]
    idle_poll_millis: u64,
}

/// Reads a seed file, refusing one other users can read.
fn read_key(path: &Path) -> anyhow::Result<SecretKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is accessible to other users (mode {:o}); restrict it to 0600",
                path.display(),
                mode & 0o777
            );
        }
    }
    let seed = Zeroizing::new(
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    );
    SecretKey::from_strkey(seed.trim()).with_context(|| format!("parsing {}", path.display()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::parse();
    if config.max_batch == 0 || config.max_batch > MAX_BATCH {
        bail!("max batch must be between 1 and {MAX_BATCH}");
    }
    let keys = Keys {
        source: read_key(&config.source_key_file)?,
        fee_source: read_key(&config.fee_source_key_file)?,
    };
    let operator = read_key(&config.operator_key_file)?;

    let rpc = RpcClient::new(&config.rpc_url, Duration::from_secs(config.rpc_timeout_secs))
        .context("building RPC client")?;
    startup::verify_rpc(&rpc, config.network).await.context("checking the RPC network")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&config.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    tracing::info!(
        network = %config.network,
        source = %keys.source.address(),
        operator = %operator.address(),
        "worker starting"
    );
    let engine = Engine::new(
        pool.clone(),
        rpc,
        SystemClock,
        config.network,
        keys,
        Policy {
            inclusion_fee: config.inclusion_fee,
            resource_fee_margin_percent: config.resource_fee_margin_percent,
            validity: Duration::from_secs(config.transaction_validity_secs),
        },
    );
    let worker = Worker::new(
        engine,
        pool,
        operator,
        Settings {
            operator_authorization_ledgers: config.operator_authorization_ledgers,
            retry_after: Duration::from_secs(config.retry_after_secs),
            max_batch: config.max_batch,
        },
    );
    worker
        .run(
            Duration::from_millis(config.busy_poll_millis),
            Duration::from_millis(config.idle_poll_millis),
            shutdown::signal(),
        )
        .await;
    tracing::info!("worker stopped");
    Ok(())
}
