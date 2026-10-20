//! The x402 facilitator interface against real PostgreSQL under the API's
//! production role: commitments signed by buyers (SEP-53), or authorized by
//! contract-account buyers through the commitment verifier, are verified and
//! settled into charges against the seller's prepaid ledger.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::atomic::Ordering;

use axum::body::Body;
use common::{Harness, Tenant, X402_VERIFIER, authed, start};
use fermah_pay_stellar_chain::authorization::signature_payload;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{
    x402_commitment_authorization, x402_commitment_call, x402_commitment_entry,
};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::sep53;
use fermah_pay_stellar_chain::stellar_xdr::{
    BytesM, Limits, ReadXdr, ScBytes, ScVal, SorobanAuthorizationEntry, SorobanCredentials, VecM,
    WriteXdr,
};
use fermah_pay_stellar_chain::transaction::ScAddressOf;
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, contract_strkey};
use fermah_pay_stellar_domain::{AccountAddress, ChainAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::ledger::BuyerCall;
use fermah_pay_stellar_gateway::x402::{commitment_message, reason};
use fermah_pay_stellar_proto::v1::CreateBuyerRequest;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

const NOW: i64 = 1_800_000_000;
const TIMEOUT: u64 = 60;

fn contract(seed: u8) -> String {
    contract_strkey([seed; 32])
}

fn usdc() -> String {
    contract_strkey(asset_contract_id(&circle_usdc(Network::Testnet), Network::Testnet))
}

struct World {
    h: Harness,
    tenant: Tenant,
    pay_to: String,
}

async fn world(opts: PgPoolOptions, connect: PgConnectOptions) -> World {
    let h = start(opts, connect, Network::Testnet).await;
    let tenant = h.tenant("shop", "main", Network::Testnet).await;
    let pay_to = contract(7);
    bind(&h, &tenant, &pay_to).await;
    World { h, tenant, pay_to }
}

async fn bind(h: &Harness, tenant: &Tenant, pay_to: &str) {
    let binding = LedgerBinding {
        contract: pay_to.to_owned(),
        treasury: Some(AccountAddress::from_public_key([11; 32])),
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, tenant.deployment_id, &binding).await.unwrap();
}

/// A buyer of `tenant` holding `available` base units.
async fn buyer(h: &Harness, tenant: &Tenant, name: &str, available: i64) -> SecretKey {
    let key = SecretKey::generate().unwrap();
    let created = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: name.to_owned(),
                wallet_address: key.address().to_string(),
            },
            &tenant.token,
        ))
        .await
        .unwrap()
        .into_inner();
    sqlx::query("UPDATE pay_stellar.buyers SET available = $2 WHERE id = $1")
        .bind(Uuid::parse_str(&created.buyer.unwrap().buyer_id).unwrap())
        .bind(available)
        .execute(&h.owner)
        .await
        .unwrap();
    key
}

fn fresh_commitment() -> String {
    let mut bytes = Vec::from(*Uuid::now_v7().as_bytes());
    bytes.extend_from_slice(Uuid::now_v7().as_bytes());
    hex_lower(&bytes)
}

/// The fields of one commitment, before signing.
#[derive(Clone)]
struct Terms {
    network: String,
    scheme: String,
    asset: String,
    pay_to: String,
    amount: String,
    commitment: String,
    valid_until: i64,
}

impl Terms {
    fn new(pay_to: &str, amount: i64) -> Self {
        Self {
            network: "stellar:testnet".to_owned(),
            scheme: "batch-settlement".to_owned(),
            asset: usdc(),
            pay_to: pay_to.to_owned(),
            amount: amount.to_string(),
            commitment: fresh_commitment(),
            valid_until: NOW + 30,
        }
    }

    fn requirements(&self) -> Value {
        json!({
            "scheme": self.scheme,
            "network": self.network,
            "amount": self.amount,
            "asset": self.asset,
            "payTo": self.pay_to,
            "maxTimeoutSeconds": TIMEOUT,
        })
    }

    /// The facilitator request for these terms, signed by `signer` as
    /// `payer`.
    fn signed(&self, payer: &AccountAddress, signer: &SecretKey) -> Value {
        let valid_until = self.valid_until.to_string();
        let message = commitment_message(
            &self.network,
            &self.asset,
            &self.pay_to,
            &self.amount,
            payer.as_str(),
            &self.commitment,
            &valid_until,
        );
        let signature = sep53::sign(signer, message.as_bytes());
        json!({
            "x402Version": 2,
            "paymentPayload": {
                "x402Version": 2,
                "resource": { "url": "https://seller.example/api/answer" },
                "accepted": self.requirements(),
                "payload": {
                    "payer": payer.as_str(),
                    "commitment": self.commitment,
                    "validUntil": valid_until,
                    "signature": base64_encode(&signature),
                },
            },
            "paymentRequirements": self.requirements(),
        })
    }

