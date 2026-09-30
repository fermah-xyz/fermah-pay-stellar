//! Chain observer: reads each bound deployment's contract events, matches
//! them against the gateway's records, and reconciles the treasury, the
//! contract's totals and the database. It holds no key; its database role
//! can append observations and findings and nothing else.

use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use fermah_pay_stellar_chain::rpc::{MAX_EVENTS_PER_PAGE, RpcClient};
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::observer::{Observer, Settings, StartPosition};
use fermah_pay_stellar_gateway::submission::SystemClock;
use fermah_pay_stellar_gateway::{shutdown, telemetry};
use sqlx::postgres::PgPoolOptions;

#[derive(Debug, Parser)]
#[command(name = "fermah-pay-stellar-observer", version, about)]
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let _telemetry = telemetry::init("fermah-pay-stellar-observer", config.metrics_addr)?;
    if config.page_size == 0 || config.page_size > MAX_EVENTS_PER_PAGE {
        bail!("page size must be between 1 and {MAX_EVENTS_PER_PAGE}");
    }
    if config.confirmations == 0 || config.poll_secs == 0 || config.reconcile_secs == 0 {
        bail!("confirmations, poll and reconcile intervals must be at least 1");
    }
    let rpc = RpcClient::new(&config.rpc_url, Duration::from_secs(config.rpc_timeout_secs))
        .context("building RPC client")?;
    rpc.verify_network(config.network).await.context("checking the RPC network")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    tracing::info!(network = %config.network, start = ?config.start, "observer starting");
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
    );
    observer
        .run(
            Duration::from_secs(config.poll_secs),
            Duration::from_secs(config.reconcile_secs),
            Duration::from_secs(config.max_backoff_secs.max(config.poll_secs)),
            shutdown::signal(),
        )
        .await;
    tracing::info!("observer stopped");
    Ok(())
}
