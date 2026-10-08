//! Settlement: submits signed deposits, withdrawals and batches of admitted
//! charges through the durable submission engine, and applies each outcome
//! to the rows it settles.
//!
//! A submission is recorded in the same database transaction that links it to
//! its deposit or charges, so there is never an envelope in flight without a
//! record of what it settles, nor a row marked submitted without its
//! envelope. Outcomes are applied only from a final submission, one database
//! transaction per submission; every balance change happens in the statement
//! that moves a row into its final state, which can happen once.
//!
//! A submission that did not succeed is not a verdict on the rows it carried:
//! the authorizations inside its broadcast envelope could still be included
//! by someone else's transaction. So a deposit or charge is only released
//! for another attempt, or declared unprocessed, after every authorization it
//! was sent with has lapsed, and then from the contract's own state: a
//! deposit's marker, or the charge's record.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fermah_pay_stellar_chain::authorization::{AuthorizationError, sign_entry_with};
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{
    CHARGE_RECORD_GRACE, ChargeRequest, Custody, DepositIntent, MAX_BATCH, Outcome,
    PrepaidDeployment, batch_outcomes, charge_record, instance_wasm,
};
use fermah_pay_stellar_chain::reserve::spendable_stroops;
use fermah_pay_stellar_chain::rpc::RpcError;
use fermah_pay_stellar_chain::signer::Signer;
use fermah_pay_stellar_chain::stellar_xdr::{
    Hash, HostFunction, LedgerEntryData, LedgerKey, LedgerKeyAccount, LedgerKeyContractCode,
    Limits, ReadXdr, ScAddress, ScVal, SorobanAddressCredentials, SorobanAuthorizationEntry,
    SorobanCredentials,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::{AccountAddress, ChainAddress, Network};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

mod recurring;
mod reserve;
mod vault;
mod withdrawals;

pub use reserve::{Reserve, ReserveSettingsError};

use crate::events::{ChargeSearch, FoundEntry, search_charge};
use crate::submission::{Chain, Clock, Engine, EngineError, Kind, Resolution, Restore, State};

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// Ledgers the operator's authorization of a batch stays valid. A batch
    /// that was not included is requeued only after this lapses, so it bounds
    /// how long its charges wait.
    pub operator_authorization_ledgers: u32,
    /// How long work the network refused in simulation is left aside.
    pub retry_after: Duration,
    pub max_batch: usize,
    /// Spendable XLM, in stroops, the fee account keeps: below it no new
    /// transaction is built, so what is left pays for finishing the work in
    /// flight rather than for starting more. Resending an envelope costs
    /// nothing until it is included.
    pub fee_floor_stroops: i64,
    /// Remaining ledgers of life below which a served contract's instance
    /// and code are extended, and how many ledgers of life they get. The
    /// contract extends itself on every write; this keeps an idle one from
    /// being archived.
    pub ttl_threshold_ledgers: u32,
    pub ttl_extend_to_ledgers: u32,
    /// How often the contracts' remaining life is read.
    pub ttl_check_every: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("network read failed")]
    Chain(#[source] RpcError),
    #[error("database operation `{operation}` failed")]
    Store {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("stored row violates an invariant: {0}")]
    Corrupt(&'static str),
    /// The node no longer holds a vault's events from `from` on: they cannot
    /// be applied, and reading on would skip them.
    #[error("vault events from ledger {from} are lost; the node's oldest ledger is {oldest}")]
    VaultEventsLost { from: u32, oldest: u32 },
    #[error("signing the operator authorization")]
    Signing(#[source] AuthorizationError),
    #[error("operating system randomness unavailable")]
    Randomness(#[source] getrandom::Error),
}

/// What one [`Worker::step`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Every source has a transaction whose outcome is open; nothing new
    /// could be sent.
    InFlight,
    /// New transactions sent this round, oldest first.
    Submitted(Vec<Uuid>),
    Idle,
}

/// Restores a buyer's request may cause: one is what an account idle long
/// enough to be archived needs, a second covers one that did not land.
const MAX_BUYER_RESTORES: u32 = 2;

pub struct Worker<C, K> {
    engine: Engine<C, K>,
    pool: PgPool,
    operator: Arc<dyn Signer>,
    operator_address: AccountAddress,
    /// The key of the treasury that pays withdrawals, for the deployments
    /// bound with it; without one, withdrawals wait.
    treasury: Option<Arc<dyn Signer>>,
    /// Where the treasury's surplus is swept; see [`Reserve`].
    reserve: Option<Reserve>,
    /// When the treasury's balance was last read against the reserve.
    reserve_checked_at: Mutex<Option<OffsetDateTime>>,
    settings: Settings,
    /// Deposits and deployments whose last attempt the network refused in
    /// simulation, and when they may be tried again. In memory: after a
    /// restart they are simply tried once more.
    set_aside: Mutex<HashMap<Uuid, OffsetDateTime>>,
    /// Restores sent for each buyer request, and when the first was.
    buyer_restores: Mutex<HashMap<Uuid, (u32, OffsetDateTime)>>,
    /// When the contracts' remaining life was last read.
    ttl_checked_at: Mutex<Option<OffsetDateTime>>,
    /// The network's base reserve and when it was read. It changes only by
    /// a network vote, and reading it fetches a whole ledger's metadata.
    base_reserve: Mutex<Option<(OffsetDateTime, u32)>>,
}

/// How long a read base reserve is used before it is read again.
const BASE_RESERVE_READ_EVERY: time::Duration = time::Duration::minutes(10);

fn store(operation: &'static str) -> impl FnOnce(sqlx::Error) -> WorkerError {
    move |source| WorkerError::Store { operation, source }
}

fn address<A: std::str::FromStr>(raw: &str) -> Result<A, WorkerError> {
    raw.parse().map_err(|_| WorkerError::Corrupt("address outside the CHECK constraint"))
}

/// A bound deployment; the binding table holds a treasury exactly when the
/// ledger is not a vault.
fn deployment(
    contract: &str,
    usdc: &str,
    treasury: Option<&str>,
) -> Result<PrepaidDeployment, WorkerError> {
    let contract_id = |raw: &str| {
        stellar_strkey::Contract::from_string(raw)
            .map(|c| c.0)
            .map_err(|_| WorkerError::Corrupt("contract address outside the CHECK constraint"))
    };
    Ok(PrepaidDeployment {
        contract: contract_id(contract)?,
        usdc: contract_id(usdc)?,
        custody: match treasury {
            Some(treasury) => Custody::Treasury(address(treasury)?),
            None => Custody::Vault,
        },
    })
}

fn hash32(bytes: Vec<u8>) -> Result<[u8; 32], WorkerError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| WorkerError::Corrupt("identifier is not 32 bytes"))
}

