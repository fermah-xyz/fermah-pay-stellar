use std::net::SocketAddr;
use std::num::NonZeroU32;

use clap::Parser;
use fermah_pay_stellar_domain::Network;

/// Gateway runtime configuration, read from flags or environment.
#[derive(Debug, Parser)]
#[command(name = "fermah-pay-stellar-gateway", version, about)]
pub struct Config {
    /// PostgreSQL URL of a login role that is a member of `pay_stellar_api`.
    #[arg(long, env = "PAY_STELLAR_DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// Address the gRPC API listens on.
    #[arg(long, env = "PAY_STELLAR_LISTEN_ADDR", default_value = "127.0.0.1:50051")]
    pub listen_addr: SocketAddr,

    /// The one Stellar network this process serves (`stellar:testnet` or
    /// `stellar:pubnet`). Keys and deployments of the other network are
    /// rejected.
    #[arg(long, env = "PAY_STELLAR_NETWORK")]
    pub network: Network,

    /// Maximum database connections.
    #[arg(long, env = "PAY_STELLAR_DATABASE_MAX_CONNECTIONS", default_value = "16")]
    pub database_max_connections: NonZeroU32,
}
