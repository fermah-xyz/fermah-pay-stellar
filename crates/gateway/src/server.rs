//! Assembles the gRPC server: health, authentication, buyer API.

use std::future::Future;

use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_proto::v1::buyer_service_server::BuyerServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

use crate::auth::AuthLayer;
use crate::buyers::BuyerApi;
use crate::store::Store;

/// Serves until `shutdown` resolves, then drains in-flight requests.
pub async fn serve(
    listener: TcpListener,
    store: Store,
    network: Network,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let (health, health_service) = tonic_health::server::health_reporter();
    health.set_serving::<BuyerServiceServer<BuyerApi>>().await;
    tonic::transport::Server::builder()
        .layer(AuthLayer::new(store.clone(), network))
        .add_service(health_service)
        .add_service(BuyerServiceServer::new(BuyerApi::new(store)))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await
}
