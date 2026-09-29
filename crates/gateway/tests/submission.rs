//! Durable submission against real PostgreSQL, running as the worker role,
//! with a scripted network standing in for Stellar RPC so that lost
//! responses, crashes and ambiguous answers can be produced on demand.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::rpc::{
    IncludedTransaction, RpcError, SendOutcome, Simulation, SimulationOutcome, TransactionStatus,
};
use fermah_pay_stellar_chain::soroban::fee_bump_hash;
use fermah_pay_stellar_chain::stellar_xdr::{
    ContractId, Hash, HostFunction, InvokeContractArgs, LedgerFootprint, Limits, ReadXdr,
    ScAddress, ScSymbol, SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
    TransactionEnvelope, TransactionResult, TransactionResultExt, TransactionResultResult, VecM,
};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::submission::{
    Broadcast, Chain, Clock, Engine, EngineError, Keys, Kind, Policy, State,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

// ---- scripted network ---------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Reply {
    Pending,
    Refused,
    Lost,
}

struct Network_ {
    sequence: Option<i64>,
    simulation_error: Option<String>,
    replies: std::collections::VecDeque<Reply>,
    /// Include an envelope in the ledger when it is sent (even if the reply
    /// to that send is lost or refused).
    include_on_send: bool,
    included: HashMap<[u8; 32], TransactionStatus>,
    sent: Vec<TransactionEnvelope>,
    /// Holds each simulation until this many are in progress, to line up
    /// concurrent installs.
    simulation_barrier: Option<Arc<tokio::sync::Barrier>>,
}

#[derive(Clone)]
struct FakeChain(Arc<Mutex<Network_>>);

impl FakeChain {
    fn new(sequence: i64) -> Self {
        Self(Arc::new(Mutex::new(Network_ {
            sequence: Some(sequence),
            simulation_error: None,
            replies: std::collections::VecDeque::new(),
            include_on_send: false,
            included: HashMap::new(),
            sent: Vec::new(),
            simulation_barrier: None,
        })))
    }

    fn with<R>(&self, f: impl FnOnce(&mut Network_) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    fn sent(&self) -> Vec<TransactionEnvelope> {
        self.with(|n| n.sent.clone())
    }

    /// Records `hash` as included with `status`, and the source's sequence as
    /// consumed.
    fn include(&self, hash: [u8; 32], envelope: TransactionEnvelope, failed: bool) {
        self.with(|n| {
            let tx = Box::new(IncludedTransaction {
                ledger: 777,
                envelope,
                result: TransactionResult {
                    fee_charged: 1_234,
                    result: if failed {
                        TransactionResultResult::TxFailed(VecM::default())
                    } else {
                        TransactionResultResult::TxSuccess(VecM::default())
                    },
                    ext: TransactionResultExt::V0,
                },
                meta: None,
            });
            n.included.insert(
                hash,
                if failed { TransactionStatus::Failed(tx) } else { TransactionStatus::Success(tx) },
            );
            n.sequence = n.sequence.map(|s| s + 1);
        });
    }
}

fn server_error() -> RpcError {
    RpcError::Server { method: "test", code: -1, message: "connection reset".to_owned() }
}

fn outer_hash(envelope: &TransactionEnvelope) -> [u8; 32] {
    let TransactionEnvelope::TxFeeBump(bump) = envelope else { panic!("not a fee bump") };
    fee_bump_hash(&bump.tx, Network::Testnet).unwrap()
}

impl Chain for FakeChain {
    async fn account_sequence(&self, _: &AccountAddress) -> Result<Option<i64>, RpcError> {
        Ok(self.with(|n| n.sequence))
    }

