//! Metrics are recorded where the money moves. This binary holds one test:
//! the recorder it installs is process-wide.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::atomic::Ordering;

use axum::body::Body;
use common::{authed, start};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::sep53;
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, contract_strkey};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::x402::commitment_message;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{CreateBuyerRequest, PrepareWithdrawalRequest};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

/// The value of counter `name` whose labels include every one of `labels`.
fn counter(
    snapshot: &[(
        metrics_util::CompositeKey,
        Option<metrics::Unit>,
        Option<metrics::SharedString>,
        DebugValue,
    )],
    name: &str,
    labels: &[(&str, &str)],
) -> u64 {
    snapshot
        .iter()
        .filter(|(key, ..)| key.key().name() == name)
        .filter(|(key, ..)| {
            labels.iter().all(|(k, v)| key.key().labels().any(|l| l.key() == *k && l.value() == *v))
        })
        .map(|(.., value)| match value {
            DebugValue::Counter(n) => *n,
            _ => 0,
        })
        .sum()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_settlements_and_refusals_are_counted(opts: PgPoolOptions, connect: PgConnectOptions) {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    recorder.install().unwrap();

    let h = start(opts, connect, Network::Testnet).await;
    let tenant = h.tenant("shop", "main", Network::Testnet).await;
    let pay_to = contract_strkey([7; 32]);
    issuance::bind_ledger_contract(
        &h.issuer,
        tenant.deployment_id,
        &LedgerBinding {
            contract: pay_to.clone(),
            treasury: AccountAddress::from_public_key([11; 32]),
            operator: AccountAddress::from_public_key([12; 32]),
        },
    )
    .await
    .unwrap();
    let buyer = SecretKey::generate().unwrap();
    let buyer_id = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: "alice".to_owned(),
                wallet_address: buyer.address().to_string(),
            },
            &tenant.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .buyer
        .unwrap()
        .buyer_id;
    sqlx::query("UPDATE pay_stellar.buyers SET available = 100").execute(&h.owner).await.unwrap();
    // A refusal through the gRPC API: registering the same wallet under
    // another reference.
    let _ = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: "bob".to_owned(),
                wallet_address: buyer.address().to_string(),
            },
            &tenant.token,
        ))
        .await
        .unwrap_err();

    h.now.store(1_800_000_000, Ordering::SeqCst);
    let asset =
        contract_strkey(asset_contract_id(&circle_usdc(Network::Testnet), Network::Testnet));
    let commitment = hex_lower(&[5_u8; 32]);
    let message = commitment_message(
        "stellar:testnet",
        &asset,
        &pay_to,
        "30",
        buyer.address().as_str(),
        &commitment,
        "1800000030",
    );
    let signature = sep53::sign(&buyer, message.as_bytes());
    let requirements = json!({ "scheme": "batch-settlement", "network": "stellar:testnet",
        "amount": "30", "asset": asset, "payTo": pay_to, "maxTimeoutSeconds": 60 });
    let body = json!({
        "x402Version": 2,
        "paymentPayload": { "x402Version": 2, "accepted": requirements.clone(),
            "payload": { "payer": buyer.address().as_str(), "commitment": commitment,
                "validUntil": "1800000030",
                "signature": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, signature) } },
        "paymentRequirements": requirements,
    });
    for _ in 0..2 {
        let request = http::Request::post("/settle")
            .header("authorization", format!("Bearer {}", tenant.token))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        assert_eq!(h.x402.clone().oneshot(request).await.unwrap().status(), 200);
    }

    // A withdrawal to the buyer's own wallet.
    LedgerServiceClient::new(h.channel().await)
        .prepare_withdrawal(authed(
            PrepareWithdrawalRequest {
                buyer_id,
                amount: 10,
                destination: String::new(),
                idempotency_key: "w-1".to_owned(),
            },
            &tenant.token,
        ))
        .await
        .unwrap();

    let snapshot = snapshotter.snapshot().into_vec();
    // The settled commitment admitted one charge of 30; its replay admitted
    // nothing.
    let deployment = tenant.deployment_id.to_string();
    let admitted = [("kind", "charge"), ("seller_deployment_id", deployment.as_str())];
    assert_eq!(
        (
            counter(&snapshot, "pay_stellar_charges_admitted_total", &admitted),
            counter(&snapshot, "pay_stellar_charges_admitted_usdc_total", &admitted),
        ),
        (1, 30)
    );
    assert_eq!(
        counter(&snapshot, "pay_stellar_withdrawals_prepared_total", &[("destination", "own")]),
        1
    );
    let x402 = |result| {
        counter(&snapshot, "pay_stellar_x402_total", &[("call", "settle"), ("result", result)])
    };
    assert_eq!((x402("settled"), x402("replayed")), (1, 1));
    assert_eq!(counter(&snapshot, "pay_stellar_api_refusals_total", &[]), 1);
}
