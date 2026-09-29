//! Shared harness: a real gRPC server over PostgreSQL, with the API and
//! issuer processes connected under their production roles, and a network
//! stub whose latest ledger a test sets.

#![allow(dead_code, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use fermah_pay_stellar_chain::rpc::RpcError;
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::issuance;
use fermah_pay_stellar_gateway::ledger::{DepositPolicy, LatestLedger, LedgerApi};
use fermah_pay_stellar_gateway::server::{ServerLimits, serve};
use fermah_pay_stellar_gateway::store::Store;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use tokio::net::TcpListener;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use uuid::Uuid;

pub const AUTHORIZATION_VALIDITY_LEDGERS: u32 = 720;

/// The latest ledger the API sees; `None` makes every read fail.
#[derive(Clone, Default)]
pub struct Ledger(Arc<AtomicU32>);

impl Ledger {
    pub fn set(&self, ledger: u32) {
        self.0.store(ledger, Ordering::SeqCst);
    }

    pub fn get(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

impl LatestLedger for Ledger {
    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        match self.get() {
            0 => Err(RpcError::Server {
                method: "getLatestLedger",
                code: -1,
                message: "unreachable".to_owned(),
            }),
            ledger => Ok(ledger),
        }
    }
}

pub struct Harness {
    pub addr: SocketAddr,
    pub owner: PgPool,
    pub api: PgPool,
    pub issuer: PgPool,
    pub ledger: Ledger,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

// The options `#[sqlx::test]` hands out draw from one master pool of 20
// permits shared by every test in the binary. A test here holds several
// connections at once (owner, API, issuer), so parented pools starve each
// other under parallel execution. Only the owner pool stays parented; the role
// pools are independent and small enough to stay under the server's limit.
pub async fn pool_as(
    connect: &PgConnectOptions,
    set_role: &'static str,
    max_connections: u32,
) -> PgPool {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                conn.execute(set_role).await?;
                Ok(())
            })
        })
        .connect_with(connect.clone())
        .await
        .unwrap()
}

pub async fn start(opts: PgPoolOptions, connect: PgConnectOptions, network: Network) -> Harness {
    let ledger = Ledger::default();
    ledger.set(1_000);
    start_with(opts, connect, network, ledger.clone(), ledger).await
}

/// Starts the API with `api_ledger` as its view of the latest ledger;
/// `ledger` is the handle the harness exposes for tests that set it.
pub async fn start_with<L: LatestLedger>(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
    network: Network,
    api_ledger: L,
    ledger: Ledger,
) -> Harness {
    let owner = opts.max_connections(1).connect_with(connect.clone()).await.unwrap();
    let api = pool_as(&connect, "SET ROLE pay_stellar_api", 3).await;
    let issuer = pool_as(&connect, "SET ROLE pay_stellar_issuer", 1).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
    let store = Store::new(api.clone());
    let ledger_api = LedgerApi::new(
        store.clone(),
        api_ledger,
        network,
        DepositPolicy { authorization_validity_ledgers: AUTHORIZATION_VALIDITY_LEDGERS },
    );
    let limits = ServerLimits {
        max_concurrent_requests: 64,
        request_timeout: std::time::Duration::from_secs(30),
    };
    tokio::spawn(serve(listener, store, network, ledger_api, limits, async {
        let _ = stop.await;
    }));
    Harness { addr, owner, api, issuer, ledger, _shutdown: shutdown }
}

pub struct Tenant {
    pub product_id: Uuid,
    pub deployment_id: Uuid,
    pub key_id: Uuid,
    pub token: String,
}

impl Harness {
    pub async fn channel(&self) -> Channel {
        Channel::from_shared(format!("http://{}", self.addr)).unwrap().connect().await.unwrap()
    }

    pub async fn tenant(&self, product: &str, deployment: &str, network: Network) -> Tenant {
        let product_id = match issuance::create_product(&self.issuer, product).await {
            Ok(id) => id,
            Err(_) => sqlx::query_scalar("SELECT id FROM pay_stellar.products WHERE name = $1")
                .bind(product)
                .fetch_one(&self.owner)
                .await
                .unwrap(),
        };
        let deployment_id =
            issuance::create_seller_deployment(&self.issuer, product_id, deployment, network)
                .await
                .unwrap();
        let key = issuance::issue_api_key(&self.issuer, deployment_id, "test").await.unwrap();
        Tenant { product_id, deployment_id, key_id: key.id, token: key.token.to_string() }
    }
}

pub fn authed<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

pub fn wallet(seed: u8) -> String {
    stellar_strkey::ed25519::PublicKey([seed; 32]).to_string().to_string()
}

pub fn assert_refused(status: &Status, code: Code, reason: &str) {
    assert_eq!((status.code(), status.message()), (code, reason), "{status:?}");
}