/// What a final submission means for one charge it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ChargeDecision {
    Charged,
    /// Nothing was debited: the contract refused the charge, or it expired.
    Refused(Outcome),
    Quarantine {
        outcome: Option<Outcome>,
        reason: String,
    },
    /// Proven unprocessed and still within its last ledger.
    Requeue(String),
}

/// The decision an outcome the contract settled, or recorded, establishes.
/// `None` for `Duplicate`: the charge's record, not this answer, says what
/// happened to it.
fn settled(outcome: Outcome) -> Option<ChargeDecision> {
    Some(match outcome {
        Outcome::Charged => ChargeDecision::Charged,
        Outcome::InsufficientBalance
        | Outcome::AboveLimit
        | Outcome::AboveDailyLimit
        | Outcome::AboveCap
        | Outcome::Expired => ChargeDecision::Refused(outcome),
        // The gateway charges only buyers a confirmed deposit created.
        Outcome::UnknownAccount => ChargeDecision::Quarantine {
            outcome: Some(outcome),
            reason: "contract answered unknown_account".to_owned(),
        },
        Outcome::Duplicate => return None,
    })
}

/// A charge whose settlement is looked for in the contract's events.
struct SettledCharge<'a> {
    owner: &'a ChainAddress,
    charge_id: &'a [u8; 32],
    amount: i64,
    last_ledger: i64,
}

struct Snapshot {
    entries: HashMap<LedgerKey, LedgerEntryData>,
    ledger: i64,
}

struct DepositRow {
    id: Uuid,
    deposit_id: [u8; 32],
    expiration_ledger: i64,
    owner: ChainAddress,
    deployment: PrepaidDeployment,
}

/// Counts a deposit this call moved to `state`.
fn deposit_closed(state: &'static str, changed: u64) {
    if changed > 0 {
        metrics::counter!("pay_stellar_deposits_closed_total", "state" => state).increment(1);
    }
}

impl<C: Chain, K: Clock> Worker<C, K> {
    pub fn new(
        engine: Engine<C, K>,
        pool: PgPool,
        operator: Arc<dyn Signer>,
        settings: Settings,
    ) -> Self {
        Self {
            engine,
            pool,
            operator_address: operator.address(),
            operator,
            treasury: None,
            reserve: None,
            reserve_checked_at: Mutex::new(None),
            settings,
            set_aside: Mutex::new(HashMap::new()),
            buyer_restores: Mutex::new(HashMap::new()),
            ttl_checked_at: Mutex::new(None),
            base_reserve: Mutex::new(None),
        }
    }

    /// Signs withdrawals with `treasury`, for the deployments whose
    /// treasury it is.
    #[must_use]
    pub fn with_treasury(mut self, treasury: Arc<dyn Signer>) -> Self {
        self.treasury = Some(treasury);
        self
    }

    /// Sweeps the treasury's surplus to `reserve`; needs the treasury's key
    /// ([`Self::with_treasury`]).
    #[must_use]
    pub fn with_reserve(mut self, reserve: Reserve) -> Self {
        self.reserve = Some(reserve);
        self
    }

    pub const fn engine(&self) -> &Engine<C, K> {
        &self.engine
    }

    fn network(&self) -> Network {
        self.engine.network()
    }

