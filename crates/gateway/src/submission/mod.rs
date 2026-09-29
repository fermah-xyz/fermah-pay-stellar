//! Durable transaction submission.
//!
//! A transaction is built, signed and wrapped in its fee bump, and the
//! complete envelope is written to the database, before it is ever sent.
//! After that, only those bytes are sent, any number of times, and the
//! outcome is established only through their hash:
//!
//! - an acknowledgement or error from `sendTransaction` never settles the
//!   outcome: a resend of an envelope that already landed is refused, so a
//!   refusal says nothing about whether the first send was included;
//! - an envelope is `expired` only when the RPC node has ingested a ledger that
//!   closed after the envelope's upper time bound, still does not find it,
//!   retains history back to when the envelope was recorded, and reads the
//!   source account's sequence below the envelope's at that ledger or later:
//!   together these prove it was never included and never can be. The local
//!   clock proves nothing here, because the node may lag behind the network;
//! - if the sequence was reached but the envelope is not found, the evidence
//!   contradicts itself and the submission is `quarantined` for an operator.
//!
//! One source account has at most one envelope in flight (enforced by a
//! unique index), because the network accepts only the next sequence number.
//!
//! An envelope's fee and time bounds are fixed when it is built. Its
//! inclusion bid follows the recent market and is raised after an envelope
//! expired unincluded ([`fees`]). Its upper time bound comes from the local
//! clock, so no envelope is built while that clock disagrees with the latest
//! ledger's close time by more than the policy allows; that check only
//! gates building, and proves nothing about any envelope's outcome.

pub mod chain;
pub mod fees;

use std::time::Duration;

use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::rpc::{
    RpcError, SendOutcome, SimulationOutcome, TransactionStatus, hex_lower,
};
use fermah_pay_stellar_chain::soroban::{self, AssemblyError};
use fermah_pay_stellar_chain::stellar_xdr::{
    FeeBumpTransactionInnerTx, HostFunction, Limits, OperationBody, ReadXdr, ScVal,
    SorobanAuthorizationEntry, SorobanCredentials, SorobanTransactionData, Transaction,
    TransactionEnvelope, TransactionV1Envelope, VecM, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::{self, SigningError};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

pub use chain::{Chain, SourceSequence};
pub use fees::{Bid, FeePolicy, FeePolicyError};

/// Where the engine reads the current time; tests control it to reach the
/// end of a validity window without waiting.
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Deposit,
    ChargeBatch,
    Withdrawal,
    Restore,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
            Self::ChargeBatch => "charge_batch",
            Self::Withdrawal => "withdrawal",
            Self::Restore => "restore",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Installed,
    Succeeded,
    Failed,
    Expired,
    Quarantined,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Quarantined => "quarantined",
        }
    }

    fn parse(raw: &str) -> Result<Self, EngineError> {
        Ok(match raw {
            "installed" => Self::Installed,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            "quarantined" => Self::Quarantined,
            _ => return Err(EngineError::Corrupt("submission state outside the CHECK constraint")),
        })
    }

    #[must_use]
    pub const fn is_final(self) -> bool {
        !matches!(self, Self::Installed)
    }
}

/// The accounts the engine signs with.
pub struct Keys {
    /// Sign and sequence transactions. Each has at most one envelope in
    /// flight, so several let one stuck envelope leave the others sending.
    sources: Vec<SecretKey>,
    /// Signs the fee bump and pays.
    pub fee_source: SecretKey,
}

#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    #[error("at least one source account is needed")]
    NoSource,
    #[error("source account {0} is listed twice")]
    Duplicate(AccountAddress),
}

impl Keys {
    /// No two engines, in this process or another, may share a source: each
    /// assumes it alone sequences the accounts it holds.
    pub fn new(sources: Vec<SecretKey>, fee_source: SecretKey) -> Result<Self, KeysError> {
        if sources.is_empty() {
            return Err(KeysError::NoSource);
        }
        for (i, key) in sources.iter().enumerate() {
            if sources[..i].iter().any(|earlier| earlier.address() == key.address()) {
                return Err(KeysError::Duplicate(key.address()));
            }
        }
        Ok(Self { sources, fee_source })
    }

