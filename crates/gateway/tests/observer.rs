//! Chain observer against real PostgreSQL, under its production role, with a
//! scripted RPC that pages events the way Stellar RPC does: a start ledger or
//! a cursor, never both; a scan window of 10,000 ledgers; a cursor that is
//! the last event of a full page or the end of the window otherwise; and a
//! refusal of any start outside the retained range.
//!
//! The gateway's rows are written directly as the owner, in the states under
//! test, and every expectation is written out rather than derived from the
//! observer's own logic.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{EventStream, pool_as};
use fermah_pay_stellar_chain::rpc::{
    EventCursor, EventPage, EventsFrom, Health, LedgerEntries, LedgerEntryRecord, RpcError,
};
use fermah_pay_stellar_chain::stellar_xdr::{
    AccountId, ContractDataDurability, ContractDataEntry, ContractExecutable, ContractId,
    ExtensionPoint, Hash, Int128Parts, LedgerEntryData, LedgerEntryExt, LedgerKey, Limits,
    PublicKey, ReadXdr, ScAddress, ScBytes, ScContractInstance, ScMap, ScMapEntry, ScSymbol, ScVal,
    ScVec, TrustLineEntry, TrustLineEntryExt, Uint256,
};
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, trustline_key};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::events::EventLog;
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::observer::{
    ChainReader, Observer, ObserverError, Settings, StartPosition,
};
use fermah_pay_stellar_gateway::submission::Clock;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

const CONTRACT: [u8; 32] = [7; 32];
const OLDEST: u32 = 1_000;
const GRACE: Duration = Duration::from_secs(600);
const CONFIRMATIONS: u32 = 3;

fn account(seed: u8) -> AccountAddress {
    AccountAddress::from_public_key([seed; 32])
}

fn operator() -> AccountAddress {
    account(201)
}

fn treasury() -> AccountAddress {
    account(202)
}

fn admin() -> AccountAddress {
    account(203)
}

fn seller() -> AccountAddress {
    account(204)
}

fn usdc_id() -> [u8; 32] {
    asset_contract_id(&circle_usdc(Network::Testnet), Network::Testnet)
}

// ---- contract encodings, written out as the contract emits them ----------

fn symbol(name: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(name.try_into().unwrap()))
}

fn address(account: &AccountAddress) -> ScVal {
    ScVal::Address(ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *account.public_key(),
    )))))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn i128_val(value: i128) -> ScVal {
    ScVal::I128(Int128Parts { hi: (value >> 64) as i64, lo: value as u64 })
}

fn bytes(value: &[u8; 32]) -> ScVal {
    ScVal::Bytes(ScBytes(value.to_vec().try_into().unwrap()))
}

fn vector(items: Vec<ScVal>) -> ScVal {
    ScVal::Vec(Some(ScVec(items.try_into().unwrap())))
}

fn map(entries: Vec<(&str, ScVal)>) -> ScVal {
    ScVal::Map(Some(ScMap(
        entries
            .into_iter()
            .map(|(key, val)| ScMapEntry { key: symbol(key), val })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
    )))
}

type Event = (Vec<ScVal>, ScVal);

fn deposit_event(owner: &AccountAddress, amount: i128, deposit_id: [u8; 32]) -> Event {
    (vec![symbol("deposit"), address(owner)], vector(vec![i128_val(amount), bytes(&deposit_id)]))
}

/// Outcome codes: 0 charged, 1 insufficient balance, 2 above limit,
/// 3 duplicate, 4 expired, 5 unknown account.
fn charges_event(entries: &[(&AccountAddress, [u8; 32], i128, u32)]) -> Event {
    let entries = entries
        .iter()
        .map(|(owner, id, amount, code)| {
            vector(vec![address(owner), bytes(id), i128_val(*amount), ScVal::U32(*code)])
        })
        .collect();
    (vec![symbol("charges")], vector(entries))
}

fn withdraw_event(owner: &AccountAddress, amount: i128, id: [u8; 32]) -> Event {
    (
        vec![symbol("withdraw"), address(owner)],
        vector(vec![address(owner), i128_val(amount), bytes(&id)]),
    )
}

fn revenue_event(amount: i128, id: [u8; 32]) -> Event {
    (vec![symbol("revenue")], vector(vec![address(&seller()), i128_val(amount), bytes(&id)]))
}

fn role_event(role: &str, previous: &AccountAddress, current: &AccountAddress) -> Event {
    (vec![symbol("role"), symbol(role)], vector(vec![address(previous), address(current)]))
}

/// A `charges` event of an earlier contract version, returned by testnet
/// `getEvents`: each charge was named by a `u64` sequence number.
fn earlier_layout_event() -> Event {
    let topic = ScVal::from_xdr_base64("AAAADwAAAAdjaGFyZ2VzAA==", Limits::none()).unwrap();
    let value = ScVal::from_xdr_base64(
        "AAAAEAAAAAEAAAABAAAAEAAAAAEAAAAEAAAAEgAAAAAAAAAApDDeli/J1RNw3at1WfsXX/9WgFg2KxTvYMoyskV5ZJcAAAAFAAAAAAAAAAIAAAAKAAAAAAAAAAAAAAAAAAGGoAAAAAMAAAAA",
        Limits::none(),
    )
    .unwrap();
    (vec![topic], value)
}

// ---- the network ----------------------------------------------------------

struct Net {
    latest: u32,
    oldest: u32,
    stream: EventStream,
    liabilities: i128,
    revenue: i128,
    config_treasury: AccountAddress,
    /// Treasury trustline balance and flags; `None` for no trustline.
    trustline: Option<(i64, u32)>,
    /// A cold reserve and its USDC balance.
    reserve: Option<(AccountAddress, i64)>,
    base_time: OffsetDateTime,
}

#[derive(Clone)]
struct Chain(Arc<Mutex<Net>>);

impl Chain {
    fn with<R>(&self, f: impl FnOnce(&mut Net) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    /// Emits `event` in its own transaction at `ledger`, which also becomes
    /// the latest ledger if it is beyond it.
    fn emit(&self, ledger: u32, event: Event) -> EventCursor {
        self.with(|net| {
            let closed = net.base_time
                + time::Duration::seconds(5 * (i64::from(ledger) - i64::from(OLDEST)));
            net.latest = net.latest.max(ledger);
            net.stream.emit(CONTRACT, ledger, closed.format(&Rfc3339).unwrap(), event.0, event.1)
        })
    }

    fn instance(net: &Net) -> LedgerEntryData {
        let config = map(vec![
            ("admin", address(&admin())),
            ("limits", map(vec![("max_charge", i128_val(1_000)), ("min_deposit", i128_val(1))])),
            ("operator", address(&operator())),
            ("paused", ScVal::Bool(false)),
            ("seller", address(&seller())),
            ("treasury", address(&net.config_treasury)),
            ("usdc", ScVal::Address(ScAddress::Contract(ContractId(Hash(usdc_id()))))),
        ]);
        let totals = map(vec![
            ("liabilities", i128_val(net.liabilities)),
            ("revenue", i128_val(net.revenue)),
        ]);
        LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: ScAddress::Contract(ContractId(Hash(CONTRACT))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
            val: ScVal::ContractInstance(ScContractInstance {
                executable: ContractExecutable::Wasm(Hash([9; 32])),
                storage: Some(ScMap(
                    vec![
                        ScMapEntry { key: vector(vec![symbol("Config")]), val: config },
                        ScMapEntry { key: vector(vec![symbol("Totals")]), val: totals },
                    ]
                    .try_into()
                    .unwrap(),
                )),
            }),
        })
    }
}

