//! Admission quotas against dust: new buyers per deployment, and deposits
//! and withdrawals per buyer, each per day and each held under concurrent
//! requests; and the smallest withdrawal.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Harness, Ledger, Tenant, assert_refused, authed, start_with_quotas};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, CreateBuyerResponse, PrepareDepositRequest, PrepareDepositResponse,
    PrepareWithdrawalRequest, PrepareWithdrawalResponse,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection as _, Executor as _};
use tonic::{Code, Status};

const QUOTAS: Quotas = Quotas {
    buyers_per_deployment: 3,
    deposits_per_buyer: 2,
    withdrawals_per_buyer: 1,
    min_withdrawal: 50,
    mandate_changes_per_buyer: 2,
};

async fn start(opts: PgPoolOptions, connect: PgConnectOptions) -> (Harness, Tenant) {
    let ledger = Ledger::default();
    ledger.set(1_000);
    let h =
        start_with_quotas(opts, connect, Network::Testnet, ledger.clone(), ledger, QUOTAS).await;
    let t = h.tenant("shop", "main", Network::Testnet).await;
    let binding = LedgerBinding {
        contract: stellar_strkey::Contract([7; 32]).to_string().to_string(),
        treasury: AccountAddress::from_public_key([11; 32]),
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, t.deployment_id, &binding).await.unwrap();
    (h, t)
}

async fn register(
    h: &Harness,
    t: &Tenant,
    external_ref: &str,
    wallet: &AccountAddress,
) -> Result<CreateBuyerResponse, Status> {
    let request = CreateBuyerRequest {
        external_ref: external_ref.to_owned(),
        wallet_address: wallet.to_string(),
    };
    Ok(BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(request, &t.token))
        .await?
        .into_inner())
}

async fn deposit(
    h: &Harness,
    t: &Tenant,
    buyer_id: &str,
    key: &str,
) -> Result<PrepareDepositResponse, Status> {
    let request = PrepareDepositRequest {
        buyer_id: buyer_id.to_owned(),
        amount: 100,
        idempotency_key: key.to_owned(),
    };
    Ok(LedgerServiceClient::new(h.channel().await)
        .prepare_deposit(authed(request, &t.token))
        .await?
        .into_inner())
}

async fn withdrawal(
    h: &Harness,
    t: &Tenant,
    buyer_id: &str,
    amount: i64,
    key: &str,
) -> Result<PrepareWithdrawalResponse, Status> {
    let request = PrepareWithdrawalRequest {
        buyer_id: buyer_id.to_owned(),
        amount,
        destination: String::new(),
        idempotency_key: key.to_owned(),
    };
    Ok(LedgerServiceClient::new(h.channel().await)
        .prepare_withdrawal(authed(request, &t.token))
        .await?
        .into_inner())
}

fn wallet() -> AccountAddress {
    SecretKey::generate().unwrap().address()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_new_buyers_beyond_the_daily_quota_are_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let (h, t) = start(opts, connect).await;
    let first = wallet();
    assert!(register(&h, &t, "b-1", &first).await.unwrap().created);
    for i in 2..=3 {
        register(&h, &t, &format!("b-{i}"), &wallet()).await.unwrap();
    }
    let refused = register(&h, &t, "b-4", &wallet()).await.unwrap_err();
    assert_refused(&refused, Code::ResourceExhausted, "buyer_quota_exceeded");
    // Repeating a registration counts nothing and is not refused.
    assert!(!register(&h, &t, "b-1", &first).await.unwrap().created);

    // A day later the oldest no longer counts.
    sqlx::query(
        "UPDATE pay_stellar.buyers SET created_at = now() - interval '25 hours'
         WHERE external_ref = 'b-1'",
    )
    .execute(&h.owner)
    .await
    .unwrap();
    assert!(register(&h, &t, "b-4", &wallet()).await.unwrap().created);
}

/// Holds a lock on `table` that stops inserts but not reads, on a connection
/// of its own, until the returned transaction ends.
async fn hold_inserts(connect: &PgConnectOptions, table: &str) -> sqlx::PgConnection {
    let mut holder = sqlx::PgConnection::connect_with(connect).await.unwrap();
    holder.execute("BEGIN").await.unwrap();
    let lock = match table {
        "buyers" => "LOCK TABLE pay_stellar.buyers IN SHARE ROW EXCLUSIVE MODE",
        "deposits" => "LOCK TABLE pay_stellar.deposits IN SHARE ROW EXCLUSIVE MODE",
        other => panic!("no lock for {other}"),
    };
    holder.execute(lock).await.unwrap();
    holder
}

