//! Buyer API over a real gRPC server and PostgreSQL, with the API and issuer
//! processes connected under their production roles.

#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;

use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::issuance;
use fermah_pay_stellar_gateway::server::serve;
use fermah_pay_stellar_gateway::store::Store;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::get_buyer_request::Lookup;
use fermah_pay_stellar_proto::v1::{Buyer, CreateBuyerRequest, GetBuyerRequest};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use tokio::net::TcpListener;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use uuid::Uuid;

struct Harness {
    addr: SocketAddr,
    owner: PgPool,
    api: PgPool,
    issuer: PgPool,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

// The options `#[sqlx::test]` hands out draw from one master pool of 20
// permits shared by every test in the binary. A test here holds several
// connections at once (owner, API, issuer), so parented pools starve each
// other under parallel execution. Only the owner pool stays parented; the role
// pools are independent and small enough to stay under the server's limit.
async fn pool_as(
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

async fn start(opts: PgPoolOptions, connect: PgConnectOptions, network: Network) -> Harness {
    let owner = opts.max_connections(1).connect_with(connect.clone()).await.unwrap();
    let api = pool_as(&connect, "SET ROLE pay_stellar_api", 3).await;
    let issuer = pool_as(&connect, "SET ROLE pay_stellar_issuer", 1).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, Store::new(api.clone()), network, async {
        let _ = stop.await;
    }));
    Harness { addr, owner, api, issuer, _shutdown: shutdown }
}

struct Tenant {
    product_id: Uuid,
    deployment_id: Uuid,
    key_id: Uuid,
    token: String,
}

impl Harness {
    async fn client(&self) -> BuyerServiceClient<Channel> {
        BuyerServiceClient::connect(format!("http://{}", self.addr)).await.unwrap()
    }

    async fn tenant(&self, product: &str, deployment: &str, network: Network) -> Tenant {
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

fn authed<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn create(external_ref: &str, wallet: &str) -> CreateBuyerRequest {
    CreateBuyerRequest { external_ref: external_ref.to_owned(), wallet_address: wallet.to_owned() }
}

fn by_id(id: &str) -> GetBuyerRequest {
    GetBuyerRequest { lookup: Some(Lookup::BuyerId(id.to_owned())) }
}

fn wallet(seed: u8) -> String {
    stellar_strkey::ed25519::PublicKey([seed; 32]).to_string().to_string()
}

fn assert_refused(status: &Status, code: Code, reason: &str) {
    assert_eq!((status.code(), status.message()), (code, reason), "{status:?}");
}

async fn create_buyer(h: &Harness, t: &Tenant, external_ref: &str, seed: u8) -> Buyer {
    h.client()
        .await
        .create_buyer(authed(create(external_ref, &wallet(seed)), &t.token))
        .await
        .unwrap()
        .into_inner()
        .buyer
        .unwrap()
}

// ---- authentication ------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_request_without_key_is_unauthenticated(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let status = h
        .client()
        .await
        .get_buyer(Request::new(by_id(&Uuid::nil().to_string())))
        .await
        .unwrap_err();
    assert_refused(&status, Code::Unauthenticated, "unauthenticated");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_well_formed_unissued_key_is_unauthenticated(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let forged = format!("fps_test_{}", "A".repeat(43));
    let status = h
        .client()
        .await
        .get_buyer(authed(by_id(&Uuid::nil().to_string()), &forged))
        .await
        .unwrap_err();
    assert_refused(&status, Code::Unauthenticated, "unauthenticated");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_revoked_key_is_unauthenticated(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let buyer = create_buyer(&h, &t, "user-1", 1).await;
    // Pre-state: the key works before revocation.
    h.client().await.get_buyer(authed(by_id(&buyer.buyer_id), &t.token)).await.unwrap();

    assert!(issuance::revoke_api_key(&h.issuer, t.key_id).await.unwrap());
    let status =
        h.client().await.get_buyer(authed(by_id(&buyer.buyer_id), &t.token)).await.unwrap_err();
    assert_refused(&status, Code::Unauthenticated, "unauthenticated");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_pubnet_key_is_refused_by_testnet_gateway(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let live = h.tenant("alpha", "live", Network::Pubnet).await;
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &wallet(1)), &live.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::Unauthenticated, "unauthenticated");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_key_row_bound_to_pubnet_is_refused_despite_testnet_prefix(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let live = h.tenant("alpha", "live", Network::Pubnet).await;
    // A row the issuer would never write: a testnet-shaped token recorded
    // against a pubnet deployment. Only the network predicate of the lookup
    // can refuse it.
    let token = format!("fps_test_{}", "B".repeat(43));
    let digest: [u8; 32] = sha2_digest(&token);
    sqlx::query(
        "INSERT INTO pay_stellar.api_keys (id, seller_deployment_id, network, token_sha256, label)
         VALUES ($1, $2, 'stellar:pubnet', $3, 'tampered')",
    )
    .bind(Uuid::now_v7())
    .bind(live.deployment_id)
    .bind(digest.as_slice())
    .execute(&h.owner)
    .await
    .unwrap();
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &wallet(1)), &token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::Unauthenticated, "unauthenticated");
}

fn sha2_digest(token: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).into()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_health_probe_needs_no_key(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let channel =
        Channel::from_shared(format!("http://{}", h.addr)).unwrap().connect().await.unwrap();
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel);
    let response = health
        .check(tonic_health::pb::HealthCheckRequest {
            service: "fermah.pay.stellar.v1.BuyerService".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.status,
        tonic_health::pb::health_check_response::ServingStatus::Serving as i32
    );
}

// ---- tenancy isolation ---------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_other_product_cannot_read_buyer_by_id(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let a = h.tenant("alpha", "main", Network::Testnet).await;
    let b = h.tenant("beta", "main", Network::Testnet).await;
    let buyer = create_buyer(&h, &a, "user-1", 1).await;
    // Positive control: the owner reads it.
    h.client().await.get_buyer(authed(by_id(&buyer.buyer_id), &a.token)).await.unwrap();

