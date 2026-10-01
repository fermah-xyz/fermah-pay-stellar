//! Ledger API over a real gRPC server and PostgreSQL under the production
//! roles: deposit preparation and signature verification, charge admission,
//! and the schema rules that protect balances.

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    AUTHORIZATION_VALIDITY_LEDGERS, CHARGE_VALIDITY_LEDGERS, Harness, Tenant, assert_refused,
    authed, start,
};
use fermah_pay_stellar_chain::authorization::{sign_entry, signature_payload};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{DepositIntent, MandateIntent, PrepaidDeployment};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::stellar_xdr::{
    BytesM, Limits, ReadXdr, ScBytes, ScMap, ScMapEntry, ScSymbol, ScVal, ScVec,
    SorobanAuthorizationEntry, SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    ChargeState, CreateBuyerRequest, CreateChargeRequest, CreateRecurringChargeRequest, Deposit,
    DepositState, GetBalanceRequest, GetChargeRequest, GetDepositRequest, GetMandateRequest,
    Mandate, MandateState, PrepareDepositRequest, PrepareMandateRequest, PrepareRevocationRequest,
    SubmitDepositRequest, SubmitMandateRequest,
};
use sha2::Digest as _;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tonic::Code;
use tonic::transport::Channel;
use uuid::Uuid;

const CONTRACT: [u8; 32] = [7; 32];

fn contract_strkey(id: [u8; 32]) -> String {
    stellar_strkey::Contract(id).to_string().to_string()
}

fn treasury() -> AccountAddress {
    AccountAddress::from_public_key([11; 32])
}

async fn ledger(h: &Harness) -> LedgerServiceClient<Channel> {
    LedgerServiceClient::new(h.channel().await)
}

async fn bind(h: &Harness, t: &Tenant) {
    let binding = LedgerBinding {
        contract: contract_strkey(CONTRACT),
        treasury: treasury(),
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, t.deployment_id, &binding).await.unwrap();
}

struct TestBuyer {
    id: String,
    key: SecretKey,
}

async fn buyer(h: &Harness, t: &Tenant, external_ref: &str) -> TestBuyer {
    let key = SecretKey::generate().unwrap();
    let request = CreateBuyerRequest {
        external_ref: external_ref.to_owned(),
        wallet_address: key.address().to_string(),
    };
    let created = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(request, &t.token))
        .await
        .unwrap()
        .into_inner();
    TestBuyer { id: created.buyer.unwrap().buyer_id, key }
}

/// A bound tenant with one buyer.
async fn setup(h: &Harness) -> (Tenant, TestBuyer) {
    let t = h.tenant("shop", "main", Network::Testnet).await;
    bind(h, &t).await;
    let b = buyer(h, &t, "alice").await;
    (t, b)
}

fn prepare(buyer_id: &str, amount: i64, key: &str) -> PrepareDepositRequest {
    PrepareDepositRequest { buyer_id: buyer_id.to_owned(), amount, idempotency_key: key.to_owned() }
}

