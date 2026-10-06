//! Chain observer: reads each bound deployment's contract events, matches
//! them against the gateway's records, and reconciles the treasury, the
//! contract's totals and the database. It holds no key; its database role
//! can append observations and findings and nothing else. Several processes
//! may run for one network: the one holding the network's lease observes,
//! and the others take over when it stops.

use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use fermah_pay_stellar_chain::rpc::{MAX_EVENTS_PER_PAGE, RpcClient};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::lease::{self, Lease};
use fermah_pay_stellar_gateway::observer::{Observer, Settings, StartPosition};
use fermah_pay_stellar_gateway::submission::SystemClock;
use fermah_pay_stellar_gateway::{shutdown, telemetry};
use sqlx::postgres::PgPoolOptions;

#[derive(Debug, Parser)]
#[command(
    name = "fermah-pay-stellar-observer",
    version,
    about = "Chain observer: reads the ledger contract's events, matches them against the gateway's records and reconciles the treasury"
)]
struct Config {
    /// PostgreSQL URL of a login role that is a member of
    /// `pay_stellar_observer`.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_DATABASE_URL", hide_env_values = true)]
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
    /// Where to start reading a deployment observed for the first time:
    /// `oldest` (the oldest ledger the RPC retains), `latest`, or a ledger
    /// number such as the contract's deployment ledger.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_START", default_value = "oldest")]
    start: StartPosition,
    /// Seconds after an event's ledger within which the records must agree
    /// with it; longer than the deposit authorization validity.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_SETTLE_WITHIN_SECS", default_value = "7200")]
    settle_within_secs: u64,
    /// Seconds between reads of new events.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_POLL_SECS", default_value = "5")]
    poll_secs: u64,
    /// Seconds between reconciliations.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_RECONCILE_SECS", default_value = "60")]
    reconcile_secs: u64,
    /// Consecutive reconciliations a discrepancy must persist through before
    /// it is recorded.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_CONFIRMATIONS", default_value = "3")]
    confirmations: u32,
    /// Events per RPC call, at most 10000.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_PAGE_SIZE", default_value = "1000")]
    page_size: u32,
    /// Longest wait between retries after a failed round, in seconds.
    #[arg(long, env = "PAY_STELLAR_OBSERVER_MAX_BACKOFF_SECS", default_value = "300")]
    max_backoff_secs: u64,
    /// Seconds an observer's lease lasts without renewal: how long a
    /// standby waits to take over from one that stopped without releasing
    /// it.
    #[arg(long, env = "PAY_STELLAR_LEASE_SECS", default_value = "15")]
    lease_secs: u64,
    /// Cold reserves, comma-separated `TREASURY:RESERVE` pairs of `G...`
    /// accounts: the reserve's USDC counts with its treasury's when
    /// checking that the treasury covers what the contract owes.
    #[arg(long, env = "PAY_STELLAR_COLD_RESERVES", value_delimiter = ',')]
    cold_reserves: Vec<String>,

    /// The Wasm each contract must run, as `CONTRACT:HASH` (the contract's
    /// `C...` address and the hex SHA-256 of its Wasm), comma-separated. A
    /// listed contract running any other code is a critical `code_changed`
    /// finding.
    #[arg(long, env = "PAY_STELLAR_EXPECTED_WASM", value_delimiter = ',')]
    expected_wasm: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let _telemetry = telemetry::init("fermah-pay-stellar-observer", config.metrics_addr)?;
    telemetry::register_observer_counters();
    if config.page_size == 0 || config.page_size > MAX_EVENTS_PER_PAGE {
        bail!("page size must be between 1 and {MAX_EVENTS_PER_PAGE}");
    }
    if config.lease_secs < 3 {
        bail!("the lease must last at least 3 seconds");
    }
    if config.confirmations == 0 || config.poll_secs == 0 || config.reconcile_secs == 0 {
        bail!("confirmations, poll and reconcile intervals must be at least 1");
    }
    let mut reserves = std::collections::HashMap::new();
    for pair in &config.cold_reserves {
        let (treasury, reserve) =
            pair.split_once(':').context("a cold reserve is written TREASURY:RESERVE")?;
        let treasury: AccountAddress = treasury.parse().context("the treasury address")?;
        let reserve: AccountAddress = reserve.parse().context("the reserve address")?;
        if reserves.insert(treasury, reserve).is_some() {
            bail!("a treasury is listed with two cold reserves");
        }
    }
    let mut expected_code = std::collections::HashMap::new();
    for pair in &config.expected_wasm {
        let (contract, hash) =
            pair.split_once(':').context("an expected Wasm is written CONTRACT:HASH")?;
        let contract =
            stellar_strkey::Contract::from_string(contract).context("the contract address")?.0;
        let hash: [u8; 32] = (0..64)
            .step_by(2)
            .map(|i| hash.get(i..i + 2).and_then(|byte| u8::from_str_radix(byte, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .and_then(|bytes| bytes.try_into().ok())
            .filter(|_| hash.len() == 64)
            .context("the Wasm hash is 64 hex digits")?;
        if expected_code.insert(contract, hash).is_some() {
            bail!("a contract is listed with two expected Wasm hashes");
        }
    }
    let rpc = RpcClient::new(&config.rpc_url, Duration::from_secs(config.rpc_timeout_secs))
        .context("building RPC client")?;
    rpc.verify_network(config.network).await.context("checking the RPC network")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    let lease = Lease::new(
        pool.clone(),
        format!("observer:{}", config.network.caip2()),
        Duration::from_secs(config.lease_secs),
    );
    tracing::info!(network = %config.network, start = ?config.start, lease_holder = %lease.holder(), "observer starting");
    let observer = Observer::new(
        pool,
        rpc,
        SystemClock,
        config.network,
        Settings {
            start: config.start,
            page_size: config.page_size,
            settle_within: Duration::from_secs(config.settle_within_secs),
            confirmations: config.confirmations,
            max_pages_per_round: 100,
        },
    )
    .with_reserves(reserves)
    .with_expected_code(expected_code);
    lease::lead(&lease, "observer", shutdown::signal(), |stop| {
        observer.run(
            Duration::from_secs(config.poll_secs),
            Duration::from_secs(config.reconcile_secs),
            Duration::from_secs(config.max_backoff_secs.max(config.poll_secs)),
            stop.wait(),
        )
    })
    .await;
    tracing::info!("observer stopped");
    Ok(())
}
