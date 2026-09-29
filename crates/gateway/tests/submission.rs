//! Durable submission against real PostgreSQL, running as the worker role,
//! with a scripted network standing in for Stellar RPC so that lost
//! responses, crashes and ambiguous answers can be produced on demand.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::rpc::{
    FeeDistribution, FeePercentile, FeeStats, IncludedTransaction, LatestLedgerInfo, LedgerEntries,
    NodeView, RpcError, SendOutcome, Simulation, SimulationOutcome, TransactionStatus,
};
use fermah_pay_stellar_chain::soroban::fee_bump_hash;
use fermah_pay_stellar_chain::stellar_xdr::{
    ContractId, FeeBumpTransactionInnerTx, Hash, HostFunction, InvokeContractArgs, LedgerFootprint,
    LedgerKey, Limits, ReadXdr, ScAddress, ScSymbol, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, TransactionEnvelope, TransactionExt, TransactionResult,
    TransactionResultExt, TransactionResultResult, VecM,
};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::submission::{
    Broadcast, Chain, Clock, Engine, EngineError, FeePolicy, Keys, Kind, Policy, SourceSequence,
    State,
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
    /// The node's view: its latest close time trails the clock by this many
    /// seconds (a negative lag is a local clock running behind); its
    /// sequence reads trail its transaction lookups by `entries_behind`
    /// ledgers; its history starts at `oldest_close_time`.
    clock: Option<ManualClock>,
    close_lag_secs: i64,
    entries_behind: u32,
    oldest_close_time: i64,
    /// The 90th percentile of recent Soroban inclusion fees the node reports;
    /// `None` makes `getFeeStats` fail.
    market_p90: Option<u64>,
    /// Checks, at the moment of each send, that the envelope's hash was
    /// already committed to the submissions table.
    records: Option<PgPool>,
    unrecorded_sends: Vec<[u8; 32]>,
}

const NODE_LEDGER: u32 = 1_000;

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
            clock: None,
            close_lag_secs: 0,
            entries_behind: 0,
            oldest_close_time: 0,
            market_p90: Some(0),
            records: None,
            unrecorded_sends: Vec::new(),
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

/// A fee window whose 90th percentile is `p90`, with every other percentile
/// distinct from it, so a bid taken from the wrong one shows.
fn distribution(p90: u64) -> FeeDistribution {
    let below = p90 / 2;
    FeeDistribution {
        max: p90 * 4 + 3,
        min: below,
        mode: below,
        p10: below,
        p20: below,
        p30: below,
        p40: below,
        p50: below,
        p60: below,
        p70: below,
        p80: below,
        p90,
        p95: p90 * 2 + 1,
        p99: p90 * 3 + 2,
        transaction_count: 50,
        ledger_count: 50,
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
    async fn account_sequence(&self, _: &AccountAddress) -> Result<SourceSequence, RpcError> {
        Ok(self.with(|n| SourceSequence {
            sequence: n.sequence,
            latest_ledger: NODE_LEDGER - n.entries_behind,
        }))
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
        if let Some(records) = self.with(|n| n.records.clone()) {
            let recorded: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pay_stellar.submissions WHERE outer_hash = $1",
            )
            .bind(hash.as_slice())
            .fetch_one(&records)
            .await
            .unwrap();
            if recorded != 1 {
                self.with(|n| n.unrecorded_sends.push(hash));
            }
        }
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
        Ok(self.with(|n| {
            n.included.get(hash).cloned().unwrap_or_else(|| {
                let now = n.clock.as_ref().expect("harness sets the clock").now();
                TransactionStatus::NotFound {
                    node: NodeView {
                        latest_ledger: NODE_LEDGER,
                        latest_close_time: now.unix_timestamp() - n.close_lag_secs,
                        oldest_close_time: n.oldest_close_time,
                    },
                }
            })
        }))
    }

    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        Ok(1)
    }

    async fn latest_ledger_info(&self) -> Result<LatestLedgerInfo, RpcError> {
        Ok(self.with(|n| {
            let now = n.clock.as_ref().expect("harness sets the clock").now();
            LatestLedgerInfo {
                sequence: NODE_LEDGER,
                close_time: now.unix_timestamp() - n.close_lag_secs,
                protocol_version: 28,
            }
        }))
    }

    async fn fee_stats(&self) -> Result<FeeStats, RpcError> {
        let p90 = self.with(|n| n.market_p90).ok_or_else(server_error)?;
        Ok(FeeStats {
            soroban_inclusion_fee: distribution(p90),
            inclusion_fee: distribution(100),
            latest_ledger: NODE_LEDGER,
        })
    }

    async fn ledger_entries(&self, _: &[LedgerKey]) -> Result<LedgerEntries, RpcError> {
        Ok(LedgerEntries { entries: Vec::new(), latest_ledger: NODE_LEDGER })
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
const MAX_SKEW: Duration = Duration::from_secs(20);
/// Floor and cap of the harness engine's bids.
const FLOOR: u32 = 100;
const CAP: u32 = 100_000;
/// The resource fee the fake simulation asks for, with the engine's 15%
/// margin: what every envelope pays besides its inclusion bid.
const RESOURCE_FEE: i64 = 11_500;

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
    let clock = ManualClock::new();
    let chain = FakeChain::new(100);
    let records = owner.clone();
    chain.with(|n| {
        n.clock = Some(clock.clone());
        n.records = Some(records);
    });
    Harness {
        owner,
        worker,
        chain,
        clock,
        source_seed: SecretKey::generate().unwrap().to_strkey().to_string(),
        fee_seed: SecretKey::generate().unwrap().to_strkey().to_string(),
    }
}

