use anyhow::Context;
use clap::Parser;
use fermah_pay_stellar_gateway::config::Config;
use fermah_pay_stellar_gateway::server::serve;
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
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(listen_addr = %config.listen_addr, network = %config.network, "gateway serving");

    serve(listener, Store::new(pool), config.network, shutdown_signal())
        .await
        .context("serving gRPC")?;
    tracing::info!("gateway stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "installing Ctrl-C handler");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "installing SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