fn instance_key() -> LedgerKey {
    fermah_pay_stellar_chain::prepaid::PrepaidDeployment {
        contract: CONTRACT,
        usdc: usdc_id(),
        treasury: treasury(),
    }
    .instance_key()
}

impl EventLog for Chain {
    async fn health(&self) -> Result<Health, RpcError> {
        Ok(self.with(|n| Health { latest_ledger: n.latest, oldest_ledger: n.oldest }))
    }

    async fn events(
        &self,
        contract: &[u8; 32],
        from: &EventsFrom,
        limit: u32,
    ) -> Result<EventPage, RpcError> {
        self.with(|net| net.stream.page(contract, from, limit, net.oldest, net.latest))
    }
}

impl ChainReader for Chain {
    async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, RpcError> {
        self.with(|net| {
            let line = trustline_key(&net.config_treasury, &circle_usdc(Network::Testnet));
            let mut entries = Vec::new();
            for key in keys {
                let data = if *key == instance_key() {
                    Some(Self::instance(net))
                } else if *key == line {
                    net.trustline.map(|(balance, flags)| {
                        LedgerEntryData::Trustline(TrustLineEntry {
                            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                                *net.config_treasury.public_key(),
                            ))),
                            asset: match trustline_key(
                                &net.config_treasury,
                                &circle_usdc(Network::Testnet),
                            ) {
                                LedgerKey::Trustline(k) => k.asset,
                                _ => unreachable!(),
                            },
                            balance,
                            limit: i64::MAX,
                            flags,
                            ext: TrustLineEntryExt::V0,
                        })
                    })
                } else if let Some((cold, balance)) = net
                    .reserve
                    .as_ref()
                    .filter(|(cold, _)| *key == trustline_key(cold, &circle_usdc(Network::Testnet)))
                {
                    let LedgerKey::Trustline(line) =
                        trustline_key(cold, &circle_usdc(Network::Testnet))
                    else {
                        unreachable!()
                    };
                    Some(LedgerEntryData::Trustline(TrustLineEntry {
                        account_id: line.account_id,
                        asset: line.asset,
                        balance: *balance,
                        limit: i64::MAX,
                        flags: 1,
                        ext: TrustLineEntryExt::V0,
                    }))
                } else {
                    None
                };
                if let Some(data) = data {
                    entries.push(LedgerEntryRecord {
                        key: key.clone(),
                        data,
                        ext: LedgerEntryExt::V0,
                        last_modified_ledger: net.latest,
                        live_until_ledger: None,
                    });
                }
            }
            Ok(LedgerEntries { entries, latest_ledger: net.latest })
        })
    }
}

#[derive(Clone)]
struct ManualClock(Arc<Mutex<OffsetDateTime>>);

impl ManualClock {
    fn set(&self, at: OffsetDateTime) {
        *self.0.lock().unwrap() = at;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

// ---- harness ------------------------------------------------------------

struct World {
    owner: PgPool,
    issuer: PgPool,
    observer_pool: PgPool,
    deployment: Uuid,
    product: Uuid,
    chain: Chain,
    clock: ManualClock,
    base_time: OffsetDateTime,
    submission: Uuid,
}

async fn world(opts: PgPoolOptions, connect: PgConnectOptions) -> World {
    let owner = opts.max_connections(2).connect_with(connect.clone()).await.unwrap();
    let issuer = pool_as(&connect, "SET ROLE pay_stellar_issuer", 1).await;
    let observer_pool = pool_as(&connect, "SET ROLE pay_stellar_observer", 2).await;
    let product = issuance::create_product(&issuer, "shop").await.unwrap();
    let deployment = issuance::create_seller_deployment(&issuer, product, "main", Network::Testnet)
        .await
        .unwrap();
    let binding = LedgerBinding {
        contract: stellar_strkey::Contract(CONTRACT).to_string().to_string(),
        treasury: treasury(),
        operator: operator(),
    };
    issuance::bind_ledger_contract(&issuer, deployment, &binding).await.unwrap();
    let base_time = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    let chain = Chain(Arc::new(Mutex::new(Net {
        latest: OLDEST + 100,
        oldest: OLDEST,
        stream: EventStream::default(),
        liabilities: 0,
        revenue: 0,
        config_treasury: treasury(),
        trustline: Some((0, 1)),
        reserve: None,
        base_time,
    })));
    let submission = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pay_stellar.submissions
            (id, network, kind, state, source_address, fee_source_address, sequence,
             valid_until, inner_hash, outer_hash, envelope_xdr, ledger, result_xdr, resolved_at)
         VALUES ($1, 'stellar:testnet', 'charge_batch', 'succeeded', $2, $2, 1, now(),
                 $3, $3, 'AAAA', 1050, 'AAAA', now())",
    )
    .bind(submission)
    .bind(account(250).as_str())
    .bind([5_u8; 32].as_slice())
    .execute(&owner)
    .await
    .unwrap();
    World {
        owner,
        issuer,
        observer_pool,
        deployment,
        product,
        chain,
        clock: ManualClock(Arc::new(Mutex::new(base_time))),
        base_time,
        submission,
    }
}

/// When the fake network closed `ledger`.
fn closed(w: &World, ledger: u32) -> OffsetDateTime {
    w.base_time + time::Duration::seconds(5 * (i64::from(ledger) - i64::from(OLDEST)))
}

impl World {
    fn observer_with(&self, start: StartPosition, page_size: u32) -> Observer<Chain, ManualClock> {
        Observer::new(
            self.observer_pool.clone(),
            self.chain.clone(),
            self.clock.clone(),
            Network::Testnet,
            Settings {
                start,
                page_size,
                settle_within: GRACE,
                confirmations: CONFIRMATIONS,
                max_pages_per_round: 100,
            },
        )
    }

    fn observer(&self) -> Observer<Chain, ManualClock> {
        self.observer_with(StartPosition::Oldest, 100)
    }

