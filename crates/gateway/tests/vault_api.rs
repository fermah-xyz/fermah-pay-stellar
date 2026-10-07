//! The ledger API for a deployment bound to a prepaid vault, over a real
//! gRPC server and PostgreSQL under the production roles: a deposit's
//! limit, admission against the buyer's daily limit and a requested exit,
//! limit changes and exit requests, and refusing when the worker's reading
//! of the contract's events is behind.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Harness, Tenant, assert_refused, authed, start};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::stellar_xdr::{
    Int128Parts, Limits, ReadXdr, ScVal, SorobanAuthorizationEntry, SorobanAuthorizedFunction,
    WriteXdr,
};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, CreateChargeRequest, GetBalanceRequest, GetBalanceResponse, GetExitRequest,
    PrepareDepositRequest, PrepareExitRequest, PrepareLimitChangeRequest, SubmitExitRequest,
    SubmitLimitChangeRequest, VaultRequestState,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tonic::Code;
use tonic::transport::Channel;
use uuid::Uuid;

const STALE_AFTER: u32 = 2880;

async fn ledger(h: &Harness) -> LedgerServiceClient<Channel> {
    LedgerServiceClient::new(h.channel().await)
}

async fn bind(h: &Harness, t: &Tenant, vault: bool) {
    let binding = LedgerBinding {
        contract: stellar_strkey::Contract(if vault { [7; 32] } else { [8; 32] })
            .to_string()
            .to_string(),
        treasury: (!vault).then(|| AccountAddress::from_public_key([11; 32])),
        operator: AccountAddress::from_public_key([12; 32]),
    };
    issuance::bind_ledger_contract(&h.issuer, t.deployment_id, &binding).await.unwrap();
}

/// Records that the worker has read the deployment's events up to `ledger`.
async fn events_read_to(h: &Harness, t: &Tenant, ledger: u32) {
    let cursor = fermah_pay_stellar_chain::rpc::EventCursor::end_of_ledger(ledger).to_string();
    sqlx::query(
        "INSERT INTO pay_stellar.vault_event_cursors (seller_deployment_id, network, cursor, ledger)
         VALUES ($1, 'stellar:testnet', $3, $2)
         ON CONFLICT (seller_deployment_id)
         DO UPDATE SET ledger = EXCLUDED.ledger, cursor = EXCLUDED.cursor",
    )
    .bind(t.deployment_id)
    .bind(i64::from(ledger))
    .bind(cursor)
    .execute(&h.owner)
    .await
    .unwrap();
}

struct Buyer {
    id: String,
    key: SecretKey,
}