    let status =
        h.client().await.get_buyer(authed(by_id(&buyer.buyer_id), &b.token)).await.unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_other_deployment_of_same_product_cannot_read_buyer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let first = h.tenant("alpha", "first", Network::Testnet).await;
    let second = h.tenant("alpha", "second", Network::Testnet).await;
    assert_eq!(first.product_id, second.product_id);
    let buyer = create_buyer(&h, &first, "user-1", 1).await;

    let request = GetBuyerRequest { lookup: Some(Lookup::ExternalRef("user-1".to_owned())) };
    let status = h.client().await.get_buyer(authed(request, &second.token)).await.unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
    let status = h
        .client()
        .await
        .get_buyer(authed(by_id(&buyer.buyer_id), &second.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_cross_scope_not_found_is_indistinguishable_from_unknown_id(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let a = h.tenant("alpha", "main", Network::Testnet).await;
    let b = h.tenant("beta", "main", Network::Testnet).await;
    let buyer = create_buyer(&h, &a, "user-1", 1).await;
    let foreign =
        h.client().await.get_buyer(authed(by_id(&buyer.buyer_id), &b.token)).await.unwrap_err();
    let unknown = h
        .client()
        .await
        .get_buyer(authed(by_id(&Uuid::now_v7().to_string()), &b.token))
        .await
        .unwrap_err();
    assert_eq!(
        (foreign.code(), foreign.message(), foreign.details()),
        (unknown.code(), unknown.message(), unknown.details())
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_same_reference_in_two_deployments_creates_independent_buyers(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let first = h.tenant("alpha", "first", Network::Testnet).await;
    let second = h.tenant("alpha", "second", Network::Testnet).await;
    let original = create_buyer(&h, &first, "user-1", 1).await;

    let response = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &wallet(1)), &second.token))
        .await
        .unwrap()
        .into_inner();
    assert!(response.created);
    assert_ne!(response.buyer.unwrap().buyer_id, original.buyer_id);
    let still =
        h.client().await.get_buyer(authed(by_id(&original.buyer_id), &first.token)).await.unwrap();
    assert_eq!(still.into_inner().buyer.unwrap(), original);
}

// ---- idempotency and conflicts -------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_identical_registration_replays_original_buyer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let original = create_buyer(&h, &t, "user-1", 1).await;
    let replay = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &wallet(1)), &t.token))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((replay.created, replay.buyer.unwrap()), (false, original));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_reference_reused_with_other_wallet_conflicts_and_keeps_link(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let original = create_buyer(&h, &t, "user-1", 1).await;
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &wallet(2)), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::AlreadyExists, "buyer_conflict");
    let stored =
        h.client().await.get_buyer(authed(by_id(&original.buyer_id), &t.token)).await.unwrap();
    assert_eq!(stored.into_inner().buyer.unwrap().wallet_address, wallet(1));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_wallet_already_linked_to_other_reference_conflicts(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    create_buyer(&h, &t, "user-1", 1).await;
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user-2", &wallet(1)), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::AlreadyExists, "buyer_conflict");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_identical_registrations_create_one_buyer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (addr, request) = (h.addr, authed(create("user-1", &wallet(1)), &t.token));
        calls.spawn(async move {
            let mut client = BuyerServiceClient::connect(format!("http://{addr}")).await.unwrap();
            client.create_buyer(request).await.unwrap().into_inner()
        });
    }
    let responses = calls.join_all().await;
    let created = responses.iter().filter(|r| r.created).count();
    let ids: std::collections::HashSet<_> =
        responses.iter().map(|r| r.buyer.as_ref().unwrap().buyer_id.clone()).collect();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pay_stellar.buyers")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    assert_eq!((created, ids.len(), rows), (1, 1, 1));
}

