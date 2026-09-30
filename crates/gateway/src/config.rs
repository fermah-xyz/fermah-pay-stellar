use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::time::Duration;

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

    /// Address to serve Prometheus metrics on (`/metrics`); not served when
    /// unset.
    #[arg(long, env = "PAY_STELLAR_METRICS_ADDR")]
    pub metrics_addr: Option<std::net::SocketAddr>,

    /// Address the x402 facilitator interface (HTTP) listens on; not served
    /// when unset.
    #[arg(long, env = "PAY_STELLAR_X402_LISTEN_ADDR")]
    pub x402_listen_addr: Option<SocketAddr>,

    /// The one Stellar network this process serves (`stellar:testnet` or
    /// `stellar:pubnet`). Keys and deployments of the other network are
    /// rejected.
    #[arg(long, env = "PAY_STELLAR_NETWORK")]
    pub network: Network,

    /// Stellar RPC endpoint of `network`; checked against the network at
    /// startup.
    #[arg(long, env = "PAY_STELLAR_RPC_URL")]
    pub rpc_url: String,

    /// Timeout for one RPC request, in seconds.
    #[arg(long, env = "PAY_STELLAR_RPC_TIMEOUT_SECS", default_value = "10")]
    pub rpc_timeout_secs: u64,

    /// Ledgers a buyer's deposit authorization stays valid (about five
    /// seconds each; 720 is about an hour).
    #[arg(long, env = "PAY_STELLAR_DEPOSIT_AUTHORIZATION_LEDGERS", default_value = "720")]
    pub deposit_authorization_ledgers: NonZeroU32,

    /// Requests processed at once, across all connections. Every request,
    /// authenticated or not, costs a database lookup, so this bounds the
    /// load unauthenticated traffic can put on the database; rate limiting
    /// per client belongs in front of the gateway.
    #[arg(long, env = "PAY_STELLAR_MAX_CONCURRENT_REQUESTS", default_value = "64")]
    pub max_concurrent_requests: std::num::NonZeroUsize,

    /// Seconds after which a request still running is cancelled.
    #[arg(long, env = "PAY_STELLAR_REQUEST_TIMEOUT_SECS", default_value = "30")]
    pub request_timeout_secs: u64,

    /// Ledgers after admission during which a charge may be settled (about
    /// five seconds each; 720 is about an hour). An unsettled charge is
    /// refunded after that. At most 17280, the contract's limit.
    #[arg(long, env = "PAY_STELLAR_CHARGE_VALIDITY_LEDGERS", default_value = "720",
          value_parser = clap::value_parser!(u32).range(1..=17_280))]
    pub charge_validity_ledgers: u32,

    /// Maximum database connections.
    #[arg(long, env = "PAY_STELLAR_DATABASE_MAX_CONNECTIONS", default_value = "16")]
    pub database_max_connections: NonZeroU32,
}

impl Config {
    #[must_use]
    pub const fn rpc_timeout(&self) -> Duration {
        Duration::from_secs(self.rpc_timeout_secs)
    }
}