async fn prepared(h: &Harness, t: &Tenant, b: &TestBuyer, amount: i64) -> Deposit {
    ledger(h)
        .await
        .prepare_deposit(authed(prepare(&b.id, amount, "dep-1"), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap()
}

fn entry_of(deposit: &Deposit) -> SorobanAuthorizationEntry {
    SorobanAuthorizationEntry::from_xdr_base64(&deposit.authorization_entry_xdr, Limits::none())
        .unwrap()
}

fn xdr(entry: &SorobanAuthorizationEntry) -> String {
    entry.to_xdr_base64(Limits::none()).unwrap()
}

fn signed_by(deposit: &Deposit, key: &SecretKey) -> String {
    xdr(&sign_entry(&entry_of(deposit), network_id(Network::Testnet), &[key]).unwrap())
}

fn submit(deposit: &Deposit, signed: String) -> SubmitDepositRequest {
    SubmitDepositRequest {
        deposit_id: deposit.deposit_id.clone(),
        signed_authorization_entry_xdr: signed,
    }
}

fn charge(buyer_id: &str, amount: i64, key: &str) -> CreateChargeRequest {
    CreateChargeRequest { buyer_id: buyer_id.to_owned(), amount, idempotency_key: key.to_owned() }
}

/// Available balance as the worker leaves it after confirming deposits.
async fn fund(h: &Harness, buyer_id: &str, available: i64) {
    sqlx::query("UPDATE pay_stellar.buyers SET available = $2 WHERE id = $1")
        .bind(Uuid::parse_str(buyer_id).unwrap())
        .bind(available)
        .execute(&h.owner)
        .await
        .unwrap();
}

async fn balance(h: &Harness, t: &Tenant, buyer_id: &str) -> (i64, i64) {
    let reply = ledger(h)
        .await
        .get_balance(authed(GetBalanceRequest { buyer_id: buyer_id.to_owned() }, &t.token))
        .await
        .unwrap()
        .into_inner();
    (reply.available, reply.pending_charges)
}

// ---- deposit preparation --------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_prepared_entry_authorizes_exactly_the_deposit_to_the_bound_treasury(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 25_000_000).await;

    assert_eq!(deposit.state(), DepositState::AwaitingSignature);
    assert_eq!(deposit.expiration_ledger, h.ledger.get() + AUTHORIZATION_VALIDITY_LEDGERS);
    let entry = entry_of(&deposit);
    let SorobanCredentials::AddressV2(creds) = &entry.credentials else {
        panic!("expected address-bound credentials: {:?}", entry.credentials)
    };
    assert_eq!(creds.signature_expiration_ledger, deposit.expiration_ledger);
    assert_eq!(creds.signature, ScVal::Void);
    // The invocation is rebuilt independently from the binding: the stored
    // deposit id is the only value not derivable from the request.
    let deposit_id: Vec<u8> = sqlx::query_scalar("SELECT deposit_id FROM pay_stellar.deposits")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    let usdc = fermah_pay_stellar_chain::usdc::asset_contract_id(
        &fermah_pay_stellar_chain::usdc::circle_usdc(Network::Testnet),
        Network::Testnet,
    );
    let expected = PrepaidDeployment { contract: CONTRACT, usdc, treasury: treasury() }
        .deposit_authorization(&DepositIntent {
            owner: b.key.address().into(),
            amount: 25_000_000,
            deposit_id: deposit_id.try_into().unwrap(),
        });
    assert_eq!(entry.root_invocation, expected);
    let payload =
        signature_payload(network_id(Network::Testnet), &entry.credentials, &entry.root_invocation)
            .unwrap();
    assert_eq!(deposit.signature_payload, hex_lower(&payload));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_for_deployment_without_ledger_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("shop", "main", Network::Testnet).await;
    let b = buyer(&h, &t, "alice").await;
    let status = ledger(&h)
        .await
        .prepare_deposit(authed(prepare(&b.id, 10, "dep-1"), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "ledger_not_configured");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_retry_returns_the_first_deposit_and_other_reuse_conflicts(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let first = prepared(&h, &t, &b, 100).await;
    // The retry must not depend on the network: it is answered from the row.
    h.ledger.set(0);
    let retry = ledger(&h)
        .await
        .prepare_deposit(authed(prepare(&b.id, 100, "dep-1"), &t.token))
        .await
        .unwrap()
        .into_inner();
    assert!(!retry.created);
    assert_eq!(retry.deposit.unwrap(), first);

    let other = buyer(&h, &t, "bob").await;
    for request in [prepare(&b.id, 101, "dep-1"), prepare(&other.id, 100, "dep-1")] {
        let status = ledger(&h).await.prepare_deposit(authed(request, &t.token)).await.unwrap_err();
        assert_refused(&status, Code::AlreadyExists, "idempotency_conflict");
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_for_buyer_of_another_deployment_is_not_found(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (_, b) = setup(&h).await;
    let other = h.tenant("shop", "second", Network::Testnet).await;
    bind_other(&h, &other).await;
    let status = ledger(&h)
        .await
        .prepare_deposit(authed(prepare(&b.id, 10, "dep-1"), &other.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
}

async fn bind_other(h: &Harness, t: &Tenant) {
    let binding = LedgerBinding {
        contract: contract_strkey([8; 32]),
        treasury: treasury(),
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, t.deployment_id, &binding).await.unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_when_network_is_unreachable_is_unavailable_and_creates_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    h.ledger.set(0);
    let status = ledger(&h)
        .await
        .prepare_deposit(authed(prepare(&b.id, 10, "dep-1"), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::Unavailable, "network_unavailable");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pay_stellar.deposits")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_malformed_deposit_requests_are_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    for (request, reason) in [
        (prepare(&b.id, 0, "dep-1"), "invalid_amount"),
        (prepare(&b.id, -5, "dep-1"), "invalid_amount"),
        (prepare(&b.id, 10, "has space"), "invalid_idempotency_key"),
        (prepare("not-a-uuid", 10, "dep-1"), "invalid_buyer_id"),
    ] {
        let status = ledger(&h).await.prepare_deposit(authed(request, &t.token)).await.unwrap_err();
        assert_refused(&status, Code::InvalidArgument, reason);
    }
}

// ---- deposit signature ----------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_signed_entry_is_stored_and_resubmitting_it_changes_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;
    let signed = signed_by(&deposit, &b.key);

    let first = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed.clone()), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(first.state(), DepositState::Signed);
    let stored: String =
        sqlx::query_scalar("SELECT signed_authorization_xdr FROM pay_stellar.deposits")
            .fetch_one(&h.owner)
            .await
            .unwrap();
    assert_eq!(stored, signed);

    let again = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(again, first);
}

/// The signature map shape the host verifies, with an arbitrary signature.
fn with_signature(
    entry: &SorobanAuthorizationEntry,
    public_key: [u8; 32],
    signature: [u8; 64],
) -> String {
    let bytes = |b: &[u8]| ScVal::Bytes(ScBytes(BytesM::try_from(b.to_vec()).unwrap()));
    let symbol = |s: &str| ScVal::Symbol(ScSymbol(s.try_into().unwrap()));
    let map = ScVal::Map(Some(ScMap(
        vec![
            ScMapEntry { key: symbol("public_key"), val: bytes(&public_key) },
            ScMapEntry { key: symbol("signature"), val: bytes(&signature) },
        ]
        .try_into()
        .unwrap(),
    )));
    let mut entry = entry.clone();
    let (SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds)) =
        &mut entry.credentials
    else {
        panic!("not an address entry")
    };
    creds.signature = ScVal::Vec(Some(ScVec(vec![map].try_into().unwrap())));
    xdr(&entry)
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_entry_signed_by_another_key_is_refused_and_leaves_deposit_unsigned(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;
    let entry = entry_of(&deposit);
    let payload =
        signature_payload(network_id(Network::Testnet), &entry.credentials, &entry.root_invocation)
            .unwrap();
    let intruder = SecretKey::generate().unwrap();
    // The buyer's public key with the intruder's signature over the right
    // payload: only the signature check itself can refuse it.
    let forged = with_signature(&entry, *b.key.address().public_key(), intruder.sign_raw(&payload));

    let status = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, forged), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_signature");

    // Positive control: the same shape with the buyer's own signature.
    let genuine = with_signature(&entry, *b.key.address().public_key(), b.key.sign_raw(&payload));
    let signed = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, genuine), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(signed.state(), DepositState::Signed);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_entry_altered_before_signing_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;

    // A wallet that extends its own validity, or signs with the legacy
    // credential shape, signs something other than what was prepared.
    let mut extended = entry_of(&deposit);
    if let SorobanCredentials::AddressV2(creds) = &mut extended.credentials {
        creds.signature_expiration_ledger += 1;
    }
    let mut legacy = entry_of(&deposit);
    if let SorobanCredentials::AddressV2(creds) = legacy.credentials.clone() {
        legacy.credentials = SorobanCredentials::Address(creds);
    }
    for altered in [extended, legacy] {
        let signed = xdr(&sign_entry(&altered, network_id(Network::Testnet), &[&b.key]).unwrap());
        let status = ledger(&h)
            .await
            .submit_deposit(authed(submit(&deposit, signed), &t.token))
            .await
            .unwrap_err();
        assert_refused(&status, Code::InvalidArgument, "authorization_mismatch");
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_entry_submitted_after_its_expiration_ledger_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;
    let signed = signed_by(&deposit, &b.key);

    h.ledger.set(deposit.expiration_ledger + 1);
    let status = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed.clone()), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "deposit_expired");

    // Boundary: the expiration ledger itself is still valid.
    h.ledger.set(deposit.expiration_ledger);
    let accepted = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(accepted.state(), DepositState::Signed);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_different_entry_for_signed_deposit_is_refused_before_verification(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;
    ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed_by(&deposit, &b.key)), &t.token))
        .await
        .unwrap();

    let garbage = with_signature(&entry_of(&deposit), [1; 32], [2; 64]);
    let status = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, garbage), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "deposit_already_signed");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_of_another_deployment_cannot_be_signed_or_read(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("shop", "main-2", Network::Testnet).await;
    let owner = h.tenant("shop", "main-3", Network::Testnet).await;
    bind_other(&h, &owner).await;
    let own_buyer = buyer(&h, &owner, "alice").await;
    let deposit = prepared(&h, &owner, &own_buyer, 100).await;

    let status = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed_by(&deposit, &own_buyer.key)), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "deposit_not_found");
    let status = ledger(&h)
        .await
        .get_deposit(authed(GetDepositRequest { deposit_id: deposit.deposit_id }, &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "deposit_not_found");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_undecodable_entry_is_refused(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;
    let status = ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, "AAAA".to_owned()), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_authorization_entry");
}

