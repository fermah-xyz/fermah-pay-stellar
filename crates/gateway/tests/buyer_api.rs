//! Buyer API over a real gRPC server and PostgreSQL, with the API and issuer
//! processes connected under their production roles.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Harness, Tenant, assert_refused, authed, start, wallet};
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::issuance;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::get_buyer_request::Lookup;
use fermah_pay_stellar_proto::v1::{Buyer, CreateBuyerRequest, GetBuyerRequest};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tonic::transport::Channel;
use tonic::{Code, Request};
use uuid::Uuid;

fn create(external_ref: &str, wallet: &str) -> CreateBuyerRequest {
    CreateBuyerRequest { external_ref: external_ref.to_owned(), wallet_address: wallet.to_owned() }
}

fn by_id(id: &str) -> GetBuyerRequest {
    GetBuyerRequest { lookup: Some(Lookup::BuyerId(id.to_owned())) }
}

async fn client(h: &Harness) -> BuyerServiceClient<Channel> {
    BuyerServiceClient::new(h.channel().await)
}

async fn create_buyer(h: &Harness, t: &Tenant, external_ref: &str, seed: u8) -> Buyer {
    client(h)
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
    let status = client(&h)
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
    let status = client(&h)
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
    client(&h).await.get_buyer(authed(by_id(&buyer.buyer_id), &t.token)).await.unwrap();

    assert!(issuance::revoke_api_key(&h.issuer, t.key_id).await.unwrap());
    let status =
        client(&h).await.get_buyer(authed(by_id(&buyer.buyer_id), &t.token)).await.unwrap_err();
    assert_refused(&status, Code::Unauthenticated, "unauthenticated");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_pubnet_key_is_refused_by_testnet_gateway(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let live = h.tenant("alpha", "live", Network::Pubnet).await;
    let status = client(&h)
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
    let status = client(&h)
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
    client(&h).await.get_buyer(authed(by_id(&buyer.buyer_id), &a.token)).await.unwrap();

    let status =
        client(&h).await.get_buyer(authed(by_id(&buyer.buyer_id), &b.token)).await.unwrap_err();
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
    let status = client(&h).await.get_buyer(authed(request, &second.token)).await.unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
    let status = client(&h)
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
        client(&h).await.get_buyer(authed(by_id(&buyer.buyer_id), &b.token)).await.unwrap_err();
    let unknown = client(&h)
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

    let response = client(&h)
        .await
        .create_buyer(authed(create("user-1", &wallet(1)), &second.token))
        .await
        .unwrap()
        .into_inner();
    assert!(response.created);
    assert_ne!(response.buyer.unwrap().buyer_id, original.buyer_id);
    let still =
        client(&h).await.get_buyer(authed(by_id(&original.buyer_id), &first.token)).await.unwrap();
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
    let replay = client(&h)
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
    let status = client(&h)
        .await
        .create_buyer(authed(create("user-1", &wallet(2)), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::AlreadyExists, "buyer_conflict");
    let stored =
        client(&h).await.get_buyer(authed(by_id(&original.buyer_id), &t.token)).await.unwrap();
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
    let status = client(&h)
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
    let status = client(&h)
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
    let status = client(&h)
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
    let status = client(&h)
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
    let status = client(&h)
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
        client(&h).await.get_buyer(authed(by_id("not-a-uuid"), &t.token)).await.unwrap_err();
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
async fn test_revoked_key_cannot_be_brought_back(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("alpha", "main", Network::Testnet).await;
    assert!(issuance::revoke_api_key(&h.issuer, t.key_id).await.unwrap());
    // The issuer may set `revoked_at`, and only a guard stops it clearing it.
    let error = sqlx::query("UPDATE pay_stellar.api_keys SET revoked_at = NULL WHERE id = $1")
        .bind(t.key_id)
        .execute(&h.issuer)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is revoked"), "{error}");
    let status =
        client(&h).await.get_buyer(authed(by_id(&Uuid::nil().to_string()), &t.token)).await;
    assert_eq!(status.unwrap_err().code(), Code::Unauthenticated);
    // Revoking again changes nothing and is not an error.
    assert!(!issuance::revoke_api_key(&h.issuer, t.key_id).await.unwrap());
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_runtime_roles_cannot_connect_through_public(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    // Group roles are shared by every database on the server; a login role
    // that belongs to one elsewhere must not reach this database by default.
    let connectable: Vec<(String, bool)> = sqlx::query_as(
        "SELECT r, has_database_privilege(r, current_database(), 'CONNECT')
         FROM unnest(ARRAY['pay_stellar_api', 'pay_stellar_issuer', 'pay_stellar_worker',
                           'pay_stellar_operator', 'pay_stellar_observer']) AS r",
    )
    .fetch_all(&h.owner)
    .await
    .unwrap();
    assert!(connectable.iter().all(|(_, can)| !can), "{connectable:?}");
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
         CROSS JOIN (VALUES ('pay_stellar_api'), ('pay_stellar_issuer'), ('pay_stellar_worker'),
                            ('pay_stellar_operator'), ('pay_stellar_observer')) AS r(role)
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
        ("pay_stellar_api", "buyers", "UPDATE"),
        ("pay_stellar_api", "charges", "INSERT"),
        ("pay_stellar_api", "charges", "SELECT"),
        ("pay_stellar_api", "deposits", "INSERT"),
        ("pay_stellar_api", "deposits", "SELECT"),
        ("pay_stellar_api", "deposits", "UPDATE"),
        ("pay_stellar_api", "ledger_contracts", "SELECT"),
        ("pay_stellar_api", "mandates", "INSERT"),
        ("pay_stellar_api", "mandates", "SELECT"),
        ("pay_stellar_api", "mandates", "UPDATE"),
        ("pay_stellar_api", "recurring_charges", "INSERT"),
        ("pay_stellar_api", "recurring_charges", "SELECT"),
        ("pay_stellar_api", "revocations", "INSERT"),
        ("pay_stellar_api", "revocations", "SELECT"),
        ("pay_stellar_api", "revocations", "UPDATE"),
        ("pay_stellar_api", "seller_deployments", "SELECT"),
        ("pay_stellar_api", "submissions", "SELECT"),
        ("pay_stellar_api", "vault_event_cursors", "SELECT"),
        ("pay_stellar_api", "vault_requests", "INSERT"),
        ("pay_stellar_api", "vault_requests", "SELECT"),
        ("pay_stellar_api", "vault_requests", "UPDATE"),
        ("pay_stellar_api", "withdrawals", "INSERT"),
        ("pay_stellar_api", "withdrawals", "SELECT"),
        ("pay_stellar_api", "withdrawals", "UPDATE"),
        ("pay_stellar_issuer", "api_keys", "INSERT"),
        ("pay_stellar_issuer", "api_keys", "SELECT"),
        ("pay_stellar_issuer", "api_keys", "UPDATE"),
        ("pay_stellar_issuer", "ledger_binding_changes", "SELECT"),
        ("pay_stellar_issuer", "ledger_contracts", "INSERT"),
        ("pay_stellar_issuer", "ledger_contracts", "SELECT"),
        ("pay_stellar_issuer", "products", "INSERT"),
        ("pay_stellar_issuer", "products", "SELECT"),
        ("pay_stellar_issuer", "seller_deployments", "INSERT"),
        ("pay_stellar_issuer", "seller_deployments", "SELECT"),
        ("pay_stellar_observer", "buyers", "SELECT"),
        ("pay_stellar_observer", "chain_charge_entries", "INSERT"),
        ("pay_stellar_observer", "chain_charge_entries", "SELECT"),
        ("pay_stellar_observer", "chain_event_checks", "INSERT"),
        ("pay_stellar_observer", "chain_event_checks", "SELECT"),
        ("pay_stellar_observer", "chain_events", "INSERT"),
        ("pay_stellar_observer", "chain_events", "SELECT"),
        ("pay_stellar_observer", "chain_recurring_entries", "INSERT"),
        ("pay_stellar_observer", "chain_recurring_entries", "SELECT"),
        ("pay_stellar_observer", "charges", "SELECT"),
        ("pay_stellar_observer", "deposits", "SELECT"),
        ("pay_stellar_observer", "leases", "DELETE"),
        ("pay_stellar_observer", "leases", "INSERT"),
        ("pay_stellar_observer", "leases", "SELECT"),
        ("pay_stellar_observer", "leases", "UPDATE"),
        ("pay_stellar_observer", "ledger_contracts", "SELECT"),
        ("pay_stellar_observer", "mandates", "SELECT"),
        ("pay_stellar_observer", "observer_cursors", "INSERT"),
        ("pay_stellar_observer", "observer_cursors", "SELECT"),
        ("pay_stellar_observer", "observer_cursors", "UPDATE"),
        ("pay_stellar_observer", "reconciliation_baselines", "SELECT"),
        ("pay_stellar_observer", "reconciliation_findings", "INSERT"),
        ("pay_stellar_observer", "reconciliation_findings", "SELECT"),
        ("pay_stellar_observer", "reconciliation_streaks", "DELETE"),
        ("pay_stellar_observer", "reconciliation_streaks", "INSERT"),
        ("pay_stellar_observer", "reconciliation_streaks", "SELECT"),
        ("pay_stellar_observer", "reconciliation_streaks", "UPDATE"),
        ("pay_stellar_observer", "recurring_charges", "SELECT"),
        ("pay_stellar_observer", "revocations", "SELECT"),
        ("pay_stellar_observer", "vault_event_cursors", "SELECT"),
        ("pay_stellar_observer", "vault_requests", "SELECT"),
        ("pay_stellar_observer", "withdrawals", "SELECT"),
        ("pay_stellar_operator", "buyers", "SELECT"),
        ("pay_stellar_operator", "chain_charge_entries", "SELECT"),
        ("pay_stellar_operator", "chain_events", "SELECT"),
        ("pay_stellar_operator", "chain_recurring_entries", "SELECT"),
        ("pay_stellar_operator", "charge_resolutions", "SELECT"),
        ("pay_stellar_operator", "charges", "SELECT"),
        ("pay_stellar_operator", "deposits", "SELECT"),
        ("pay_stellar_operator", "ledger_contracts", "SELECT"),
        ("pay_stellar_operator", "mandates", "SELECT"),
        ("pay_stellar_operator", "reconciliation_baselines", "INSERT"),
        ("pay_stellar_operator", "reconciliation_baselines", "SELECT"),
        ("pay_stellar_operator", "reconciliation_findings", "SELECT"),
        ("pay_stellar_operator", "recurring_charge_resolutions", "SELECT"),
        ("pay_stellar_operator", "recurring_charges", "SELECT"),
        ("pay_stellar_operator", "revocations", "SELECT"),
        ("pay_stellar_operator", "submissions", "SELECT"),
        ("pay_stellar_operator", "vault_event_cursors", "SELECT"),
        ("pay_stellar_operator", "vault_requests", "SELECT"),
        ("pay_stellar_operator", "withdrawals", "SELECT"),
        ("pay_stellar_worker", "buyers", "SELECT"),
        ("pay_stellar_worker", "buyers", "UPDATE"),
        ("pay_stellar_worker", "charges", "SELECT"),
        ("pay_stellar_worker", "charges", "UPDATE"),
        ("pay_stellar_worker", "deposits", "SELECT"),
        ("pay_stellar_worker", "deposits", "UPDATE"),
        ("pay_stellar_worker", "leases", "DELETE"),
        ("pay_stellar_worker", "leases", "INSERT"),
        ("pay_stellar_worker", "leases", "SELECT"),
        ("pay_stellar_worker", "leases", "UPDATE"),
        ("pay_stellar_worker", "ledger_contracts", "SELECT"),
        ("pay_stellar_worker", "mandates", "SELECT"),
        ("pay_stellar_worker", "mandates", "UPDATE"),
        ("pay_stellar_worker", "recurring_charges", "SELECT"),
        ("pay_stellar_worker", "recurring_charges", "UPDATE"),
        ("pay_stellar_worker", "revocations", "SELECT"),
        ("pay_stellar_worker", "revocations", "UPDATE"),
        ("pay_stellar_worker", "submissions", "INSERT"),
        ("pay_stellar_worker", "submissions", "SELECT"),
        ("pay_stellar_worker", "submissions", "UPDATE"),
        ("pay_stellar_worker", "vault_event_cursors", "INSERT"),
        ("pay_stellar_worker", "vault_event_cursors", "SELECT"),
        ("pay_stellar_worker", "vault_event_cursors", "UPDATE"),
        ("pay_stellar_worker", "vault_requests", "SELECT"),
        ("pay_stellar_worker", "vault_requests", "UPDATE"),
        ("pay_stellar_worker", "withdrawals", "SELECT"),
        ("pay_stellar_worker", "withdrawals", "UPDATE"),
    ]
    .into_iter()
    .map(|(r, t, p)| (r.to_owned(), t.to_owned(), p.to_owned()))
    .collect();
    assert_eq!(privilege_map(&h.owner).await, expected);
}

/// Which columns each runtime role may write, on the tables where a write
/// moves money, rebinds identity or moves the observer's position. Table-level
/// presence is swept above; this pins the columns, so e.g. a grant letting the
/// API insert a buyer with a balance, or letting the worker rewrite a charge's
/// amount, shows up here.
#[sqlx::test(migrations = "../../db/migrations")]
async fn test_runtime_roles_write_exactly_the_documented_columns(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT r.role, a.attrelid::regclass::text, a.attname::text, p.privilege
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'pay_stellar'
         CROSS JOIN (VALUES ('pay_stellar_api'), ('pay_stellar_worker'), ('pay_stellar_operator'),
                            ('pay_stellar_observer')) AS r(role)
         CROSS JOIN (VALUES ('INSERT'), ('UPDATE')) AS p(privilege)
         WHERE c.relname IN ('buyers', 'deposits', 'charges', 'withdrawals', 'observer_cursors')
           AND a.attnum > 0 AND NOT a.attisdropped
           AND has_column_privilege(r.role, a.attrelid, a.attnum, p.privilege)",
    )
    .fetch_all(&h.owner)
    .await
    .unwrap();
    let actual: std::collections::BTreeSet<String> =
        rows.into_iter().map(|(r, t, c, p)| format!("{r} {p} {t}.{c}")).collect();

    let columns = |role: &str, privilege: &str, table: &str, columns: &[&str]| {
        columns
            .iter()
            .map(|c| format!("{role} {privilege} pay_stellar.{table}.{c}"))
            .collect::<Vec<_>>()
    };
    let expected: std::collections::BTreeSet<String> = [
        columns(
            "pay_stellar_api",
            "INSERT",
            "buyers",
            &[
                "id",
                "product_id",
                "seller_deployment_id",
                "network",
                "external_ref",
                "wallet_address",
            ],
        ),
        columns("pay_stellar_api", "UPDATE", "buyers", &["available"]),
        columns(
            "pay_stellar_api",
            "INSERT",
            "deposits",
            &[
                "id",
                "buyer_id",
                "seller_deployment_id",
                "network",
                "idempotency_key",
                "amount",
                "deposit_id",
                "authorization_xdr",
                "expiration_ledger",
                "cap",
            ],
        ),
        columns(
            "pay_stellar_api",
            "UPDATE",
            "deposits",
            &["state", "signed_authorization_xdr", "signed_at"],
        ),
        columns(
            "pay_stellar_api",
            "INSERT",
            "charges",
            &[
                "id",
                "buyer_id",
                "seller_deployment_id",
                "network",
                "idempotency_key",
                "amount",
                "charge_id",
                "last_ledger",
                "day",
            ],
        ),
        columns(
            "pay_stellar_api",
            "INSERT",
            "withdrawals",
            &[
                "id",
                "buyer_id",
                "seller_deployment_id",
                "network",
                "idempotency_key",
                "amount",
                "destination_address",
                "withdrawal_id",
                "authorization_xdr",
                "expiration_ledger",
            ],
        ),
        columns(
            "pay_stellar_api",
            "UPDATE",
            "withdrawals",
            &["state", "signed_authorization_xdr", "signed_at"],
        ),
        columns(
            "pay_stellar_worker",
            "UPDATE",
            "buyers",
            &[
                "available",
                "cap",
                "pending_cap",
                "pending_cap_at",
                "exit_amount",
                "exit_unlock_at",
                "vault_synced_ledger",
                "vault_entry_absent",
            ],
        ),
        columns(
            "pay_stellar_worker",
            "UPDATE",
            "withdrawals",
            &["state", "submission_id", "last_error", "resolved_at"],
        ),
        columns(
            "pay_stellar_worker",
            "UPDATE",
            "deposits",
            &["state", "submission_id", "last_error", "resolved_at"],
        ),
        columns(
            "pay_stellar_worker",
            "UPDATE",
            "charges",
            &["state", "outcome", "submission_id", "batch_index", "last_error", "settled_at"],
        ),
        // The observer creates its position and moves it; it cannot rewrite
        // where observation began.
        columns(
            "pay_stellar_observer",
            "INSERT",
            "observer_cursors",
            &[
                "seller_deployment_id",
                "observed_from_ledger",
                "start_ledger",
                "cursor",
                "updated_at",
            ],
        ),
        columns(
            "pay_stellar_observer",
            "UPDATE",
            "observer_cursors",
            &["start_ledger", "cursor", "updated_at"],
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert_eq!(actual, expected);
}