    async fn buyer(&self, seed: u8, available: i64) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO pay_stellar.buyers
                (id, product_id, seller_deployment_id, network, external_ref, wallet_address,
                 available)
             VALUES ($1, $2, $3, 'stellar:testnet', $4, $5, $6)",
        )
        .bind(id)
        .bind(self.product)
        .bind(self.deployment)
        .bind(format!("buyer-{seed}"))
        .bind(account(seed).as_str())
        .bind(available)
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }

    /// A charge in `state`, linked to a batch unless it is admitted.
    async fn charge(
        &self,
        buyer: Uuid,
        charge_id: [u8; 32],
        amount: i64,
        state: &str,
        outcome: Option<&str>,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let linked = state != "admitted";
        let settled = matches!(state, "charged" | "refused" | "quarantined");
        sqlx::query(
            "INSERT INTO pay_stellar.charges
                (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, charge_id,
                 last_ledger, state, outcome, submission_id, batch_index, settled_at)
             VALUES ($1, $2, $3, 'stellar:testnet', $4, $5, $6, 2000, $7, $8, $9, $10,
                     CASE WHEN $11 THEN now() END)",
        )
        .bind(id)
        .bind(buyer)
        .bind(self.deployment)
        .bind(format!("key-{id}"))
        .bind(amount)
        .bind(charge_id.as_slice())
        .bind(state)
        .bind(outcome)
        .bind(linked.then_some(self.submission))
        .bind(linked.then_some(0_i16))
        .bind(settled)
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }

    async fn deposit(&self, buyer: Uuid, deposit_id: [u8; 32], amount: i64, state: &str) -> Uuid {
        let id = Uuid::now_v7();
        let signed = state != "awaiting_signature";
        let resolved = matches!(state, "confirmed" | "failed" | "expired");
        sqlx::query(
            "INSERT INTO pay_stellar.deposits
                (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, deposit_id,
                 state, authorization_xdr, signed_authorization_xdr, signed_at,
                 expiration_ledger, submission_id, resolved_at)
             VALUES ($1, $2, $3, 'stellar:testnet', $4, $5, $6, $7, 'AAAA',
                     CASE WHEN $8 THEN 'AAAA' END, CASE WHEN $8 THEN now() END, 1500,
                     CASE WHEN $7 = 'submitted' THEN $9 END, CASE WHEN $10 THEN now() END)",
        )
        .bind(id)
        .bind(buyer)
        .bind(self.deployment)
        .bind(format!("key-{id}"))
        .bind(amount)
        .bind(deposit_id.as_slice())
        .bind(state)
        .bind(signed)
        .bind(self.submission)
        .bind(resolved)
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }

    /// A withdrawal to `destination` in `state`, held from `available` once
    /// signed (the caller sets `available` to match).
    async fn withdrawal(
        &self,
        buyer: Uuid,
        withdrawal_id: [u8; 32],
        amount: i64,
        destination: &AccountAddress,
        state: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let signed = state != "awaiting_signature";
        let resolved = matches!(state, "confirmed" | "failed" | "expired");
        sqlx::query(
            "INSERT INTO pay_stellar.withdrawals
                (id, buyer_id, seller_deployment_id, network, idempotency_key, amount,
                 destination_address, withdrawal_id, state, authorization_xdr,
                 signed_authorization_xdr, signed_at, expiration_ledger, submission_id,
                 resolved_at)
             VALUES ($1, $2, $3, 'stellar:testnet', $4, $5, $6, $7, $8, 'AAAA',
                     CASE WHEN $9 THEN 'AAAA' END, CASE WHEN $9 THEN now() END, 1500,
                     CASE WHEN $8 = 'submitted' THEN $10 END, CASE WHEN $11 THEN now() END)",
        )
        .bind(id)
        .bind(buyer)
        .bind(self.deployment)
        .bind(format!("key-{id}"))
        .bind(amount)
        .bind(destination.as_str())
        .bind(withdrawal_id.as_slice())
        .bind(state)
        .bind(signed)
        .bind(self.submission)
        .bind(resolved)
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }

    /// (kind, severity) of every finding, in the order recorded.
    async fn findings(&self) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT kind, severity FROM pay_stellar.reconciliation_findings
             ORDER BY observed_at, id",
        )
        .fetch_all(&self.owner)
        .await
        .unwrap()
    }

    async fn finding_detail(&self, kind: &str) -> serde_json::Value {
        let text: String = sqlx::query_scalar(
            "SELECT detail::text FROM pay_stellar.reconciliation_findings WHERE kind = $1",
        )
        .bind(kind)
        .fetch_one(&self.owner)
        .await
        .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    async fn stored_event_ids(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT event_id FROM pay_stellar.chain_events ORDER BY event_id")
            .fetch_all(&self.owner)
            .await
            .unwrap()
    }

    async fn verdicts(&self) -> Vec<(i16, String)> {
        sqlx::query_as(
            "SELECT entry_index, verdict FROM pay_stellar.chain_event_checks
             ORDER BY chain_event_id, entry_index",
        )
        .fetch_all(&self.owner)
        .await
        .unwrap()
    }

    async fn position(&self) -> (i64, Option<String>) {
        sqlx::query_as(
            "SELECT start_ledger, cursor FROM pay_stellar.observer_cursors
             WHERE seller_deployment_id = $1",
        )
        .bind(self.deployment)
        .fetch_one(&self.owner)
        .await
        .unwrap()
    }
}

fn pair(kind: &str, severity: &str) -> (String, String) {
    (kind.to_owned(), severity.to_owned())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- charges --------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_settled_as_recorded_is_matched_without_a_finding(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 70).await;
    w.charge(alice, [1; 32], 30, "charged", Some("charged")).await;
    w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 30, 0)]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, []);
    assert_eq!(w.verdicts().await, [(0, "matched".to_owned())]);
    assert_eq!(w.stored_event_ids().await.len(), 1);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_account_buyers_charge_is_matched_and_a_strangers_is_not(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let wallet = stellar_strkey::Contract([31; 32]).to_string().to_string();
    let buyer = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pay_stellar.buyers
            (id, product_id, seller_deployment_id, network, external_ref, wallet_address,
             available)
         VALUES ($1, $2, $3, 'stellar:testnet', 'smart-wallet', $4, 70)",
    )
    .bind(buyer)
    .bind(w.product)
    .bind(w.deployment)
    .bind(&wallet)
    .execute(&w.owner)
    .await
    .unwrap();
    w.charge(buyer, [1; 32], 30, "charged", Some("charged")).await;
    let contract = |id: [u8; 32]| ScVal::Address(ScAddress::Contract(ContractId(Hash(id))));
    let entry = |owner: ScVal| vector(vec![owner, bytes(&[1; 32]), i128_val(30), ScVal::U32(0)]);
    // The same charge identifier under the wallet, then under another
    // contract account the gateway does not know.
    w.chain.emit(
        1010,
        (
            vec![symbol("charges")],
            vector(vec![entry(contract([31; 32])), entry(contract([32; 32]))]),
        ),
    );
    w.observer().observe().await.unwrap();
    assert_eq!(w.verdicts().await, [(0, "matched".to_owned()), (1, "finding".to_owned())]);
    assert_eq!(w.findings().await, [pair("unknown_charge", "critical")]);
    let detail = w.finding_detail("unknown_charge").await;
    assert_eq!(detail["owner"], stellar_strkey::Contract([32; 32]).to_string().to_string());
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_the_gateway_never_admitted_is_a_critical_finding(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 70).await;
    // The gateway knows another charge of the same buyer, not this one.
    w.charge(alice, [2; 32], 30, "charged", Some("charged")).await;
    let id = w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 30, 0)]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("unknown_charge", "critical")]);
    let detail = w.finding_detail("unknown_charge").await;
    assert_eq!(detail["charge_id"], hex(&[1; 32]));
    assert_eq!(detail["owner"], account(1).as_str());
    assert_eq!(detail["event_id"], id.to_string());
    assert_eq!(detail["ledger"], 1010);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_of_another_amount_is_a_critical_finding(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 70).await;
    w.charge(alice, [1; 32], 30, "charged", Some("charged")).await;
    w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 31, 0)]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("charge_amount_mismatch", "critical")]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charge_the_gateway_refunded_but_the_contract_debited_is_a_critical_finding(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 100).await;
    w.charge(alice, [1; 32], 30, "refused", Some("insufficient_balance")).await;
    w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 30, 0)]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("charge_outcome_mismatch", "critical")]);
    let detail = w.finding_detail("charge_outcome_mismatch").await;
    assert_eq!(detail["charge"]["state"], "refused");
    assert_eq!(detail["outcome"], "charged");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_duplicate_and_expired_entries_are_refusals_not_debits(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 70).await;
    w.charge(alice, [1; 32], 30, "charged", Some("charged")).await;
    w.chain.emit(
        1010,
        charges_event(&[
            (&account(1), [1; 32], 30, 0),
            (&account(1), [1; 32], 30, 3),
            (&account(1), [1; 32], 30, 4),
        ]),
    );
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, []);
    assert_eq!(
        w.verdicts().await,
        [(0, "matched".to_owned()), (1, "matched".to_owned()), (2, "matched".to_owned())]
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_unsettled_charge_is_a_finding_only_once_the_grace_has_passed(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 70).await;
    let bob = w.buyer(2, 70).await;
    w.charge(alice, [1; 32], 30, "submitted", None).await;
    let settles = w.charge(bob, [2; 32], 30, "submitted", None).await;
    w.chain
        .emit(1010, charges_event(&[(&account(1), [1; 32], 30, 0), (&account(2), [2; 32], 30, 0)]));
    let observer = w.observer();
    observer.observe().await.unwrap();
    assert_eq!(w.findings().await, [], "not a finding when observed");
    assert_eq!(w.verdicts().await, []);

    // Within the grace the worker settles Bob's charge; Alice's stays.
    w.clock.set(closed(&w, 1010) + GRACE - time::Duration::SECOND);
    sqlx::query(
        "UPDATE pay_stellar.charges SET state = 'charged', outcome = 'charged', settled_at = now()
         WHERE id = $1",
    )
    .bind(settles)
    .execute(&w.owner)
    .await
    .unwrap();
    observer.recheck().await.unwrap();
    assert_eq!(w.findings().await, []);
    assert_eq!(w.verdicts().await.len(), 1, "Bob's entry matched");

    w.clock.set(closed(&w, 1010) + GRACE);
    observer.recheck().await.unwrap();
    observer.recheck().await.unwrap();
    assert_eq!(w.findings().await, [pair("charge_unsettled", "warning")], "recorded once");
    let detail = w.finding_detail("charge_unsettled").await;
    assert_eq!(detail["owner"], account(1).as_str());
    assert_eq!(detail["charge"]["state"], "submitted");
}

