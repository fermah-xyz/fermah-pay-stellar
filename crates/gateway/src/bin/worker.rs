//! Settlement worker: sends signed deposits and charge batches to Stellar and
//! applies their outcomes. Several processes may run with the same
//! configuration: the one holding the operator's lease settles, and the
//! others take over when it stops.

use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use fermah_pay_stellar_chain::prepaid::MAX_BATCH;
use fermah_pay_stellar_chain::rpc::{FeePercentile, RpcClient};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::lease::{self, Lease};
use fermah_pay_stellar_gateway::submission::{Engine, FeePolicy, Keys, Policy, SystemClock};
use fermah_pay_stellar_gateway::worker::{Reserve, Settings, Worker};
use fermah_pay_stellar_gateway::{shutdown, signing, startup, telemetry};
use sqlx::postgres::PgPoolOptions;

#[derive(Debug, Parser)]
#[command(
    name = "fermah-pay-stellar-worker",
    version,
    about = "Settlement worker: sends deposits, charge batches, withdrawals, mandates and recurring charges to Stellar, and decides their outcomes"
)]
struct Config {
    /// PostgreSQL URL of a login role that is a member of `pay_stellar_worker`.
    #[arg(long, env = "PAY_STELLAR_WORKER_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    /// The Stellar network the worker settles on: `stellar:testnet`,
    /// `stellar:pubnet`, or `stellar:local`.
    #[arg(long, env = "PAY_STELLAR_NETWORK")]
    network: Network,
    /// Stellar RPC endpoint of that network; checked against it at startup.
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
    /// several keep sending while one waits. Nothing but these workers may
    /// submit from these accounts. A key reference is the path of a seed file; see
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
    /// Key reference of the treasury that pays buyer withdrawals. The worker
    /// sends the withdrawals of the deployments bound with this treasury;
    /// without it, withdrawals wait and lapse.
    #[arg(long, env = "PAY_STELLAR_TREASURY_KEY_FILE")]
    treasury_key_file: Option<String>,
    /// `G...` account of the cold reserve. With the treasury key, the worker
    /// moves what the treasury holds above the ceiling there; the worker
    /// never holds the reserve's keys.
    #[arg(long, env = "PAY_STELLAR_COLD_RESERVE", requires = "treasury_key_file")]
    cold_reserve: Option<AccountAddress>,
    /// USDC base units below which the treasury needs topping up from the
    /// reserve (reported, not acted on).
    #[arg(long, env = "PAY_STELLAR_HOT_TREASURY_FLOOR", requires = "cold_reserve")]
    hot_treasury_floor: Option<i64>,
    /// USDC base units a sweep leaves in the treasury, or more while held
    /// withdrawals need it.
    #[arg(long, env = "PAY_STELLAR_HOT_TREASURY_TARGET", requires = "cold_reserve")]
    hot_treasury_target: Option<i64>,
    /// USDC base units above which the treasury is swept.
    #[arg(long, env = "PAY_STELLAR_HOT_TREASURY_CEILING", requires = "cold_reserve")]
    hot_treasury_ceiling: Option<i64>,
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
    /// Headroom over the simulated resource fee, in percent; the unused part
    /// is refunded.
    #[arg(long, env = "PAY_STELLAR_RESOURCE_FEE_MARGIN_PERCENT", default_value = "20")]
    resource_fee_margin_percent: u8,
    /// Largest resource fee, in stroops, of a deposit, withdrawal, mandate or
    /// revocation, or of a restore one needs: a contract-account wallet runs
    /// its own code in it at the operator's expense. Above it, the request
    /// is not sent and ends expired.
    #[arg(
        long,
        env = "PAY_STELLAR_MAX_BUYER_RESOURCE_FEE_STROOPS",
        default_value_t = fermah_pay_stellar_gateway::submission::DEFAULT_MAX_BUYER_RESOURCE_FEE
    )]
    max_buyer_resource_fee_stroops: i64,
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
    /// Seconds a request the network refused in simulation, or that needed a
    /// restore, waits before it is tried again.
    #[arg(long, env = "PAY_STELLAR_RETRY_AFTER_SECS", default_value = "30")]
    retry_after_secs: u64,
    /// Milliseconds between rounds while there is work.
    #[arg(long, env = "PAY_STELLAR_BUSY_POLL_MILLIS", default_value = "1000")]
    busy_poll_millis: u64,
    /// Milliseconds between rounds while there is none.
    #[arg(long, env = "PAY_STELLAR_IDLE_POLL_MILLIS", default_value = "2000")]
    idle_poll_millis: u64,
    /// Seconds a worker's lease lasts without renewal: how long a standby
    /// waits to take over from a worker that stopped without releasing it.
    #[arg(long, env = "PAY_STELLAR_LEASE_SECS", default_value = "15")]
    lease_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let _telemetry = telemetry::init("fermah-pay-stellar-worker", config.metrics_addr)?;
    if config.max_batch == 0 || config.max_batch > MAX_BATCH {
        bail!("max batch must be between 1 and {MAX_BATCH}");
    }
    if config.lease_secs < 3 {
        bail!("the lease must last at least 3 seconds");
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
    telemetry::register_worker_counters(&keys.source_addresses());
    let operator =
        signing::open(&config.operator_key_file).await.context("opening the operator key")?;
    let reserve = match config.cold_reserve.clone() {
        Some(cold) => {
            let (Some(floor), Some(target), Some(ceiling)) = (
                config.hot_treasury_floor,
                config.hot_treasury_target,
                config.hot_treasury_ceiling,
            ) else {
                bail!("a cold reserve needs the hot treasury's floor, target and ceiling");
            };
            Some(Reserve::new(cold, floor, target, ceiling).context("hot treasury settings")?)
        }
        None => None,
    };
    let treasury = match &config.treasury_key_file {
        Some(reference) => {
            Some(signing::open(reference).await.context("opening the treasury key")?)
        }
        None => None,
    };

    let rpc = RpcClient::new(&config.rpc_url, Duration::from_secs(config.rpc_timeout_secs))
        .context("building RPC client")?;
    startup::verify_rpc(&rpc, config.network).await.context("checking the RPC network")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&config.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    let lease = Lease::new(
        pool.clone(),
        format!("worker:{}:{}", config.network.caip2(), operator.address()),
        Duration::from_secs(config.lease_secs),
    );
    tracing::info!(
        network = %config.network,
        sources = ?keys.source_addresses().iter().map(ToString::to_string).collect::<Vec<_>>(),
        fee_source = %keys.fee_source_address(),
        operator = %operator.address(),
        treasury = ?treasury.as_ref().map(|t| t.address().to_string()),
        cold_reserve = ?reserve.as_ref().map(|r| r.cold().to_string()),
        lease_holder = %lease.holder(),
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
            max_buyer_resource_fee: config.max_buyer_resource_fee_stroops,
        },
    );
    let mut worker = Worker::new(
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
    if let Some(treasury) = treasury {
        worker = worker.with_treasury(treasury);
    }
    if let Some(reserve) = reserve {
        worker = worker.with_reserve(reserve);
    }
    lease::lead(&lease, "worker", shutdown::signal(), |stop| {
        worker.run(
            Duration::from_millis(config.busy_poll_millis),
            Duration::from_millis(config.idle_poll_millis),
            stop.wait(),
        )
    })
    .await;
    tracing::info!("worker stopped");
    Ok(())
}