    /// One round: resend and resolve what is in flight, apply final
    /// outcomes, conclude lapsed deposits and expired charges, then send new
    /// transactions from every source that is free. Settling does not wait
    /// for open envelopes: it only touches rows whose submission is final, or
    /// that were never sent.
    #[tracing::instrument(name = "settlement_round", skip_all)]
    pub async fn step(&self) -> Result<Step, WorkerError> {
        let mut open = 0_usize;
        for (_, resolution) in self.engine.recover().await? {
            if !resolution.state.is_final() {
                open += 1;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!("pay_stellar_submissions_in_flight").set(open as f64);
        // Vault limit and exit events first: settling and admission rely on
        // the buyers' rows being current.
        self.ingest_vault_events().await?;
        self.settle().await?;
        self.conclude_lapsed_deposits().await?;
        self.conclude_lapsed_withdrawals().await?;
        self.conclude_lapsed_mandates().await?;
        self.conclude_lapsed_revocations().await?;
        self.conclude_vault_requests().await?;
        self.expire_charges().await?;
        self.expire_recurring().await?;
        self.end_mandates().await?;
        self.record_backlog().await?;
        let mut submitted = Vec::new();
        let funded = self.fee_source_funded().await?;
        if funded {
            for id in self.keep_alive(self.engine.capacity().saturating_sub(open)).await? {
                self.engine.broadcast(id).await?;
                self.engine.resolve(id).await?;
                submitted.push(id);
            }
            if open + submitted.len() < self.engine.capacity()
                && let Some(id) = self.sweep().await?
            {
                self.engine.broadcast(id).await?;
                self.engine.resolve(id).await?;
                submitted.push(id);
            }
        }
        while funded && open + submitted.len() < self.engine.capacity() {
            let mut next = self.submit_deposit().await?;
            if next.is_none() {
                next = self.submit_withdrawal().await?;
            }
            if next.is_none() {
                next = self.submit_mandate().await?;
            }
            if next.is_none() {
                next = self.submit_revocation().await?;
            }
            if next.is_none() {
                next = self.submit_vault_request().await?;
            }
            if next.is_none() {
                next = self.submit_exit().await?;
            }
            if next.is_none() {
                next = self.submit_charges().await?;
            }
            if next.is_none() {
                next = self.submit_recurring().await?;
            }
            let Some(id) = next else { break };
            self.engine.broadcast(id).await?;
            if self.engine.resolve(id).await?.state.is_final() {
                self.settle().await?;
            }
            submitted.push(id);
        }
        Ok(match (submitted.is_empty(), open) {
            (false, _) => Step::Submitted(submitted),
            (true, 0) => Step::Idle,
            (true, _) => Step::InFlight,
        })
    }

    /// Steps until `shutdown` resolves: `busy_poll` apart while there is work,
    /// `idle_poll` apart otherwise. A failed step is logged and retried.
    pub async fn run(
        &self,
        busy_poll: Duration,
        idle_poll: Duration,
        shutdown: impl Future<Output = ()> + Send,
    ) {
        tokio::pin!(shutdown);
        loop {
            let wait = match self.step().await {
                Ok(Step::Idle) => idle_poll,
                Ok(Step::InFlight | Step::Submitted(_)) => busy_poll,
                Err(error) => {
                    metrics::counter!("pay_stellar_worker_step_failures_total").increment(1);
                    tracing::error!(error = %error, source = ?std::error::Error::source(&error), "settlement step failed");
                    idle_poll
                }
            };
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Whether the fee account's spendable XLM is above the floor. Its
    /// reserve is taken from the ledger header's base reserve; a node that
    /// sends no header is refused as unable to tell.
    async fn fee_source_funded(&self) -> Result<bool, WorkerError> {
        let address = self.engine.fee_source_address();
        let key = LedgerKey::Account(LedgerKeyAccount { account_id: account_id(&address) });
        let chain = self.engine.chain();
        let read = chain.ledger_entries(&[key]).await.map_err(WorkerError::Chain)?;
        let base_reserve = self.base_reserve().await?;
        let (Some(LedgerEntryData::Account(account)), Some(base_reserve)) =
            (read.entries.first().map(|entry| &entry.data), base_reserve)
        else {
            tracing::warn!(fee_source = %address, "the fee account or the base reserve cannot be read; building nothing");
            return Ok(false);
        };
        let spendable = spendable_stroops(account, base_reserve);
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!("pay_stellar_fee_source_spendable_stroops").set(spendable as f64);
        let funded = spendable >= self.settings.fee_floor_stroops;
        if !funded {
            tracing::error!(
                fee_source = %address,
                spendable,
                floor = self.settings.fee_floor_stroops,
                "the fee account is at its floor; only work in flight continues"
            );
        }
        Ok(funded)
    }

    /// The network's base reserve, read again once [`BASE_RESERVE_READ_EVERY`]
    /// has passed; `None` while the node sends no ledger header.
    async fn base_reserve(&self) -> Result<Option<u32>, WorkerError> {
        let now = self.engine.clock().now();
        let cached = *self.base_reserve.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, reserve)) = cached
            && now < at + BASE_RESERVE_READ_EVERY
        {
            return Ok(Some(reserve));
        }
        let info = self.engine.chain().latest_ledger_info().await.map_err(WorkerError::Chain)?;
        if let Some(reserve) = info.base_reserve {
            *self.base_reserve.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((now, reserve));
        }
        Ok(info.base_reserve)
    }

    /// Extends the life of each served contract whose instance or code has
    /// fewer than the threshold's ledgers left, sending at most `room`
    /// extensions. Runs at most once per check interval; a contract missed
    /// for lack of room is taken at the next check, well before it could be
    /// archived.
    async fn keep_alive(&self, room: usize) -> Result<Vec<Uuid>, WorkerError> {
        let now = self.engine.clock().now();
        let every = time::Duration::try_from(self.settings.ttl_check_every)
            .map_err(|_| WorkerError::Corrupt("TTL check interval out of range"))?;
        {
            // A poisoned lock only held a timestamp; the value is still usable.
            let mut checked =
                self.ttl_checked_at.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if room == 0 || checked.is_some_and(|at| at + every > now) {
                return Ok(Vec::new());
            }
            *checked = Some(now);
        }
        let contracts = sqlx::query!(
            r#"
            SELECT l.seller_deployment_id, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.ledger_contracts l
            WHERE l.network = $1 AND l.operator_address = $2
            ORDER BY l.seller_deployment_id
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read served contracts"))?;
        let chain = self.engine.chain();
        let mut sent = Vec::new();
        for contract in contracts {
            if sent.len() >= room {
                break;
            }
            let pinned = deployment(
                &contract.contract_address,
                &contract.usdc_address,
                contract.treasury_address.as_deref(),
            )?;
            let instance_key = pinned.instance_key();
            let instance = chain
                .ledger_entries(std::slice::from_ref(&instance_key))
                .await
                .map_err(WorkerError::Chain)?;
            let Some(wasm) = instance.entries.first().and_then(|entry| instance_wasm(&entry.data))
            else {
                tracing::warn!(contract = %contract.contract_address, "the contract instance cannot be read; skipping its life check");
                continue;
            };
            let code_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: Hash(wasm) });
            // A vault's USDC is an entry of the USDC contract, extended only
            // when the vault's balance moves: kept alive here too.
            let balance_key = pinned.is_vault().then(|| pinned.vault_balance_key());
            let mut keys = vec![instance_key.clone(), code_key.clone()];
            keys.extend(balance_key.clone());
            let read = chain.ledger_entries(&keys).await.map_err(WorkerError::Chain)?;
            let mut shortest = u32::MAX;
            for entry in &read.entries {
                let left = entry
                    .live_until_ledger
                    .map_or(0, |until| until.saturating_sub(read.latest_ledger));
                let part = if entry.key == code_key {
                    "code"
                } else if Some(&entry.key) == balance_key.as_ref() {
                    "balance"
                } else {
                    "instance"
                };
                metrics::gauge!(
                    "pay_stellar_contract_ttl_ledgers",
                    "deployment" => contract.seller_deployment_id.to_string(),
                    "entry" => part
                )
                .set(f64::from(left));
                shortest = shortest.min(left);
            }
            let has = |key: &LedgerKey| read.entries.iter().any(|entry| entry.key == *key);
            if !has(&instance_key)
                || !has(&code_key)
                || shortest >= self.settings.ttl_threshold_ledgers
            {
                continue;
            }
            // A vault that holds nothing yet has no balance entry to extend.
            let extend: Vec<LedgerKey> = keys.into_iter().filter(|key| has(key)).collect();
            let prepared =
                match self.engine.prepare_extend(extend, self.settings.ttl_extend_to_ledgers).await
                {
                    Ok(prepared) => prepared,
                    Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => break,
                    Err(error) => return Err(error.into()),
                };
            let mut conn = self.pool.acquire().await.map_err(store("acquire connection"))?;
            match self.engine.record(&mut conn, &prepared).await {
                Ok(_) => {}
                Err(EngineError::SourceBusy { .. }) => break,
                Err(error) => return Err(error.into()),
            }
            tracing::info!(
                seller_deployment_id = %contract.seller_deployment_id,
                ledgers_left = shortest,
                extend_to = self.settings.ttl_extend_to_ledgers,
                submission_id = %prepared.id,
                "extending the contract's life"
            );
            sent.push(prepared.id);
        }
        Ok(sent)
    }

    /// Charges waiting for a batch, and signed withdrawals waiting to be
    /// sent, on this operator's deployments, and how long the oldest of each
    /// has waited: a growing age means they are not going out.
    async fn record_backlog(&self) -> Result<(), WorkerError> {
        let backlog = sqlx::query!(
            r#"
            SELECT count(*) AS "waiting!",
                   COALESCE(EXTRACT(EPOCH FROM now() - min(c.created_at)), 0)::float8 AS "oldest!"
            FROM pay_stellar.charges c
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = c.seller_deployment_id AND l.network = c.network
            WHERE c.state = 'admitted' AND c.network = $1 AND l.operator_address = $2
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store("read charge backlog"))?;
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!("pay_stellar_charges_waiting").set(backlog.waiting as f64);
        metrics::gauge!("pay_stellar_oldest_waiting_charge_seconds").set(backlog.oldest);
        // Signed withdrawals not yet sent: typically the treasury is short
        // of USDC, or no worker holds its key.
        let withdrawals = sqlx::query!(
            r#"
            SELECT count(*) AS "waiting!",
                   COALESCE(EXTRACT(EPOCH FROM now() - min(w.signed_at)), 0)::float8 AS "oldest!"
            FROM pay_stellar.withdrawals w
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = w.seller_deployment_id AND l.network = w.network
            WHERE w.state = 'signed' AND w.network = $1 AND l.operator_address = $2
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store("read withdrawal backlog"))?;
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!("pay_stellar_withdrawals_waiting").set(withdrawals.waiting as f64);
        metrics::gauge!("pay_stellar_oldest_waiting_withdrawal_seconds").set(withdrawals.oldest);
        Ok(())
    }

    /// Why a buyer's request may not have `restore` sent for it, if it may
    /// not. A contract-account wallet's own state can be what is archived,
    /// and the operator pays for each restore: each is bounded like the
    /// request's transaction, and a request gets at most
    /// `MAX_BUYER_RESTORES`. Past that the request is not sent, and ends
    /// when its authorization lapses. The count lives in this process, so
    /// a restart allows as many again.
    pub(crate) fn buyer_restore_refusal(&self, request: Uuid, restore: &Restore) -> Option<String> {
        if let Err(error) = self.engine.check_buyer_resource_fee(restore.min_resource_fee) {
            return Some(error.to_string());
        }
        let now = self.engine.clock().now();
        let mut restores = self.buyer_restores.lock().ok()?;
        // A request lives about an hour; a day bounds the map.
        restores.retain(|_, (_, first)| now - *first < time::Duration::days(1));
        let (count, _) = restores.entry(request).or_insert((0, now));
        if *count >= MAX_BUYER_RESTORES {
            return Some(format!(
                "its archived state was restored {MAX_BUYER_RESTORES} times and is needed again"
            ));
        }
        *count += 1;
        None
    }

    fn set_aside_ids(&self) -> Vec<Uuid> {
        let now = self.engine.clock().now();
        self.set_aside
            .lock()
            .map(|map| map.iter().filter(|(_, until)| **until > now).map(|(id, _)| *id).collect())
            .unwrap_or_default()
    }

    fn set_aside(&self, id: Uuid) {
        let until = self.engine.clock().now()
            + time::Duration::try_from(self.settings.retry_after).unwrap_or(time::Duration::MINUTE);
        if let Ok(mut map) = self.set_aside.lock() {
            map.insert(id, until);
        }
    }

    async fn latest_ledger(&self) -> Result<i64, WorkerError> {
        self.engine.chain().latest_ledger().await.map(i64::from).map_err(WorkerError::Chain)
    }

    /// The entries that exist among `keys`, and the ledger the node read
    /// them at: an absent entry is evidence only as of that ledger.
    async fn existing(&self, keys: Vec<LedgerKey>) -> Result<Snapshot, WorkerError> {
        // The RPC refuses a read of no keys; with nothing to read there is
        // nothing to decide, and ledger 0 is before every expiration.
        if keys.is_empty() {
            return Ok(Snapshot { entries: HashMap::new(), ledger: 0 });
        }
        let read = self.engine.chain().ledger_entries(&keys).await.map_err(WorkerError::Chain)?;
        Ok(Snapshot {
            entries: read.entries.into_iter().map(|record| (record.key, record.data)).collect(),
            ledger: i64::from(read.latest_ledger),
        })
    }

    // ---- applying outcomes -------------------------------------------------

    async fn settle(&self) -> Result<(), WorkerError> {
        let pending = sqlx::query!(
            r#"
            SELECT s.id, s.kind FROM pay_stellar.submissions s
            WHERE s.network = $1 AND s.state <> 'installed'
              AND (EXISTS (SELECT 1 FROM pay_stellar.charges c
                           WHERE c.submission_id = s.id AND c.state = 'submitted')
                OR EXISTS (SELECT 1 FROM pay_stellar.deposits d
                           WHERE d.submission_id = s.id AND d.state = 'submitted')
                OR EXISTS (SELECT 1 FROM pay_stellar.withdrawals w
                           WHERE w.submission_id = s.id AND w.state = 'submitted')
                OR EXISTS (SELECT 1 FROM pay_stellar.mandates m
                           WHERE m.submission_id = s.id AND m.state = 'submitted')
                OR EXISTS (SELECT 1 FROM pay_stellar.revocations r
                           WHERE r.submission_id = s.id AND r.state = 'submitted')
                OR EXISTS (SELECT 1 FROM pay_stellar.recurring_charges rc
                           WHERE rc.submission_id = s.id AND rc.state = 'submitted'))
            ORDER BY s.created_at
            "#,
            self.network().caip2(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find settled submissions"))?;
        for row in pending {
            let resolution = self.engine.resolve(row.id).await?;
            match row.kind.as_str() {
                "charge_batch" => self.settle_charges(row.id, &resolution).await?,
                "deposit" => self.settle_deposit(row.id, &resolution).await?,
                "withdrawal" => self.settle_withdrawal(row.id, &resolution).await?,
                "mandate" => self.settle_mandate(row.id, &resolution).await?,
                "revocation" => self.settle_revocation(row.id, &resolution).await?,
                "recurring_batch" => self.settle_recurring(row.id, &resolution).await?,
                "set_cap" | "request_exit" => {
                    self.settle_vault_request(row.id, &resolution).await?
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn settle_charges(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        let rows = sqlx::query!(
            r#"
            SELECT c.id, c.charge_id, c.last_ledger, c.amount, c.batch_index AS "batch_index!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address,
                   (SELECT count(*) FROM pay_stellar.charges a
                    WHERE a.submission_id = c.submission_id) AS "batch_size!"
            FROM pay_stellar.charges c
            JOIN pay_stellar.buyers b ON b.id = c.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = c.seller_deployment_id AND l.network = c.network
            WHERE c.submission_id = $1 AND c.state = 'submitted'
            ORDER BY c.batch_index
            "#,
            submission,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read submitted charges"))?;
        let Some(first) = rows.first() else { return Ok(()) };
        let deployment = deployment(
            &first.contract_address,
            &first.usdc_address,
            first.treasury_address.as_deref(),
        )?;

        // What the batch's answer settles directly; the rest is decided from
        // each charge's record on the contract.
        let mut decisions: Vec<(Uuid, ChargeDecision)> = Vec::new();
        let mut from_records = Vec::new();
        match resolution.state {
            State::Installed => return Ok(()),
            State::Succeeded => {
                let outcomes = resolution.return_value.as_ref().and_then(batch_outcomes);
                match outcomes {
                    Some(outcomes)
                        if i64::try_from(outcomes.len()).ok() == Some(first.batch_size) =>
                    {
                        for row in &rows {
                            let outcome = usize::try_from(row.batch_index)
                                .ok()
                                .and_then(|i| outcomes.get(i).copied())
                                .ok_or(WorkerError::Corrupt("batch index outside the batch"))?;
                            match settled(outcome) {
                                Some(decision) => decisions.push((row.id, decision)),
                                None => from_records.push(row),
                            }
                        }
                    }
                    // Included and successful, but the answer is unreadable:
                    // every charge it applied left a record.
                    _ => from_records.extend(rows.iter()),
                }
            }
            State::Failed | State::Expired | State::Quarantined => from_records.extend(rows.iter()),
        }

        if !from_records.is_empty() {
            let applied = resolution.state == State::Succeeded;
            let included = resolution.ledger.map_or(i64::MAX, i64::from);
            let horizon = i64::from(self.engine.authorization_horizon(submission).await?);
            let owners = from_records
                .iter()
                .map(|row| Ok((address(&row.wallet_address)?, hash32(row.charge_id.clone())?)))
                .collect::<Result<Vec<_>, WorkerError>>()?;
            let keys =
                owners.iter().map(|(owner, id)| deployment.charge_record_key(owner, id)).collect();
            let snapshot = self.existing(keys).await?;
            for (row, (owner, id)) in from_records.iter().zip(&owners) {
                let decision = match snapshot.entries.get(&deployment.charge_record_key(owner, id))
                {
                    Some(entry) => {
                        let outcome = charge_record(entry)
                            .ok_or(WorkerError::Corrupt("charge record does not decode"))?;
                        settled(outcome).ok_or(WorkerError::Corrupt("a record holds duplicate"))?
                    }
                    // A node behind the inclusion cannot show its records yet.
                    None if applied && snapshot.ledger < included => continue,
                    // Applied, per the network, yet no record: contradiction.
                    None if applied => ChargeDecision::Quarantine {
                        outcome: Some(Outcome::Duplicate),
                        reason: format!(
                            "submission {submission} answered duplicate or unreadably, and the contract holds no record at ledger {}",
                            snapshot.ledger
                        ),
                    },
                    // A copy of the batch's authorization could still land.
                    None if snapshot.ledger <= horizon => continue,
                    None if snapshot.ledger <= row.last_ledger => ChargeDecision::Requeue(format!(
                        "submission {submission} did not apply it; no record at ledger {}",
                        snapshot.ledger
                    )),
                    // Past its last ledger, and a record would still live:
                    // it was never applied and never can be.
                    None if snapshot.ledger <= row.last_ledger + i64::from(CHARGE_RECORD_GRACE) => {
                        ChargeDecision::Refused(Outcome::Expired)
                    }
                    // Only the contract's events can still tell.
                    None => {
                        let charge = SettledCharge {
                            owner,
                            charge_id: id,
                            amount: row.amount,
                            last_ledger: row.last_ledger,
                        };
                        match self
                            .decide_from_events(submission, &deployment, &charge, snapshot.ledger)
                            .await?
                        {
                            Some(decision) => decision,
                            None => continue,
                        }
                    }
                };
                decisions.push((row.id, decision));
            }
        }
        self.apply_charge_decisions(&decisions).await
    }

    /// Decides a charge whose record has lapsed from the contract's `charges`
    /// events, from the ledger at which its batch was authorized through the
    /// charge's last ledger. No transaction could include the batch earlier,
    /// and after that ledger the contract refuses the charge whoever sends it,
    /// so an entry in that range is the charge's settlement and no entry
    /// proves it was never applied. `None` while the node has not reached the
    /// charge's last ledger; a quarantine when the range cannot be read whole.
    async fn decide_from_events(
        &self,
        submission: Uuid,
        deployment: &PrepaidDeployment,
        charge: &SettledCharge<'_>,
        read_at: i64,
    ) -> Result<Option<ChargeDecision>, WorkerError> {
        let lapsed = format!(
            "no record at ledger {read_at}, past the ledger its record would have lived to"
        );
        let quarantine = |reason: String| ChargeDecision::Quarantine { outcome: None, reason };
        let Some(from) = self.engine.authorized_from(submission).await? else {
            return Ok(Some(quarantine(format!(
                "{lapsed}; the ledger its batch was authorized at is not recorded, so its events cannot be searched"
            ))));
        };
        let to = u32::try_from(charge.last_ledger)
            .map_err(|_| WorkerError::Corrupt("charge last ledger out of range"))?;
        let search = search_charge(
            self.engine.chain(),
            &deployment.contract,
            charge.owner,
            charge.charge_id,
            from,
            to,
        )
        .await
        .map_err(WorkerError::Chain)?;
        let entries = match search {
            ChargeSearch::Behind { .. } => return Ok(None),
            ChargeSearch::Pruned { oldest } => {
                return Ok(Some(quarantine(format!(
                    "{lapsed}; the node retains events only from ledger {oldest}, so ledgers {from} to {} cannot be searched",
                    oldest.saturating_sub(1)
                ))));
            }
            ChargeSearch::Unreadable { event } => {
                return Ok(Some(quarantine(format!(
                    "{lapsed}; charges event {event} does not decode"
                ))));
            }
            ChargeSearch::Complete { entries, .. } => entries,
        };
        // Inside the range the contract answers `duplicate` only while the
        // charge's record lives, which an earlier entry created; with no
        // settling entry, a duplicate answer means part of the story is
        // missing. `expired` answers change nothing.
        let Some(found) = entries.iter().find(|found| found.settles()) else {
            return Ok(Some(if !entries.iter().any(FoundEntry::is_duplicate) {
                ChargeDecision::Refused(Outcome::Expired)
            } else {
                ChargeDecision::Quarantine {
                    outcome: Some(Outcome::Duplicate),
                    reason: format!(
                        "{lapsed}; ledgers {from} to {to} show the charge only as a duplicate"
                    ),
                }
            }));
        };
        if found.entry.amount != i128::from(charge.amount) {
            return Ok(Some(quarantine(format!(
                "charges event {} settled the charge for {}, not {}",
                found.event, found.entry.amount, charge.amount
            ))));
        }
        Ok(Some(settled(found.entry.outcome).unwrap_or_else(|| {
            quarantine(format!("charges event {} answered duplicate", found.event))
        })))
    }

    /// Admitted charges past their last ledger were never applied (an
    /// admitted charge is not in flight, and is readmitted only once its
    /// earlier submission is proven not to have applied it) and never can be:
    /// they are refused as expired and their amount returned.
    async fn expire_charges(&self) -> Result<(), WorkerError> {
        let latest = self.latest_ledger().await?;
        sqlx::query!(
            r#"
            WITH expired AS (
                UPDATE pay_stellar.charges c
                SET state = 'refused', outcome = 'expired', settled_at = now(),
                    last_error = 'not settled before its last ledger'
                FROM pay_stellar.ledger_contracts l
                WHERE c.state = 'admitted' AND c.last_ledger < $1 AND c.network = $2
                  AND l.seller_deployment_id = c.seller_deployment_id AND l.network = c.network
                  AND l.operator_address = $3
                RETURNING c.buyer_id, c.amount
            ),
            refunds AS (
                SELECT buyer_id, sum(amount) AS amount FROM expired GROUP BY buyer_id
            )
            UPDATE pay_stellar.buyers b
            SET available = b.available + refunds.amount
            FROM refunds WHERE b.id = refunds.buyer_id
            "#,
            latest,
            self.network().caip2(),
            self.operator_address.as_str(),
        )
        .execute(&self.pool)
        .await
        .map_err(store("expire charges"))?;
        Ok(())
    }

    async fn apply_charge_decisions(
        &self,
        decisions: &[(Uuid, ChargeDecision)],
    ) -> Result<(), WorkerError> {
        let mut tx = self.pool.begin().await.map_err(store("begin charge settlement"))?;
        // What this call changed, counted once the transaction commits.
        let mut applied: Vec<&'static str> = Vec::new();
        for (id, decision) in decisions {
            let (result, changed) = match decision {
                ChargeDecision::Charged => {
                    let changed = sqlx::query!(
                        r#"
                        UPDATE pay_stellar.charges
                        SET state = 'charged', outcome = 'charged', settled_at = now()
                        WHERE id = $1 AND state = 'submitted'
                        "#,
                        id,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(store("settle charge"))?
                    .rows_affected();
                    ("charged", changed)
                }
                // The refund is part of the same statement as the transition,
                // so it happens exactly when the charge leaves `submitted`.
                ChargeDecision::Refused(outcome) => {
                    let changed = sqlx::query!(
                        r#"
                        WITH refused AS (
                            UPDATE pay_stellar.charges
                            SET state = 'refused', outcome = $2, settled_at = now()
                            WHERE id = $1 AND state = 'submitted'
                            RETURNING buyer_id, amount
                        )
                        UPDATE pay_stellar.buyers b
                        SET available = b.available + refused.amount
                        FROM refused WHERE b.id = refused.buyer_id
                        "#,
                        id,
                        outcome.token(),
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(store("refuse charge"))?
                    .rows_affected();
                    (outcome.token(), changed)
                }
                ChargeDecision::Quarantine { outcome, reason } => {
                    tracing::error!(charge_id = %id, reason, "charge quarantined");
                    let changed = sqlx::query!(
                        r#"
                        UPDATE pay_stellar.charges
                        SET state = 'quarantined', outcome = $2, last_error = $3, settled_at = now()
                        WHERE id = $1 AND state = 'submitted'
                        "#,
                        id,
                        outcome.map(Outcome::token),
                        reason,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(store("quarantine charge"))?
                    .rows_affected();
                    ("quarantined", changed)
                }
                ChargeDecision::Requeue(reason) => {
                    let changed = sqlx::query!(
                        r#"
                        UPDATE pay_stellar.charges
                        SET state = 'admitted', submission_id = NULL, batch_index = NULL,
                            last_error = $2
                        WHERE id = $1 AND state = 'submitted'
                        "#,
                        id,
                        reason,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(store("requeue charge"))?
                    .rows_affected();
                    ("requeued", changed)
                }
            };
            if changed > 0 {
                applied.push(result);
            }
        }
        tx.commit().await.map_err(store("commit charge settlement"))?;
        for result in applied {
            metrics::counter!("pay_stellar_charges_settled_total", "result" => result).increment(1);
        }
        Ok(())
    }

    async fn settle_deposit(
        &self,
        submission: Uuid,
        resolution: &Resolution,
    ) -> Result<(), WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT d.id, d.deposit_id, d.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.deposits d
            JOIN pay_stellar.buyers b ON b.id = d.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = d.seller_deployment_id AND l.network = d.network
            WHERE d.submission_id = $1 AND d.state = 'submitted'
            "#,
            submission,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("read submitted deposit"))?
        else {
            return Ok(());
        };
        let deposit = DepositRow {
            id: row.id,
            deposit_id: hash32(row.deposit_id)?,
            expiration_ledger: row.expiration_ledger,
            owner: address(&row.wallet_address)?,
            deployment: deployment(
                &row.contract_address,
                &row.usdc_address,
                row.treasury_address.as_deref(),
            )?,
        };
        match resolution.state {
            State::Installed => Ok(()),
            State::Succeeded => self.confirm(deposit.id).await,
            // Never included, and the buyer's signature is still good: the
            // same entry can be sent again. Its nonce makes at most one
            // inclusion possible, so a node that still sees the signature as
            // valid only costs a refused send.
            State::Expired if self.latest_ledger().await? <= deposit.expiration_ledger => {
                self.resend_deposit(deposit.id).await
            }
            // Failed, expired, or of unknown fate: once the signature has
            // lapsed the contract's own marker decides; until then the entry
            // may still be included elsewhere, and `conclude` waits.
            State::Expired | State::Failed | State::Quarantined => {
                let closing = if resolution.state == State::Failed { "failed" } else { "expired" };
                self.conclude(&[deposit], closing).await
            }
        }
    }

    /// Decides deposits from the contract's deposit marker: credited if it
    /// exists, `closing` if it is absent at a ledger after the buyer's
    /// authorization lapsed. A deposit whose authorization the reading node
    /// has not yet seen lapse stays undecided.
    async fn conclude(
        &self,
        deposits: &[DepositRow],
        closing: &'static str,
    ) -> Result<(), WorkerError> {
        let keys =
            deposits.iter().map(|d| d.deployment.deposit_key(&d.owner, &d.deposit_id)).collect();
        let snapshot = self.existing(keys).await?;
        for deposit in deposits {
            let key = deposit.deployment.deposit_key(&deposit.owner, &deposit.deposit_id);
            if snapshot.entries.contains_key(&key) {
                self.confirm(deposit.id).await?;
            } else if snapshot.ledger > deposit.expiration_ledger {
                self.close_deposit(
                    deposit.id,
                    closing,
                    "authorization lapsed; the contract never processed it",
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Credits the deposit in the statement that moves it to `confirmed`, so
    /// the credit happens exactly once.
    async fn confirm(&self, id: Uuid) -> Result<(), WorkerError> {
        let confirmed = sqlx::query!(
            r#"
            WITH confirmed AS (
                UPDATE pay_stellar.deposits
                SET state = 'confirmed', resolved_at = now()
                WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
                RETURNING buyer_id, amount
            )
            UPDATE pay_stellar.buyers b
            SET available = b.available + confirmed.amount,
                -- A vault buyer read as having no account may have had one
                -- the node no longer served, archived with its limit and
                -- exit; the deposit restored it, so it is read again.
                vault_synced_ledger = CASE
                    WHEN b.cap = 0 AND b.pending_cap IS NULL AND b.exit_amount IS NULL
                         AND b.available >= 0
                    THEN NULL ELSE b.vault_synced_ledger END
            FROM confirmed WHERE b.id = confirmed.buyer_id
            "#,
            id,
        )
        .execute(&self.pool)
        .await
        .map_err(store("confirm deposit"))?;
        deposit_closed("confirmed", confirmed.rows_affected());
        Ok(())
    }

    async fn close_deposit(
        &self,
        id: Uuid,
        state: &'static str,
        reason: &str,
    ) -> Result<(), WorkerError> {
        let closed = sqlx::query!(
            r#"
            UPDATE pay_stellar.deposits
            SET state = $2, last_error = $3, resolved_at = now()
            WHERE id = $1 AND state IN ('awaiting_signature', 'signed', 'submitted')
            "#,
            id,
            state,
            reason,
        )
        .execute(&self.pool)
        .await
        .map_err(store("close deposit"))?;
        deposit_closed(state, closed.rows_affected());
        Ok(())
    }

    async fn resend_deposit(&self, id: Uuid) -> Result<(), WorkerError> {
        sqlx::query!(
            r#"
            UPDATE pay_stellar.deposits
            SET state = 'signed', submission_id = NULL,
                last_error = 'not included before its transaction expired; sending again'
            WHERE id = $1 AND state = 'submitted'
            "#,
            id,
        )
        .execute(&self.pool)
        .await
        .map_err(store("requeue deposit"))?;
        Ok(())
    }

    /// Deposits that were never sent, or were requeued, and whose
    /// authorization has lapsed. The buyer holds the signed entry and may
    /// have included it through a transaction of their own, so the marker
    /// still decides.
    async fn conclude_lapsed_deposits(&self) -> Result<(), WorkerError> {
        let latest = self.latest_ledger().await?;
        let rows = sqlx::query!(
            r#"
            SELECT d.id, d.deposit_id, d.expiration_ledger, b.wallet_address,
                   l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.deposits d
            JOIN pay_stellar.buyers b ON b.id = d.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = d.seller_deployment_id AND l.network = d.network
            WHERE d.network = $1 AND l.operator_address = $2
              AND d.state IN ('awaiting_signature', 'signed') AND d.expiration_ledger < $3
            ORDER BY d.expiration_ledger
            LIMIT 100
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            latest,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("find lapsed deposits"))?;
        let deposits = rows
            .into_iter()
            .map(|row| {
                Ok(DepositRow {
                    id: row.id,
                    deposit_id: hash32(row.deposit_id)?,
                    expiration_ledger: row.expiration_ledger,
                    owner: address(&row.wallet_address)?,
                    deployment: deployment(
                        &row.contract_address,
                        &row.usdc_address,
                        row.treasury_address.as_deref(),
                    )?,
                })
            })
            .collect::<Result<Vec<_>, WorkerError>>()?;
        self.conclude(&deposits, "expired").await
    }

    // ---- sending -----------------------------------------------------------

    #[tracing::instrument(skip_all, fields(deposit_id, submission_id))]
    async fn submit_deposit(&self) -> Result<Option<Uuid>, WorkerError> {
        let Some(row) = sqlx::query!(
            r#"
            SELECT d.id, d.amount, d.deposit_id, d.cap,
                   d.signed_authorization_xdr AS "signed_authorization_xdr!",
                   b.wallet_address, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.deposits d
            JOIN pay_stellar.buyers b ON b.id = d.buyer_id
            JOIN pay_stellar.ledger_contracts l
              ON l.seller_deployment_id = d.seller_deployment_id AND l.network = d.network
            WHERE d.network = $1 AND l.operator_address = $2 AND d.state = 'signed'
              AND d.id <> ALL($3)
            ORDER BY d.created_at
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            &self.set_aside_ids(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find signed deposit"))?
        else {
            return Ok(None);
        };
        let deployment =
            deployment(&row.contract_address, &row.usdc_address, row.treasury_address.as_deref())?;
        let intent = DepositIntent {
            owner: address(&row.wallet_address)?,
            amount: i128::from(row.amount),
            deposit_id: hash32(row.deposit_id)?,
            cap: row.cap.map(i128::from),
        };
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &row.signed_authorization_xdr,
            Limits::none(),
        )
        .map_err(|_| WorkerError::Corrupt("stored signed entry does not decode"))?;
        // The API verified this entry against the same deposit; a mismatch
        // here means the stored row changed, and nothing is sent for it.
        if entry.root_invocation != deployment.deposit_authorization(&intent) {
            tracing::error!(deposit_id = %row.id, "signed entry does not authorize this deposit");
            self.set_aside(row.id);
            return Ok(None);
        }
        let function = HostFunction::InvokeContract(deployment.deposit_call(&intent));
        let prepared = match self.engine.prepare(Kind::Deposit, function, vec![entry]).await {
            Ok(prepared) => prepared,
            // The restore a buyer's transaction needs is bounded like the
            // transaction itself: a contract-account wallet's own state can
            // be what is archived.
            Err(EngineError::RestoreRequired(restore)) => {
                match self.buyer_restore_refusal(row.id, &restore) {
                    None => return self.restore(&restore, row.id).await,
                    Some(reason) => {
                        tracing::warn!(deposit_id = %row.id, reason, "its restore is not sent");
                        self.note_deposit(row.id, &reason).await?;
                        self.set_aside(row.id);
                        return Ok(None);
                    }
                }
            }
            Err(
                error
                @ (EngineError::SimulationFailed(_) | EngineError::ResourceFeeAboveCap { .. }),
            ) => {
                tracing::warn!(deposit_id = %row.id, error = %error, "network refused the deposit in simulation, or it costs too much");
                self.note_deposit(row.id, &error.to_string()).await?;
                self.set_aside(row.id);
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let span = tracing::Span::current();
        span.record("deposit_id", tracing::field::display(row.id));
        span.record("submission_id", tracing::field::display(prepared.id));

        let mut tx = self.pool.begin().await.map_err(store("begin deposit submission"))?;
        match self.engine.record(&mut tx, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let linked = sqlx::query!(
            r#"
            UPDATE pay_stellar.deposits
            SET state = 'submitted', submission_id = $2, last_error = NULL
            WHERE id = $1 AND state = 'signed'
            "#,
            row.id,
            prepared.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("link deposit"))?;
        if linked.rows_affected() != 1 {
            return Ok(None);
        }
        tx.commit().await.map_err(store("commit deposit submission"))?;
        Ok(Some(prepared.id))
    }

    /// Sends the restore of the archived entries a simulation named, e.g.
    /// the account of a buyer idle long enough for its entry to expire. The
    /// refused work is set aside until the next retry, by which time the
    /// restore has usually landed; if it failed, simulation asks again.
    async fn restore(&self, restore: &Restore, refused: Uuid) -> Result<Option<Uuid>, WorkerError> {
        let prepared = match self.engine.prepare_restore(restore).await {
            Ok(prepared) => prepared,
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut conn = self.pool.acquire().await.map_err(store("acquire connection"))?;
        match self.engine.record(&mut conn, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        tracing::info!(submission_id = %prepared.id, refused = %refused, "restoring archived ledger entries");
        self.set_aside(refused);
        Ok(Some(prepared.id))
    }

    async fn note_deposit(&self, id: Uuid, error: &str) -> Result<(), WorkerError> {
        sqlx::query!(
            "UPDATE pay_stellar.deposits SET last_error = $2 WHERE id = $1 AND state = 'signed'",
            id,
            error,
        )
        .execute(&self.pool)
        .await
        .map_err(store("note deposit error"))?;
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(seller_deployment_id, submission_id, charges))]
    async fn submit_charges(&self) -> Result<Option<Uuid>, WorkerError> {
        let latest = self.engine.chain().latest_ledger().await.map_err(WorkerError::Chain)?;
        // A charge too close to its last ledger may not be included in time;
        // it is left to expire and be refunded rather than sent.
        let sendable =
            i64::from(latest.saturating_add(self.settings.operator_authorization_ledgers));
        let Some(target) = sqlx::query!(
            r#"
            SELECT l.seller_deployment_id, l.contract_address, l.usdc_address, l.treasury_address
            FROM pay_stellar.ledger_contracts l
            JOIN LATERAL (
                SELECT min(c.created_at) AS oldest FROM pay_stellar.charges c
                WHERE c.seller_deployment_id = l.seller_deployment_id AND c.state = 'admitted'
                  AND c.last_ledger > $4
            ) o ON o.oldest IS NOT NULL
            WHERE l.network = $1 AND l.operator_address = $2
              AND l.seller_deployment_id <> ALL($3)
            ORDER BY o.oldest
            LIMIT 1
            "#,
            self.network().caip2(),
            self.operator_address.as_str(),
            &self.set_aside_ids(),
            sendable,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store("find deployment with admitted charges"))?
        else {
            return Ok(None);
        };
        let deployment = deployment(
            &target.contract_address,
            &target.usdc_address,
            target.treasury_address.as_deref(),
        )?;
        let limit = i64::try_from(self.settings.max_batch.min(MAX_BATCH)).unwrap_or(1);
        let batch = sqlx::query!(
            r#"
            SELECT c.id, c.charge_id, c.last_ledger, c.amount, c.day, b.wallet_address
            FROM pay_stellar.charges c
            JOIN pay_stellar.buyers b ON b.id = c.buyer_id
            WHERE c.seller_deployment_id = $1 AND c.state = 'admitted' AND c.last_ledger > $3
            ORDER BY c.created_at, c.id
            LIMIT $2
            "#,
            target.seller_deployment_id,
            limit,
            sendable,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read admitted charges"))?;
        if batch.is_empty() {
            return Ok(None);
        }
        let requests = batch
            .iter()
            .map(|c| {
                Ok(ChargeRequest {
                    owner: address(&c.wallet_address)?,
                    charge_id: hash32(c.charge_id.clone())?,
                    amount: i128::from(c.amount),
                    last_ledger: u32::try_from(c.last_ledger)
                        .map_err(|_| WorkerError::Corrupt("charge last ledger out of range"))?,
                    day: u64::try_from(c.day)
                        .map_err(|_| WorkerError::Corrupt("charge day out of range"))?,
                })
            })
            .collect::<Result<Vec<_>, WorkerError>>()?;

        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(WorkerError::Randomness)?;
        let unsigned = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: ScAddress::Account(account_id(&self.operator_address)),
                nonce: i64::from_le_bytes(nonce),
                signature_expiration_ledger: latest
                    .saturating_add(self.settings.operator_authorization_ledgers),
                signature: ScVal::Void,
            }),
            root_invocation: deployment.charge_batch_authorization(&requests),
        };
        let signed = sign_entry_with(&unsigned, network_id(self.network()), self.operator.as_ref())
            .await
            .inspect_err(|_| {
                crate::submission::signing_failed(crate::submission::SigningRole::Operator)
            })
            .map_err(WorkerError::Signing)?;
        let function = HostFunction::InvokeContract(deployment.charge_batch_call(&requests));
        let prepared = match self.engine.prepare(Kind::ChargeBatch, function, vec![signed]).await {
            Ok(prepared) => prepared,
            Err(EngineError::RestoreRequired(restore)) => {
                return self.restore(&restore, target.seller_deployment_id).await;
            }
            Err(error @ EngineError::SimulationFailed(_)) => {
                tracing::warn!(
                    seller_deployment_id = %target.seller_deployment_id,
                    error = %error,
                    "network refused the charge batch in simulation"
                );
                self.set_aside(target.seller_deployment_id);
                return Ok(None);
            }
            Err(EngineError::SourceBusy { .. } | EngineError::NoFreeSource) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // The operator signed after the network reached `latest`, so no
        // transaction can include this batch's authorization earlier.
        let prepared = prepared.authorized_from(latest);

        let ids: Vec<Uuid> = batch.iter().map(|c| c.id).collect();
        let span = tracing::Span::current();
        span.record("seller_deployment_id", tracing::field::display(target.seller_deployment_id));
        span.record("submission_id", tracing::field::display(prepared.id));
        span.record("charges", ids.len());
        tracing::info!(charge_ids = ?ids, "charges batched");
        let indexes: Vec<i16> =
            (0..batch.len()).map(|i| i16::try_from(i).unwrap_or(i16::MAX)).collect();
        let mut tx = self.pool.begin().await.map_err(store("begin batch submission"))?;
        match self.engine.record(&mut tx, &prepared).await {
            Ok(_) => {}
            Err(EngineError::SourceBusy { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let linked = sqlx::query!(
            r#"
            UPDATE pay_stellar.charges c
            SET state = 'submitted', submission_id = $1, batch_index = u.batch_index,
                last_error = NULL
            FROM unnest($2::uuid[], $3::int2[]) AS u(id, batch_index)
            WHERE c.id = u.id AND c.state = 'admitted'
            "#,
            prepared.id,
            &ids,
            &indexes,
        )
        .execute(&mut *tx)
        .await
        .map_err(store("link charges"))?;
        // Another worker linked some of these charges first; its batch is the
        // one that settles them.
        if linked.rows_affected() != u64::try_from(ids.len()).unwrap_or(u64::MAX) {
            return Ok(None);
        }
        tx.commit().await.map_err(store("commit batch submission"))?;
        Ok(Some(prepared.id))
    }
}
