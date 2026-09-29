use anyhow::Context;
use clap::Parser;
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_gateway::config::Config;
use fermah_pay_stellar_gateway::ledger::{LedgerApi, LedgerPolicy};
use fermah_pay_stellar_gateway::server::{ServerLimits, serve};
use fermah_pay_stellar_gateway::store::Store;
use fermah_pay_stellar_gateway::{shutdown, startup};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::parse();

    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections.get())
        .connect(&config.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    let rpc =
        RpcClient::new(&config.rpc_url, config.rpc_timeout()).context("building RPC client")?;
    startup::verify_rpc(&rpc, config.network).await.context("checking the RPC network")?;
    let store = Store::new(pool);
    let ledger = LedgerApi::new(
        store.clone(),
        rpc,
        config.network,
        LedgerPolicy {
            authorization_validity_ledgers: config.deposit_authorization_ledgers.get(),
            charge_validity_ledgers: config.charge_validity_ledgers,
        },
    );
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(listen_addr = %config.listen_addr, network = %config.network, "gateway serving");

    let limits = ServerLimits {
        max_concurrent_requests: config.max_concurrent_requests.get(),
        request_timeout: std::time::Duration::from_secs(config.request_timeout_secs),
    };
    serve(listener, store, config.network, ledger, limits, shutdown::signal())
        .await
        .context("serving gRPC")?;
    tracing::info!("gateway stopped");
    Ok(())
}