    fn by(&self, buyer: &SecretKey) -> Value {
        self.signed(&buyer.address(), buyer)
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl World {
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call_on(&self.h.x402, method, path, token, body).await
    }

    async fn call_on(
        &self,
        router: &axum::Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn verify(&self, body: Value) -> Value {
        let (status, reply) =
            self.call("POST", "/verify", Some(&self.tenant.token), Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        reply
    }

    async fn settle(&self, body: Value) -> Value {
        let (status, reply) =
            self.call("POST", "/settle", Some(&self.tenant.token), Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        reply
    }

    async fn available(&self, buyer: &SecretKey) -> i64 {
        sqlx::query_scalar("SELECT available FROM pay_stellar.buyers WHERE wallet_address = $1")
            .bind(buyer.address().as_str())
            .fetch_one(&self.h.owner)
            .await
            .unwrap()
    }

    async fn charges(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pay_stellar.charges")
            .fetch_one(&self.h.owner)
            .await
            .unwrap()
    }
}

fn invalid_reason(reply: &Value) -> &str {
    assert_eq!(reply["isValid"], false, "{reply}");
    reply["invalidReason"].as_str().unwrap()
}

fn error_reason(reply: &Value) -> &str {
    assert_eq!(reply["success"], false, "{reply}");
    reply["errorReason"].as_str().unwrap()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_supported_names_the_scheme_on_the_served_network(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let (status, reply) = w.call("GET", "/supported", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reply["kinds"],
        json!([{
            "x402Version": 2,
            "scheme": "batch-settlement",
            "network": "stellar:testnet",
            "extra": { "commitmentVerifier": contract_strkey(X402_VERIFIER) },
        }])
    );
    // Without a verifier, no kind names one.
    let (status, reply) = w.call_on(&w.h.x402_classic_only, "GET", "/supported", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reply["kinds"],
        json!([{ "x402Version": 2, "scheme": "batch-settlement", "network": "stellar:testnet" }])
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_signed_commitment_verifies_settles_once_and_reports_its_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let terms = Terms::new(&w.pay_to, 30);

    let verified = w.verify(terms.by(&alice)).await;
    assert_eq!(verified, json!({ "isValid": true, "payer": alice.address().as_str() }));
    assert_eq!(w.available(&alice).await, 100, "verifying debits nothing");

    let settled = w.settle(terms.by(&alice)).await;
    assert_eq!(settled["success"], true, "{settled}");
    assert_eq!(settled["transaction"], terms.commitment);
    assert_eq!(settled["amount"], "30");
    assert_eq!(settled["extensions"]["prepaidLedger"]["state"], "admitted");
    assert_eq!(settled["extensions"]["prepaidLedger"]["replayed"], false);
    assert_eq!(w.available(&alice).await, 70);

    // A retried settlement answers with the same charge and debits nothing.
    let again = w.settle(terms.by(&alice)).await;
    assert_eq!(again["success"], true);
    assert_eq!(
        again["extensions"]["prepaidLedger"]["chargeId"],
        settled["extensions"]["prepaidLedger"]["chargeId"]
    );
    assert_eq!(again["extensions"]["prepaidLedger"]["replayed"], true);
    assert_eq!(w.available(&alice).await, 70);
    assert_eq!(w.charges().await, 1);

    // The spent commitment no longer verifies.
    assert_eq!(invalid_reason(&w.verify(terms.by(&alice)).await), reason::USED);

    let (status, settlement) = w
        .call("GET", &format!("/settlements/{}", terms.commitment), Some(&w.tenant.token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        settlement["settlement"]["chargeId"],
        settled["extensions"]["prepaidLedger"]["chargeId"]
    );
    assert_eq!(settlement["amount"], "30");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_each_altered_term_is_refused_and_nothing_is_debited(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let base = Terms::new(&w.pay_to, 30);
    // The positive control: these terms, unaltered, verify.
    assert_eq!(w.verify(base.by(&alice)).await["isValid"], true);

    let mut cases: Vec<(&str, Value)> = Vec::new();
    // Signed by a key other than the payer's.
    let stranger = SecretKey::generate().unwrap();
    cases.push((reason::SIGNATURE, base.signed(&alice.address(), &stranger)));
    // Signed over one amount, presented with another.
    let mut forged = base.by(&alice);
    forged["paymentPayload"]["accepted"]["amount"] = json!("31");
    forged["paymentRequirements"]["amount"] = json!("31");
    cases.push((reason::SIGNATURE, forged));
    // The accepted terms differ from the requirements.
    let mut mismatched = base.by(&alice);
    mismatched["paymentRequirements"]["amount"] = json!("31");
    cases.push((reason::INVALID_REQUIREMENTS, mismatched));
    // Another contract, or another asset, than the seller's.
    cases.push((
        reason::INVALID_REQUIREMENTS,
        Terms { pay_to: contract(8), ..base.clone() }.by(&alice),
    ));
    cases.push((
        reason::INVALID_REQUIREMENTS,
        Terms { asset: contract(9), ..base.clone() }.by(&alice),
    ));
    cases.push((
        reason::INVALID_NETWORK,
        Terms { network: "stellar:pubnet".to_owned(), ..base.clone() }.by(&alice),
    ));
    cases.push((
        reason::UNSUPPORTED_SCHEME,
        Terms { scheme: "exact".to_owned(), ..base.clone() }.by(&alice),
    ));
    let mut version = base.by(&alice);
    version["x402Version"] = json!(1);
    cases.push((reason::INVALID_VERSION, version));
    cases.push((reason::EXPIRED, Terms { valid_until: NOW - 1, ..base.clone() }.by(&alice)));
    cases.push((
        reason::TOO_FAR,
        Terms { valid_until: NOW + i64::try_from(TIMEOUT).unwrap() + 1, ..base.clone() }.by(&alice),
    ));
    // A valid signature, from a wallet that is not this seller's buyer.
    cases.push((reason::UNKNOWN_PAYER, base.by(&stranger)));
    cases.push((reason::INSUFFICIENT_FUNDS, Terms::new(&w.pay_to, 101).by(&alice)));

    for (expected, body) in cases {
        assert_eq!(invalid_reason(&w.verify(body.clone()).await), expected, "{body}");
        assert_eq!(error_reason(&w.settle(body.clone()).await), expected, "{body}");
    }
    assert_eq!(w.available(&alice).await, 100);
    assert_eq!(w.charges().await, 0);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_settled_commitment_is_answered_after_its_window_but_not_reused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let terms = Terms::new(&w.pay_to, 30);
    assert_eq!(w.settle(terms.by(&alice)).await["success"], true);

    // Past the window, a retry of the settlement still gets its answer...
    w.h.now.store(terms.valid_until + 1, Ordering::SeqCst);
    let retried = w.settle(terms.by(&alice)).await;
    assert_eq!(retried["success"], true);
    assert_eq!(retried["extensions"]["prepaidLedger"]["replayed"], true);
    // ...while a new commitment with the same window is refused.
    let late = Terms { commitment: fresh_commitment(), ..terms.clone() };
    assert_eq!(error_reason(&w.settle(late.by(&alice)).await), reason::EXPIRED);

    // The same commitment identifier under other terms is a conflict.
    w.h.now.store(NOW, Ordering::SeqCst);
    let reused = Terms { amount: "20".to_owned(), ..terms.clone() };
    assert_eq!(error_reason(&w.settle(reused.by(&alice)).await), reason::CONFLICT);
    assert_eq!(w.available(&alice).await, 70);
    assert_eq!(w.charges().await, 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_seller_can_neither_call_unauthenticated_nor_settle_another_sellers_buyer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let terms = Terms::new(&w.pay_to, 30);

    let (status, _) = w.call("POST", "/settle", None, Some(terms.by(&alice))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) =
        w.call("POST", "/settle", Some("fps_test_not-a-key"), Some(terms.by(&alice))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Another seller, with its own contract: Alice's commitment to the first
    // seller names a contract it does not own, and Alice is not its buyer.
    let other = w.h.tenant("shop", "other", Network::Testnet).await;
    let other_pay_to = contract(21);
    bind(&w.h, &other, &other_pay_to).await;
    let settle_as = |token: String, body: Value| {
        let w = &w;
        async move { w.call("POST", "/settle", Some(&token), Some(body)).await.1 }
    };
    assert_eq!(
        error_reason(&settle_as(other.token.clone(), terms.by(&alice)).await),
        reason::INVALID_REQUIREMENTS
    );
    let redirected = Terms::new(&other_pay_to, 30);
    assert_eq!(
        error_reason(&settle_as(other.token.clone(), redirected.by(&alice)).await),
        reason::UNKNOWN_PAYER
    );
    assert_eq!(w.charges().await, 0);

    // A malformed body is answered with the protocol's reason.
    let (status, reply) =
        w.call("POST", "/settle", Some(&w.tenant.token), Some(json!({ "x402Version": 2 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(reply["errorReason"], reason::INVALID_PAYLOAD);
}

// ---- contract-account payers

const WALLET: [u8; 32] = [21; 32];

fn wallet() -> ChainAddress {
    ChainAddress::Contract(WALLET)
}

/// Registers the contract account `WALLET` as a buyer of `tenant` holding
/// `available` base units.
async fn contract_buyer(h: &Harness, tenant: &Tenant, available: i64) {
    let created = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: "smart-wallet".to_owned(),
                wallet_address: wallet().to_string(),
            },
            &tenant.token,
        ))
        .await
        .unwrap()
        .into_inner();
    sqlx::query("UPDATE pay_stellar.buyers SET available = $2 WHERE id = $1")
        .bind(Uuid::parse_str(&created.buyer.unwrap().buyer_id).unwrap())
        .bind(available)
        .execute(&h.owner)
        .await
        .unwrap();
}

impl Terms {
    fn message(&self, payer: &str) -> String {
        commitment_message(
            &self.network,
            &self.asset,
            &self.pay_to,
            &self.amount,
            payer,
            &self.commitment,
            &self.valid_until.to_string(),
        )
    }

    /// The wallet's entry for these terms against `verifier`, with a
    /// credential only the account interprets: whether it holds is the
    /// network's answer, which the harness sets.
    fn entry(&self, verifier: [u8; 32]) -> SorobanAuthorizationEntry {
        let mut entry = x402_commitment_entry(
            verifier,
            &wallet(),
            &self.message(&wallet().to_string()),
            7,
            5_000,
        );
        let SorobanCredentials::Address(creds) = &mut entry.credentials else { unreachable!() };
        creds.signature = ScVal::Bytes(ScBytes(BytesM::try_from(vec![9_u8; 64]).unwrap()));
        entry
    }

    /// The facilitator request for these terms, proved by the contract
    /// account with `entry`.
    fn authorized(&self, entry: &SorobanAuthorizationEntry) -> Value {
        let mut body = self.by(&SecretKey::generate().unwrap());
        let payload = &mut body["paymentPayload"]["payload"];
        payload["payer"] = json!(wallet().to_string());
        payload.as_object_mut().unwrap().remove("signature");
        payload["authorizationEntry"] = json!(entry.to_xdr_base64(Limits::none()).unwrap());
        body
    }

    fn by_wallet(&self) -> Value {
        self.authorized(&self.entry(X402_VERIFIER))
    }
}

impl World {
    async fn wallet_available(&self) -> i64 {
        sqlx::query_scalar("SELECT available FROM pay_stellar.buyers WHERE wallet_address = $1")
            .bind(wallet().to_string())
            .fetch_one(&self.h.owner)
            .await
            .unwrap()
    }

    fn simulations(&self) -> usize {
        self.h.ledger.simulations().seen.len()
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_account_pays_once_the_network_accepts_its_authorization(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    let terms = Terms::new(&w.pay_to, 30);

    let verified = w.verify(terms.by_wallet()).await;
    assert_eq!(verified, json!({ "isValid": true, "payer": wallet().to_string() }));
    // The network was asked about exactly the verifier call the wallet
    // authorized, with the wallet's entry as its only authorization, from
    // the deployment's treasury.
    {
        let simulations = w.h.ledger.simulations();
        let [(source, call, auth)] = simulations.seen.as_slice() else {
            panic!("expected one simulation, got {}", simulations.seen.len())
        };
        assert_eq!(*source, AccountAddress::from_public_key([11; 32]));
        assert_eq!(
            *call,
            x402_commitment_call(X402_VERIFIER, &wallet(), &terms.message(&wallet().to_string()))
        );
        assert_eq!(*auth, vec![terms.entry(X402_VERIFIER)]);
    }

    let settled = w.settle(terms.by_wallet()).await;
    assert_eq!(settled["success"], true, "{settled}");
    assert_eq!(settled["payer"], wallet().to_string());
    assert_eq!(settled["extensions"]["prepaidLedger"]["replayed"], false);
    assert_eq!(w.wallet_available().await, 70);
    assert_eq!(w.simulations(), 2);

    // The wallet's entry no longer holds (it lapsed, or the wallet changed
    // its rules), yet a retry is answered from the charge, without asking
    // the network again and without debiting.
    w.h.ledger.simulations().answer = BuyerCall::Refused("Error(Auth, InvalidAction)".to_owned());
    let again = w.settle(terms.by_wallet()).await;
    assert_eq!(again["success"], true, "{again}");
    assert_eq!(again["extensions"]["prepaidLedger"]["replayed"], true);
    assert_eq!(
        again["extensions"]["prepaidLedger"]["chargeId"],
        settled["extensions"]["prepaidLedger"]["chargeId"]
    );
    assert_eq!(invalid_reason(&w.verify(terms.by_wallet()).await), reason::USED);
    assert_eq!(w.simulations(), 2);
    assert_eq!(w.wallet_available().await, 70);
    assert_eq!(w.charges().await, 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_accounts_commitment_is_refused_when_the_network_refuses_it(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    let terms = Terms::new(&w.pay_to, 30);
    w.h.ledger.simulations().answer = BuyerCall::Refused(
        "HostError: Error(Auth, InvalidAction) \"failed account authentication\"".to_owned(),
    );

    assert_eq!(invalid_reason(&w.verify(terms.by_wallet()).await), reason::SIGNATURE);
    assert_eq!(error_reason(&w.settle(terms.by_wallet()).await), reason::SIGNATURE);
    assert_eq!(w.simulations(), 2, "each call asked the network");
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(w.charges().await, 0);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_answer_that_says_nothing_about_the_account_is_not_its_consent(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    // The verifier missing from the network, or archived: the call never
    // reaches the account's `__check_auth`.
    for answer in [
        BuyerCall::Refused("HostError: Error(Storage, MissingValue)".to_owned()),
        BuyerCall::RestoreRequired,
    ] {
        w.h.ledger.simulations().answer = answer;
        let terms = Terms::new(&w.pay_to, 30);
        assert_eq!(invalid_reason(&w.verify(terms.by_wallet()).await), reason::UNEXPECTED_VERIFY);
        assert_eq!(error_reason(&w.settle(terms.by_wallet()).await), reason::UNEXPECTED_SETTLE);
    }
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(w.charges().await, 0);
    // The positive control: a request like these once the network accepts it.
    w.h.ledger.simulations().answer = common::accepted(1_000);
    assert_eq!(w.settle(Terms::new(&w.pay_to, 30).by_wallet()).await["success"], true);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_entry_for_anything_but_these_terms_is_refused_before_the_network_is_asked(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    let base = Terms::new(&w.pay_to, 30);

    let mut cases: Vec<(&str, Value)> = Vec::new();
    // Authorized for one amount, presented with another.
    let mut forged = base.by_wallet();
    forged["paymentPayload"]["accepted"]["amount"] = json!("31");
    forged["paymentRequirements"]["amount"] = json!("31");
    cases.push((reason::SIGNATURE, forged));
    // For another commitment identifier.
    let other = Terms { commitment: fresh_commitment(), ..base.clone() };
    cases.push((reason::SIGNATURE, base.authorized(&other.entry(X402_VERIFIER))));
    // Against another contract than the configured verifier.
    cases.push((reason::SIGNATURE, base.authorized(&base.entry([43; 32]))));
    // Signed for another address than the payer.
    let mut stranger = base.entry(X402_VERIFIER);
    let SorobanCredentials::Address(creds) = &mut stranger.credentials else { unreachable!() };
    creds.address = ChainAddress::Contract([22; 32]).sc_address();
    cases.push((reason::SIGNATURE, base.authorized(&stranger)));
    // Authorizing more than the verifier call.
    let mut wider = base.entry(X402_VERIFIER);
    wider.root_invocation.sub_invocations =
        VecM::try_from(vec![base.entry(X402_VERIFIER).root_invocation]).unwrap();
    cases.push((reason::SIGNATURE, base.authorized(&wider)));
    // Not an entry at all.
    let mut garbled = base.by_wallet();
    garbled["paymentPayload"]["payload"]["authorizationEntry"] = json!("AAAA");
    cases.push((reason::INVALID_PAYLOAD, garbled));
    // An entry whose signature declares four gigabytes of bytes.
    let mut hostile = Vec::new();
    for word in [1_u32, 1] {
        hostile.extend_from_slice(&word.to_be_bytes());
    }
    hostile.extend_from_slice(&WALLET);
    hostile.extend_from_slice(&7_i64.to_be_bytes());
    for word in [5_000_u32, 13, u32::MAX] {
        hostile.extend_from_slice(&word.to_be_bytes());
    }
    hostile.extend_from_slice(&[0; 8]);
    let mut huge = base.by_wallet();
    huge["paymentPayload"]["payload"]["authorizationEntry"] = json!(base64_encode(&hostile));
    cases.push((reason::INVALID_PAYLOAD, huge));
    // A classic account's proof for a contract payer, both proofs, and a
    // contract account's proof for a classic payer.
    let mut signature_only = base.by_wallet();
    let payload = signature_only["paymentPayload"]["payload"].as_object_mut().unwrap();
    payload.remove("authorizationEntry");
    payload.insert("signature".to_owned(), json!(base64_encode(&[1; 64])));
    cases.push((reason::INVALID_PAYLOAD, signature_only));
    let mut both = base.by_wallet();
    both["paymentPayload"]["payload"]["signature"] = json!(base64_encode(&[1; 64]));
    cases.push((reason::INVALID_PAYLOAD, both));
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let mut entry_for_account = base.by(&alice);
    let payload = entry_for_account["paymentPayload"]["payload"].as_object_mut().unwrap();
    payload.remove("signature");
    payload.insert(
        "authorizationEntry".to_owned(),
        json!(base.entry(X402_VERIFIER).to_xdr_base64(Limits::none()).unwrap()),
    );
    cases.push((reason::INVALID_PAYLOAD, entry_for_account));

    for (expected, body) in cases {
        assert_eq!(invalid_reason(&w.verify(body.clone()).await), expected, "{body}");
        assert_eq!(error_reason(&w.settle(body.clone()).await), expected, "{body}");
    }
    assert_eq!(w.simulations(), 0, "no request reached the network");
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(w.charges().await, 0);
    // The positive control: the base terms, as the wallet authorized them.
    assert_eq!(w.verify(base.by_wallet()).await["isValid"], true);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_without_a_verifier_contract_payers_are_refused_and_classic_ones_served(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let classic_only = &w.h.x402_classic_only;
    let call = |path: &'static str, body: Value| {
        let w = &w;
        async move { w.call_on(classic_only, "POST", path, Some(&w.tenant.token), Some(body)).await.1 }
    };
    let terms = Terms::new(&w.pay_to, 30);
    assert_eq!(
        invalid_reason(&call("/verify", terms.by_wallet()).await),
        reason::CONTRACT_PAYER_UNSUPPORTED
    );
    assert_eq!(
        error_reason(&call("/settle", terms.by_wallet()).await),
        reason::CONTRACT_PAYER_UNSUPPORTED
    );
    assert_eq!(w.simulations(), 0);
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(call("/settle", terms.by(&alice)).await["success"], true);
}

/// A vault binding: no treasury, and the buyer's daily limit and exit as
/// the worker read them from the contract's events.
async fn bind_vault(h: &Harness, tenant: &Tenant, pay_to: &str) {
    let binding = LedgerBinding {
        contract: pay_to.to_owned(),
        treasury: None,
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, tenant.deployment_id, &binding).await.unwrap();
    let cursor = fermah_pay_stellar_chain::rpc::EventCursor::end_of_ledger(h.ledger.get());
    sqlx::query(
        "INSERT INTO pay_stellar.vault_event_cursors (seller_deployment_id, network, cursor, ledger)
         VALUES ($1, 'stellar:testnet', $2, $3)",
    )
    .bind(tenant.deployment_id)
    .bind(cursor.to_string())
    .bind(i64::from(h.ledger.get()))
    .execute(&h.owner)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_vaults_limit_and_exit_are_named_when_settling(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let tenant = h.tenant("shop", "vault", Network::Testnet).await;
    let pay_to = contract(7);
    bind_vault(&h, &tenant, &pay_to).await;
    let w = World { h, tenant, pay_to };
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    sqlx::query(
        "UPDATE pay_stellar.buyers SET cap = 40, vault_synced_ledger = 1 WHERE wallet_address = $1",
    )
    .bind(wallet().to_string())
    .execute(&w.h.owner)
    .await
    .unwrap();

    // A vault's co-signer is its operator: the simulation is sent from it.
    assert_eq!(w.settle(Terms::new(&w.pay_to, 30).by_wallet()).await["success"], true);
    assert_eq!(
        w.h.ledger.simulations().seen.last().unwrap().0,
        AccountAddress::from_public_key([12; 32])
    );
    assert_eq!(
        error_reason(&w.settle(Terms::new(&w.pay_to, 11).by_wallet()).await),
        reason::ABOVE_SPENDING_LIMIT
    );
    sqlx::query(
        "UPDATE pay_stellar.buyers SET cap = 1000, exit_amount = 65, exit_unlock_at = 5000
         WHERE wallet_address = $1",
    )
    .bind(wallet().to_string())
    .execute(&w.h.owner)
    .await
    .unwrap();
    assert_eq!(
        error_reason(&w.settle(Terms::new(&w.pay_to, 10).by_wallet()).await),
        reason::EXIT_REQUESTED
    );
    // Within what the exit leaves, it settles.
    assert_eq!(w.settle(Terms::new(&w.pay_to, 5).by_wallet()).await["success"], true);
    assert_eq!(w.wallet_available().await, 65);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_verifier_that_does_not_require_the_payers_authorization_is_not_consent(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    // The configured contract runs `commit` but never asks for the payer's
    // authorization, so the network accepts the call without consuming the
    // entry: any signature would pass.
    w.h.ledger.simulations().consumes_authorization = false;
    let terms = Terms::new(&w.pay_to, 30);
    assert_eq!(invalid_reason(&w.verify(terms.by_wallet()).await), reason::UNEXPECTED_VERIFY);
    assert_eq!(error_reason(&w.settle(terms.by_wallet()).await), reason::UNEXPECTED_SETTLE);
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(w.charges().await, 0);
    // The positive control: the same request once the call consumes it.
    w.h.ledger.simulations().consumes_authorization = true;
    assert_eq!(w.settle(terms.by_wallet()).await["success"], true);
}

/// How the witness answers differently from the gateway's own node.
type Disagreement = fn(&mut common::Simulations);

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_the_witness_must_accept_the_payers_authorization_too(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    // The gateway's own node accepts every entry; the witness disagrees.
    let cases: [(&str, &str, Disagreement); 3] = [
        (reason::SIGNATURE, reason::SIGNATURE, |s| {
            s.answer = BuyerCall::Refused("Error(Auth, InvalidAction)".to_owned());
        }),
        (reason::UNEXPECTED_VERIFY, reason::UNEXPECTED_SETTLE, |s| {
            s.consumes_authorization = false;
        }),
        (reason::NETWORK_UNAVAILABLE, reason::NETWORK_UNAVAILABLE, |s| s.unreachable = true),
    ];
    for (verified, settled, disagree) in cases {
        *w.h.witness.simulations() = common::Simulations::default();
        disagree(&mut w.h.witness.simulations());
        let terms = Terms::new(&w.pay_to, 30);
        assert_eq!(invalid_reason(&w.verify(terms.by_wallet()).await), verified);
        assert_eq!(error_reason(&w.settle(terms.by_wallet()).await), settled);
    }
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(w.charges().await, 0);
    // The positive control: both nodes accept, and the witness was asked
    // about exactly the call the gateway's node was.
    *w.h.witness.simulations() = common::Simulations::default();
    let terms = Terms::new(&w.pay_to, 30);
    assert_eq!(w.settle(terms.by_wallet()).await["success"], true);
    let asked = w.h.witness.simulations().seen.last().unwrap().1.clone();
    assert_eq!(asked, w.h.ledger.simulations().seen.last().unwrap().1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_unreachable_node_is_reported_and_nothing_is_debited(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    w.h.ledger.simulations().unreachable = true;
    let terms = Terms::new(&w.pay_to, 30);
    assert_eq!(invalid_reason(&w.verify(terms.by_wallet()).await), reason::NETWORK_UNAVAILABLE);
    assert_eq!(error_reason(&w.settle(terms.by_wallet()).await), reason::NETWORK_UNAVAILABLE);
    assert_eq!(w.wallet_available().await, 100);
    assert_eq!(w.charges().await, 0);
    w.h.ledger.simulations().unreachable = false;
    assert_eq!(w.settle(terms.by_wallet()).await["success"], true);
}

// ---- prepared commitments

impl World {
    async fn prepare(&self, body: Value) -> (StatusCode, Value) {
        self.call("POST", "/commitments", Some(&self.tenant.token), Some(body)).await
    }
}

fn prepare_request(terms: &Terms, payer: &str) -> Value {
    json!({ "paymentRequirements": terms.requirements(), "payer": payer })
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_prepared_commitment_signed_by_the_wallet_verifies_and_settles(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    let terms = Terms::new(&w.pay_to, 30);

    let (status, prepared) = w.prepare(prepare_request(&terms, &wallet().to_string())).await;
    assert_eq!(status, StatusCode::OK, "{prepared}");
    assert_eq!(w.charges().await, 0, "preparing records nothing");
    assert_eq!(prepared["validUntil"], (NOW + i64::try_from(TIMEOUT).unwrap()).to_string());
    assert_eq!(prepared["commitmentVerifier"], contract_strkey(X402_VERIFIER));
    // The entry is the payer's authorization of exactly the verifier call
    // over the text returned, valid as long as a deposit's, and the payload
    // is what that entry's signature covers.
    let text = prepared["commitmentText"].as_str().unwrap();
    let issued = Terms {
        commitment: prepared["commitment"].as_str().unwrap().to_owned(),
        valid_until: prepared["validUntil"].as_str().unwrap().parse().unwrap(),
        ..terms.clone()
    };
    assert_eq!(text, issued.message(&wallet().to_string()));
    let mut entry = SorobanAuthorizationEntry::from_xdr_base64(
        prepared["authorizationEntry"].as_str().unwrap(),
        Limits::none(),
    )
    .unwrap();
    let SorobanCredentials::AddressV2(creds) = &entry.credentials else {
        panic!("expected address credentials (V2): {:?}", entry.credentials)
    };
    assert_eq!(creds.address, wallet().sc_address());
    assert_eq!(creds.signature_expiration_ledger, 1_000 + common::AUTHORIZATION_VALIDITY_LEDGERS);
    assert_eq!(
        entry.root_invocation,
        x402_commitment_authorization(X402_VERIFIER, &wallet(), text)
    );
    let payload =
        signature_payload(network_id(Network::Testnet), &entry.credentials, &entry.root_invocation)
            .unwrap();
    assert_eq!(prepared["signaturePayload"], hex_lower(&payload));

    // The wallet signs; the facilitator presents it.
    let SorobanCredentials::AddressV2(creds) = &mut entry.credentials else { unreachable!() };
    creds.signature = ScVal::Bytes(ScBytes(BytesM::try_from(vec![9_u8; 64]).unwrap()));
    let request = issued.authorized(&entry);
    assert_eq!(w.verify(request.clone()).await["isValid"], true);
    let settled = w.settle(request).await;
    assert_eq!(settled["success"], true, "{settled}");
    assert_eq!(settled["transaction"], issued.commitment);
    assert_eq!(w.wallet_available().await, 70);
    assert_eq!(w.h.ledger.simulations().seen.last().unwrap().2, vec![entry]);

    // Each call prepares another commitment.
    let (_, again) = w.prepare(prepare_request(&terms, &wallet().to_string())).await;
    assert_ne!(again["commitment"], prepared["commitment"]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_commitment_is_prepared_only_for_a_contract_buyer_of_these_terms(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.h.now.store(NOW, Ordering::SeqCst);
    contract_buyer(&w.h, &w.tenant, 100).await;
    let alice = buyer(&w.h, &w.tenant, "alice", 100).await;
    let terms = Terms::new(&w.pay_to, 30);
    let error =
        |reply: (StatusCode, Value)| (reply.0, reply.1["error"].as_str().unwrap().to_owned());
    let refused = |status: StatusCode, reason: &str| (status, reason.to_owned());

    // The positive control.
    assert_eq!(w.prepare(prepare_request(&terms, &wallet().to_string())).await.0, StatusCode::OK);
    // A classic account signs the text itself.
    assert_eq!(
        error(w.prepare(prepare_request(&terms, alice.address().as_str())).await),
        refused(StatusCode::BAD_REQUEST, reason::INVALID_PAYLOAD)
    );
    // A contract that is not this seller's buyer.
    let stranger = ChainAddress::Contract([22; 32]).to_string();
    assert_eq!(
        error(w.prepare(prepare_request(&terms, &stranger)).await),
        refused(StatusCode::BAD_REQUEST, reason::UNKNOWN_PAYER)
    );
    // Terms naming another contract, a too distant window, another scheme.
    let other = Terms { pay_to: contract(8), ..terms.clone() };
    assert_eq!(
        error(w.prepare(prepare_request(&other, &wallet().to_string())).await),
        refused(StatusCode::BAD_REQUEST, reason::INVALID_REQUIREMENTS)
    );
    let mut far = prepare_request(&terms, &wallet().to_string());
    far["validUntil"] = json!((NOW + i64::try_from(TIMEOUT).unwrap() + 1).to_string());
    assert_eq!(error(w.prepare(far).await), refused(StatusCode::BAD_REQUEST, reason::TOO_FAR));
    let exact = Terms { scheme: "exact".to_owned(), ..terms.clone() };
    assert_eq!(
        error(w.prepare(prepare_request(&exact, &wallet().to_string())).await),
        refused(StatusCode::BAD_REQUEST, reason::UNSUPPORTED_SCHEME)
    );
    // Without a verifier, no contract account can pay.
    let (status, reply) = w
        .call_on(
            &w.h.x402_classic_only,
            "POST",
            "/commitments",
            Some(&w.tenant.token),
            Some(prepare_request(&terms, &wallet().to_string())),
        )
        .await;
    assert_eq!(
        (status, reply["error"].as_str().unwrap()),
        (StatusCode::BAD_REQUEST, reason::CONTRACT_PAYER_UNSUPPORTED)
    );
    // An unreachable network, and no key.
    w.h.ledger.set(0);
    assert_eq!(
        error(w.prepare(prepare_request(&terms, &wallet().to_string())).await),
        refused(StatusCode::SERVICE_UNAVAILABLE, reason::NETWORK_UNAVAILABLE)
    );
    w.h.ledger.set(1_000);
    let (status, _) = w
        .call("POST", "/commitments", None, Some(prepare_request(&terms, &wallet().to_string())))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(w.charges().await, 0);
}
