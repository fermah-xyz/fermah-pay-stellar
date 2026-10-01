//! Shared harness: a real gRPC server over PostgreSQL, with the API and
//! issuer processes connected under their production roles, and a network
//! stub whose latest ledger a test sets.

#![allow(dead_code, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use fermah_pay_stellar_chain::rpc::{
    ContractEventRecord, EventCursor, EventPage, EventsFrom, RpcError,
};
use fermah_pay_stellar_chain::stellar_xdr::{InvokeContractArgs, ScVal, SorobanAuthorizationEntry};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance;
use fermah_pay_stellar_gateway::ledger::{LatestLedger, LedgerApi, LedgerPolicy};
use fermah_pay_stellar_gateway::server::{ServerLimits, serve};
use fermah_pay_stellar_gateway::store::{Quotas, Store};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use tokio::net::TcpListener;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use uuid::Uuid;

pub const AUTHORIZATION_VALIDITY_LEDGERS: u32 = 720;
pub const CHARGE_VALIDITY_LEDGERS: u32 = 720;
pub const MIN_MANDATE_PERIOD_SECS: u64 = 60;
pub const MAX_MANDATE_LEDGERS: u32 = 3_000_000;

/// The latest ledger the API sees (0 makes every read fail), and the
/// network's answer to a contract account's authorization.
#[derive(Clone, Default)]
pub struct Ledger(Arc<AtomicU32>, Arc<Mutex<Simulations>>);

/// Calls simulated for contract accounts, and how the network answers.
#[derive(Default)]
pub struct Simulations {
    /// The network's reason to refuse every call; `None` accepts them.
    pub refusal: Option<String>,
    pub seen: Vec<(AccountAddress, InvokeContractArgs, Vec<SorobanAuthorizationEntry>)>,
}

impl Ledger {
    pub fn set(&self, ledger: u32) {
        self.0.store(ledger, Ordering::SeqCst);
    }

