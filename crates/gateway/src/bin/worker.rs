//! Settlement worker: sends signed deposits and charge batches to Stellar and
//! applies their outcomes. Run one process per source account.

use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use fermah_pay_stellar_chain::prepaid::MAX_BATCH;
use fermah_pay_stellar_chain::rpc::{FeePercentile, RpcClient};
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::submission::{Engine, FeePolicy, Keys, Policy, SystemClock};
use fermah_pay_stellar_gateway::worker::{Settings, Worker};
use fermah_pay_stellar_gateway::{shutdown, signing, startup, telemetry};
use sqlx::postgres::PgPoolOptions;

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
    /// Address to serve Prometheus metrics on (`/metrics`); not served when
    /// unset.
    #[arg(long, env = "PAY_STELLAR_METRICS_ADDR")]
    metrics_addr: Option<std::net::SocketAddr>,
    #[arg(long, env = "PAY_STELLAR_RPC_TIMEOUT_SECS", default_value = "10")]
    rpc_timeout_secs: u64,
    /// Key references, comma-separated, of the accounts that sequence
    /// transactions. Each account has at most one transaction in flight, so
    /// several keep sending while one waits. No other process may submit
    /// from these accounts. A key reference is the path of a seed file; see
    /// docs/self-hosting/keys.md.
    #[arg(long, env = "PAY_STELLAR_SOURCE_KEY_FILE", value_delimiter = ',', required = true)]
    source_key_file: Vec<String>,
    /// Key reference of the account that pays fees.
    #[arg(long, env = "PAY_STELLAR_FEE_SOURCE_KEY_FILE")]
    fee_source_key_file: String,
    /// Key reference of the ledger contracts' operator. The worker serves
    /// exactly the deployments bound with this operator.
    #[arg(long, env = "PAY_STELLAR_OPERATOR_KEY_FILE")]
    operator_key_file: String,
    /// Lowest inclusion bid per operation, in stroops. An envelope bids more
    /// when recent fees, or the expiry of the previous envelope, call for it.
    #[arg(long, env = "PAY_STELLAR_INCLUSION_FEE", default_value = "10000")]
    inclusion_fee: u32,
    /// Highest inclusion bid per operation, in stroops. Set it equal to the
    /// lowest for a fixed bid.
    #[arg(long, env = "PAY_STELLAR_MAX_INCLUSION_FEE", default_value = "1000000")]
    max_inclusion_fee: u32,
    /// Percentile of recent Soroban inclusion fees to bid at: 10 to 90 in
    /// steps of 10, 95, 99 or max.
    #[arg(long, env = "PAY_STELLAR_INCLUSION_FEE_PERCENTILE", default_value = "90")]
    inclusion_fee_percentile: FeePercentile,
    #[arg(long, env = "PAY_STELLAR_RESOURCE_FEE_MARGIN_PERCENT", default_value = "20")]
    resource_fee_margin_percent: u8,
    /// Seconds a transaction may be included after it is built.
    #[arg(long, env = "PAY_STELLAR_TRANSACTION_VALIDITY_SECS", default_value = "60")]
    transaction_validity_secs: u64,
    /// Seconds the local clock may differ from the latest ledger's close time
    /// before the worker stops building transactions; below the validity.
    #[arg(long, env = "PAY_STELLAR_MAX_CLOCK_SKEW_SECS", default_value = "20")]
    max_clock_skew_secs: u64,
    /// Ledgers the operator's authorization of a batch stays valid.
    #[arg(long, env = "PAY_STELLAR_OPERATOR_AUTHORIZATION_LEDGERS", default_value = "24")]
    operator_authorization_ledgers: u32,
    /// Charges per batch, at most 98, the contract's limit.
    #[arg(long, env = "PAY_STELLAR_MAX_BATCH", default_value = "98")]
    max_batch: usize,
    /// Spendable XLM, in stroops, below which the fee account pays only for
    /// finishing work in flight (10 XLM by default).
    #[arg(long, env = "PAY_STELLAR_FEE_FLOOR_STROOPS", default_value = "100000000")]
    fee_floor_stroops: i64,
    /// Ledgers of life left below which a served contract's instance and
    /// code are extended (about 7 days, as the contract itself uses).
    #[arg(long, env = "PAY_STELLAR_TTL_THRESHOLD_LEDGERS", default_value = "120960")]
    ttl_threshold_ledgers: u32,
    /// Ledgers of life an extension gives them (about 30 days).
    #[arg(long, env = "PAY_STELLAR_TTL_EXTEND_TO_LEDGERS", default_value = "518400")]
    ttl_extend_to_ledgers: u32,
    /// Seconds between reads of the contracts' remaining life.
    #[arg(long, env = "PAY_STELLAR_TTL_CHECK_SECS", default_value = "600")]
    ttl_check_secs: u64,
    #[arg(long, env = "PAY_STELLAR_RETRY_AFTER_SECS", default_value = "30")]
    retry_after_secs: u64,
    #[arg(long, env = "PAY_STELLAR_BUSY_POLL_MILLIS", default_value = "1000")]
    busy_poll_millis: u64,
    #[arg(long, env = "PAY_STELLAR_IDLE_POLL_MILLIS", default_value = "2000")]
    idle_poll_millis: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let _telemetry = telemetry::init("fermah-pay-stellar-worker", config.metrics_addr)?;
    if config.max_batch == 0 || config.max_batch > MAX_BATCH {
        bail!("max batch must be between 1 and {MAX_BATCH}");
    }
    if config.max_clock_skew_secs >= config.transaction_validity_secs {
        bail!("max clock skew must be below the transaction validity");
    }
    let fees = FeePolicy::new(
        config.inclusion_fee,
        config.max_inclusion_fee,
        config.inclusion_fee_percentile,
    )
    .context("inclusion fee settings")?;
    let mut sources = Vec::new();
    for reference in &config.source_key_file {
        sources.push(signing::open(reference).await.context("opening a source key")?);
    }
    let fee_source =
        signing::open(&config.fee_source_key_file).await.context("opening the fee key")?;
    let keys = Keys::new(sources, fee_source).context("source account settings")?;
    let operator =
        signing::open(&config.operator_key_file).await.context("opening the operator key")?;

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
        sources = ?keys.source_addresses().iter().map(ToString::to_string).collect::<Vec<_>>(),
        fee_source = %keys.fee_source_address(),
        operator = %operator.address(),
        inclusion_fee_floor = fees.floor,
        inclusion_fee_cap = fees.cap,
        inclusion_fee_percentile = %fees.percentile,
        "worker starting"
    );
    let engine = Engine::new(
        pool.clone(),
        rpc,
        SystemClock,
        config.network,
        keys,
        Policy {
            fees,
            resource_fee_margin_percent: config.resource_fee_margin_percent,
            validity: Duration::from_secs(config.transaction_validity_secs),
            max_clock_skew: Duration::from_secs(config.max_clock_skew_secs),
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
            fee_floor_stroops: config.fee_floor_stroops,
            ttl_threshold_ledgers: config.ttl_threshold_ledgers,
            ttl_extend_to_ledgers: config.ttl_extend_to_ledgers,
            ttl_check_every: Duration::from_secs(config.ttl_check_secs),
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