    async fn simulate(&self, _: &TransactionEnvelope) -> Result<SimulationOutcome, RpcError> {
        if let Some(barrier) = self.with(|n| n.simulation_barrier.clone()) {
            barrier.wait().await;
        }
        Ok(match self.with(|n| n.simulation_error.clone()) {
            Some(error) => SimulationOutcome::Failed { error, latest_ledger: 1 },
            None => SimulationOutcome::Succeeded(Box::new(Simulation {
                transaction_data: SorobanTransactionData {
                    ext: SorobanTransactionDataExt::V0,
                    resources: SorobanResources {
                        footprint: LedgerFootprint {
                            read_only: VecM::default(),
                            read_write: VecM::default(),
                        },
                        instructions: 1,
                        disk_read_bytes: 0,
                        write_bytes: 0,
                    },
                    resource_fee: 0,
                },
                min_resource_fee: 10_000,
                auth: vec![],
                result: None,
                latest_ledger: 1,
            })),
        })
    }

    async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendOutcome, RpcError> {
        let hash = outer_hash(envelope);
        let (reply, include) = self.with(|n| {
            n.sent.push(envelope.clone());
            (n.replies.pop_front().unwrap_or(Reply::Pending), n.include_on_send)
        });
        if include && !self.with(|n| n.included.contains_key(&hash)) {
            self.include(hash, envelope.clone(), false);
        }
        match reply {
            Reply::Pending => Ok(SendOutcome::Pending { hash }),
            Reply::Refused => Ok(SendOutcome::Rejected {
                hash,
                result: Box::new(TransactionResult {
                    fee_charged: 0,
                    result: TransactionResultResult::TxBadSeq,
                    ext: TransactionResultExt::V0,
                }),
            }),
            Reply::Lost => Err(server_error()),
        }
    }

    async fn transaction(&self, hash: &[u8; 32]) -> Result<TransactionStatus, RpcError> {
        Ok(self.with(|n| n.included.get(hash).cloned()).unwrap_or(TransactionStatus::NotFound))
    }
}

// ---- controllable clock -------------------------------------------------

#[derive(Clone)]
struct ManualClock(Arc<Mutex<OffsetDateTime>>);

impl ManualClock {
    /// Starts on a whole second: envelope validity is stored in whole
    /// seconds, so window boundaries are then exact.
    fn new() -> Self {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        Self(Arc::new(Mutex::new(now)))
    }

    fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

// ---- harness ------------------------------------------------------------

const VALIDITY: Duration = Duration::from_secs(60);
const MARGIN: Duration = Duration::from_secs(30);

struct Harness {
    owner: PgPool,
    worker: PgPool,
    chain: FakeChain,
    clock: ManualClock,
    source_seed: String,
    fee_seed: String,
}

async fn harness(opts: PgPoolOptions, connect: PgConnectOptions) -> Harness {
    let owner = opts.max_connections(1).connect_with(connect.clone()).await.unwrap();
    // Independent of the master pool `#[sqlx::test]` hands out, which parallel
    // tests would otherwise exhaust.
    let worker = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET ROLE pay_stellar_worker").await?;
                Ok(())
            })
        })
        .connect_with(connect)
        .await
        .unwrap();
    Harness {
        owner,
        worker,
        chain: FakeChain::new(100),
        clock: ManualClock::new(),
        source_seed: SecretKey::generate().unwrap().to_strkey().to_string(),
        fee_seed: SecretKey::generate().unwrap().to_strkey().to_string(),
    }
}

impl Harness {
    /// A fresh engine over the same database and network, as after a restart.
    fn engine(&self) -> Engine<FakeChain, ManualClock> {
        Engine::new(
            self.worker.clone(),
            self.chain.clone(),
            self.clock.clone(),
            Network::Testnet,
            Keys {
                source: SecretKey::from_strkey(&self.source_seed).unwrap(),
                fee_source: SecretKey::from_strkey(&self.fee_seed).unwrap(),
            },
            Policy {
                inclusion_fee: 100,
                resource_fee_margin_percent: 15,
                validity: VALIDITY,
                ingestion_margin: MARGIN,
            },
        )
    }

    async fn stored_envelope(&self, id: Uuid) -> TransactionEnvelope {
        let xdr: String =
            sqlx::query_scalar("SELECT envelope_xdr FROM pay_stellar.submissions WHERE id = $1")
                .bind(id)
                .fetch_one(&self.owner)
                .await
                .unwrap();
        TransactionEnvelope::from_xdr_base64(xdr, Limits::none()).unwrap()
    }