// ---- charge admission -----------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_admitted_charge_debits_at_once_and_names_its_contract_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    fund(&h, &b.id, 100).await;
    for key in ["c-1", "c-2"] {
        let reply =
            ledger(&h).await.create_charge(authed(charge(&b.id, 30, key), &t.token)).await.unwrap();
        let charge = reply.into_inner().charge.unwrap();
        assert_eq!(charge.state(), ChargeState::Admitted);
        // The contract identifier is the key's digest, so any retry of the
        // request names the same charge on-chain.
        let digest: [u8; 32] = sha2::Sha256::digest(key.as_bytes()).into();
        assert_eq!(charge.contract_charge_id, hex_lower(&digest));
        assert_eq!(charge.last_ledger, h.ledger.get() + CHARGE_VALIDITY_LEDGERS);
    }
    assert_eq!(balance(&h, &t, &b.id).await, (40, 60));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_new_charge_needs_the_network_but_a_retry_does_not(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    fund(&h, &b.id, 100).await;
    let first = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 30, "c-1"), &t.token))
        .await
        .unwrap()
        .into_inner();
    h.ledger.set(0);
    let status = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 30, "c-2"), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::Unavailable, "network_unavailable");
    let retry = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 30, "c-1"), &t.token))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((retry.created, retry.charge), (false, first.charge));
    assert_eq!(balance(&h, &t, &b.id).await, (70, 30));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_above_available_is_refused(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    fund(&h, &b.id, 50).await;
    let status = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 51, "c-1"), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "insufficient_balance");
    assert_eq!(balance(&h, &t, &b.id).await, (50, 0));

    // Boundary: exactly the available amount is admitted.
    let admitted = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 50, "c-2"), &t.token))
        .await
        .unwrap()
        .into_inner()
        .charge
        .unwrap();
    assert_eq!(admitted.state(), ChargeState::Admitted);
    assert_eq!(balance(&h, &t, &b.id).await, (0, 50));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_retry_debits_once_and_other_reuse_conflicts(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let other = buyer(&h, &t, "bob").await;
    // The first charge drains the balance: a retry must still find it,
    // rather than being judged as a new charge against nothing.
    fund(&h, &b.id, 30).await;
    fund(&h, &other.id, 100).await;
    let first = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 30, "c-1"), &t.token))
        .await
        .unwrap()
        .into_inner();
    let retry = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 30, "c-1"), &t.token))
        .await
        .unwrap()
        .into_inner();
    assert!(first.created && !retry.created);
    assert_eq!(retry.charge, first.charge);
    assert_eq!(balance(&h, &t, &b.id).await, (0, 30));

    for request in [charge(&b.id, 31, "c-1"), charge(&other.id, 30, "c-1")] {
        let status = ledger(&h).await.create_charge(authed(request, &t.token)).await.unwrap_err();
        assert_refused(&status, Code::AlreadyExists, "idempotency_conflict");
    }
    assert_eq!(balance(&h, &t, &other.id).await, (100, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_charges_never_overdraw(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    fund(&h, &b.id, 70).await;
    let attempts = (0..12).map(|i| {
        let token = t.token.clone();
        let request = charge(&b.id, 10, &format!("c-{i}"));
        let url = format!("http://{}", h.addr);
        async move {
            LedgerServiceClient::connect(url)
                .await
                .unwrap()
                .create_charge(authed(request, &token))
                .await
        }
    });
    let results = futures_join_all(attempts).await;

    let mut admitted = std::collections::BTreeSet::new();
    for result in results {
        match result {
            Ok(reply) => {
                admitted.insert(reply.into_inner().charge.unwrap().contract_charge_id);
            }
            // Every refusal is the balance refusal, never an internal error.
            Err(status) => {
                assert_refused(&status, Code::FailedPrecondition, "insufficient_balance")
            }
        }
    }
    assert_eq!(admitted.len(), 7);
    assert_eq!(balance(&h, &t, &b.id).await, (0, 70));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_retries_of_one_charge_debit_once(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    // Exactly one charge's worth: a retry that judged the balance before the
    // first attempt committed would find nothing left and refuse, instead of
    // returning the charge it repeats.
    fund(&h, &b.id, 10).await;
    let attempts = (0..8).map(|_| {
        let token = t.token.clone();
        let request = charge(&b.id, 10, "same");
        let url = format!("http://{}", h.addr);
        async move {
            LedgerServiceClient::connect(url)
                .await
                .unwrap()
                .create_charge(authed(request, &token))
                .await
        }
    });
    let replies: Vec<_> =
        futures_join_all(attempts).await.into_iter().map(|r| r.unwrap().into_inner()).collect();
    assert_eq!(replies.iter().filter(|r| r.created).count(), 1);
    assert!(replies.iter().all(|r| r.charge == replies[0].charge));
    assert_eq!(balance(&h, &t, &b.id).await, (0, 10));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_reuse_of_one_key_for_two_buyers_admits_one_and_conflicts_the_rest(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let other = buyer(&h, &t, "bob").await;
    fund(&h, &b.id, 100).await;
    fund(&h, &other.id, 100).await;
    // Requests for different buyers lock different rows, so only the key's
    // uniqueness can order them.
    let attempts = (0..10).map(|i| {
        let token = t.token.clone();
        let buyer_id = if i % 2 == 0 { b.id.clone() } else { other.id.clone() };
        let url = format!("http://{}", h.addr);
        async move {
            LedgerServiceClient::connect(url)
                .await
                .unwrap()
                .create_charge(authed(charge(&buyer_id, 10, "shared"), &token))
                .await
        }
    });
    let mut created = 0;
    for result in futures_join_all(attempts).await {
        match result {
            Ok(reply) => created += usize::from(reply.into_inner().created),
            Err(status) => assert_refused(&status, Code::AlreadyExists, "idempotency_conflict"),
        }
    }
    assert_eq!(created, 1);
    let debited = balance(&h, &t, &b.id).await.1 + balance(&h, &t, &other.id).await.1;
    assert_eq!(debited, 10);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_deposit_preparations_with_one_key_create_one_deposit(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let attempts = (0..8).map(|_| {
        let token = t.token.clone();
        let request = prepare(&b.id, 100, "dep-1");
        let url = format!("http://{}", h.addr);
        async move {
            LedgerServiceClient::connect(url)
                .await
                .unwrap()
                .prepare_deposit(authed(request, &token))
                .await
        }
    });
    let replies: Vec<_> =
        futures_join_all(attempts).await.into_iter().map(|r| r.unwrap().into_inner()).collect();
    assert_eq!(replies.iter().filter(|r| r.created).count(), 1);
    assert!(replies.iter().all(|r| r.deposit == replies[0].deposit));
}

async fn futures_join_all<F: std::future::Future + Send + 'static>(
    futures: impl Iterator<Item = F>,
) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.map(tokio::spawn).collect();
    let mut outputs = Vec::with_capacity(handles.len());
    for handle in handles {
        outputs.push(handle.await.unwrap());
    }
    outputs
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_scope_and_input_refusals(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    fund(&h, &b.id, 100).await;
    let other = h.tenant("shop", "second", Network::Testnet).await;

    let status = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 10, "c-1"), &other.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
    let own = ledger(&h)
        .await
        .create_charge(authed(charge(&b.id, 10, "c-1"), &t.token))
        .await
        .unwrap()
        .into_inner()
        .charge
        .unwrap();
    let status = ledger(&h)
        .await
        .get_charge(authed(GetChargeRequest { charge_id: own.charge_id }, &other.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "charge_not_found");
    let status = ledger(&h)
        .await
        .get_balance(authed(GetBalanceRequest { buyer_id: b.id.clone() }, &other.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");

    for (request, reason) in [
        (charge(&b.id, 0, "c-2"), "invalid_amount"),
        (charge(&b.id, 10, ""), "invalid_idempotency_key"),
        (charge("x", 10, "c-2"), "invalid_buyer_id"),
    ] {
        let status = ledger(&h).await.create_charge(authed(request, &t.token)).await.unwrap_err();
        assert_refused(&status, Code::InvalidArgument, reason);
    }
    assert_eq!(balance(&h, &t, &b.id).await, (90, 10));
}

// ---- schema rules ---------------------------------------------------------

async fn submission(h: &Harness) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pay_stellar.submissions
             (id, network, kind, state, source_address, fee_source_address, sequence,
              valid_until, inner_hash, outer_hash, envelope_xdr, ledger, result_xdr, resolved_at)
         VALUES ($1, 'stellar:testnet', 'charge_batch', 'succeeded', $2, $2, 1, now(),
                 $3, $4, 'AAAA', 10, 'AAAA', now())",
    )
    .bind(id)
    .bind(treasury().as_str())
    .bind(vec![1_u8; 32])
    .bind(id.as_bytes().repeat(2))
    .execute(&h.owner)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_settled_charge_cannot_leave_its_final_state(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    fund(&h, &b.id, 100).await;
    ledger(&h).await.create_charge(authed(charge(&b.id, 10, "c-1"), &t.token)).await.unwrap();
    let submission = submission(&h).await;
    sqlx::query(
        "UPDATE pay_stellar.charges
         SET state = 'refused', outcome = 'insufficient_balance', submission_id = $1,
             batch_index = 0, settled_at = now()",
    )
    .bind(submission)
    .execute(&h.owner)
    .await
    .unwrap();

    // Leaving `refused` would let the refund on entering it run again.
    let error = sqlx::query(
        "UPDATE pay_stellar.charges SET state = 'submitted', outcome = NULL, settled_at = NULL",
    )
    .execute(&h.owner)
    .await
    .unwrap_err();
    let message = error.as_database_error().unwrap().message().to_owned();
    assert!(message.ends_with("is already refused"), "{message}");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_confirmed_deposit_cannot_leave_its_final_state(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let deposit = prepared(&h, &t, &b, 100).await;
    ledger(&h)
        .await
        .submit_deposit(authed(submit(&deposit, signed_by(&deposit, &b.key)), &t.token))
        .await
        .unwrap();
    let submission = submission(&h).await;
    sqlx::query(
        "UPDATE pay_stellar.deposits SET state = 'confirmed', submission_id = $1, resolved_at = now()",
    )
    .bind(submission)
    .execute(&h.owner)
    .await
    .unwrap();

    // Leaving `confirmed` would let the credit on entering it run again.
    let error =
        sqlx::query("UPDATE pay_stellar.deposits SET state = 'submitted', resolved_at = NULL")
            .execute(&h.owner)
            .await
            .unwrap_err();
    let message = error.as_database_error().unwrap().message().to_owned();
    assert!(message.ends_with("is already confirmed"), "{message}");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_api_role_cannot_create_rows_past_their_initial_state(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let buyer_id = Uuid::parse_str(&b.id).unwrap();
    let error = sqlx::query(
        "INSERT INTO pay_stellar.charges
             (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, charge_id,
              last_ledger, state)
         VALUES ($1, $2, $3, 'stellar:testnet', 'k', 1, sha256('k'), 5, 'admitted')",
    )
    .bind(Uuid::now_v7())
    .bind(buyer_id)
    .bind(t.deployment_id)
    .execute(&h.api)
    .await
    .unwrap_err();
    let code = error.as_database_error().unwrap().code().unwrap().into_owned();
    assert_eq!(code, "42501", "{error}");
}

// ---- ledger binding -------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_binding_follows_a_rotation_only_through_the_audited_sync(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, _) = setup(&h).await;
    let operator = AccountAddress::from_public_key([21; 32]);
    let treasury = AccountAddress::from_public_key([22; 32]);

    // Nothing but the function may change a binding.
    let error = sqlx::query("UPDATE pay_stellar.ledger_contracts SET operator_address = $1")
        .bind(operator.as_str())
        .execute(&h.issuer)
        .await
        .unwrap_err();
    assert_eq!(error.as_database_error().unwrap().code().unwrap(), "42501", "{error}");
    for pool in [&h.api] {
        let error = sqlx::query("SELECT pay_stellar.sync_ledger_binding($1, $2, $3, 'x')")
            .bind(t.deployment_id)
            .bind(operator.as_str())
            .bind(treasury.as_str())
            .execute(pool)
            .await
            .unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().unwrap(), "42501", "{error}");
    }

    let changed = issuance::sync_ledger_binding(
        &h.issuer,
        t.deployment_id,
        &operator,
        &treasury,
        "get_config",
    )
    .await
    .unwrap();
    let again = issuance::sync_ledger_binding(
        &h.issuer,
        t.deployment_id,
        &operator,
        &treasury,
        "get_config",
    )
    .await
    .unwrap();
    assert!(changed && !again);
    let bound = issuance::ledger_binding(&h.issuer, t.deployment_id).await.unwrap();
    assert_eq!((bound.operator, bound.treasury), (operator, treasury.clone()));
    let audit: Vec<(String, String)> = sqlx::query_as(
        "SELECT previous_treasury, current_treasury FROM pay_stellar.ledger_binding_changes",
    )
    .fetch_all(&h.owner)
    .await
    .unwrap();
    assert_eq!(audit, [(treasury_of_setup(), treasury.as_str().to_owned())]);
    let error = sqlx::query("DELETE FROM pay_stellar.ledger_binding_changes")
        .execute(&h.owner)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("append-only"), "{error}");
}

fn treasury_of_setup() -> String {
    treasury().as_str().to_owned()
}

// ---- mandates -----------------------------------------------------------------

const DAY: u64 = 86_400;

fn mandate_request(
    buyer_id: &str,
    amount: i64,
    period: u64,
    cycles: u32,
    key: &str,
) -> PrepareMandateRequest {
    PrepareMandateRequest {
        buyer_id: buyer_id.to_owned(),
        amount,
        period_secs: period,
        cycles,
        idempotency_key: key.to_owned(),
    }
}

async fn prepare_mandate(
    h: &Harness,
    t: &Tenant,
    request: PrepareMandateRequest,
) -> Result<(Mandate, bool), tonic::Status> {
    let reply = ledger(h).await.prepare_mandate(authed(request, &t.token)).await?.into_inner();
    Ok((reply.mandate.unwrap(), reply.created))
}

fn mandate_entry(mandate: &Mandate) -> SorobanAuthorizationEntry {
    SorobanAuthorizationEntry::from_xdr_base64(&mandate.authorization_entry_xdr, Limits::none())
        .unwrap()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_prepared_mandate_authorizes_exactly_the_mandate_and_its_approval(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let (mandate, created) =
        prepare_mandate(&h, &t, mandate_request(&b.id, 5_000_000, 30 * DAY, 2, "m-1"))
            .await
            .unwrap();
    assert!(created);
    assert_eq!(mandate.state(), MandateState::AwaitingSignature);
    let latest = h.ledger.get();
    assert_eq!(mandate.expiration_ledger, latest + AUTHORIZATION_VALIDITY_LEDGERS);
    // Every period has ended by the last ledger even at four seconds a
    // ledger, after the time the buyer has to sign.
    let periods_ledgers = u32::try_from((2 * 30 * DAY).div_ceil(4)).unwrap();
    assert_eq!(
        mandate.live_until_ledger,
        latest + AUTHORIZATION_VALIDITY_LEDGERS + periods_ledgers
    );
    let mandate_id: Vec<u8> = sqlx::query_scalar("SELECT mandate_id FROM pay_stellar.mandates")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    let usdc = fermah_pay_stellar_chain::usdc::asset_contract_id(
        &fermah_pay_stellar_chain::usdc::circle_usdc(Network::Testnet),
        Network::Testnet,
    );
    let expected = PrepaidDeployment { contract: CONTRACT, usdc, treasury: treasury() }
        .authorize_recurring_authorization(&MandateIntent {
            owner: b.key.address(),
            mandate_id: mandate_id.try_into().unwrap(),
            amount: 5_000_000,
            period_secs: 30 * DAY,
            cycles: 2,
            live_until: mandate.live_until_ledger,
        });
    let entry = mandate_entry(&mandate);
    assert_eq!(entry.root_invocation, expected);
    let payload =
        signature_payload(network_id(Network::Testnet), &entry.credentials, &entry.root_invocation)
            .unwrap();
    assert_eq!(mandate.signature_payload, hex_lower(&payload));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_mandate_requests_outside_the_policy_or_the_scope_are_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let refused = [
        (mandate_request(&b.id, 0, DAY, 2, "a"), Code::InvalidArgument, "invalid_amount"),
        (mandate_request(&b.id, 10, 59, 2, "b"), Code::InvalidArgument, "invalid_period"),
        (mandate_request(&b.id, 10, DAY, 0, "c"), Code::InvalidArgument, "invalid_cycles"),
        // About 140 days at four seconds a ledger: past the network's limit.
        (mandate_request(&b.id, 10, DAY, 140, "d"), Code::InvalidArgument, "mandate_too_long"),
        (mandate_request(&b.id, 10, u64::MAX, 2, "e"), Code::InvalidArgument, "mandate_too_long"),
        (mandate_request("not-a-uuid", 10, DAY, 2, "f"), Code::InvalidArgument, "invalid_buyer_id"),
    ];
    for (request, code, reason) in refused {
        let status = prepare_mandate(&h, &t, request).await.unwrap_err();
        assert_refused(&status, code, reason);
    }
    // A buyer of another deployment does not exist for this one.
    let other = h.tenant("shop", "other", Network::Testnet).await;
    let stranger = buyer(&h, &other, "bob").await;
    let status =
        prepare_mandate(&h, &t, mandate_request(&stranger.id, 10, DAY, 2, "g")).await.unwrap_err();
    assert_refused(&status, Code::NotFound, "buyer_not_found");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pay_stellar.mandates")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_mandate_retry_returns_the_first_and_other_reuse_conflicts(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let (first, _) =
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 2, "m-1")).await.unwrap();
    let (again, created) =
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 2, "m-1")).await.unwrap();
    assert_eq!((again.mandate_id, created), (first.mandate_id, false));
    let status =
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 3, "m-1")).await.unwrap_err();
    assert_refused(&status, Code::AlreadyExists, "idempotency_conflict");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_mandates_and_revocations_share_a_daily_quota_per_buyer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    for i in 0..4 {
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 2, &format!("m-{i}")))
            .await
            .unwrap();
    }
    let revocation =
        PrepareRevocationRequest { buyer_id: b.id.clone(), idempotency_key: "r-1".to_owned() };
    ledger(&h).await.prepare_revocation(authed(revocation, &t.token)).await.unwrap();
    let status =
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 2, "m-5")).await.unwrap_err();
    assert_refused(&status, Code::ResourceExhausted, "mandate_quota_exceeded");
    let revocation =
        PrepareRevocationRequest { buyer_id: b.id.clone(), idempotency_key: "r-2".to_owned() };
    let status =
        ledger(&h).await.prepare_revocation(authed(revocation, &t.token)).await.unwrap_err();
    assert_refused(&status, Code::ResourceExhausted, "mandate_quota_exceeded");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_signed_mandate_is_verified_stored_once_and_invisible_to_other_deployments(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let (mandate, _) =
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 2, "m-1")).await.unwrap();
    let entry = mandate_entry(&mandate);
    let payload =
        signature_payload(network_id(Network::Testnet), &entry.credentials, &entry.root_invocation)
            .unwrap();
    // The buyer's public key with this key's signature over the right
    // payload: only the signature check itself can refuse a stranger's.
    let sign = |key: &SecretKey| {
        with_signature(&entry, *b.key.address().public_key(), key.sign_raw(&payload))
    };
    let submit = |signed: String| SubmitMandateRequest {
        mandate_id: mandate.mandate_id.clone(),
        signed_authorization_entry_xdr: signed,
    };
    let stranger = SecretKey::generate().unwrap();
    let status = ledger(&h)
        .await
        .submit_mandate(authed(submit(sign(&stranger)), &t.token))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_signature");
    let signed = sign(&b.key);
    let stored = ledger(&h)
        .await
        .submit_mandate(authed(submit(signed.clone()), &t.token))
        .await
        .unwrap()
        .into_inner()
        .mandate
        .unwrap();
    assert_eq!(stored.state(), MandateState::Signed);
    let again = ledger(&h).await.submit_mandate(authed(submit(signed), &t.token)).await.unwrap();
    assert_eq!(again.into_inner().mandate.unwrap().state(), MandateState::Signed);

    let other = h.tenant("shop", "other", Network::Testnet).await;
    let request = GetMandateRequest { mandate_id: mandate.mandate_id.clone() };
    let status = ledger(&h).await.get_mandate(authed(request, &other.token)).await.unwrap_err();
    assert_refused(&status, Code::NotFound, "mandate_not_found");
    let request = CreateRecurringChargeRequest {
        mandate_id: mandate.mandate_id.clone(),
        amount: 5,
        idempotency_key: "c-1".to_owned(),
    };
    let status =
        ledger(&h).await.create_recurring_charge(authed(request, &other.token)).await.unwrap_err();
    assert_refused(&status, Code::NotFound, "mandate_not_found");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_api_role_cannot_activate_mandates_or_settle_recurring_charges(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let (t, b) = setup(&h).await;
    let (mandate, _) =
        prepare_mandate(&h, &t, mandate_request(&b.id, 10, DAY, 2, "m-1")).await.unwrap();
    let id = Uuid::parse_str(&mandate.mandate_id).unwrap();
    let code_of =
        |error: sqlx::Error| error.as_database_error().unwrap().code().unwrap().into_owned();
    // Activation needs a start only the worker may write.
    let error = sqlx::query(
        "UPDATE pay_stellar.mandates SET state = 'active', starts_at = 1 WHERE id = $1",
    )
    .bind(id)
    .execute(&h.api)
    .await
    .unwrap_err();
    assert_eq!(code_of(error), "42501");
    let error = sqlx::query("UPDATE pay_stellar.mandates SET state = 'active' WHERE id = $1")
        .bind(id)
        .execute(&h.api)
        .await
        .unwrap_err();
    assert_eq!(code_of(error), "23514", "an active mandate needs its start");
    let error = sqlx::query(
        "INSERT INTO pay_stellar.recurring_charges
             (id, mandate_row_id, buyer_id, seller_deployment_id, network, idempotency_key, cycle,
              charge_id, amount, last_ledger, state)
         VALUES ($1, $2, $3, $4, 'stellar:testnet', 'k', 0, sha256('k'), 5, 5, 'charged')",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(Uuid::parse_str(&b.id).unwrap())
    .bind(t.deployment_id)
    .execute(&h.api)
    .await
    .unwrap_err();
    assert_eq!(code_of(error), "42501");
}