// ---- deposits -------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposits_are_matched_by_owner_and_deposit_id(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 100).await;
    w.deposit(alice, [1; 32], 100, "confirmed").await;
    w.deposit(alice, [3; 32], 40, "submitted").await;
    w.chain.emit(1010, deposit_event(&account(1), 100, [1; 32]));
    // The same deposit id under another owner is a different deposit.
    w.chain.emit(1011, deposit_event(&account(9), 100, [1; 32]));
    w.chain.emit(1012, deposit_event(&account(1), 40, [3; 32]));
    let observer = w.observer();
    observer.observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("unknown_deposit", "warning")]);
    assert_eq!(w.finding_detail("unknown_deposit").await["owner"], account(9).as_str());

    w.clock.set(closed(&w, 1012) + GRACE);
    observer.recheck().await.unwrap();
    assert_eq!(
        w.findings().await,
        [pair("unknown_deposit", "warning"), pair("deposit_unsettled", "warning")]
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_deposit_the_gateway_closed_but_the_contract_credited_is_critical(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 0).await;
    w.deposit(alice, [1; 32], 100, "expired").await;
    w.chain.emit(1010, deposit_event(&account(1), 100, [1; 32]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("deposit_outcome_mismatch", "critical")]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_charge_refused_past_the_daily_limit_is_matched_with_its_refusal(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 100).await;
    w.charge(alice, [1; 32], 30, "refused", Some("above_daily_limit")).await;
    w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 30, 6)]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, []);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_daily_limit_refusal_observed_before_the_worker_settles_is_matched_on_recheck(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 100).await;
    let charge = w.charge(alice, [1; 32], 30, "submitted", None).await;
    w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 30, 6)]));
    let observer = w.observer();
    observer.observe().await.unwrap();
    assert_eq!(w.verdicts().await, [], "waiting for the worker");
    sqlx::query(
        "UPDATE pay_stellar.charges
         SET state = 'refused', outcome = 'above_daily_limit', settled_at = now()
         WHERE id = $1",
    )
    .bind(charge)
    .execute(&w.owner)
    .await
    .unwrap();
    observer.recheck().await.unwrap();
    assert_eq!((w.findings().await, w.verdicts().await.len()), (vec![], 1));
}

// ---- withdrawals ----------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawals_are_matched_by_owner_id_amount_and_destination(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 0).await;
    w.withdrawal(alice, [1; 32], 30, &account(1), "confirmed").await;
    w.withdrawal(alice, [2; 32], 20, &account(1), "submitted").await;
    w.withdrawal(alice, [3; 32], 10, &account(1), "expired").await;
    w.withdrawal(alice, [4; 32], 25, &account(1), "submitted").await;
    w.withdrawal(alice, [5; 32], 15, &account(2), "submitted").await;
    // As recorded, final or still in flight: matched.
    w.chain.emit(1010, withdraw_event(&account(1), 30, [1; 32]));
    w.chain.emit(1011, withdraw_event(&account(1), 20, [2; 32]));
    // Returned to the buyer, yet the USDC left.
    w.chain.emit(1012, withdraw_event(&account(1), 10, [3; 32]));
    // No such withdrawal for this owner.
    w.chain.emit(1013, withdraw_event(&account(9), 30, [1; 32]));
    // Another amount, or another destination, than the one prepared.
    w.chain.emit(1014, withdraw_event(&account(1), 26, [4; 32]));
    w.chain.emit(1015, withdraw_event(&account(1), 15, [5; 32]));
    w.observer().observe().await.unwrap();
    let mut findings = w.findings().await;
    findings.sort();
    assert_eq!(
        findings,
        [
            pair("unknown_withdrawal", "warning"),
            pair("withdrawal_mismatch", "critical"),
            pair("withdrawal_mismatch", "critical"),
            pair("withdrawal_outcome_mismatch", "critical"),
        ]
    );
    assert_eq!(w.finding_detail("unknown_withdrawal").await["owner"], account(9).as_str());
}

// ---- roles ----------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_rotation_the_binding_does_not_follow_is_a_finding_after_the_grace(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let (new_operator, new_treasury) = (account(211), account(212));
    w.chain.emit(1010, role_event("operator", &operator(), &new_operator));
    w.chain.emit(1011, role_event("treasury", &treasury(), &new_treasury));
    let observer = w.observer();
    observer.observe().await.unwrap();
    assert_eq!(w.findings().await, [], "the binding may follow later");

    // The binding follows the operator rotation only.
    issuance::sync_ledger_binding(&w.issuer, w.deployment, &new_operator, &treasury(), "test")
        .await
        .unwrap();
    w.clock.set(closed(&w, 1011) + GRACE);
    observer.recheck().await.unwrap();
    assert_eq!(w.findings().await, [pair("binding_out_of_date", "warning")]);
    let detail = w.finding_detail("binding_out_of_date").await;
    assert_eq!(
        (detail["role"].as_str(), detail["current"].as_str(), detail["bound"].as_str()),
        (Some("treasury"), Some(new_treasury.as_str()), Some(treasury().as_str()))
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_pause_and_limit_changes_are_reported_at_once(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.chain.emit(1010, (vec![symbol("pause")], ScVal::Bool(true)));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("admin_change", "warning")]);
    assert_eq!(w.finding_detail("admin_change").await["paused"].as_bool(), Some(true));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_admin_rotation_is_reported_at_once(opts: PgPoolOptions, connect: PgConnectOptions) {
    let w = world(opts, connect).await;
    w.chain.emit(1010, role_event("admin", &admin(), &account(213)));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("role_changed", "warning")]);
}

