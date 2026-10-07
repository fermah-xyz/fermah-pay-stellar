use anyhow::Context;
use clap::Parser;
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_gateway::config::Config;
use fermah_pay_stellar_gateway::ledger::{LedgerApi, LedgerPolicy};
use fermah_pay_stellar_gateway::server::{ServerLimits, serve, serve_x402};
use fermah_pay_stellar_gateway::store::{Quotas, Store};
use fermah_pay_stellar_gateway::{shutdown, startup, telemetry};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let _telemetry = telemetry::init("fermah-pay-stellar-gateway", config.metrics_addr)?;
    telemetry::register_gateway_counters();

    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections.get())
        .connect(&config.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    let rpc =
        RpcClient::new(&config.rpc_url, config.rpc_timeout()).context("building RPC client")?;
    startup::verify_rpc(&rpc, config.network).await.context("checking the RPC network")?;
    let store = Store::new(pool).with_quotas(Quotas {
        buyers_per_deployment: config.max_new_buyers_per_day,
        deposits_per_buyer: config.max_deposits_per_buyer_per_day,
        withdrawals_per_buyer: config.max_withdrawals_per_buyer_per_day,
        min_withdrawal: config.min_withdrawal,
        mandate_changes_per_buyer: config.max_mandate_changes_per_buyer_per_day,
        deposits_per_deployment: config.max_deposits_per_deployment_per_day,
        mandate_changes_per_deployment: config.max_mandate_changes_per_deployment_per_day,
    });
    telemetry::register_deployment_counters(
        &store.bound_deployments(config.network).await.context("reading bound deployments")?,
    );
    // A vault buyer's limit change or exit made outside the gateway must be
    // known before any charge admitted without it could settle after the
    // change takes effect.
    anyhow::ensure!(
        config.vault_events_stale_ledgers + config.charge_validity_ledgers
            < fermah_pay_stellar_chain::prepaid::VAULT_NOTICE_LEDGERS,
        "PAY_STELLAR_VAULT_EVENTS_STALE_LEDGERS plus PAY_STELLAR_CHARGE_VALIDITY_LEDGERS must be \
         below the vault's notice of {} ledgers",
        fermah_pay_stellar_chain::prepaid::VAULT_NOTICE_LEDGERS
    );
    let ledger = LedgerApi::new(
        store.clone(),
        rpc,
        config.network,
        LedgerPolicy {
            authorization_validity_ledgers: config.deposit_authorization_ledgers.get(),
            charge_validity_ledgers: config.charge_validity_ledgers,
            min_mandate_period_secs: config.min_mandate_period_secs,
            max_mandate_ledgers: config.max_mandate_ledgers,
            withdrawals_to_other_accounts: config.withdrawals_to_other_accounts,
            max_buyer_resource_fee: config.max_buyer_resource_fee_stroops,
            vault_events_stale_ledgers: config.vault_events_stale_ledgers,
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
    // One shutdown signal stops both servers.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown::signal().await;
        let _ = stop.send(true);
    });
    let until_stopped = |mut stopped: tokio::sync::watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };
    let x402 = match config.x402_listen_addr {
        Some(addr) => {
            let listener =
                TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
            tracing::info!(x402_listen_addr = %addr, "x402 interface serving");
            let ledger = ledger.clone();
            let stopped = until_stopped(stopped.clone());
            Some(tokio::spawn(async move { serve_x402(listener, ledger, limits, stopped).await }))
        }
        None => None,
    };
    serve(listener, store, config.network, ledger, limits, until_stopped(stopped))
        .await
        .context("serving gRPC")?;
    if let Some(x402) = x402 {
        x402.await.context("x402 task")?.context("serving x402")?;
    }
    tracing::info!("gateway stopped");
    Ok(())
}