    #[must_use]
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn source_addresses(&self) -> Vec<AccountAddress> {
        self.sources.iter().map(SecretKey::address).collect()
    }

    fn source(&self, address: &AccountAddress) -> Option<&SecretKey> {
        self.sources.iter().find(|key| key.address() == *address)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// How each envelope's inclusion bid is chosen.
    pub fees: FeePolicy,
    /// Headroom over the simulated resource fee; unused fee is refunded.
    pub resource_fee_margin_percent: u8,
    /// How long an envelope may be included after it is built.
    pub validity: Duration,
    /// Largest difference between the local clock and the latest ledger's
    /// close time at which envelopes are still built. Keep it well below
    /// `validity`: a clock that far behind builds envelopes whose window has
    /// already closed on the network.
    pub max_clock_skew: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("source {account} already has submission {id} in flight")]
    SourceBusy { account: AccountAddress, id: Uuid },
    #[error("every source account has an envelope in flight")]
    NoFreeSource,
    #[error("source account {0} does not exist")]
    SourceMissing(AccountAddress),
    #[error("simulation refused the transaction: {0}")]
    SimulationFailed(String),
    #[error("archived ledger state must be restored before this call")]
    RestoreRequired(Box<Restore>),
    #[error("assembling the transaction")]
    Assembly(#[source] AssemblyError),
    #[error("signing the transaction")]
    Signing(#[source] SigningError),
    #[error("network request failed before anything was installed")]
    Chain(#[source] RpcError),
    #[error(
        "local clock reads {local}, but ledger {ledger} closed at {ledger_close}: \
         more than {bound_secs}s apart, so no envelope is built until they agree"
    )]
    ClockSkew { local: i64, ledger: u32, ledger_close: i64, bound_secs: u64 },
    #[error("database operation `{operation}` failed")]
    Store {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("no submission {0}")]
    UnknownSubmission(Uuid),
    #[error("stored submission violates an invariant: {0}")]
    Corrupt(&'static str),
}

/// What a simulation that needs archived state restored returned: the
/// footprint to restore and its resource fee.
#[derive(Clone, Debug)]
pub struct Restore {
    pub transaction_data: SorobanTransactionData,
    pub min_resource_fee: i64,
}

struct Slot {
    source: AccountAddress,
    sequence: i64,
    valid_until: OffsetDateTime,
    valid_until_unix: u64,
    /// Inclusion bid per operation, in stroops.
    inclusion_fee: u32,
}

/// A signed, fee-bumped envelope not yet recorded.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub id: Uuid,
    pub kind: Kind,
    pub source: AccountAddress,
    pub fee_source: AccountAddress,
    pub sequence: i64,
    pub valid_until: OffsetDateTime,
    pub inner_hash: [u8; 32],
    pub outer_hash: [u8; 32],
    envelope_xdr: String,
    /// See [`Prepared::authorized_from`].
    authorized_from_ledger: Option<u32>,
}

impl Prepared {
    /// Records `ledger` as the latest ledger known when the authorizations
    /// this envelope carries were created: none of them can be included in
    /// an earlier ledger, which bounds where their effects are searched for.
    #[must_use]
    pub const fn authorized_from(mut self, ledger: u32) -> Self {
        self.authorized_from_ledger = Some(ledger);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub id: Uuid,
    pub sequence: i64,
    pub inner_hash: [u8; 32],
    pub outer_hash: [u8; 32],
    pub valid_until: OffsetDateTime,
}

/// What a send attempt returned. None of these settle the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Broadcast {
    Accepted,
    RetryLater,
    /// The node refused the bytes; they may still have landed earlier.
    Refused,
    /// The request failed; the bytes may or may not have reached the network.
    Unknown,
    /// Nothing to send: the submission is final or its window has closed.
    Skipped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub state: State,
    pub ledger: Option<i32>,
    pub fee_charged: Option<i64>,
    pub return_value: Option<ScVal>,
}

struct Row {
    state: State,
    source: AccountAddress,
    sequence: i64,
    valid_until: OffsetDateTime,
    created_at: OffsetDateTime,
    outer_hash: [u8; 32],
    envelope_xdr: String,
    ledger: Option<i32>,
    fee_charged: Option<i64>,
    return_value_xdr: Option<String>,
}

pub struct Engine<C, K = SystemClock> {
    pool: PgPool,
    chain: C,
    clock: K,
    network: Network,
    keys: Keys,
    policy: Policy,
}

fn store(operation: &'static str) -> impl FnOnce(sqlx::Error) -> EngineError {
    move |source| EngineError::Store { operation, source }
}

fn hash32(bytes: Vec<u8>) -> Result<[u8; 32], EngineError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| EngineError::Corrupt("hash is not 32 bytes"))
}

