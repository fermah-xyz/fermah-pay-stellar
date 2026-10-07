//! The x402 facilitator interface against real PostgreSQL under the API's
//! production role: commitments signed by buyers (SEP-53) are verified and
//! settled into charges against the seller's prepaid ledger.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::atomic::Ordering;

use axum::body::Body;
use common::{Harness, Tenant, authed, start};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::sep53;
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, contract_strkey};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
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
        let response = self.h.x402.clone().oneshot(request).await.unwrap();
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
