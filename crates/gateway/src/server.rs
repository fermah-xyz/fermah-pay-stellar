//! Assembles the gRPC server: health, authentication, buyer and ledger APIs.

use std::future::Future;
use std::time::Duration;

use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_proto::v1::buyer_service_server::BuyerServiceServer;
use fermah_pay_stellar_proto::v1::ledger_service_server::LedgerServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

use crate::auth::AuthLayer;
use crate::buyers::BuyerApi;
use crate::ledger::{LatestLedger, LedgerApi};
use crate::store::Store;

/// Bounds on the work the server accepts, applied before authentication:
/// every request, authenticated or not, costs a database lookup.
#[derive(Clone, Copy, Debug)]
pub struct ServerLimits {
    /// Requests processed at once, across all connections; others wait.
    pub max_concurrent_requests: usize,
    /// A request still running after this long is cancelled.
    pub request_timeout: Duration,
}

/// Serves until `shutdown` resolves, then drains in-flight requests.
pub async fn serve<L: LatestLedger>(
    listener: TcpListener,
    store: Store,
    network: Network,
    ledger: LedgerApi<L>,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let (health, health_service) = tonic_health::server::health_reporter();
    health.set_serving::<BuyerServiceServer<BuyerApi>>().await;
    tonic::transport::Server::builder()
        .timeout(limits.request_timeout)
        .layer(tower::limit::GlobalConcurrencyLimitLayer::new(limits.max_concurrent_requests))
        .layer(AuthLayer::new(store.clone(), network))
        .add_service(health_service)
        .add_service(BuyerServiceServer::new(BuyerApi::new(store)))
        .add_service(LedgerServiceServer::new(ledger))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await
}