impl Harness {
    /// A fresh engine over the same database and network, as after a restart.
    fn engine(&self) -> Engine<FakeChain, ManualClock> {
        self.engine_with(FeePolicy::new(FLOOR, CAP, FeePercentile::P90).unwrap())
    }

    fn engine_with(&self, fees: FeePolicy) -> Engine<FakeChain, ManualClock> {
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
                fees,
                resource_fee_margin_percent: 15,
                validity: VALIDITY,
                max_clock_skew: MAX_SKEW,
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
    VALIDITY + Duration::from_secs(1)
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
    // Boundary: a ledger closing exactly at the upper time bound may still
    // include the envelope.
    h.clock.advance(VALIDITY);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Installed);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_node_lagging_behind_the_window_cannot_expire_an_envelope(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    // Locally the window closed long ago, but the node has not ingested a
    // ledger past it: its "not found" and its sequence are both stale.
    h.clock.advance(closed_window() + Duration::from_secs(600));
    h.chain.with(|n| n.close_lag_secs = 700);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Installed);
    h.chain.with(|n| n.close_lag_secs = 0);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Expired);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_sequence_read_behind_the_node_that_missed_the_envelope_proves_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.clock.advance(closed_window());
    h.chain.with(|n| n.entries_behind = 1);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Installed);
    let (_, note) = h.state(installed.id).await;
    assert!(note.unwrap().contains("is behind ledger"));
    h.chain.with(|n| n.entries_behind = 0);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Expired);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_node_without_history_back_to_the_recording_cannot_prove_expiry(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.clock.advance(closed_window());
    // The node retains nothing older than a point after the envelope was
    // recorded, so an inclusion before that would be invisible to it.
    let after_recording = h.clock.now().unix_timestamp();
    h.chain.with(|n| n.oldest_close_time = after_recording);
    let resolution = engine.resolve(installed.id).await.unwrap();
    let (_, error) = h.state(installed.id).await;
    assert_eq!(resolution.state, State::Quarantined);
    assert!(error.unwrap().contains("history starts after"));
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

// ---- inclusion bids -----------------------------------------------------

/// The inclusion bid per operation `envelope` carries, read from its fee
/// fields: the outer fee less the inner resource fee, over the inner
/// operation and the fee bump itself.
fn bid_of(envelope: &TransactionEnvelope) -> i64 {
    let TransactionEnvelope::TxFeeBump(bump) = envelope else { panic!("not a fee bump") };
    let FeeBumpTransactionInnerTx::Tx(inner) = &bump.tx.inner_tx;
    let TransactionExt::V1(data) = &inner.tx.ext else { panic!("not assembled") };
    assert_eq!(data.resource_fee, RESOURCE_FEE);
    let bid = (bump.tx.fee - RESOURCE_FEE) / 2;
    assert_eq!((bump.tx.fee - RESOURCE_FEE) % 2, 0);
    // The inner transaction bids the same per operation.
    assert_eq!(i64::from(inner.tx.fee) - RESOURCE_FEE, bid);
    bid
}

impl Harness {
    /// Installs an envelope and lets it be included, freeing the source;
    /// returns its bid.
    async fn install_included(&self, engine: &Engine<FakeChain, ManualClock>) -> i64 {
        let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
        let envelope = self.stored_envelope(installed.id).await;
        self.chain.include(installed.outer_hash, envelope.clone(), false);
        assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Succeeded);
        bid_of(&envelope)
    }

    /// Installs and sends an envelope that is never included, then proves
    /// it expired; returns it.
    async fn install_expired(
        &self,
        engine: &Engine<FakeChain, ManualClock>,
    ) -> (fermah_pay_stellar_gateway::submission::Installed, TransactionEnvelope) {
        let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
        assert_eq!(engine.broadcast(installed.id).await.unwrap(), Broadcast::Accepted);
        self.clock.advance(closed_window());
        assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Expired);
        let envelope = self.stored_envelope(installed.id).await;
        (installed, envelope)
    }

    fn unrecorded_sends(&self) -> Vec<[u8; 32]> {
        self.chain.with(|n| n.unrecorded_sends.clone())
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_bid_follows_the_market_between_floor_and_cap(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let mut bids = Vec::new();
    // A quiet window, a busy one, one above the cap, and an unreadable one.
    for market in [Some(0), Some(5_000), Some(u64::from(CAP) + 1), None] {
        h.chain.with(|n| n.market_p90 = market);
        bids.push(h.install_included(&engine).await);
    }
    assert_eq!(bids, [i64::from(FLOOR), 5_000, i64::from(CAP), i64::from(FLOOR)]);
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_bid_reads_the_configured_percentile(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = harness(opts, connect).await;
    h.chain.with(|n| n.market_p90 = Some(5_000));
    let p99 = h.engine_with(FeePolicy::new(FLOOR, CAP, FeePercentile::P99).unwrap());
    let p50 = h.engine_with(FeePolicy::new(FLOOR, CAP, FeePercentile::P50).unwrap());
    // `distribution` puts p99 at 3 * p90 + 2 and p50 at p90 / 2.
    assert_eq!((h.install_included(&p99).await, h.install_included(&p50).await), (15_002, 2_500));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_envelope_rebuilt_after_expiry_doubles_its_bid_up_to_the_cap(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine_with(FeePolicy::new(FLOOR, 400, FeePercentile::P90).unwrap());
    let mut bids = Vec::new();
    let mut sequences = Vec::new();
    for _ in 0..4 {
        let (installed, envelope) = h.install_expired(&engine).await;
        bids.push(bid_of(&envelope));
        sequences.push(installed.sequence);
    }
    // Still at the cap when one finally lands; the next bids from the
    // market again.
    bids.push(h.install_included(&engine).await);
    bids.push(h.install_included(&engine).await);
    assert_eq!(bids, [100, 200, 400, 400, 400, 100]);
    // Each rebuild reused the sequence the expired envelope never consumed.
    assert_eq!(sequences, [101; 4]);
    assert_eq!(h.unrecorded_sends(), Vec::<[u8; 32]>::new());
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_quarantined_or_failed_envelope_does_not_raise_the_next_bid(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    // Failed: included, so its bid was high enough.
    let failed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.chain.include(failed.outer_hash, h.stored_envelope(failed.id).await, true);
    assert_eq!(engine.resolve(failed.id).await.unwrap().state, State::Failed);
    let after_failed = h.install_included(&engine).await;
    // Quarantined: its fate is unknown, not a verdict on its bid.
    let quarantined = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.chain.with(|n| n.sequence = Some(quarantined.sequence));
    h.clock.advance(closed_window());
    assert_eq!(engine.resolve(quarantined.id).await.unwrap().state, State::Quarantined);
    let after_quarantine = h.install_included(&engine).await;
    assert_eq!((after_failed, after_quarantine), (i64::from(FLOOR), i64::from(FLOOR)));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_rebuilt_envelope_is_sent_only_after_it_is_recorded(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let (expired, _) = h.install_expired(&engine).await;
    let prepared = engine.prepare(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    // Built but not recorded: nothing knows of it, so nothing sends it.
    assert_eq!(engine.recover().await.unwrap(), Vec::new());
    assert_eq!(h.chain.sent().len(), 1);
    let mut conn = h.worker.acquire().await.unwrap();
    let installed = engine.record(&mut conn, &prepared).await.unwrap();
    drop(conn);
    assert_eq!(engine.broadcast(installed.id).await.unwrap(), Broadcast::Accepted);
    let sent: Vec<[u8; 32]> = h.chain.sent().iter().map(outer_hash).collect();
    assert_eq!(sent, [expired.outer_hash, installed.outer_hash]);
    assert_eq!(h.unrecorded_sends(), Vec::<[u8; 32]>::new());
    assert_eq!(
        (installed.sequence, bid_of(&h.stored_envelope(installed.id).await)),
        (expired.sequence, 2 * i64::from(FLOOR))
    );
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_envelope_in_flight_is_resent_unchanged_whatever_the_market_does(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    engine.broadcast(installed.id).await.unwrap();
    h.chain.with(|n| n.market_p90 = Some(50_000));
    engine.recover().await.unwrap();
    let sent: Vec<[u8; 32]> = h.chain.sent().iter().map(outer_hash).collect();
    assert_eq!(sent, [installed.outer_hash; 2]);
}

// ---- local clock --------------------------------------------------------

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_clock_skew_beyond_the_bound_builds_nothing(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    // Local clock ahead of the latest close by 21s, then behind it by 21s.
    for lag in [21, -21] {
        h.chain.with(|n| n.close_lag_secs = lag);
        let now = h.clock.now().unix_timestamp();
        let refused = engine.install(Kind::ChargeBatch, call(), vec![]).await;
        assert!(
            matches!(
                refused,
                Err(EngineError::ClockSkew { local, ledger_close, bound_secs: 20, .. })
                    if local == now && ledger_close == now - lag
            ),
            "{refused:?}"
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pay_stellar.submissions")
        .fetch_one(&h.owner)
        .await
        .unwrap();
    assert_eq!((rows, h.chain.sent().len()), (0, 0));
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_clock_skew_within_the_bound_builds(opts: PgPoolOptions, connect: PgConnectOptions) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    for lag in [20, -20] {
        h.chain.with(|n| n.close_lag_secs = lag);
        h.install_included(&engine).await;
    }
}

#[sqlx::test(migrations = "../../db/migrations")]
async fn test_clock_skew_does_not_hold_back_an_expiry_proof(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let h = harness(opts, connect).await;
    let engine = h.engine();
    let installed = engine.install(Kind::ChargeBatch, call(), vec![]).await.unwrap();
    h.clock.advance(closed_window());
    // The network is now 30s ahead of the local clock: too far to build
    // on, but its close times alone decide the outcome of what was built.
    h.chain.with(|n| n.close_lag_secs = -30);
    assert_eq!(engine.resolve(installed.id).await.unwrap().state, State::Expired);
    let refused = engine.install(Kind::ChargeBatch, call(), vec![]).await;
    assert!(matches!(refused, Err(EngineError::ClockSkew { .. })), "{refused:?}");
}