    async fn state(&self, id: Uuid) -> (String, Option<String>) {
        sqlx::query_as("SELECT state, last_error FROM pay_stellar.submissions WHERE id = $1")
            .bind(id)
            .fetch_one(&self.owner)
            .await
            .unwrap()
    }
}

fn call() -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash([7; 32]))),
        function_name: ScSymbol("charge_batch".try_into().unwrap()),
        args: VecM::default(),
    })
}

fn closed_window() -> Duration {
    VALIDITY + MARGIN + Duration::from_secs(1)
}

// ---- installation -------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_install_records_the_envelope_without_sending_it(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let installed = h.engine().install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    let stored = h.stored_envelope(installed.id).await;
    assert_eq!(
        (h.chain.sent().len(), outer_hash(&stored), installed.sequence),
        (0, installed.outer_hash, 101)
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_second_install_while_one_is_in_flight_is_refused(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let first = h.engine().install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    let second = h.engine().install(Kind::Deposit, call(), vec![]).await;
    assert!(
        matches!(second, Err(EngineError::SourceBusy { id, .. }) if id == first.id),
        "{second:?}"
    );
}

fn call_named(function: &str) -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash([7; 32]))),
        function_name: ScSymbol(function.try_into().unwrap()),
        args: VecM::default(),
    })
}