    pub fn get(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn simulations(&self) -> std::sync::MutexGuard<'_, Simulations> {
        self.1.lock().unwrap()
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

    async fn latest_close_time(&self) -> Result<i64, RpcError> {
        Ok(time::OffsetDateTime::now_utc().unix_timestamp())
    }

    async fn refusal_of(
        &self,
        source: &AccountAddress,
        call: InvokeContractArgs,
        auth: Vec<SorobanAuthorizationEntry>,
    ) -> Result<Option<String>, RpcError> {
        let mut simulations = self.simulations();
        simulations.seen.push((source.clone(), call, auth));
        Ok(simulations.refusal.clone())
    }
}

pub struct Harness {
    pub addr: SocketAddr,
    /// The x402 interface over the same store and network view, called in
    /// process.
    pub x402: axum::Router,
    /// What the x402 interface reads as the current unix time.
    pub now: Arc<std::sync::atomic::AtomicI64>,
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
/// `ledger` is the handle the harness exposes for tests that set it. The
/// production quotas apply, except that any withdrawal amount is accepted:
/// the tests move a few base units at a time.
pub async fn start_with<L: LatestLedger>(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
    network: Network,
    api_ledger: L,
    ledger: Ledger,
) -> Harness {
    let quotas = Quotas { min_withdrawal: 1, ..Quotas::default() };
    start_with_quotas(opts, connect, network, api_ledger, ledger, quotas).await
}

pub async fn start_with_quotas<L: LatestLedger>(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
    network: Network,
    api_ledger: L,
    ledger: Ledger,
    quotas: Quotas,
) -> Harness {
    start_with_options(opts, connect, network, api_ledger, ledger, quotas, false).await
}

/// As [`start_with_quotas`]; `other_destinations` lets withdrawals go to an
/// account other than the buyer's wallet, which the production default
/// refuses.
pub async fn start_with_options<L: LatestLedger>(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
    network: Network,
    api_ledger: L,
    ledger: Ledger,
    quotas: Quotas,
    other_destinations: bool,
) -> Harness {
    let owner = opts.max_connections(1).connect_with(connect.clone()).await.unwrap();
    let api = pool_as(&connect, "SET ROLE pay_stellar_api", 3).await;
    let issuer = pool_as(&connect, "SET ROLE pay_stellar_issuer", 1).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
    let store = Store::new(api.clone()).with_quotas(quotas);
    let ledger_api = LedgerApi::new(
        store.clone(),
        api_ledger,
        network,
        LedgerPolicy {
            authorization_validity_ledgers: AUTHORIZATION_VALIDITY_LEDGERS,
            charge_validity_ledgers: CHARGE_VALIDITY_LEDGERS,
            min_mandate_period_secs: MIN_MANDATE_PERIOD_SECS,
            max_mandate_ledgers: MAX_MANDATE_LEDGERS,
            withdrawals_to_other_accounts: other_destinations,
        },
    );
    let limits = ServerLimits {
        max_concurrent_requests: 64,
        request_timeout: std::time::Duration::from_secs(30),
    };
    let now = Arc::new(std::sync::atomic::AtomicI64::new(1_800_000_000));
    let clock = Arc::clone(&now);
    let x402 = fermah_pay_stellar_gateway::x402::router_with_clock(
        ledger_api.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    );
    tokio::spawn(serve(listener, store, network, ledger_api, limits, async {
        let _ = stop.await;
    }));
    Harness { addr, x402, now, owner, api, issuer, ledger, _shutdown: shutdown }
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

/// A contract event stream paged the way Stellar RPC pages `getEvents`: a
/// start ledger or a cursor, never both; a start outside the retained range
/// refused; a scan window of 10,000 ledgers ending at the latest ledger; the
/// cursor of a full page is its last event, otherwise the end of the window.
#[derive(Clone, Default)]
pub struct EventStream {
    events: Vec<ContractEventRecord>,
}

impl EventStream {
    /// Appends an event in its own transaction at `ledger`.
    pub fn emit(
        &mut self,
        contract: [u8; 32],
        ledger: u32,
        closed_at: String,
        topics: Vec<ScVal>,
        value: ScVal,
    ) -> EventCursor {
        let transaction =
            u64::try_from(self.events.iter().filter(|e| e.ledger == ledger).count() + 1).unwrap();
        let toid = (u64::from(ledger) << 32) | (transaction << 12);
        let id = EventCursor::parse(&format!("{toid:019}-{:010}", 0)).unwrap();
        let mut hash = [0_u8; 32];
        hash[..8].copy_from_slice(&toid.to_be_bytes());
        self.events.push(ContractEventRecord {
            id,
            ledger,
            ledger_closed_at: closed_at,
            contract,
            transaction_hash: hash,
            in_successful_contract_call: true,
            topics,
            value,
        });
        self.events.sort_by_key(|e| e.id);
        id
    }

    pub fn page(
        &self,
        contract: &[u8; 32],
        from: &EventsFrom,
        limit: u32,
        oldest: u32,
        latest: u32,
    ) -> Result<EventPage, RpcError> {
        let start = match from {
            EventsFrom::Ledger(ledger) => *ledger,
            EventsFrom::Cursor(cursor) => cursor.ledger(),
        };
        if start < oldest || start > latest {
            return Err(RpcError::Server {
                method: "getEvents",
                code: -32600,
                message: format!(
                    "startLedger must be within the ledger range: {oldest} - {latest}"
                ),
            });
        }
        let end = (start + 10_000).min(latest + 1);
        let limit = usize::try_from(limit).unwrap();
        let events: Vec<ContractEventRecord> = self
            .events
            .iter()
            .filter(|e| e.contract == *contract && e.ledger < end)
            .filter(|e| match from {
                EventsFrom::Ledger(ledger) => e.ledger >= *ledger,
                EventsFrom::Cursor(cursor) => e.id > *cursor,
            })
            .take(limit)
            .cloned()
            .collect();
        let cursor = if events.len() == limit {
            events.last().unwrap().id
        } else {
            EventCursor::end_of_ledger(end - 1)
        };
        Ok(EventPage { events, cursor, latest_ledger: latest, oldest_ledger: oldest })
    }
}