// ---- edge validation -----------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_wallet_with_bad_checksum_is_invalid(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let mut corrupted = wallet(1);
    corrupted.replace_range(55.., if corrupted.ends_with('A') { "B" } else { "A" });
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &corrupted), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_wallet_address");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_muxed_wallet_is_unsupported(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let muxed =
        stellar_strkey::ed25519::MuxedAccount { ed25519: [1; 32], id: 7 }.to_string().to_string();
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user-1", &muxed), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "unsupported_wallet_address");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_invalid_external_reference_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let status = h
        .client()
        .await
        .create_buyer(authed(create("user 1", &wallet(1)), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_external_ref");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_lookup_without_selector_is_refused(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let status = h
        .client()
        .await
        .get_buyer(authed(GetBuyerRequest { lookup: None }, &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "missing_lookup");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_malformed_buyer_id_is_refused(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let status =
        h.client().await.get_buyer(authed(by_id("not-a-uuid"), &t.token)).await.unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_buyer_id");
}

// ---- database privileges -------------------------------------------------

fn is_permission_denied(error: &sqlx::Error) -> bool {
    error.as_database_error().and_then(|e| e.code()).as_deref() == Some("42501")
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_api_role_cannot_rewrite_wallet_link(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    create_buyer(&h, &t, "user-1", 1).await;
    let error = sqlx::query("UPDATE pay_stellar.buyers SET wallet_address = $1")
        .bind(wallet(2))
        .execute(&h.api)
        .await
        .unwrap_err();
    assert!(is_permission_denied(&error), "{error:?}");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_api_role_cannot_mint_keys(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let error = issuance::issue_api_key(&h.api, t.deployment_id, "escalation").await.unwrap_err();
    let issuance::IssuanceError::Query { source, .. } = &error else { panic!("{error:?}") };
    assert!(is_permission_denied(source), "{error:?}");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_issuer_role_cannot_delete_keys(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    h.tenant("alpha", "main", Network::Testnet).await;
    let error =
        sqlx::query("DELETE FROM pay_stellar.api_keys").execute(&h.issuer).await.unwrap_err();
    assert!(is_permission_denied(&error), "{error:?}");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_issued_key_is_stored_only_as_its_digest(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    let row: String =
        sqlx::query_scalar("SELECT row_to_json(k)::text FROM pay_stellar.api_keys k WHERE id = $1")
            .bind(t.key_id)
            .fetch_one(&h.owner)
            .await
            .unwrap();
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_sha256 FROM pay_stellar.api_keys WHERE id = $1")
            .bind(t.key_id)
            .fetch_one(&h.owner)
            .await
            .unwrap();
    assert!(!row.contains(&t.token[9..]));
    assert_eq!(stored, sha2_digest(&t.token).to_vec());
}

/// Every (role, table, privilege) the runtime roles hold in the schema, at
/// table or any-column level (`has_any_column_privilege` is true for a
/// table-level grant too). Sweeping the catalog rather than naming tables makes a
/// privilege granted on a table added later show up here too.
async fn privilege_map(owner: &PgPool) -> std::collections::BTreeSet<(String, String, String)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT r.role, c.relname::text, p.privilege
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'pay_stellar'
         CROSS JOIN (VALUES ('pay_stellar_api'), ('pay_stellar_issuer'), ('pay_stellar_worker')) AS r(role)
         CROSS JOIN (VALUES ('SELECT'), ('INSERT'), ('UPDATE'), ('DELETE'), ('TRUNCATE')) AS p(privilege)
         WHERE c.relkind = 'r'
           AND CASE
                 WHEN p.privilege IN ('SELECT', 'INSERT', 'UPDATE')
                   THEN has_any_column_privilege(r.role, c.oid, p.privilege)
                 ELSE has_table_privilege(r.role, c.oid, p.privilege)
               END",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    rows.into_iter().collect()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_runtime_roles_hold_exactly_the_documented_privileges(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let expected: std::collections::BTreeSet<(String, String, String)> = [
        ("pay_stellar_api", "api_keys", "SELECT"),
        ("pay_stellar_api", "buyers", "INSERT"),
        ("pay_stellar_api", "buyers", "SELECT"),
        ("pay_stellar_api", "seller_deployments", "SELECT"),
        ("pay_stellar_issuer", "api_keys", "INSERT"),
        ("pay_stellar_issuer", "api_keys", "SELECT"),
        ("pay_stellar_issuer", "api_keys", "UPDATE"),
        ("pay_stellar_issuer", "products", "INSERT"),
        ("pay_stellar_issuer", "products", "SELECT"),
        ("pay_stellar_issuer", "seller_deployments", "INSERT"),
        ("pay_stellar_issuer", "seller_deployments", "SELECT"),
        ("pay_stellar_worker", "submissions", "INSERT"),
        ("pay_stellar_worker", "submissions", "SELECT"),
        ("pay_stellar_worker", "submissions", "UPDATE"),
    ]
    .into_iter()
    .map(|(r, t, p)| (r.to_owned(), t.to_owned(), p.to_owned()))
    .collect();
    assert_eq!(privilege_map(&h.owner).await, expected);
}