/// Runs two installs whose in-flight checks both pass before either inserts,
/// so only the database can refuse one; returns (refused as busy, rows).
async fn race(h: &Harness, first: HostFunction, second: HostFunction) -> (usize, i64) {
    h.chain.with(|n| n.simulation_barrier = Some(Arc::new(tokio::sync::Barrier::new(2))));
    let (a, b) = (h.engine(), h.engine());
    let (ra, rb) = tokio::join!(
        a.install(Kind::ChargeBatch, first, vec![]),
        b.install(Kind::ChargeBatch, second, vec![])
    );
    let busy =
        [&ra, &rb].iter().filter(|r| matches!(r, Err(EngineError::SourceBusy { .. }))).count();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pay_stellar.submissions")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    (busy, rows)
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_installs_of_different_calls_leave_one_in_flight(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    assert_eq!(race(&h, call_named("deposit"), call_named("charge_batch")).await, (1, 1));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_concurrent_installs_of_the_same_call_leave_one_in_flight(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    assert_eq!(race(&h, call(), call()).await, (1, 1));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_install_refused_by_simulation_persists_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    h.chain.with(|n| n.simulation_error = Some("HostError: Error(Contract, #110)".to_owned()));
    let result = h.engine().install(Kind::ChargeBatch, call(), vec![]).await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pay_stellar.submissions")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    assert!(matches!(result, Err(EngineError::SimulationFailed(_))), "{result:?}");
    assert_eq!(rows, 0);
}

// ---- crash and lost-response recovery -----------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_recovery_after_crash_sends_the_installed_bytes(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    h.chain.with(|n| n.include_on_send = true);
    // The process stops after installing, before any broadcast.
    let installed = h.engine().install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    let recovered = h.engine().recover().await.unwrap();
    let sent = h.chain.sent();
    assert_eq!(
        (sent.len(), outer_hash(&sent[0]), recovered[0].1.state),
        (1, installed.outer_hash, State::Succeeded)
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_lost_send_response_resends_identical_bytes(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    h.chain.with(|n| {
        n.replies.extend([Reply::Lost, Reply::Pending]);
        n.include_on_send = true;
    });
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    let first = engine.broadcast(installed.id).await.unwrap();
    let second = engine.broadcast(installed.id).await.unwrap();
    let resolution = engine.resolve(installed.id).await.unwrap();
    let hashes: Vec<[u8; 32]> = h.chain.sent().iter().map(outer_hash).collect();
    assert_eq!(
        (first, second, hashes, resolution.state),
        (Broadcast::Unknown, Broadcast::Accepted, vec![installed.outer_hash; 2], State::Succeeded)
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_refused_resend_of_an_included_envelope_resolves_as_succeeded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    // The first send landed but its reply was lost; the resend is refused
    // because the sequence is already consumed.
    h.chain.with(|n| {
        n.replies.extend([Reply::Lost, Reply::Refused]);
        n.include_on_send = true;
    });
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    engine.broadcast(installed.id).await.unwrap();
    let refused = engine.broadcast(installed.id).await.unwrap();
    let resolution = engine.resolve(installed.id).await.unwrap();
    assert_eq!((refused, resolution.state), (Broadcast::Refused, State::Succeeded));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_refused_send_alone_does_not_fail_the_submission(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    h.chain.with(|n| n.replies.push_back(Reply::Refused));
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    engine.broadcast(installed.id).await.unwrap();
    let resolution = engine.resolve(installed.id).await.unwrap();
    let (state, error) = h.state(installed.id).await;
    assert_eq!((resolution.state, state.as_str()), (State::Installed, "installed"));
    assert!(error.unwrap().contains("send refused"));
}

// ---- outcome resolution -------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_unseen_envelope_within_its_window_stays_in_flight(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.clock.advance(VALIDITY + MARGIN);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Installed);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_unseen_envelope_after_its_window_with_unreached_sequence_expires(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.clock.advance(closed_window());
    let resolution = engine.resolve(installed.id).await.unwrap();
    // The source is free again, and the next envelope reuses the sequence
    // that was never consumed.
    let next = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    assert_eq!((resolution.state, next.sequence), (State::Expired, installed.sequence));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_unseen_envelope_whose_sequence_was_consumed_is_quarantined(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    // Something consumed the sequence, yet the envelope is not found.
    h.chain.with(|n| n.sequence = Some(installed.sequence));
    h.clock.advance(closed_window());
    let resolution = engine.resolve(installed.id).await.unwrap();
    let (_, error) = h.state(installed.id).await;
    assert_eq!(resolution.state, State::Quarantined);
    assert!(error.unwrap().contains("not found after its window"));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_failed_inclusion_is_recorded_with_its_fee(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.chain.include(installed.outer_hash, h.stored_envelope(installed.id).await, true);
    let resolution = engine.resolve(installed.id).await.unwrap();
    assert_eq!(
        (resolution.state, resolution.fee_charged, resolution.ledger),
        (State::Failed, Some(1_234), Some(777))
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_closed_window_sends_nothing(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.clock.advance(VALIDITY + Duration::from_secs(1));
    let broadcast = engine.broadcast(installed.id).await.unwrap();
    assert_eq!((broadcast, h.chain.sent().len()), (Broadcast::Skipped, 0));
}

// ---- final outcomes and stored bytes ------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_final_outcome_is_not_rewritten_by_a_later_answer(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.chain.include(installed.outer_hash, h.stored_envelope(installed.id).await, false);
    engine.resolve(installed.id).await.unwrap();
    // The node later forgets the transaction.
    h.chain.with(|n| n.included.clear());
    h.clock.advance(closed_window());
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Succeeded);
}

fn check_violation(error: &sqlx::Error) -> bool {
    error.as_database_error().and_then(|e| e.code()).as_deref() == Some("23514")
}

fn permission_denied(error: &sqlx::Error) -> bool {
    error.as_database_error().and_then(|e| e.code()).as_deref() == Some("42501")
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_worker_cannot_change_a_final_outcome(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.chain.include(installed.outer_hash, h.stored_envelope(installed.id).await, false);
    engine.resolve(installed.id).await.unwrap();
    let error = sqlx::query(
        "UPDATE pay_stellar.submissions SET state = 'expired', resolved_at = now() WHERE id = $1",
    )
    .bind(installed.id)
    .execute(&h.worker)
    .await
    .unwrap_err();
    assert!(
        check_violation(&error) && error.to_string().contains("is already succeeded"),
        "{error:?}"
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_worker_cannot_rewrite_installed_bytes(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let installed = h.engine().install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    let error =
        sqlx::query("UPDATE pay_stellar.submissions SET envelope_xdr = 'AAAA' WHERE id = $1")
            .bind(installed.id)
            .execute(&h.worker)
            .await
            .unwrap_err();
    assert!(permission_denied(&error), "{error:?}");
}