// ---- reading the stream -------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_restarted_observer_resumes_from_its_cursor_without_repeats_or_skips(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let mut emitted = Vec::new();
    for (ledger, id) in [(1010, 1), (1010, 2), (1020, 3), (1030, 4), (1040, 5)] {
        emitted.push(w.chain.emit(ledger, revenue_event(10, [id; 32])).to_string());
    }
    // Two events per page: the first process stores one page and stops.
    let first = w.observer_with(StartPosition::Oldest, 2);
    let deployment = first.deployments().await.unwrap().remove(0);
    first.ingest(&deployment).await.unwrap();
    assert_eq!(w.stored_event_ids().await, emitted[..2]);
    assert_eq!(w.position().await.1.as_deref(), Some(emitted[1].as_str()));
    drop(first);

    let second = w.observer_with(StartPosition::Latest, 2);
    second.observe().await.unwrap();
    assert_eq!(w.stored_event_ids().await, emitted);
    assert_eq!(
        w.position().await.1,
        Some(EventCursor::end_of_ledger(OLDEST + 100).to_string()),
        "caught up with the latest ledger"
    );
    // Nothing new: another round stores nothing.
    second.observe().await.unwrap();
    assert_eq!(w.stored_event_ids().await, emitted);

    emitted.push(w.chain.emit(1200, revenue_event(10, [6; 32])).to_string());
    second.observe().await.unwrap();
    assert_eq!(w.stored_event_ids().await, emitted);
    assert_eq!(w.findings().await, []);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_event_scan_window_is_crossed_without_events(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    // Events further apart than one scan window.
    let near = w.chain.emit(1005, revenue_event(10, [1; 32]));
    let far = w.chain.emit(35_000, revenue_event(10, [2; 32]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.stored_event_ids().await, [near.to_string(), far.to_string()]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_start_before_the_retained_range_records_the_gap(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let kept = w.chain.emit(1000, revenue_event(10, [1; 32]));
    w.observer_with(StartPosition::Ledger(900), 100).observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("event_gap", "warning")]);
    let detail = w.finding_detail("event_gap").await;
    assert_eq!(
        (detail["from_ledger"].as_u64(), detail["to_ledger"].as_u64()),
        (Some(900), Some(999))
    );
    assert_eq!(w.stored_event_ids().await, [kept.to_string()]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_start_inside_the_retained_range_records_no_gap(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let kept = w.chain.emit(1000, revenue_event(10, [1; 32]));
    w.observer_with(StartPosition::Ledger(1000), 100).observe().await.unwrap();
    assert_eq!(w.findings().await, []);
    assert_eq!(w.stored_event_ids().await, [kept.to_string()]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_ledgers_pruned_while_the_observer_was_away_are_a_gap(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let seen = w.chain.emit(1050, revenue_event(10, [1; 32]));
    let observer = w.observer();
    observer.observe().await.unwrap();
    // Caught up at 1100. While the observer is away the chain moves on and
    // the node prunes everything before 1300, including an event at 1200.
    w.chain.emit(1200, revenue_event(10, [2; 32]));
    let after = w.chain.emit(1400, revenue_event(10, [3; 32]));
    w.chain.with(|n| n.oldest = 1300);
    observer.observe().await.unwrap();
    let detail = w.finding_detail("event_gap").await;
    assert_eq!(
        (detail["from_ledger"].as_u64(), detail["to_ledger"].as_u64()),
        (Some(1101), Some(1299))
    );
    assert_eq!(w.stored_event_ids().await, [seen.to_string(), after.to_string()]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_event_of_another_layout_is_reported_and_reading_goes_on(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    w.chain.emit(1010, earlier_layout_event());
    let later = w.chain.emit(1011, revenue_event(10, [1; 32]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, [pair("unrecognized_event", "warning")]);
    assert_eq!(w.stored_event_ids().await.last(), Some(&later.to_string()));
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM pay_stellar.chain_events ORDER BY event_id")
            .fetch_all(&w.owner)
            .await
            .unwrap();
    assert_eq!(kinds, ["unrecognized", "revenue_withdrawal"]);
}

/// A failure while a page is stored leaves neither its events nor the moved
/// position behind, and the next process stores the page exactly once.
#[sqlx::test(migrations = "../../db/migrations")]
async fn test_failure_while_storing_a_page_stores_nothing_and_moves_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let ids: Vec<String> = [(1010, 1), (1011, 2), (1012, 3)]
        .into_iter()
        .map(|(ledger, id)| w.chain.emit(ledger, revenue_event(10, [id; 32])))
        .map(|id| id.to_string())
        .collect();
    // The database refuses the second event of the page.
    // The id is one this test generated, never outside input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION pay_stellar.test_refuse() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.event_id = '{}' THEN RAISE EXCEPTION 'injected failure'; END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER test_refuse BEFORE INSERT ON pay_stellar.chain_events
             FOR EACH ROW EXECUTE FUNCTION pay_stellar.test_refuse();",
        ids[1]
    )))
    .execute(&w.owner)
    .await
    .unwrap();

    assert!(w.observer().observe().await.is_err());
    assert_eq!(w.stored_event_ids().await, Vec::<String>::new());
    assert_eq!(w.position().await, (i64::from(OLDEST), None), "position unmoved");

    sqlx::raw_sql("DROP TRIGGER test_refuse ON pay_stellar.chain_events")
        .execute(&w.owner)
        .await
        .unwrap();
    w.observer().observe().await.unwrap();
    assert_eq!(w.stored_event_ids().await, ids);
}

// ---- reconciliation -------------------------------------------------------

/// A deployment whose chain, event stream and database agree: Alice
/// deposited 100 and was charged 30 of it, and the seller withdrew 10 of
/// that revenue, so 70 is owed to Alice, 20 to the seller, and the treasury
/// holds 90.
async fn agreeing(w: &World) -> Uuid {
    let alice = w.buyer(1, 70).await;
    w.deposit(alice, [1; 32], 100, "confirmed").await;
    w.charge(alice, [1; 32], 30, "charged", Some("charged")).await;
    w.chain.emit(1010, deposit_event(&account(1), 100, [1; 32]));
    w.chain.emit(1020, charges_event(&[(&account(1), [1; 32], 30, 0)]));
    w.chain.emit(1030, revenue_event(10, [9; 32]));
    w.chain.with(|n| {
        n.liabilities = 70;
        n.revenue = 20;
        n.trustline = Some((90, 1));
    });
    alice
}

async fn reconcile_times(observer: &Observer<Chain, ManualClock>, times: u32) {
    for _ in 0..times {
        observer.reconcile().await.unwrap();
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_agreeing_records_produce_no_finding(opts: PgPoolOptions, connect: PgConnectOptions) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    reconcile_times(&w.observer(), CONFIRMATIONS + 1).await;
    assert_eq!(w.findings().await, []);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_transient_deficit_is_not_recorded(opts: PgPoolOptions, connect: PgConnectOptions) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    let observer = w.observer();
    w.chain.with(|n| n.trustline = Some((89, 1)));
    reconcile_times(&observer, CONFIRMATIONS - 1).await;
    w.chain.with(|n| n.trustline = Some((90, 1)));
    reconcile_times(&observer, 1).await;
    w.chain.with(|n| n.trustline = Some((89, 1)));
    reconcile_times(&observer, CONFIRMATIONS - 1).await;
    assert_eq!(w.findings().await, []);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_persistent_deficit_is_recorded_once_as_critical(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    let observer = w.observer();
    w.chain.with(|n| n.trustline = Some((89, 1)));
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("treasury_deficit", "critical")]);
    let detail = w.finding_detail("treasury_deficit").await;
    assert_eq!(
        (detail["treasury_usdc"].as_str(), detail["owed"].as_str(), detail["difference"].as_str()),
        (Some("89"), Some("90"), Some("-1"))
    );
    reconcile_times(&observer, 2).await;
    assert_eq!(w.findings().await.len(), 1, "a standing deficit is recorded once");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_restarted_observer_neither_repeats_nor_forgets_a_streak(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    w.chain.with(|n| n.trustline = Some((89, 1)));
    // A streak one check short, then a restart: the next check completes it.
    reconcile_times(&w.observer(), CONFIRMATIONS - 1).await;
    assert_eq!(w.findings().await, []);
    reconcile_times(&w.observer(), 1).await;
    assert_eq!(w.findings().await, [pair("treasury_deficit", "critical")]);
    // Recorded, then restarted with the deficit still standing: not again.
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await.len(), 1);
    // Cleared and back: a new streak, recorded once it is confirmed.
    w.chain.with(|n| n.trustline = Some((90, 1)));
    reconcile_times(&w.observer(), 1).await;
    w.chain.with(|n| n.trustline = Some((89, 1)));
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await.len(), 2);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_the_cold_reserve_counts_towards_what_the_treasury_owes(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    // 60 of the 90 the contract owes was swept to the reserve.
    let cold = account(42);
    w.chain.with(|n| {
        n.trustline = Some((30, 1));
        n.reserve = Some((cold.clone(), 60));
    });
    let reserves = std::collections::HashMap::from([(treasury(), cold)]);
    reconcile_times(&w.observer().with_reserves(reserves), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, []);
    let (hot, reserve) = w.chain.with(|n| (n.trustline, n.reserve.clone()));
    assert_eq!((hot.unwrap().0, reserve.unwrap().1), (30, 60));

    // An observer not told about the reserve sees only the treasury.
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("treasury_deficit", "critical")]);
    let detail = w.finding_detail("treasury_deficit").await;
    assert_eq!((detail["held"].as_str(), detail["owed"].as_str()), (Some("30"), Some("90")));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_surplus_is_informational(opts: PgPoolOptions, connect: PgConnectOptions) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    w.chain.with(|n| n.trustline = Some((91, 1)));
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("treasury_surplus", "info")]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_frozen_treasury_is_critical_even_when_its_balance_covers(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    // AUTHORIZED_TO_MAINTAIN_LIABILITIES only: the issuer revoked the
    // trustline's authorization.
    w.chain.with(|n| n.trustline = Some((90, 2)));
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("treasury_deauthorized", "critical")]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_rotated_treasury_is_read_where_the_contract_points(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    // The contract names a new treasury holding the USDC; the binding still
    // names the previous one, which the fake no longer answers for.
    w.chain.with(|n| n.config_treasury = account(212));
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, []);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_charges_in_flight_are_within_the_expected_totals(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = agreeing(&w).await;
    // Admitted (available 70 -> 50) and already debited on-chain, but not
    // yet settled in the database.
    w.charge(alice, [2; 32], 20, "submitted", None).await;
    sqlx::query("UPDATE pay_stellar.buyers SET available = 50 WHERE id = $1")
        .bind(alice)
        .execute(&w.owner)
        .await
        .unwrap();
    w.chain.emit(1040, charges_event(&[(&account(1), [2; 32], 20, 0)]));
    w.chain.with(|n| {
        n.liabilities = 50;
        n.revenue = 40;
    });
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, []);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_withdrawals_held_and_made_elsewhere_are_within_the_expected_totals(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = agreeing(&w).await;
    let set_available = |available: i64| {
        sqlx::query("UPDATE pay_stellar.buyers SET available = $2 WHERE id = $1")
            .bind(alice)
            .bind(available)
            .execute(&w.owner)
    };
    let observer = w.observer();

    // Held (available 70 -> 40) but not yet sent: the contract still owes 70.
    let held = w.withdrawal(alice, [1; 32], 30, &account(1), "signed").await;
    set_available(40).await.unwrap();
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await, []);

    // Sent and processed on-chain: the contract owes 40 and the treasury
    // holds 60.
    sqlx::query(
        "UPDATE pay_stellar.withdrawals SET state = 'submitted', submission_id = $2 WHERE id = $1",
    )
    .bind(held)
    .bind(w.submission)
    .execute(&w.owner)
    .await
    .unwrap();
    w.chain.emit(1110, withdraw_event(&account(1), 30, [1; 32]));
    w.chain.with(|n| {
        n.liabilities = 40;
        n.trustline = Some((60, 1));
    });
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await, []);
    // And confirmed by the worker: nothing is pending any more.
    sqlx::query(
        "UPDATE pay_stellar.withdrawals SET state = 'confirmed', resolved_at = now() WHERE id = $1",
    )
    .bind(held)
    .execute(&w.owner)
    .await
    .unwrap();
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await, []);

    // A withdrawal the gateway never prepared: its own finding, and counted
    // so the totals still agree.
    w.chain.emit(1120, withdraw_event(&account(1), 10, [7; 32]));
    w.chain.with(|n| {
        n.liabilities = 30;
        n.trustline = Some((50, 1));
    });
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("unknown_withdrawal", "warning")]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_database_balance_the_contract_does_not_hold_is_recorded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = agreeing(&w).await;
    // Available exceeds what the confirmed deposit and the charge leave.
    sqlx::query("UPDATE pay_stellar.buyers SET available = 75 WHERE id = $1")
        .bind(alice)
        .execute(&w.owner)
        .await
        .unwrap();
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("ledger_totals_mismatch", "warning")]);
    let detail = w.finding_detail("ledger_totals_mismatch").await;
    assert_eq!(detail["expected"]["liabilities"], serde_json::json!(["75", "75"]));
    assert_eq!(detail["contract"]["liabilities"], "70");
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_contract_totals_the_events_do_not_explain_are_recorded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    // A withdrawal the stream does not show: the contract owes 5 less,
    // and so does the treasury hold.
    w.chain.with(|n| {
        n.liabilities = 65;
        n.trustline = Some((85, 1));
    });
    reconcile_times(&w.observer(), CONFIRMATIONS).await;
    // Recorded in one transaction: sorted, as their order is not defined.
    let mut kinds: Vec<String> = w.findings().await.into_iter().map(|(kind, _)| kind).collect();
    kinds.sort();
    assert_eq!(kinds, ["event_totals_mismatch", "ledger_totals_mismatch"]);
}