/// Waits until `sessions` database sessions wait on a lock, failing the test
/// after ten seconds.
async fn blocked(h: &Harness, sessions: i64) {
    let wait = async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&h.owner)
            .await
            .unwrap();
            if waiting >= sessions {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), wait)
        .await
        .expect("the requests never waited on a lock");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_registrations_stop_at_the_quota(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let (h, t) = start(opts, connect.clone()).await;
    register(&h, &t, "b-1", &wallet()).await.unwrap();
    register(&h, &t, "b-2", &wallet()).await.unwrap();

    // Three registrations for the one place left, all past their count
    // before any inserts: only one may take it.
    let mut holder = hold_inserts(&connect, "buyers").await;
    let channel = h.channel().await;
    let attempts: Vec<_> = (3..=5)
        .map(|i| {
            let (channel, token) = (channel.clone(), t.token.clone());
            tokio::spawn(async move {
                let request = CreateBuyerRequest {
                    external_ref: format!("b-{i}"),
                    wallet_address: wallet().to_string(),
                };
                BuyerServiceClient::new(channel).create_buyer(authed(request, &token)).await
            })
        })
        .collect();
    blocked(&h, 3).await;
    holder.execute("COMMIT").await.unwrap();
    let mut created = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(_) => created += 1,
            Err(refused) => {
                assert_refused(&refused, Code::ResourceExhausted, "buyer_quota_exceeded");
            }
        }
    }
    assert_eq!(created, 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposits_per_buyer_are_limited_per_day_even_when_concurrent(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let (h, t) = start(opts, connect.clone()).await;
    let alice = register(&h, &t, "alice", &wallet()).await.unwrap().buyer.unwrap().buyer_id;
    let bob = register(&h, &t, "bob", &wallet()).await.unwrap().buyer.unwrap().buyer_id;
    assert!(deposit(&h, &t, &alice, "d-0").await.unwrap().created);

    let mut holder = hold_inserts(&connect, "deposits").await;
    let channel = h.channel().await;
    let attempts: Vec<_> = (1..=3)
        .map(|i| {
            let (channel, token, alice) = (channel.clone(), t.token.clone(), alice.clone());
            tokio::spawn(async move {
                let request = PrepareDepositRequest {
                    buyer_id: alice,
                    amount: 100,
                    idempotency_key: format!("d-{i}"),
                };
                LedgerServiceClient::new(channel).prepare_deposit(authed(request, &token)).await
            })
        })
        .collect();
    blocked(&h, 3).await;
    holder.execute("COMMIT").await.unwrap();
    let mut created = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(_) => created += 1,
            Err(refused) => {
                assert_refused(&refused, Code::ResourceExhausted, "deposit_quota_exceeded");
            }
        }
    }
    assert_eq!(created, 1);
    // A repeated request is answered from its row; another buyer has its
    // own quota.
    assert!(!deposit(&h, &t, &alice, "d-0").await.unwrap().created);
    assert!(deposit(&h, &t, &bob, "d-bob").await.unwrap().created);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawals_below_the_minimum_or_beyond_the_quota_are_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let (h, t) = start(opts, connect).await;
    let alice = register(&h, &t, "alice", &wallet()).await.unwrap().buyer.unwrap().buyer_id;
    sqlx::query("UPDATE pay_stellar.buyers SET available = 1000").execute(&h.owner).await.unwrap();

    let small = withdrawal(&h, &t, &alice, 49, "w-small").await.unwrap_err();
    assert_refused(&small, Code::InvalidArgument, "withdrawal_below_minimum");
    assert!(withdrawal(&h, &t, &alice, 50, "w-1").await.unwrap().created);
    let refused = withdrawal(&h, &t, &alice, 60, "w-2").await.unwrap_err();
    assert_refused(&refused, Code::ResourceExhausted, "withdrawal_quota_exceeded");
    assert!(!withdrawal(&h, &t, &alice, 50, "w-1").await.unwrap().created);
}
