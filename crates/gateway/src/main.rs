use anyhow::Context;
use clap::Parser;
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_gateway::config::Config;
use fermah_pay_stellar_gateway::ledger::{DepositPolicy, LedgerApi};
use fermah_pay_stellar_gateway::server::serve;
use fermah_pay_stellar_gateway::shutdown;
use fermah_pay_stellar_gateway::store::Store;
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
    rpc.verify_network(config.network).await.context("checking the RPC network")?;
    let store = Store::new(pool);
    let ledger = LedgerApi::new(
        store.clone(),
        rpc,
        config.network,
        DepositPolicy {
            authorization_validity_ledgers: config.deposit_authorization_ledgers.get(),
        },
    );
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(listen_addr = %config.listen_addr, network = %config.network, "gateway serving");

    serve(listener, store, config.network, ledger, shutdown::signal())
        .await
        .context("serving gRPC")?;
    tracing::info!("gateway stopped");
    Ok(())
}