// ---- baselines ------------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_baseline_lets_reconciliation_resume_after_lost_history(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let operator = pool_as(&connect, "SET ROLE pay_stellar_operator", 1).await;
    let w = world(opts, connect).await;
    // Alice's deposit happened before observation began: the stream lacks
    // it, so the events cannot explain the contract's totals.
    let alice = w.buyer(1, 70).await;
    w.deposit(alice, [1; 32], 100, "confirmed").await;
    w.charge(alice, [1; 32], 30, "charged", Some("charged")).await;
    w.chain.emit(1020, charges_event(&[(&account(1), [1; 32], 30, 0)]));
    w.chain.emit(1030, revenue_event(10, [9; 32]));
    w.chain.with(|n| {
        n.liabilities = 70;
        n.revenue = 20;
        n.trustline = Some((90, 1));
    });
    let observer = w.observer();
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("event_totals_mismatch", "warning")]);

    // The operator acknowledges the books as they stand.
    let ledger = observer.record_baseline(&operator, w.deployment, "history lost").await.unwrap();
    assert_eq!(ledger, w.chain.with(|n| n.latest));
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await.len(), 1, "compared from the baseline on");

    // A charge after it, on-chain and in the books: nothing to record.
    w.chain.with(|n| n.latest += 20);
    w.charge(alice, [2; 32], 5, "charged", Some("charged")).await;
    sqlx::query("UPDATE pay_stellar.buyers SET available = 65 WHERE id = $1")
        .bind(alice)
        .execute(&w.owner)
        .await
        .unwrap();
    w.chain.emit(ledger + 10, charges_event(&[(&account(1), [2; 32], 5, 0)]));
    w.chain.with(|n| {
        n.liabilities = 65;
        n.revenue = 25;
    });
    reconcile_times(&observer, CONFIRMATIONS).await;
    assert_eq!(w.findings().await.len(), 1);

    // A change after it that nothing explains is still caught.
    w.chain.with(|n| {
        n.liabilities = 60;
        n.trustline = Some((85, 1));
    });
    reconcile_times(&observer, CONFIRMATIONS).await;
    let mut kinds: Vec<String> = w.findings().await.into_iter().map(|(kind, _)| kind).collect();
    kinds.sort();
    assert_eq!(kinds, ["event_totals_mismatch", "event_totals_mismatch", "ledger_totals_mismatch"]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_baseline_waits_until_nothing_is_in_flight(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let operator = pool_as(&connect, "SET ROLE pay_stellar_operator", 1).await;
    let w = world(opts, connect).await;
    let alice = agreeing(&w).await;
    let pending = w.charge(alice, [2; 32], 5, "submitted", None).await;
    let refused = w.observer().record_baseline(&operator, w.deployment, "now").await;
    assert!(matches!(refused, Err(ObserverError::NotQuiet)), "{refused:?}");
    // The positive control: the charge final, the baseline is taken.
    sqlx::query(
        "UPDATE pay_stellar.charges SET state = 'charged', outcome = 'charged', settled_at = now()
         WHERE id = $1",
    )
    .bind(pending)
    .execute(&w.owner)
    .await
    .unwrap();
    w.observer().record_baseline(&operator, w.deployment, "now").await.unwrap();
    // The table is append-only, for the operator as for everyone.
    let error = sqlx::query("DELETE FROM pay_stellar.reconciliation_baselines")
        .execute(&w.owner)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("append-only"), "{error}");
}