/// A buyer with `available` credit and a daily limit of `cap` known in
/// force, as the worker leaves them after confirming a deposit.
async fn buyer(h: &Harness, t: &Tenant, available: i64, cap: i64) -> Buyer {
    let key = SecretKey::generate().unwrap();
    let created = BuyerServiceClient::new(h.channel().await)
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: format!("buyer-{}", Uuid::now_v7()),
                wallet_address: key.address().to_string(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner();
    let id = created.buyer.unwrap().buyer_id;
    sqlx::query("UPDATE pay_stellar.buyers SET available = $2, cap = $3 WHERE id = $1")
        .bind(Uuid::parse_str(&id).unwrap())
        .bind(available)
        .bind(cap)
        .execute(&h.owner)
        .await
        .unwrap();
    Buyer { id, key }
}

/// A bound vault tenant whose events the worker has read to the latest
/// ledger.
async fn vault(h: &Harness) -> Tenant {
    let t = h.tenant("shop", "vault", Network::Testnet).await;
    bind(h, &t, true).await;
    events_read_to(h, &t, h.ledger.get()).await;
    t
}

async fn charge(
    h: &Harness,
    t: &Tenant,
    b: &Buyer,
    amount: i64,
    key: &str,
) -> Result<(), tonic::Status> {
    ledger(h)
        .await
        .create_charge(authed(
            CreateChargeRequest { buyer_id: b.id.clone(), amount, idempotency_key: key.into() },
            &t.token,
        ))
        .await
        .map(|_| ())
}

async fn balance(h: &Harness, t: &Tenant, b: &Buyer) -> GetBalanceResponse {
    ledger(h)
        .await
        .get_balance(authed(GetBalanceRequest { buyer_id: b.id.clone() }, &t.token))
        .await
        .unwrap()
        .into_inner()
}

fn signed(entry_xdr: &str, key: &SecretKey) -> String {
    let entry = SorobanAuthorizationEntry::from_xdr_base64(entry_xdr, Limits::none()).unwrap();
    sign_entry(&entry, network_id(Network::Testnet), &[key])
        .unwrap()
        .to_xdr_base64(Limits::none())
        .unwrap()
}

/// Prepares and submits a limit change signed by the buyer.
async fn change_limit(h: &Harness, t: &Tenant, b: &Buyer, limit: i64, key: &str) {
    let prepared = ledger(h)
        .await
        .prepare_limit_change(authed(
            PrepareLimitChangeRequest {
                buyer_id: b.id.clone(),
                daily_limit: limit,
                idempotency_key: key.into(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .limit_change
        .unwrap();
    let submitted = ledger(h)
        .await
        .submit_limit_change(authed(
            SubmitLimitChangeRequest {
                limit_change_id: prepared.limit_change_id,
                signed_authorization_entry_xdr: signed(&prepared.authorization_entry_xdr, &b.key),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .limit_change
        .unwrap();
    assert_eq!(submitted.state(), VaultRequestState::Signed);
}

/// Prepares and submits an exit to the buyer's own wallet.
async fn request_exit(h: &Harness, t: &Tenant, b: &Buyer, amount: i64, key: &str) -> String {
    let prepared = ledger(h)
        .await
        .prepare_exit(authed(
            PrepareExitRequest {
                buyer_id: b.id.clone(),
                amount,
                destination: String::new(),
                idempotency_key: key.into(),
            },
            &t.token,
        ))
        .await
        .unwrap()
        .into_inner()
        .exit
        .unwrap();
    ledger(h)
        .await
        .submit_exit(authed(
            SubmitExitRequest {
                exit_id: prepared.exit_id.clone(),
                signed_authorization_entry_xdr: signed(&prepared.authorization_entry_xdr, &b.key),
            },
            &t.token,
        ))
        .await
        .unwrap();
    prepared.exit_id
}

/// The entry with a signature by another key over the same call: it names
/// the buyer, and the signature is not the buyer's.
fn signed_by_another(entry_xdr: &str) -> String {
    use fermah_pay_stellar_chain::stellar_xdr::{ScAddress, SorobanCredentials};
    let mut entry = SorobanAuthorizationEntry::from_xdr_base64(entry_xdr, Limits::none()).unwrap();
    let SorobanCredentials::AddressV2(credentials) = &mut entry.credentials else {
        panic!("prepared with AddressV2 credentials")
    };
    let buyer = credentials.address.clone();
    let stranger = SecretKey::generate().unwrap();
    credentials.address =
        ScAddress::Account(fermah_pay_stellar_chain::transaction::account_id(&stranger.address()));
    let mut forged = sign_entry(&entry, network_id(Network::Testnet), &[&stranger]).unwrap();
    let SorobanCredentials::AddressV2(credentials) = &mut forged.credentials else {
        unreachable!()
    };
    credentials.address = buyer;
    forged.to_xdr_base64(Limits::none()).unwrap()
}

fn args_of(entry_xdr: &str) -> Vec<ScVal> {
    let entry = SorobanAuthorizationEntry::from_xdr_base64(entry_xdr, Limits::none()).unwrap();
    let SorobanAuthorizedFunction::ContractFn(call) = entry.root_invocation.function else {
        panic!("not a contract call")
    };
    call.args.to_vec()
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_vault_deposit_carries_the_limit_the_buyer_signs(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = vault(&h).await;
    let b = buyer(&h, &t, 0, 0).await;
    let prepare = |limit: Option<i64>| PrepareDepositRequest {
        buyer_id: b.id.clone(),
        amount: 10_000_000,
        idempotency_key: format!("dep-{}", limit.map_or(-2, |l: i64| l)),
        daily_limit: limit,
    };
    let deposit = ledger(&h)
        .await
        .prepare_deposit(authed(prepare(Some(5_000_000)), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(
        args_of(&deposit.authorization_entry_xdr).last(),
        Some(&ScVal::I128(Int128Parts { hi: 0, lo: 5_000_000 }))
    );
    let without = ledger(&h)
        .await
        .prepare_deposit(authed(prepare(None), &t.token))
        .await
        .unwrap()
        .into_inner()
        .deposit
        .unwrap();
    assert_eq!(args_of(&without.authorization_entry_xdr).last(), Some(&ScVal::Void));
    let status =
        ledger(&h).await.prepare_deposit(authed(prepare(Some(-1)), &t.token)).await.unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_limit");

    // A prepaid ledger takes no limit.
    let other = h.tenant("shop", "prepaid", Network::Testnet).await;
    bind(&h, &other, false).await;
    let other_buyer = buyer(&h, &other, 0, 0).await;
    let request = PrepareDepositRequest {
        buyer_id: other_buyer.id.clone(),
        amount: 10_000_000,
        idempotency_key: "dep".into(),
        daily_limit: Some(1),
    };
    let status = ledger(&h).await.prepare_deposit(authed(request, &other.token)).await.unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "not_a_vault");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charges_fit_the_limit_of_the_day_they_are_admitted_in(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = vault(&h).await;
    let b = buyer(&h, &t, 100, 30).await;
    charge(&h, &t, &b, 20, "c-1").await.unwrap();
    charge(&h, &t, &b, 10, "c-2").await.unwrap();
    let status = charge(&h, &t, &b, 1, "c-3").await.unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "above_spending_limit");
    // A refused charge does not use up the day's share.
    sqlx::query("UPDATE pay_stellar.charges SET state = 'refused', outcome = 'above_cap', settled_at = now() WHERE idempotency_key = 'c-2'")
        .execute(&h.owner)
        .await
        .unwrap();
    charge(&h, &t, &b, 10, "c-4").await.unwrap();
    // Charges admitted on an earlier day count against that day.
    sqlx::query("UPDATE pay_stellar.charges SET day = day - 1 WHERE state <> 'refused'")
        .execute(&h.owner)
        .await
        .unwrap();
    charge(&h, &t, &b, 30, "c-5").await.unwrap();
    let days: Vec<i64> =
        sqlx::query_scalar("SELECT DISTINCT day FROM pay_stellar.charges ORDER BY day")
            .fetch_all(&h.owner)
            .await
            .unwrap();
    let today = time::OffsetDateTime::now_utc().unix_timestamp() / 86_400;
    assert_eq!(days, [today - 1, today]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_vault_charges_wait_for_a_recent_reading_of_the_events(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    h.ledger.set(10_000);
    let t = h.tenant("shop", "vault", Network::Testnet).await;
    bind(&h, &t, true).await;
    let b = buyer(&h, &t, 100, 100).await;
    // Never read: refused.
    let status = charge(&h, &t, &b, 1, "c-1").await.unwrap_err();
    assert_refused(&status, Code::Unavailable, "network_unavailable");
    let latest = h.ledger.get();
    events_read_to(&h, &t, latest - STALE_AFTER - 1).await;
    let status = charge(&h, &t, &b, 1, "c-1").await.unwrap_err();
    assert_refused(&status, Code::Unavailable, "network_unavailable");
    events_read_to(&h, &t, latest - STALE_AFTER).await;
    charge(&h, &t, &b, 1, "c-1").await.unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_lower_limit_applies_at_once_and_a_raise_waits_for_the_contract(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = vault(&h).await;
    let b = buyer(&h, &t, 100, 30).await;
    change_limit(&h, &t, &b, 10, "lower").await;
    // Admitted against at once; the contract has not seen it yet.
    let known = balance(&h, &t, &b).await;
    assert_eq!(
        (known.daily_limit, known.pending_daily_limit, known.admitted_daily_limit),
        (30, None, 10)
    );
    let status = charge(&h, &t, &b, 11, "c-1").await.unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "above_spending_limit");
    charge(&h, &t, &b, 10, "c-2").await.unwrap();

    change_limit(&h, &t, &b, 50, "raise").await;
    let known = balance(&h, &t, &b).await;
    assert_eq!((known.daily_limit, known.admitted_daily_limit), (30, 10));
    let status = charge(&h, &t, &b, 1, "c-3").await.unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "above_spending_limit");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_an_exit_holds_its_amount_and_keeps_the_rest_free(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = vault(&h).await;
    let b = buyer(&h, &t, 100, 1_000).await;
    let exit_id = request_exit(&h, &t, &b, 80, "exit-1").await;
    let known = balance(&h, &t, &b).await;
    assert_eq!((known.available, known.exit_amount, known.reserved_for_exit), (100, None, 80));
    let status = charge(&h, &t, &b, 21, "c-0").await.unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "exit_requested");
    charge(&h, &t, &b, 20, "c-00").await.unwrap();
    let exit = ledger(&h)
        .await
        .get_exit(authed(GetExitRequest { exit_id }, &t.token))
        .await
        .unwrap()
        .into_inner()
        .exit
        .unwrap();
    assert_eq!((exit.amount, exit.state()), (80, VaultRequestState::Signed));

    // An exit for more than is available keeps credit that arrives later
    // free too, up to its amount.
    request_exit(&h, &t, &b, 150, "exit-2").await;
    assert_eq!(balance(&h, &t, &b).await.reserved_for_exit, 150);
    sqlx::query("UPDATE pay_stellar.buyers SET available = available + 80")
        .execute(&h.owner)
        .await
        .unwrap();
    let status = charge(&h, &t, &b, 11, "c-1").await.unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "exit_requested");
    charge(&h, &t, &b, 10, "c-2").await.unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_limit_changes_and_exits_are_for_vaults_only(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = h.tenant("shop", "prepaid", Network::Testnet).await;
    bind(&h, &t, false).await;
    let b = buyer(&h, &t, 100, 0).await;
    let status = ledger(&h)
        .await
        .prepare_limit_change(authed(
            PrepareLimitChangeRequest {
                buyer_id: b.id.clone(),
                daily_limit: 5,
                idempotency_key: "l".into(),
            },
            &t.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "not_a_vault");
    let status = ledger(&h)
        .await
        .prepare_exit(authed(
            PrepareExitRequest {
                buyer_id: b.id.clone(),
                amount: 5,
                destination: String::new(),
                idempotency_key: "e".into(),
            },
            &t.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&status, Code::FailedPrecondition, "not_a_vault");
    // The prepaid ledger's charges need no reading of vault events.
    charge(&h, &t, &b, 5, "c-1").await.unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_vault_requests_are_idempotent_and_signed_by_the_buyer_only(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = start(opts, connect, Network::Testnet).await;
    let t = vault(&h).await;
    let b = buyer(&h, &t, 100, 30).await;
    let request = |limit| PrepareLimitChangeRequest {
        buyer_id: b.id.clone(),
        daily_limit: limit,
        idempotency_key: "l-1".into(),
    };
    let first = ledger(&h)
        .await
        .prepare_limit_change(authed(request(5), &t.token))
        .await
        .unwrap()
        .into_inner();
    let again = ledger(&h)
        .await
        .prepare_limit_change(authed(request(5), &t.token))
        .await
        .unwrap()
        .into_inner();
    assert!(first.created && !again.created);
    assert_eq!(first.limit_change, again.limit_change);
    let status =
        ledger(&h).await.prepare_limit_change(authed(request(6), &t.token)).await.unwrap_err();
    assert_refused(&status, Code::AlreadyExists, "idempotency_conflict");

    let change = first.limit_change.unwrap();
    let status = ledger(&h)
        .await
        .submit_limit_change(authed(
            SubmitLimitChangeRequest {
                limit_change_id: change.limit_change_id.clone(),
                signed_authorization_entry_xdr: signed_by_another(&change.authorization_entry_xdr),
            },
            &t.token,
        ))
        .await
        .unwrap_err();
    assert_refused(&status, Code::InvalidArgument, "invalid_signature");
    // A refused submission counts for nothing.
    assert_eq!(balance(&h, &t, &b).await.admitted_daily_limit, 30);
}