const IN_FLIGHT_INDEX: &str = "submissions_one_in_flight_per_source";
/// Two concurrent installs of the same call in the same second build
/// byte-identical envelopes, which collide on the hash before the in-flight
/// index; either way, the other install won.
const OUTER_HASH_KEY: &str = "submissions_outer_hash_key";

impl<C: Chain, K: Clock> Engine<C, K> {
    pub const fn new(
        pool: PgPool,
        chain: C,
        clock: K,
        network: Network,
        keys: Keys,
        policy: Policy,
    ) -> Self {
        Self { pool, chain, clock, network, keys, policy }
    }

    pub const fn chain(&self) -> &C {
        &self.chain
    }

    pub const fn clock(&self) -> &K {
        &self.clock
    }

    pub const fn network(&self) -> Network {
        self.network
    }

    /// Builds, signs and fee-bumps a transaction invoking `function` with the
    /// already-signed `auth` entries, and records the envelope. Nothing is
    /// sent: the caller broadcasts the installed submission afterwards, and a
    /// crash in between leaves bytes that recovery resends unchanged.
    pub async fn install(
        &self,
        kind: Kind,
        function: HostFunction,
        auth: Vec<SorobanAuthorizationEntry>,
    ) -> Result<Installed, EngineError> {
        let prepared = self.prepare(kind, function, auth).await?;
        let mut conn = self.pool.acquire().await.map_err(store("acquire connection"))?;
        self.record(&mut conn, &prepared).await
    }