// ---- privileges -----------------------------------------------------------

fn is_permission_denied(error: &sqlx::Error) -> bool {
    error.as_database_error().and_then(|e| e.code()).as_deref() == Some("42501")
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_observer_cannot_rewrite_observations_or_touch_money(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 70).await;
    w.chain.emit(1010, charges_event(&[(&account(1), [1; 32], 30, 0)]));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await.len(), 1, "an unknown charge was recorded");

    // Control: the observer can read what it recorded.
    let visible: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pay_stellar.reconciliation_findings")
            .fetch_one(&w.observer_pool)
            .await
            .unwrap();
    assert_eq!(visible, 1);
    for statement in [
        "UPDATE pay_stellar.chain_events SET kind = 'withdrawal'",
        "DELETE FROM pay_stellar.chain_events",
        "UPDATE pay_stellar.chain_charge_entries SET amount = 1",
        "DELETE FROM pay_stellar.chain_event_checks",
        "UPDATE pay_stellar.reconciliation_findings SET severity = 'info'",
        "DELETE FROM pay_stellar.reconciliation_findings",
        "UPDATE pay_stellar.observer_cursors SET observed_from_ledger = 1",
        "DELETE FROM pay_stellar.observer_cursors",
        "UPDATE pay_stellar.buyers SET available = available + 1",
        "UPDATE pay_stellar.charges SET state = 'refused'",
        "UPDATE pay_stellar.deposits SET state = 'confirmed'",
        "UPDATE pay_stellar.ledger_contracts SET operator_address = treasury_address",
    ] {
        let error = sqlx::query(statement).execute(&w.observer_pool).await.unwrap_err();
        assert!(is_permission_denied(&error), "{statement}: {error:?}");
    }
    let error = sqlx::query(
        "INSERT INTO pay_stellar.charges (id, buyer_id, seller_deployment_id, network,
             idempotency_key, amount, charge_id, last_ledger)
         VALUES ($1, $2, $3, 'stellar:testnet', 'k', 1, $4, 1)",
    )
    .bind(Uuid::now_v7())
    .bind(alice)
    .bind(w.deployment)
    .bind([1_u8; 32].as_slice())
    .execute(&w.observer_pool)
    .await
    .unwrap_err();
    assert!(is_permission_denied(&error), "{error:?}");

    // Not even the owner can rewrite an observation.
    for statement in [
        "UPDATE pay_stellar.chain_events SET kind = 'withdrawal'",
        "DELETE FROM pay_stellar.reconciliation_findings",
        "UPDATE pay_stellar.chain_charge_entries SET amount = 1",
        "DELETE FROM pay_stellar.chain_event_checks",
    ] {
        let error = sqlx::query(statement).execute(&w.owner).await.unwrap_err();
        assert!(error.to_string().contains("is append-only"), "{statement}: {error}");
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_observer_position_only_moves_forward(opts: PgPoolOptions, connect: PgConnectOptions) {
    let w = world(opts, connect).await;
    w.chain.emit(1010, revenue_event(10, [1; 32]));
    w.observer().observe().await.unwrap();
    let (_, cursor) = w.position().await;
    let cursor = EventCursor::parse(&cursor.unwrap()).unwrap();

    let set = |cursor: EventCursor| {
        sqlx::query(
            "UPDATE pay_stellar.observer_cursors SET cursor = $1 WHERE seller_deployment_id = $2",
        )
        .bind(cursor.to_string())
        .bind(w.deployment)
        .execute(&w.observer_pool)
    };
    let error = set(EventCursor::end_of_ledger(cursor.ledger() - 1)).await.unwrap_err();
    assert!(error.to_string().contains("cannot move back"), "{error}");
    // Control: moving forward is allowed.
    set(EventCursor::end_of_ledger(cursor.ledger() + 1)).await.unwrap();
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_round_reads_matches_and_reconciles_before_shutdown(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    // A deficit on every check, so the round's reconciliation is visible
    // once the confirmations are reached.
    w.chain.with(|n| n.trustline = Some((89, 1)));
    let observer = w.observer_with(StartPosition::Oldest, 100);
    for _ in 0..CONFIRMATIONS {
        // Shutdown is already signalled: exactly one round runs.
        observer
            .run(
                Duration::from_secs(3600),
                Duration::ZERO,
                Duration::from_secs(3600),
                std::future::ready(()),
            )
            .await;
    }
    assert_eq!(w.stored_event_ids().await.len(), 3);
    assert_eq!(w.findings().await, [pair("treasury_deficit", "critical")]);
}

// ---- recurring charges ------------------------------------------------------

/// Owner, attempt identifier, mandate identifier, period, amount, outcome code.
type RecurringEntryShape<'a> = (&'a AccountAddress, [u8; 32], [u8; 32], u32, i128, u32);

fn recurring_event(entries: &[RecurringEntryShape<'_>]) -> Event {
    let entries = entries
        .iter()
        .map(|(owner, id, mandate, cycle, amount, code)| {
            vector(vec![
                address(owner),
                bytes(id),
                bytes(mandate),
                ScVal::U32(*cycle),
                i128_val(*amount),
                ScVal::U32(*code),
            ])
        })
        .collect();
    (vec![symbol("recurring")], vector(entries))
}

fn mandate_event(owner: &AccountAddress, mandate: [u8; 32]) -> Event {
    (
        vec![symbol("mandate"), address(owner)],
        map(vec![
            ("amount", i128_val(10)),
            ("cycles", ScVal::U32(3)),
            ("live_until", ScVal::U32(9_000)),
            ("mandate_id", bytes(&mandate)),
            ("next_cycle", ScVal::U32(0)),
            ("period_secs", ScVal::U64(86_400)),
            ("start", ScVal::U64(1_000)),
        ]),
    )
}

fn revoke_event(owner: &AccountAddress, mandate: Option<[u8; 32]>) -> Event {
    (vec![symbol("revoke"), address(owner)], mandate.map_or(ScVal::Void, |m| bytes(&m)))
}

impl World {
    /// An active mandate of `buyer` with contract identifier `mandate_id`.
    async fn mandate(&self, buyer: Uuid, mandate_id: [u8; 32]) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO pay_stellar.mandates
                (id, buyer_id, seller_deployment_id, network, idempotency_key, mandate_id, amount,
                 period_secs, cycles, live_until, state, authorization_xdr, expiration_ledger,
                 starts_at, activated_at)
             VALUES ($1, $2, $3, 'stellar:testnet', $4, $5, 10, 86400, 3, 9000, 'active',
                     'AAAA', 1100, 1000, now())",
        )
        .bind(id)
        .bind(buyer)
        .bind(self.deployment)
        .bind(format!("mandate-{id}"))
        .bind(mandate_id.as_slice())
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }

    /// A recurring charge attempt in `state`, linked to a batch unless it
    /// is admitted.
    async fn recurring(
        &self,
        (buyer, mandate): (Uuid, Uuid),
        charge_id: [u8; 32],
        amount: i64,
        state: &str,
        outcome: Option<&str>,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let linked = state != "admitted";
        let settled = matches!(state, "charged" | "refused" | "quarantined");
        sqlx::query(
            "INSERT INTO pay_stellar.recurring_charges
                (id, mandate_row_id, buyer_id, seller_deployment_id, network, idempotency_key,
                 cycle, charge_id, amount, last_ledger, state, outcome, submission_id,
                 batch_index, settled_at)
             VALUES ($1, $2, $3, $4, 'stellar:testnet', $5, 0, $6, $7, 2000, $8, $9, $10, $11,
                     CASE WHEN $12 THEN now() END)",
        )
        .bind(id)
        .bind(mandate)
        .bind(buyer)
        .bind(self.deployment)
        .bind(format!("recurring-{id}"))
        .bind(charge_id.as_slice())
        .bind(amount)
        .bind(state)
        .bind(outcome)
        .bind(linked.then_some(self.submission))
        .bind(linked.then_some(0_i16))
        .bind(settled)
        .execute(&self.owner)
        .await
        .unwrap();
        id
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_recurring_charges_settled_as_recorded_are_matched(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 0).await;
    let mandate = w.mandate(alice, [7; 32]).await;
    w.recurring((alice, mandate), [1; 32], 10, "charged", Some("charged")).await;
    w.recurring((alice, mandate), [2; 32], 10, "refused", Some("wallet_short")).await;
    w.chain.emit(1010, mandate_event(&account(1), [7; 32]));
    w.chain.emit(
        1020,
        recurring_event(&[
            (&account(1), [1; 32], [7; 32], 0, 10, 0),
            (&account(1), [2; 32], [7; 32], 0, 10, 12),
            // A second copy of the first attempt, answered as a duplicate.
            (&account(1), [1; 32], [7; 32], 0, 10, 1),
        ]),
    );
    w.chain.emit(1030, revoke_event(&account(1), Some([7; 32])));
    w.observer().observe().await.unwrap();
    assert_eq!(w.findings().await, []);
    assert_eq!(w.verdicts().await.len(), 3);
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM pay_stellar.chain_events ORDER BY event_id")
            .fetch_all(&w.owner)
            .await
            .unwrap();
    assert_eq!(kinds, ["mandate", "recurring", "revoke"]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_recurring_charges_the_records_do_not_explain_are_findings(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = w.buyer(1, 0).await;
    let mandate = w.mandate(alice, [7; 32]).await;
    // Refused as recorded, but the contract moved the USDC.
    w.recurring((alice, mandate), [1; 32], 10, "refused", Some("wallet_short")).await;
    // Recorded for another amount.
    w.recurring((alice, mandate), [2; 32], 10, "submitted", None).await;
    w.chain.emit(
        1020,
        recurring_event(&[
            (&account(1), [1; 32], [7; 32], 0, 10, 0),
            (&account(1), [2; 32], [7; 32], 0, 11, 0),
            // Never admitted by the gateway, and charged.
            (&account(1), [3; 32], [7; 32], 0, 10, 0),
        ]),
    );
    w.observer().observe().await.unwrap();
    let mut findings = w.findings().await;
    findings.sort();
    assert_eq!(
        findings,
        [
            pair("recurring_charge_mismatch", "critical"),
            pair("recurring_outcome_mismatch", "critical"),
            pair("unknown_recurring_charge", "critical"),
        ]
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_recurring_revenue_reconciles_with_the_contract_and_the_treasury(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    let alice = agreeing(&w).await;
    let mandate = w.mandate(alice, [7; 32]).await;
    w.recurring((alice, mandate), [5; 32], 15, "charged", Some("charged")).await;
    w.chain.emit(1040, recurring_event(&[(&account(1), [5; 32], [7; 32], 0, 15, 0)]));
    // Revenue grows by 15, the treasury holds 15 more, liabilities stay.
    w.chain.with(|n| {
        n.revenue = 35;
        n.trustline = Some((105, 1));
    });
    let observer = w.observer();
    observer.observe().await.unwrap();
    reconcile_times(&observer, CONFIRMATIONS + 1).await;
    assert_eq!(w.findings().await, []);

    // Had the contract not counted it as revenue, both views disagree.
    w.chain.with(|n| n.revenue = 20);
    reconcile_times(&observer, CONFIRMATIONS).await;
    let mut findings = w.findings().await;
    findings.sort();
    assert_eq!(
        findings,
        [
            pair("event_totals_mismatch", "warning"),
            pair("ledger_totals_mismatch", "warning"),
            pair("treasury_surplus", "info"),
        ]
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_a_contract_running_other_code_than_expected_is_a_critical_finding(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let w = world(opts, connect).await;
    agreeing(&w).await;
    // The fake contract runs Wasm [9; 32].
    let expecting = |hash: [u8; 32]| {
        w.observer().with_expected_code(std::collections::HashMap::from([(CONTRACT, hash)]))
    };
    reconcile_times(&expecting([9; 32]), CONFIRMATIONS + 1).await;
    assert_eq!(w.findings().await, []);
    reconcile_times(&expecting([8; 32]), CONFIRMATIONS).await;
    assert_eq!(w.findings().await, [pair("code_changed", "critical")]);
    let detail = w.finding_detail("code_changed").await;
    assert_eq!(
        (detail["expected_wasm"].as_str(), detail["running_wasm"].as_str()),
        (Some(hex(&[8; 32]).as_str()), Some(hex(&[9; 32]).as_str()))
    );
}