    /// The network-facing half of [`Self::install`]: builds and signs the
    /// envelope without touching the database, so a caller can then record it
    /// in the same database transaction that links it to the business rows it
    /// settles. No database lock is held while this talks to the network.
    pub async fn prepare(
        &self,
        kind: Kind,
        function: HostFunction,
        auth: Vec<SorobanAuthorizationEntry>,
    ) -> Result<Prepared, EngineError> {
        let slot = self.next_slot(kind).await?;
        let unassembled = soroban::invocation_transaction(
            &slot.source,
            slot.sequence,
            function,
            auth,
            slot.inclusion_fee,
            slot.valid_until_unix,
        )
        .map_err(EngineError::Assembly)?;
        let for_simulation = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: unassembled.clone(),
            signatures: VecM::default(),
        });
        let simulation =
            match self.chain.simulate(&for_simulation).await.map_err(EngineError::Chain)? {
                SimulationOutcome::Succeeded(simulation) => *simulation,
                SimulationOutcome::Failed { error, .. } => {
                    return Err(EngineError::SimulationFailed(error));
                }
                SimulationOutcome::RestoreRequired { transaction_data, min_resource_fee } => {
                    return Err(EngineError::RestoreRequired(Box::new(Restore {
                        transaction_data: *transaction_data,
                        min_resource_fee,
                    })));
                }
            };
        let tx = soroban::assemble(
            unassembled,
            simulation.transaction_data,
            simulation.min_resource_fee,
            self.policy.resource_fee_margin_percent,
        )
        .map_err(EngineError::Assembly)?;
        self.seal(kind, slot, tx)
    }

    /// Builds the transaction that restores the archived entries a refused
    /// simulation named, with the resources that simulation returned. It is
    /// recorded and sent like any other submission.
    pub async fn prepare_restore(&self, restore: &Restore) -> Result<Prepared, EngineError> {
        let slot = self.next_slot(Kind::Restore).await?;
        let unassembled = soroban::restore_transaction(
            &slot.source,
            slot.sequence,
            slot.inclusion_fee,
            slot.valid_until_unix,
        );
        let tx = soroban::assemble(
            unassembled,
            restore.transaction_data.clone(),
            restore.min_resource_fee,
            self.policy.resource_fee_margin_percent,
        )
        .map_err(EngineError::Assembly)?;
        self.seal(Kind::Restore, slot, tx)
    }

    async fn next_slot(&self, kind: Kind) -> Result<Slot, EngineError> {
        let source = self.free_source().await?;
        self.check_clock().await?;
        let current = self
            .chain
            .account_sequence(&source)
            .await
            .map_err(EngineError::Chain)?
            .sequence
            .ok_or_else(|| EngineError::SourceMissing(source.clone()))?;
        let valid_until_unix = self
            .clock
            .now()
            .unix_timestamp()
            .saturating_add(i64::try_from(self.policy.validity.as_secs()).unwrap_or(i64::MAX));
        let valid_until = OffsetDateTime::from_unix_timestamp(valid_until_unix)
            .map_err(|_| EngineError::Corrupt("validity window beyond the representable range"))?;
        let inclusion_fee = self.inclusion_bid(kind, &source, current + 1).await?;
        Ok(Slot {
            source,
            sequence: current + 1,
            valid_until,
            valid_until_unix: u64::try_from(valid_until_unix).unwrap_or(0),
            inclusion_fee,
        })
    }

    /// Refuses to go on while the local clock, which sets the envelope's
    /// upper time bound, is further from the latest ledger's close time than
    /// the policy allows. A node lagging behind the network looks the same
    /// as a local clock running ahead; either way nothing is built until the
    /// two agree again.
    async fn check_clock(&self) -> Result<(), EngineError> {
        let ledger = self.chain.latest_ledger_info().await.map_err(EngineError::Chain)?;
        let local = self.clock.now().unix_timestamp();
        let bound_secs = self.policy.max_clock_skew.as_secs();
        if local.abs_diff(ledger.close_time) > bound_secs {
            return Err(EngineError::ClockSkew {
                local,
                ledger: ledger.sequence,
                ledger_close: ledger.close_time,
                bound_secs,
            });
        }
        Ok(())
    }

    /// The inclusion bid for the next envelope from `source`, logged with
    /// what it was chosen from. An unreadable fee market is not an error:
    /// the bid then rests on the floor and the escalation alone.
    async fn inclusion_bid(
        &self,
        kind: Kind,
        source: &AccountAddress,
        sequence: i64,
    ) -> Result<u32, EngineError> {
        let fees = self.policy.fees;
        let market = match self.chain.fee_stats().await {
            Ok(stats) => Some(stats.soroban_inclusion_fee.at(fees.percentile)),
            Err(error) => {
                tracing::warn!(error = %error, "fee statistics unavailable; bidding without the market");
                None
            }
        };
        let bid = fees.bid(market, self.expired_bid(source).await?);
        tracing::info!(
            kind = kind.as_str(),
            source = %source,
            sequence,
            bid = bid.stroops,
            market = ?bid.market,
            percentile = %fees.percentile,
            escalated_from = ?bid.escalated_from,
            floor = fees.floor,
            cap = fees.cap,
            "inclusion bid per operation"
        );
        if bid.capped() {
            tracing::warn!(
                wanted = bid.wanted,
                cap = fees.cap,
                "inclusion bid held at the cap; the envelope may not be included while fees stay this high"
            );
        }
        Ok(bid.stroops)
    }

    /// The bid of `source`'s latest envelope, if that envelope expired
    /// without being included. Identifiers are time-ordered (UUIDv7), and a
    /// source's next envelope is built only once the previous one is final,
    /// so the highest identifier is the latest envelope; reading it walks the
    /// primary key backwards instead of scanning the source's history.
    async fn expired_bid(&self, source: &AccountAddress) -> Result<Option<u32>, EngineError> {
        let latest = sqlx::query!(
            r#"
            SELECT state, envelope_xdr FROM pay_stellar.submissions
            WHERE network = $1 AND source_address = $2
            ORDER BY id DESC
            LIMIT 1
            "#,
            self.network.caip2(),
            source.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read the latest submission"))?;
        let Some(latest) = latest.filter(|row| row.state == State::Expired.as_str()) else {
            return Ok(None);
        };
        match TransactionEnvelope::from_xdr_base64(&latest.envelope_xdr, Limits::none()) {
            Ok(TransactionEnvelope::TxFeeBump(bump)) => soroban::fee_bump_inclusion_fee(&bump.tx)
                .map(Some)
                .ok_or(EngineError::Corrupt("expired envelope's fee is not one the engine builds")),
            _ => Err(EngineError::Corrupt("installed envelope does not decode")),
        }
    }

    /// The first source with no envelope in flight. Callers in one process
    /// prepare one envelope at a time, and `record` refuses a second envelope
    /// for a source in any case.
    async fn free_source(&self) -> Result<AccountAddress, EngineError> {
        for key in &self.keys.sources {
            let source = key.address();
            if self.in_flight(&source).await?.is_none() {
                return Ok(source);
            }
        }
        Err(EngineError::NoFreeSource)
    }

    fn seal(&self, kind: Kind, slot: Slot, tx: Transaction) -> Result<Prepared, EngineError> {
        let inclusion_fee = slot.inclusion_fee;
        let source_key = self
            .keys
            .source(&slot.source)
            .ok_or(EngineError::Corrupt("slot names a source this engine does not hold"))?;
        let inner_hash =
            transaction::transaction_hash(&tx, self.network).map_err(EngineError::Signing)?;
        let TransactionEnvelope::Tx(inner) =
            transaction::sign(tx, self.network, &[source_key]).map_err(EngineError::Signing)?
        else {
            unreachable!("transaction::sign produces a v1 envelope")
        };
        let bump = soroban::fee_bump(inner, &self.keys.fee_source.address(), inclusion_fee)
            .map_err(EngineError::Assembly)?;
        let outer_hash =
            soroban::fee_bump_hash(&bump, self.network).map_err(EngineError::Assembly)?;
        let envelope = soroban::sign_fee_bump(bump, self.network, &self.keys.fee_source)
            .map_err(EngineError::Assembly)?;
        let envelope_xdr = envelope
            .to_xdr_base64(Limits::none())
            .map_err(|e| EngineError::Assembly(AssemblyError::Encode(e.to_string())))?;
        Ok(Prepared {
            id: Uuid::now_v7(),
            kind,
            source: slot.source,
            fee_source: self.keys.fee_source.address(),
            sequence: slot.sequence,
            valid_until: slot.valid_until,
            inner_hash,
            outer_hash,
            envelope_xdr,
            authorized_from_ledger: None,
        })
    }

    /// Records a prepared envelope as in flight, on `conn`, which may be inside
    /// the caller's transaction. On `SourceBusy` that transaction is aborted
    /// and must be rolled back.
    pub async fn record(
        &self,
        conn: &mut sqlx::PgConnection,
        prepared: &Prepared,
    ) -> Result<Installed, EngineError> {
        let Prepared {
            id,
            kind,
            ref source,
            ref fee_source,
            sequence,
            valid_until,
            inner_hash,
            outer_hash,
            ref envelope_xdr,
            authorized_from_ledger,
        } = *prepared;
        let authorized_from_ledger = authorized_from_ledger
            .map(i32::try_from)
            .transpose()
            .map_err(|_| EngineError::Corrupt("ledger beyond the INTEGER column"))?;
        let inserted = sqlx::query!(
            r#"
            INSERT INTO pay_stellar.submissions
                (id, network, kind, state, source_address, fee_source_address, sequence,
                 valid_until, inner_hash, outer_hash, envelope_xdr, authorized_from_ledger)
            VALUES ($1, $2, $3, 'installed', $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
            id,
            self.network.caip2(),
            kind.as_str(),
            source.as_str(),
            fee_source.as_str(),
            sequence,
            valid_until,
            inner_hash.as_slice(),
            outer_hash.as_slice(),
            envelope_xdr,
            authorized_from_ledger,
        )
        .execute(&mut *conn)
        .await;
        match inserted {
            Ok(_) => Ok(Installed { id, sequence, inner_hash, outer_hash, valid_until }),
            Err(sqlx::Error::Database(db))
                if matches!(db.constraint(), Some(IN_FLIGHT_INDEX | OUTER_HASH_KEY)) =>
            {
                // Another installer won the race between the check above and
                // this insert; its envelope is the one in flight.
                let id = self.in_flight(source).await?.unwrap_or(id);
                Err(EngineError::SourceBusy { account: source.clone(), id })
            }
            Err(source) => Err(EngineError::Store { operation: "install submission", source }),
        }
    }

    /// Sends the installed bytes. The answer is recorded for diagnosis but
    /// never changes the submission's state.
    pub async fn broadcast(&self, id: Uuid) -> Result<Broadcast, EngineError> {
        let row = self.load(id).await?;
        if row.state.is_final() || self.clock.now() > row.valid_until {
            return Ok(Broadcast::Skipped);
        }
        let envelope = TransactionEnvelope::from_xdr_base64(&row.envelope_xdr, Limits::none())
            .map_err(|_| EngineError::Corrupt("installed envelope does not decode"))?;
        match self.chain.send(&envelope).await {
            Ok(SendOutcome::Pending { .. } | SendOutcome::Duplicate { .. }) => {
                Ok(Broadcast::Accepted)
            }
            Ok(SendOutcome::TryAgainLater { .. }) => Ok(Broadcast::RetryLater),
            Ok(SendOutcome::Rejected { result, .. }) => {
                self.note(id, &format!("send refused: {:?}", result.result)).await?;
                Ok(Broadcast::Refused)
            }
            Err(error) => {
                self.note(id, &format!("send failed: {error}")).await?;
                Ok(Broadcast::Unknown)
            }
        }
    }

    /// Establishes the outcome, if the evidence allows, and records it.
    pub async fn resolve(&self, id: Uuid) -> Result<Resolution, EngineError> {
        let row = self.load(id).await?;
        if row.state.is_final() {
            return resolution_of(&row);
        }
        let status = match self.chain.transaction(&row.outer_hash).await {
            Ok(status) => status,
            Err(error) => {
                self.note(id, &format!("status query failed: {error}")).await?;
                return resolution_of(&row);
            }
        };
        match status {
            TransactionStatus::Success(tx) => self.finish(id, State::Succeeded, &tx).await,
            TransactionStatus::Failed(tx) => self.finish(id, State::Failed, &tx).await,
            TransactionStatus::NotFound { node } => {
                if node.latest_close_time <= row.valid_until.unix_timestamp() {
                    return resolution_of(&row);
                }
                if node.oldest_close_time > row.created_at.unix_timestamp() {
                    let reason = format!(
                        "envelope {} not found, but the node's history starts after it was recorded",
                        hex_lower(&row.outer_hash)
                    );
                    return self.close(id, State::Quarantined, Some(&reason)).await;
                }
                match self.chain.account_sequence(&row.source).await {
                    Err(error) => {
                        self.note(id, &format!("sequence query failed: {error}")).await?;
                        resolution_of(&row)
                    }
                    // Read from a node behind the one that did not find the
                    // envelope: a low sequence there proves nothing.
                    Ok(read) if read.latest_ledger < node.latest_ledger => {
                        let note = format!(
                            "sequence read at ledger {} is behind ledger {}",
                            read.latest_ledger, node.latest_ledger
                        );
                        self.note(id, &note).await?;
                        resolution_of(&row)
                    }
                    Ok(SourceSequence { sequence: Some(current), .. })
                        if current < row.sequence =>
                    {
                        self.close(id, State::Expired, None).await
                    }
                    Ok(read) => {
                        let reason = format!(
                            "envelope {} not found after its window, but the source sequence is {:?} (envelope sequence {})",
                            hex_lower(&row.outer_hash),
                            read.sequence,
                            row.sequence
                        );
                        self.close(id, State::Quarantined, Some(&reason)).await
                    }
                }
            }
        }
    }

    /// Sends and resolves until the outcome is final or `deadline` passes.
    pub async fn drive(
        &self,
        id: Uuid,
        poll: Duration,
        deadline: OffsetDateTime,
    ) -> Result<Resolution, EngineError> {
        let mut send = true;
        loop {
            if send {
                send =
                    !matches!(self.broadcast(id).await?, Broadcast::Accepted | Broadcast::Skipped);
            }
            let resolution = self.resolve(id).await?;
            if resolution.state.is_final() || self.clock.now() >= deadline {
                return Ok(resolution);
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Resends and resolves every submission this engine's source still has
    /// in flight, e.g. after a restart.
    pub async fn recover(&self) -> Result<Vec<(Uuid, Resolution)>, EngineError> {
        let mut resolved = Vec::new();
        for key in &self.keys.sources {
            if let Some(id) = self.in_flight(&key.address()).await? {
                self.broadcast(id).await?;
                resolved.push((id, self.resolve(id).await?));
            }
        }
        Ok(resolved)
    }

    /// How many envelopes this engine can have in flight at once.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.keys.source_count()
    }

    /// The last ledger in which any address authorization carried by the
    /// submission's envelope is valid. Until it has passed, a copy of that
    /// authorization taken from the broadcast envelope could still be
    /// included by someone else's transaction, even after this envelope's own
    /// window closed.
    pub async fn authorization_horizon(&self, id: Uuid) -> Result<u32, EngineError> {
        let row = self.load(id).await?;
        let envelope = TransactionEnvelope::from_xdr_base64(&row.envelope_xdr, Limits::none())
            .map_err(|_| EngineError::Corrupt("installed envelope does not decode"))?;
        Ok(authorization_horizon(&envelope))
    }

    /// The first ledger in which the authorizations the submission carries
    /// could have been included, if it was recorded.
    pub async fn authorized_from(&self, id: Uuid) -> Result<Option<u32>, EngineError> {
        sqlx::query_scalar!(
            "SELECT authorized_from_ledger FROM pay_stellar.submissions WHERE id = $1",
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read authorization ledger"))?
        .ok_or(EngineError::UnknownSubmission(id))?
        .map(u32::try_from)
        .transpose()
        .map_err(|_| EngineError::Corrupt("authorization ledger out of range"))
    }

    async fn in_flight(&self, source: &AccountAddress) -> Result<Option<Uuid>, EngineError> {
        sqlx::query_scalar!(
            r#"
            SELECT id FROM pay_stellar.submissions
            WHERE network = $1 AND source_address = $2 AND state = 'installed'
            "#,
            self.network.caip2(),
            source.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find in-flight submission"))
    }

    async fn load(&self, id: Uuid) -> Result<Row, EngineError> {
        let row = sqlx::query!(
            r#"
            SELECT state, source_address, sequence, valid_until, created_at, outer_hash,
                   envelope_xdr, ledger, fee_charged, return_value_xdr
            FROM pay_stellar.submissions
            WHERE id = $1 AND network = $2
            "#,
            id,
            self.network.caip2(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("load submission"))?
        .ok_or(EngineError::UnknownSubmission(id))?;
        Ok(Row {
            state: State::parse(&row.state)?,
            source: row
                .source_address
                .parse()
                .map_err(|_| EngineError::Corrupt("source address outside the CHECK constraint"))?,
            sequence: row.sequence,
            valid_until: row.valid_until,
            created_at: row.created_at,
            outer_hash: hash32(row.outer_hash)?,
            envelope_xdr: row.envelope_xdr,
            ledger: row.ledger,
            fee_charged: row.fee_charged,
            return_value_xdr: row.return_value_xdr,
        })
    }

    async fn note(&self, id: Uuid, error: &str) -> Result<(), EngineError> {
        sqlx::query!(
            "UPDATE pay_stellar.submissions SET last_error = $2 WHERE id = $1 AND state = 'installed'",
            id,
            error,
        )
        .execute(&self.pool)
        .await
        .map_err(store("record submission error"))?;
        Ok(())
    }

    async fn finish(
        &self,
        id: Uuid,
        state: State,
        tx: &fermah_pay_stellar_chain::rpc::IncludedTransaction,
    ) -> Result<Resolution, EngineError> {
        let encode = |_| EngineError::Corrupt("included transaction does not encode");
        let result_xdr = tx.result.to_xdr_base64(Limits::none()).map_err(encode)?;
        let return_value_xdr = tx
            .return_value()
            .map(|v| v.to_xdr_base64(Limits::none()))
            .transpose()
            .map_err(encode)?;
        let ledger =
            i32::try_from(tx.ledger).map_err(|_| EngineError::Corrupt("ledger out of range"))?;
        sqlx::query!(
            r#"
            UPDATE pay_stellar.submissions
            SET state = $2, ledger = $3, fee_charged = $4, result_xdr = $5,
                return_value_xdr = $6, resolved_at = now()
            WHERE id = $1 AND state = 'installed'
            "#,
            id,
            state.as_str(),
            ledger,
            tx.result.fee_charged,
            result_xdr,
            return_value_xdr,
        )
        .execute(&self.pool)
        .await
        .map_err(store("record submission outcome"))?;
        resolution_of(&self.load(id).await?)
    }

    async fn close(
        &self,
        id: Uuid,
        state: State,
        reason: Option<&str>,
    ) -> Result<Resolution, EngineError> {
        sqlx::query!(
            r#"
            UPDATE pay_stellar.submissions
            SET state = $2, last_error = COALESCE($3, last_error), resolved_at = now()
            WHERE id = $1 AND state = 'installed'
            "#,
            id,
            state.as_str(),
            reason,
        )
        .execute(&self.pool)
        .await
        .map_err(store("close submission"))?;
        resolution_of(&self.load(id).await?)
    }
}

/// [`Engine::authorization_horizon`] of a stored envelope, for readers that
/// hold the row rather than the engine.
pub fn stored_authorization_horizon(envelope_xdr: &str) -> Option<u32> {
    TransactionEnvelope::from_xdr_base64(envelope_xdr, Limits::none())
        .ok()
        .map(|envelope| authorization_horizon(&envelope))
}

fn authorization_horizon(envelope: &TransactionEnvelope) -> u32 {
    let operations = match envelope {
        TransactionEnvelope::TxFeeBump(bump) => match &bump.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.operations.as_slice(),
        },
        TransactionEnvelope::Tx(v1) => v1.tx.operations.as_slice(),
        TransactionEnvelope::TxV0(v0) => v0.tx.operations.as_slice(),
    };
    operations
        .iter()
        .filter_map(|operation| match &operation.body {
            OperationBody::InvokeHostFunction(invoke) => Some(invoke.auth.as_slice()),
            _ => None,
        })
        .flatten()
        .filter_map(|entry| match &entry.credentials {
            SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds) => {
                Some(creds.signature_expiration_ledger)
            }
            SorobanCredentials::AddressWithDelegates(delegated) => {
                Some(delegated.address_credentials.signature_expiration_ledger)
            }
            SorobanCredentials::SourceAccount => None,
        })
        .max()
        .unwrap_or(0)
}

fn resolution_of(row: &Row) -> Result<Resolution, EngineError> {
    let return_value = row
        .return_value_xdr
        .as_deref()
        .map(|x| ScVal::from_xdr_base64(x, Limits::none()))
        .transpose()
        .map_err(|_| EngineError::Corrupt("stored return value does not decode"))?;
    Ok(Resolution {
        state: row.state,
        ledger: row.ledger,
        fee_charged: row.fee_charged,
        return_value,
    })
}
