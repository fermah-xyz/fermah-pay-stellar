//! Chain observer: reads each bound deployment's contract events through
//! `getEvents`, matches them against the gateway's records, and reconciles
//! the treasury's USDC, the contract's totals and the database.
//!
//! The observer holds no key and writes no balance. It stores every event of
//! the contract once, in order: a page of events and the position after it
//! are stored in one database transaction, and the position only moves
//! forward, so a crash between pages neither skips nor repeats an event.
//! A range the RPC no longer retains is recorded as an `event_gap` finding
//! before reading continues past it.
//!
//! A deposit, a charge entry or an operator or treasury rotation can be seen
//! on-chain before the worker, or an operator, records it. Such a subject is
//! not a finding when it is observed: it is checked again on every round, and
//! becomes one only if the records still disagree once the settlement grace
//! after the event's ledger has passed. Every subject ends with one verdict.

pub mod matching;
mod reconcile;

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use fermah_pay_stellar_chain::prepaid::{ChainAddress, LedgerEvent, Role, ledger_event};
use fermah_pay_stellar_chain::rpc::{
    ContractEventRecord, EventCursor, EventPage, EventsFrom, Health, LedgerEntries, RpcClient,
    RpcError, hex_lower,
};
use fermah_pay_stellar_chain::stellar_xdr::{LedgerKey, Limits, WriteXdr};
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use self::matching::{
    ChargeRow, DepositRow, RecurringRow, Verdict, WithdrawalRow, judge_charge, judge_deposit,
    judge_recurring, judge_role, judge_withdrawal,
};
pub use self::matching::{Finding, FindingKind, Severity};
use crate::events::EventLog;
use crate::submission::Clock;

/// The network reads the observer needs: the event stream, and ledger
/// entries read at one ledger. `RpcClient` provides them; tests substitute a
/// scripted network.
pub trait ChainReader: EventLog {
    /// The entries that exist among `keys`, all read at one ledger.
    fn ledger_entries(
        &self,
        keys: &[LedgerKey],
    ) -> impl Future<Output = Result<LedgerEntries, RpcError>> + Send;
}

impl ChainReader for RpcClient {
    async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, RpcError> {
        self.get_ledger_entries_at(keys).await
    }
}

/// Where the observer starts reading a deployment it has never read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartPosition {
    /// The oldest ledger the RPC retains (about seven days on public nodes).
    Oldest,
    /// The RPC's latest ledger: earlier events are never read.
    Latest,
    /// A given ledger, such as the contract's deployment ledger. Reading
    /// from a ledger the RPC no longer retains records an `event_gap`.
    Ledger(u32),
}

impl FromStr for StartPosition {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "oldest" => Ok(Self::Oldest),
            "latest" => Ok(Self::Latest),
            ledger => ledger
                .parse::<u32>()
                .ok()
                .filter(|l| *l > 0)
                .map(Self::Ledger)
                .ok_or_else(|| format!("`{text}` is not `oldest`, `latest` or a ledger number")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub start: StartPosition,
    /// Events per `getEvents` call, at most 10,000.
    pub page_size: u32,
    /// How long after an event's ledger the records may still disagree with
    /// it before that is a finding. It must exceed the longest time the
    /// worker legitimately takes, which for a deposit a buyer included
    /// through their own transaction is the deposit authorization validity.
    pub settle_within: Duration,
    /// Consecutive reconciliation checks a discrepancy must persist through
    /// before it is recorded.
    pub confirmations: u32,
    /// Pages read for one deployment in one round before moving on.
    pub max_pages_per_round: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ObserverError {
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
    /// The RPC answered something its own paging rules exclude; nothing of
    /// the page is stored.
    #[error("inconsistent event page: {0}")]
    Inconsistent(String),
    #[error("the contract's instance entry is missing or not the prepaid ledger's layout")]
    UnreadableContract,
    #[error("no deployment {0} is bound on this network")]
    UnknownDeployment(Uuid),
    #[error("charges or deposits are in flight, or the books moved while they were read; retry")]
    NotQuiet,
}

fn store(operation: &'static str) -> impl FnOnce(sqlx::Error) -> ObserverError {
    move |source| ObserverError::Store { operation, source }
}

/// A bound deployment, as the observer reads its binding.
#[derive(Clone, Debug)]
pub struct Deployment {
    pub id: Uuid,
    pub contract: [u8; 32],
    pub treasury: AccountAddress,
    pub operator: AccountAddress,
}

/// A read position: after `cursor`, or from `start_ledger` without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Position {
    start_ledger: u32,
    cursor: Option<EventCursor>,
}

impl Position {
    fn request(&self) -> EventsFrom {
        self.cursor.map_or(EventsFrom::Ledger(self.start_ledger), EventsFrom::Cursor)
    }

    /// The ledger the RPC checks against the range it retains.
    fn request_ledger(&self) -> u32 {
        self.cursor.map_or(self.start_ledger, |c| c.ledger())
    }

    fn first_unread_ledger(&self) -> u32 {
        self.cursor.map_or(self.start_ledger, |c| c.first_unread_ledger())
    }

    /// Whether every event of `ledger` has been read.
    fn covers(&self, ledger: u32) -> bool {
        self.first_unread_ledger() > ledger
    }

    fn precedes(&self, id: EventCursor) -> bool {
        self.cursor.map_or(id.ledger() >= self.start_ledger, |c| c < id)
    }
}

/// What one read of a deployment's events did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ingested {
    /// A page was stored; `caught_up` when it reached the RPC's latest
    /// ledger.
    Page { events: usize, caught_up: bool },
    /// The position was older than the RPC retains; reading continues from
    /// its oldest ledger.
    SkippedGap,
    /// Nothing new: at the RPC's latest ledger, or the RPC has not reached
    /// the position yet.
    Idle,
    /// Another observer moved the position first; its page stands.
    Contended,
}

/// A finding with the deployment and event it belongs to, as recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    pub deployment: Uuid,
    pub event: Option<(Uuid, i16)>,
    pub finding: Finding,
}

pub struct Observer<R, K> {
    pool: PgPool,
    chain: R,
    clock: K,
    network: Network,
    settings: Settings,
    /// Each treasury's cold reserve, whose USDC counts towards what the
    /// treasury owes.
    reserves: std::collections::HashMap<AccountAddress, AccountAddress>,
    /// The Wasm hash each contract is expected to run, by contract id.
    expected_code: std::collections::HashMap<[u8; 32], [u8; 32]>,
}

fn i64_of(value: u32) -> i64 {
    i64::from(value)
}

fn u32_of(value: i64) -> Result<u32, ObserverError> {
    u32::try_from(value).map_err(|_| ObserverError::Corrupt("ledger outside u32"))
}

/// Logs a stored finding at the level its severity calls for, with
/// structured fields a log pipeline can alert on, and counts it.
fn log(recorded: &Recorded) {
    let Recorded { deployment, finding, .. } = recorded;
    let (kind, severity, detail) =
        (finding.kind.as_str(), finding.severity.as_str(), finding.detail.to_string());
    metrics::counter!("pay_stellar_findings_total", "kind" => kind, "severity" => severity)
        .increment(1);
    match finding.severity {
        Severity::Critical => {
            tracing::error!(seller_deployment_id = %deployment, kind, severity, detail, "reconciliation finding");
        }
        Severity::Warning => {
            tracing::warn!(seller_deployment_id = %deployment, kind, severity, detail, "reconciliation finding");
        }
        Severity::Info => {
            tracing::info!(seller_deployment_id = %deployment, kind, severity, detail, "reconciliation finding");
        }
    }
}

async fn insert_finding(conn: &mut PgConnection, recorded: &Recorded) -> Result<(), ObserverError> {
    let (event, index) = recorded.event.unzip();
    sqlx::query!(
        r#"
        INSERT INTO pay_stellar.reconciliation_findings
            (seller_deployment_id, kind, severity, chain_event_id, entry_index, detail)
        VALUES ($1, $2, $3, $4, $5, $6::text::jsonb)
        "#,
        recorded.deployment,
        recorded.finding.kind.as_str(),
        recorded.finding.severity.as_str(),
        event,
        index,
        recorded.finding.detail.to_string(),
    )
    .execute(conn)
    .await
    .map_err(store("record finding"))?;
    Ok(())
}

/// How an event is stored: its kind, the fields matching queries, and every
/// decoded field.
struct Described {
    kind: &'static str,
    owner: Option<String>,
    amount: Option<i128>,
    reference: Option<[u8; 32]>,
    payload: Value,
    event: Option<LedgerEvent>,
}

fn describe(record: &ContractEventRecord) -> Described {
    let Some(event) = ledger_event(&record.topics, &record.value) else {
        let xdr = |value: &fermah_pay_stellar_chain::stellar_xdr::ScVal| {
            value.to_xdr_base64(Limits::none()).unwrap_or_default()
        };
        return Described {
            kind: "unrecognized",
            owner: None,
            amount: None,
            reference: None,
            payload: json!({
                "topics": record.topics.iter().map(xdr).collect::<Vec<_>>(),
                "value": xdr(&record.value),
            }),
            event: None,
        };
    };
    let (kind, owner, amount, reference, payload) = match &event {
        LedgerEvent::Deposited { owner, amount, deposit_id } => (
            "deposit",
            Some(owner.to_string()),
            Some(*amount),
            Some(*deposit_id),
            json!({ "owner": owner.to_string(), "amount": amount.to_string(),
                    "deposit_id": hex_lower(deposit_id) }),
        ),
        LedgerEvent::Charges(entries) => (
            "charges",
            None,
            None,
            None,
            json!({ "entries": entries.iter().map(|entry| json!({
                "owner": entry.owner.to_string(),
                "charge_id": hex_lower(&entry.charge_id),
                "amount": entry.amount.to_string(),
                "outcome": entry.outcome.token(),
            })).collect::<Vec<_>>() }),
        ),
        LedgerEvent::Withdrawn { owner, destination, amount, withdrawal_id } => (
            "withdrawal",
            Some(owner.to_string()),
            Some(*amount),
            Some(*withdrawal_id),
            json!({ "owner": owner.to_string(), "destination": destination.to_string(),
                    "amount": amount.to_string(), "withdrawal_id": hex_lower(withdrawal_id) }),
        ),
        LedgerEvent::RevenueWithdrawn { destination, amount, withdrawal_id } => (
            "revenue_withdrawal",
            None,
            Some(*amount),
            Some(*withdrawal_id),
            json!({ "destination": destination.to_string(), "amount": amount.to_string(),
                    "withdrawal_id": hex_lower(withdrawal_id) }),
        ),
        LedgerEvent::RoleChanged { role, previous, current } => (
            "role",
            None,
            None,
            None,
            json!({ "role": role.token(), "previous": previous.to_string(),
                    "current": current.to_string() }),
        ),
        LedgerEvent::PauseChanged { paused } => {
            ("pause", None, None, None, json!({ "paused": paused }))
        }
        LedgerEvent::LimitsChanged { previous, current } => (
            "limits",
            None,
            None,
            None,
            json!({
                "previous": { "min_deposit": previous.min_deposit.to_string(),
                              "max_charge": previous.max_charge.to_string() },
                "current": { "min_deposit": current.min_deposit.to_string(),
                             "max_charge": current.max_charge.to_string() },
            }),
        ),
        LedgerEvent::DailyLimitsChanged { previous, current } => {
            let limits = |l: &fermah_pay_stellar_chain::prepaid::DailyLimits| {
                json!({ "per_buyer": l.per_buyer.to_string(),
                        "per_seller": l.per_seller.to_string() })
            };
            (
                "daily_limits",
                None,
                None,
                None,
                json!({ "previous": previous.as_ref().map(limits), "current": limits(current) }),
            )
        }
        LedgerEvent::MandateAuthorized { owner, mandate } => (
            "mandate",
            Some(owner.to_string()),
            Some(mandate.amount),
            Some(mandate.mandate_id),
            json!({ "owner": owner.to_string(), "mandate_id": hex_lower(&mandate.mandate_id),
                    "amount": mandate.amount.to_string(), "period_secs": mandate.period_secs,
                    "start": mandate.start, "cycles": mandate.cycles,
                    "live_until": mandate.live_until }),
        ),
        LedgerEvent::MandateRevoked { owner, mandate_id } => (
            "revoke",
            Some(owner.to_string()),
            None,
            *mandate_id,
            json!({ "owner": owner.to_string(),
                    "mandate_id": mandate_id.as_ref().map(|id| hex_lower(id)) }),
        ),
        LedgerEvent::Recurring(entries) => (
            "recurring",
            None,
            None,
            None,
            json!({ "entries": entries.iter().map(|entry| json!({
                "owner": entry.owner.to_string(),
                "charge_id": hex_lower(&entry.charge_id),
                "mandate_id": hex_lower(&entry.mandate_id),
                "cycle": entry.cycle,
                "amount": entry.amount.to_string(),
                "outcome": entry.outcome.token(),
            })).collect::<Vec<_>>() }),
        ),
    };
    Described { kind, owner, amount, reference, payload, event: Some(event) }
}

/// Where an event sits, for a finding's detail.
fn locate(detail: &mut Value, event_id: &str, ledger: i64, transaction_hash: &[u8]) {
    detail["event_id"] = json!(event_id);
    detail["ledger"] = json!(ledger);
    detail["transaction_hash"] = json!(hex_lower(transaction_hash));
}

impl<R: ChainReader, K: Clock> Observer<R, K> {
    pub fn new(pool: PgPool, chain: R, clock: K, network: Network, settings: Settings) -> Self {
        Self {
            pool,
            chain,
            clock,
            network,
            settings,
            reserves: std::collections::HashMap::new(),
            expected_code: std::collections::HashMap::new(),
        }
    }

    /// Counts the USDC of each treasury's reserve, keyed by treasury, with
    /// the treasury's own when checking solvency.
    #[must_use]
    pub fn with_reserves(
        mut self,
        reserves: std::collections::HashMap<AccountAddress, AccountAddress>,
    ) -> Self {
        self.reserves = reserves;
        self
    }

    /// Checks, at each reconciliation, that each listed contract runs the
    /// listed Wasm hash, and records `code_changed` while it does not.
    #[must_use]
    pub fn with_expected_code(
        mut self,
        expected: std::collections::HashMap<[u8; 32], [u8; 32]>,
    ) -> Self {
        self.expected_code = expected;
        self
    }

    /// The deployments bound on this observer's network whose binding names
    /// the network's USDC; any other is skipped with an error, since what it
    /// holds could not be reconciled.
    pub async fn deployments(&self) -> Result<Vec<Deployment>, ObserverError> {
        let rows = sqlx::query!(
            r#"
            SELECT seller_deployment_id, contract_address, usdc_address, treasury_address,
                   operator_address
            FROM pay_stellar.ledger_contracts
            WHERE network = $1
            ORDER BY seller_deployment_id
            "#,
            self.network.caip2(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read ledger bindings"))?;
        let usdc = asset_contract_id(&circle_usdc(self.network), self.network);
        let mut deployments = Vec::with_capacity(rows.len());
        for row in rows {
            let contract = stellar_strkey::Contract::from_string(&row.contract_address)
                .map_err(|_| ObserverError::Corrupt("contract address outside the CHECK"))?;
            let bound_usdc = stellar_strkey::Contract::from_string(&row.usdc_address)
                .map_err(|_| ObserverError::Corrupt("USDC address outside the CHECK"))?;
            if bound_usdc.0 != usdc {
                tracing::error!(seller_deployment_id = %row.seller_deployment_id, "binding names another USDC contract; not observed");
                continue;
            }
            let account = |raw: &str| {
                raw.parse().map_err(|_| ObserverError::Corrupt("address outside the CHECK"))
            };
            deployments.push(Deployment {
                id: row.seller_deployment_id,
                contract: contract.0,
                treasury: account(&row.treasury_address)?,
                operator: account(&row.operator_address)?,
            });
        }
        Ok(deployments)
    }

    /// One round: reads every deployment's new events, then checks again
    /// what was waiting for the records. A deployment whose read fails does
    /// not hold up the others; the first error is returned after the round.
    pub async fn observe(&self) -> Result<Vec<Recorded>, ObserverError> {
        let mut first_error = None;
        for deployment in self.deployments().await? {
            if let Err(error) = self.catch_up(&deployment).await {
                tracing::error!(seller_deployment_id = %deployment.id, error = %error, source = ?std::error::Error::source(&error), "reading contract events failed");
                first_error.get_or_insert(error);
            }
        }
        let recorded = self.recheck().await?;
        first_error.map_or(Ok(recorded), Err)
    }

    /// Reads pages until the deployment is caught up with the RPC, or the
    /// round's page budget is spent.
    pub async fn catch_up(&self, deployment: &Deployment) -> Result<(), ObserverError> {
        for _ in 0..self.settings.max_pages_per_round.max(1) {
            match self.ingest(deployment).await? {
                Ingested::Page { caught_up: false, .. } | Ingested::SkippedGap => {}
                Ingested::Page { caught_up: true, .. } | Ingested::Idle | Ingested::Contended => {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    async fn position(
        &self,
        deployment: &Deployment,
        health: &Health,
    ) -> Result<Position, ObserverError> {
        let read = || {
            sqlx::query!(
                r#"
                SELECT start_ledger, cursor FROM pay_stellar.observer_cursors
                WHERE seller_deployment_id = $1
                "#,
                deployment.id,
            )
            .fetch_optional(&self.pool)
        };
        let row = match read().await.map_err(store("read observer position"))? {
            Some(row) => row,
            None => {
                let start = match self.settings.start {
                    StartPosition::Oldest => health.oldest_ledger,
                    StartPosition::Latest => health.latest_ledger,
                    StartPosition::Ledger(ledger) => ledger,
                };
                sqlx::query!(
                    r#"
                    INSERT INTO pay_stellar.observer_cursors
                        (seller_deployment_id, observed_from_ledger, start_ledger)
                    VALUES ($1, $2, $2)
                    ON CONFLICT (seller_deployment_id) DO NOTHING
                    "#,
                    deployment.id,
                    i64_of(start),
                )
                .execute(&self.pool)
                .await
                .map_err(store("create observer position"))?;
                tracing::info!(seller_deployment_id = %deployment.id, start_ledger = start, "observing a new deployment");
                read()
                    .await
                    .map_err(store("read observer position"))?
                    .ok_or(ObserverError::Corrupt("observer position vanished"))?
            }
        };
        let cursor = row
            .cursor
            .as_deref()
            .map(|c| EventCursor::parse(c).ok_or(ObserverError::Corrupt("stored cursor")))
            .transpose()?;
        Ok(Position { start_ledger: u32_of(row.start_ledger)?, cursor })
    }

    /// Moves the position from `from` to `to` if no one else moved it.
    async fn advance(
        conn: &mut PgConnection,
        deployment: Uuid,
        from: &Position,
        to: &Position,
    ) -> Result<bool, ObserverError> {
        let moved = sqlx::query!(
            r#"
            UPDATE pay_stellar.observer_cursors
            SET start_ledger = $2, cursor = $3, updated_at = now()
            WHERE seller_deployment_id = $1 AND start_ledger = $4
              AND cursor IS NOT DISTINCT FROM $5
            "#,
            deployment,
            i64_of(to.start_ledger),
            to.cursor.map(|c| c.to_string()),
            i64_of(from.start_ledger),
            from.cursor.map(|c| c.to_string()),
        )
        .execute(conn)
        .await
        .map_err(store("advance observer position"))?;
        Ok(moved.rows_affected() == 1)
    }

    /// Reads and stores one page of the deployment's events.
    pub async fn ingest(&self, deployment: &Deployment) -> Result<Ingested, ObserverError> {
        let health = self.chain.health().await.map_err(ObserverError::Chain)?;
        let position = self.position(deployment, &health).await?;
        if position.request_ledger() < health.oldest_ledger {
            return self.skip_gap(deployment, &position, health.oldest_ledger).await;
        }
        // A node behind the one that answered before, e.g. behind a load
        // balancer, refuses a position past its latest ledger until it
        // catches up.
        if position.request_ledger() > health.latest_ledger {
            return Ok(Ingested::Idle);
        }
        let page = self
            .chain
            .events(&deployment.contract, &position.request(), self.settings.page_size)
            .await
            .map_err(ObserverError::Chain)?;
        Self::validate(deployment, &position, &page)?;
        let next = Position { cursor: Some(page.cursor), ..position };
        if next == position {
            return Ok(Ingested::Idle);
        }
        let caught_up = page.cursor.ends_ledger() && page.cursor.ledger() >= page.latest_ledger;
        metrics::gauge!("pay_stellar_observer_lag_ledgers", "deployment" => deployment.id.to_string())
            .set(f64::from(page.latest_ledger.saturating_sub(page.cursor.ledger())));
        let Some(recorded) = self.store_page(deployment, &position, &next, &page).await? else {
            return Ok(Ingested::Contended);
        };
        recorded.iter().for_each(log);
        Ok(Ingested::Page { events: page.events.len(), caught_up })
    }

    /// Refuses a page that does not continue from `position` in order.
    fn validate(
        deployment: &Deployment,
        position: &Position,
        page: &EventPage,
    ) -> Result<(), ObserverError> {
        let mut last = None;
        for event in &page.events {
            if event.contract != deployment.contract {
                return Err(ObserverError::Inconsistent(format!(
                    "event {} of another contract",
                    event.id
                )));
            }
            if !position.precedes(event.id) || last.is_some_and(|last| last >= event.id) {
                return Err(ObserverError::Inconsistent(format!(
                    "event {} out of order",
                    event.id
                )));
            }
            last = Some(event.id);
        }
        let behind = match position.cursor {
            Some(cursor) => page.cursor < cursor,
            None => page.cursor.first_unread_ledger() < position.start_ledger,
        };
        if behind || last.is_some_and(|last| page.cursor < last) {
            return Err(ObserverError::Inconsistent(format!("cursor {} moves back", page.cursor)));
        }
        Ok(())
    }

    /// Continues from the RPC's oldest ledger, recording the ledgers that
    /// can no longer be read, if any, in the same transaction.
    async fn skip_gap(
        &self,
        deployment: &Deployment,
        position: &Position,
        oldest: u32,
    ) -> Result<Ingested, ObserverError> {
        let next = Position { start_ledger: oldest, cursor: None };
        let mut tx = self.pool.begin().await.map_err(store("begin gap"))?;
        if !Self::advance(&mut tx, deployment.id, position, &next).await? {
            return Ok(Ingested::Contended);
        }
        let first_missing = position.first_unread_ledger();
        let recorded = (first_missing < oldest).then(|| Recorded {
            deployment: deployment.id,
            event: None,
            finding: Finding::new(
                FindingKind::EventGap,
                Severity::Warning,
                json!({
                    "from_ledger": first_missing,
                    "to_ledger": oldest - 1,
                    "ledgers": oldest - first_missing,
                    "oldest_retained_ledger": oldest,
                }),
            ),
        });
        if let Some(recorded) = &recorded {
            insert_finding(&mut tx, recorded).await?;
        }
        tx.commit().await.map_err(store("commit gap"))?;
        recorded.iter().for_each(log);
        Ok(Ingested::SkippedGap)
    }

    /// Stores the page's events, the verdicts reached at once, and the new
    /// position, in one transaction. `None` if another observer moved the
    /// position first.
    async fn store_page(
        &self,
        deployment: &Deployment,
        from: &Position,
        to: &Position,
        page: &EventPage,
    ) -> Result<Option<Vec<Recorded>>, ObserverError> {
        let mut tx = self.pool.begin().await.map_err(store("begin page"))?;
        // First, so that a second observer waits on this row and then finds
        // the position moved.
        if !Self::advance(&mut tx, deployment.id, from, to).await? {
            return Ok(None);
        }
        let mut recorded = Vec::new();
        for record in &page.events {
            // Emitted inside a call that failed: nothing it describes
            // happened.
            if !record.in_successful_contract_call {
                tracing::warn!(event_id = %record.id, "event of a failed call ignored");
                continue;
            }
            let described = describe(record);
            let event_id = record.id.to_string();
            let ledger = i64_of(record.ledger);
            let stored = sqlx::query_scalar!(
                r#"
                INSERT INTO pay_stellar.chain_events
                    (seller_deployment_id, network, event_id, ledger, ledger_closed_at,
                     transaction_hash, kind, owner, amount, reference, payload)
                VALUES ($1, $2, $3, $4, $5::text::timestamptz, $6, $7, $8, $9::text::numeric,
                        $10, $11::text::jsonb)
                RETURNING id
                "#,
                deployment.id,
                self.network.caip2(),
                event_id,
                ledger,
                record.ledger_closed_at,
                record.transaction_hash.as_slice(),
                described.kind,
                described.owner,
                described.amount.map(|a| a.to_string()),
                described.reference.as_ref().map(<[u8; 32]>::as_slice),
                described.payload.to_string(),
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(store("store event"))?;
            let at = |index: i16, mut finding: Finding| {
                locate(&mut finding.detail, &event_id, ledger, &record.transaction_hash);
                Recorded { deployment: deployment.id, event: Some((stored, index)), finding }
            };
            match &described.event {
                None => {
                    let verdict = Verdict::Finding(Finding::new(
                        FindingKind::UnrecognizedEvent,
                        Severity::Warning,
                        described.payload.clone(),
                    ));
                    conclude(&mut tx, stored, 0, verdict, |f| at(0, f), &mut recorded).await?;
                }
                Some(LedgerEvent::Charges(entries)) => {
                    for (index, entry) in entries.iter().enumerate() {
                        let index = i16::try_from(index)
                            .map_err(|_| ObserverError::Inconsistent("oversized batch".into()))?;
                        sqlx::query!(
                            r#"
                            INSERT INTO pay_stellar.chain_charge_entries
                                (chain_event_id, entry_index, owner, charge_id, amount, outcome)
                            VALUES ($1, $2, $3, $4, $5::text::numeric, $6)
                            "#,
                            stored,
                            index,
                            entry.owner.to_string(),
                            entry.charge_id.as_slice(),
                            entry.amount.to_string(),
                            entry.outcome.token(),
                        )
                        .execute(&mut *tx)
                        .await
                        .map_err(store("store charge entry"))?;
                        let row =
                            charge_row(&mut tx, deployment.id, &entry.owner, &entry.charge_id)
                                .await?;
                        let verdict = judge_charge(entry, row.as_ref(), false);
                        conclude(&mut tx, stored, index, verdict, |f| at(index, f), &mut recorded)
                            .await?;
                    }
                }
                Some(LedgerEvent::Deposited { owner, amount, deposit_id }) => {
                    let row = deposit_row(&mut tx, deployment.id, owner, deposit_id).await?;
                    let verdict =
                        judge_deposit(&owner.to_string(), *amount, deposit_id, row.as_ref(), false);
                    conclude(&mut tx, stored, 0, verdict, |f| at(0, f), &mut recorded).await?;
                }
                Some(LedgerEvent::RoleChanged { role, previous, current }) => {
                    let bound = bound_role(&mut tx, deployment.id, *role).await?;
                    let verdict = judge_role(
                        *role,
                        &previous.to_string(),
                        &current.to_string(),
                        bound.as_deref(),
                        false,
                        false,
                    );
                    conclude(&mut tx, stored, 0, verdict, |f| at(0, f), &mut recorded).await?;
                }
                Some(LedgerEvent::Withdrawn { owner, destination, amount, withdrawal_id }) => {
                    let row = withdrawal_row(&mut tx, deployment.id, owner, withdrawal_id).await?;
                    let verdict = judge_withdrawal(
                        &owner.to_string(),
                        &destination.to_string(),
                        *amount,
                        withdrawal_id,
                        row.as_ref(),
                    );
                    conclude(&mut tx, stored, 0, verdict, |f| at(0, f), &mut recorded).await?;
                }
                // Only the admin can pause or change limits; each is reported
                // so an unexpected one is noticed.
                Some(
                    LedgerEvent::PauseChanged { .. }
                    | LedgerEvent::LimitsChanged { .. }
                    | LedgerEvent::DailyLimitsChanged { .. },
                ) => {
                    let verdict = Verdict::Finding(Finding::new(
                        FindingKind::AdminChange,
                        Severity::Warning,
                        described.payload.clone(),
                    ));
                    conclude(&mut tx, stored, 0, verdict, |f| at(0, f), &mut recorded).await?;
                }
                Some(LedgerEvent::Recurring(entries)) => {
                    for (index, entry) in entries.iter().enumerate() {
                        let index = i16::try_from(index)
                            .map_err(|_| ObserverError::Inconsistent("oversized batch".into()))?;
                        sqlx::query!(
                            r#"
                            INSERT INTO pay_stellar.chain_recurring_entries
                                (chain_event_id, entry_index, owner, charge_id, mandate_id, cycle,
                                 amount, outcome)
                            VALUES ($1, $2, $3, $4, $5, $6, $7::text::numeric, $8)
                            "#,
                            stored,
                            index,
                            entry.owner.to_string(),
                            entry.charge_id.as_slice(),
                            entry.mandate_id.as_slice(),
                            i64::from(entry.cycle),
                            entry.amount.to_string(),
                            entry.outcome.token(),
                        )
                        .execute(&mut *tx)
                        .await
                        .map_err(store("store recurring entry"))?;
                        let row =
                            recurring_row(&mut tx, deployment.id, &entry.owner, &entry.charge_id)
                                .await?;
                        let verdict = judge_recurring(entry, row.as_ref(), false);
                        conclude(&mut tx, stored, index, verdict, |f| at(index, f), &mut recorded)
                            .await?;
                    }
                }
                // Recorded for reconciliation; the gateway keeps no record of
                // the seller's revenue withdrawals to match them against. A
                // mandate or revocation moves nothing by itself: only the
                // recurring charges made under a mandate do, and each of those
                // is matched.
                Some(
                    LedgerEvent::RevenueWithdrawn { .. }
                    | LedgerEvent::MandateAuthorized { .. }
                    | LedgerEvent::MandateRevoked { .. },
                ) => {}
            }
        }
        tx.commit().await.map_err(store("commit page"))?;
        Ok(Some(recorded))
    }

    /// Checks again every observed subject still waiting for the records,
    /// and records a verdict for those now decided.
    pub async fn recheck(&self) -> Result<Vec<Recorded>, ObserverError> {
        let now = self.clock.now();
        let grace = time::Duration::try_from(self.settings.settle_within)
            .map_err(|_| ObserverError::Corrupt("settlement grace out of range"))?;
        let overdue = |closed_at: OffsetDateTime| closed_at + grace <= now;
        let mut recorded = Vec::new();

        let entries = sqlx::query!(
            r#"
            SELECT e.id, e.seller_deployment_id, e.event_id, e.ledger, e.transaction_hash,
                   e.ledger_closed_at, ce.entry_index, ce.owner, ce.charge_id,
                   ce.amount::text AS "amount!", ce.outcome
            FROM pay_stellar.chain_charge_entries ce
            JOIN pay_stellar.chain_events e ON e.id = ce.chain_event_id
            WHERE e.network = $1
              AND NOT EXISTS (SELECT 1 FROM pay_stellar.chain_event_checks k
                              WHERE k.chain_event_id = ce.chain_event_id
                                AND k.entry_index = ce.entry_index)
            ORDER BY e.event_id, ce.entry_index
            LIMIT 1000
            "#,
            self.network.caip2(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read unsettled charge entries"))?;
        for row in entries {
            let entry = fermah_pay_stellar_chain::prepaid::ChargeEntry {
                owner: chain_address(&row.owner)?,
                charge_id: row
                    .charge_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| ObserverError::Corrupt("charge id is not 32 bytes"))?,
                amount: row.amount.parse().map_err(|_| ObserverError::Corrupt("entry amount"))?,
                outcome: outcome_of(&row.outcome)?,
            };
            let mut tx = self.pool.begin().await.map_err(store("begin recheck"))?;
            let charge =
                charge_row(&mut tx, row.seller_deployment_id, &entry.owner, &entry.charge_id)
                    .await?;
            let verdict = judge_charge(&entry, charge.as_ref(), overdue(row.ledger_closed_at));
            let deployment = row.seller_deployment_id;
            let (event, index) = (row.id, row.entry_index);
            let place = |mut finding: Finding| {
                locate(&mut finding.detail, &row.event_id, row.ledger, &row.transaction_hash);
                Recorded { deployment, event: Some((event, index)), finding }
            };
            decide(&mut tx, event, index, verdict, place, &mut recorded).await?;
            tx.commit().await.map_err(store("commit recheck"))?;
        }

        let recurring = sqlx::query!(
            r#"
            SELECT e.id, e.seller_deployment_id, e.event_id, e.ledger, e.transaction_hash,
                   e.ledger_closed_at, re.entry_index, re.owner, re.charge_id, re.mandate_id,
                   re.cycle, re.amount::text AS "amount!", re.outcome
            FROM pay_stellar.chain_recurring_entries re
            JOIN pay_stellar.chain_events e ON e.id = re.chain_event_id
            WHERE e.network = $1
              AND NOT EXISTS (SELECT 1 FROM pay_stellar.chain_event_checks k
                              WHERE k.chain_event_id = re.chain_event_id
                                AND k.entry_index = re.entry_index)
            ORDER BY e.event_id, re.entry_index
            LIMIT 1000
            "#,
            self.network.caip2(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read unsettled recurring entries"))?;
        for row in recurring {
            let bytes32 = |bytes: &[u8]| -> Result<[u8; 32], ObserverError> {
                bytes.try_into().map_err(|_| ObserverError::Corrupt("identifier is not 32 bytes"))
            };
            let entry = fermah_pay_stellar_chain::prepaid::RecurringEntry {
                owner: chain_address(&row.owner)?,
                charge_id: bytes32(&row.charge_id)?,
                mandate_id: bytes32(&row.mandate_id)?,
                cycle: u32::try_from(row.cycle).map_err(|_| ObserverError::Corrupt("cycle"))?,
                amount: row.amount.parse().map_err(|_| ObserverError::Corrupt("entry amount"))?,
                outcome: recurring_outcome_of(&row.outcome)?,
            };
            let mut tx = self.pool.begin().await.map_err(store("begin recheck"))?;
            let found =
                recurring_row(&mut tx, row.seller_deployment_id, &entry.owner, &entry.charge_id)
                    .await?;
            let verdict = judge_recurring(&entry, found.as_ref(), overdue(row.ledger_closed_at));
            let deployment = row.seller_deployment_id;
            let (event, index) = (row.id, row.entry_index);
            let place = |mut finding: Finding| {
                locate(&mut finding.detail, &row.event_id, row.ledger, &row.transaction_hash);
                Recorded { deployment, event: Some((event, index)), finding }
            };
            decide(&mut tx, event, index, verdict, place, &mut recorded).await?;
            tx.commit().await.map_err(store("commit recheck"))?;
        }

        let subjects = sqlx::query!(
            r#"
            SELECT e.id, e.seller_deployment_id, e.event_id, e.ledger, e.transaction_hash,
                   e.ledger_closed_at, e.kind, e.owner, e.amount::text AS amount, e.reference,
                   e.payload->>'role' AS role, e.payload->>'previous' AS previous,
                   e.payload->>'current' AS current
            FROM pay_stellar.chain_events e
            WHERE e.network = $1 AND e.kind IN ('deposit', 'role')
              AND NOT EXISTS (SELECT 1 FROM pay_stellar.chain_event_checks k
                              WHERE k.chain_event_id = e.id AND k.entry_index = 0)
            ORDER BY e.event_id
            LIMIT 1000
            "#,
            self.network.caip2(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store("read unsettled events"))?;
        for row in subjects {
            let mut tx = self.pool.begin().await.map_err(store("begin recheck"))?;
            let deployment = row.seller_deployment_id;
            let late = overdue(row.ledger_closed_at);
            let verdict = if row.kind == "deposit" {
                let owner = row.owner.as_deref().ok_or(ObserverError::Corrupt("deposit owner"))?;
                let deposit_id: [u8; 32] = row
                    .reference
                    .as_deref()
                    .and_then(|r| r.try_into().ok())
                    .ok_or(ObserverError::Corrupt("deposit id"))?;
                let amount: i128 = row
                    .amount
                    .as_deref()
                    .and_then(|a| a.parse().ok())
                    .ok_or(ObserverError::Corrupt("deposit amount"))?;
                let found =
                    deposit_row(&mut tx, deployment, &chain_address(owner)?, &deposit_id).await?;
                judge_deposit(owner, amount, &deposit_id, found.as_ref(), late)
            } else {
                let role = role_of(row.role.as_deref())?;
                let current = row.current.as_deref().ok_or(ObserverError::Corrupt("role"))?;
                let previous = row.previous.as_deref().ok_or(ObserverError::Corrupt("role"))?;
                let bound = bound_role(&mut tx, deployment, role).await?;
                let superseded = sqlx::query_scalar!(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM pay_stellar.chain_events later
                        WHERE later.seller_deployment_id = $1 AND later.network = $2
                          AND later.kind = 'role' AND later.payload->>'role' = $3
                          AND later.event_id > $4
                    ) AS "superseded!"
                    "#,
                    deployment,
                    self.network.caip2(),
                    role.token(),
                    row.event_id,
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(store("read later rotations"))?;
                judge_role(role, previous, current, bound.as_deref(), superseded, late)
            };
            let place = |mut finding: Finding| {
                locate(&mut finding.detail, &row.event_id, row.ledger, &row.transaction_hash);
                Recorded { deployment, event: Some((row.id, 0)), finding }
            };
            decide(&mut tx, row.id, 0, verdict, place, &mut recorded).await?;
            tx.commit().await.map_err(store("commit recheck"))?;
        }
        recorded.iter().for_each(log);
        Ok(recorded)
    }

    /// Runs rounds until `shutdown` resolves: `poll` apart, and a
    /// reconciliation every `reconcile_every`. A failed round is logged and
    /// retried after a delay that doubles up to `max_backoff`.
    pub async fn run(
        &self,
        poll: Duration,
        reconcile_every: Duration,
        max_backoff: Duration,
        shutdown: impl Future<Output = ()> + Send,
    ) {
        tokio::pin!(shutdown);
        let mut next_reconcile = tokio::time::Instant::now();
        let mut backoff = poll;
        loop {
            // One deployment failing to read must not stop the others'
            // reconciliation; each call reports its first failure after
            // finishing the rest.
            let mut result = self.observe().await.map(drop);
            if tokio::time::Instant::now() >= next_reconcile {
                next_reconcile = tokio::time::Instant::now() + reconcile_every;
                result = result.and(self.reconcile().await.map(drop));
            }
            let wait = match result {
                Ok(()) => {
                    backoff = poll;
                    poll
                }
                Err(error) => {
                    tracing::error!(error = %error, source = ?std::error::Error::source(&error), "observer round failed");
                    backoff = (backoff * 2).min(max_backoff);
                    backoff
                }
            };
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Counts one more check for each discrepancy that is present, and ends
    /// the streak of each that is not, then records the findings of those
    /// that have now lasted `confirmations` checks and were not recorded
    /// yet, all in one transaction: a crash leaves a streak and its finding
    /// together, and a restart does not record a standing discrepancy again.
    async fn record_checks(
        &self,
        deployment: Uuid,
        checks: Vec<(FindingKind, Option<Finding>)>,
    ) -> Result<Vec<Recorded>, ObserverError> {
        let confirmations = i32::try_from(self.settings.confirmations.max(1)).unwrap_or(i32::MAX);
        let mut tx = self.pool.begin().await.map_err(store("begin reconciliation"))?;
        let mut recorded = Vec::new();
        for (kind, finding) in checks {
            let Some(finding) = finding else {
                sqlx::query!(
                    "DELETE FROM pay_stellar.reconciliation_streaks
                     WHERE seller_deployment_id = $1 AND kind = $2",
                    deployment,
                    kind.as_str(),
                )
                .execute(&mut *tx)
                .await
                .map_err(store("end streak"))?;
                continue;
            };
            let streak = sqlx::query!(
                r#"
                INSERT INTO pay_stellar.reconciliation_streaks (seller_deployment_id, kind, checks)
                VALUES ($1, $2, 1)
                ON CONFLICT (seller_deployment_id, kind) DO UPDATE
                SET checks = pay_stellar.reconciliation_streaks.checks + 1, updated_at = now()
                RETURNING checks, recorded
                "#,
                deployment,
                kind.as_str(),
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(store("count streak"))?;
            if streak.checks < confirmations || streak.recorded {
                continue;
            }
            let entry = Recorded { deployment, event: None, finding };
            insert_finding(&mut tx, &entry).await?;
            sqlx::query!(
                "UPDATE pay_stellar.reconciliation_streaks SET recorded = true
                 WHERE seller_deployment_id = $1 AND kind = $2",
                deployment,
                kind.as_str(),
            )
            .execute(&mut *tx)
            .await
            .map_err(store("mark streak recorded"))?;
            recorded.push(entry);
        }
        tx.commit().await.map_err(store("commit reconciliation"))?;
        Ok(recorded)
    }
}

/// Records a verdict reached when the event is first stored.
async fn conclude(
    conn: &mut PgConnection,
    event: Uuid,
    index: i16,
    verdict: Verdict,
    place: impl FnOnce(Finding) -> Recorded,
    recorded: &mut Vec<Recorded>,
) -> Result<(), ObserverError> {
    let (label, finding) = match verdict {
        Verdict::Pending => return Ok(()),
        Verdict::Matched => ("matched", None),
        Verdict::Finding(finding) => ("finding", Some(place(finding))),
    };
    sqlx::query!(
        r#"
        INSERT INTO pay_stellar.chain_event_checks (chain_event_id, entry_index, verdict)
        VALUES ($1, $2, $3)
        "#,
        event,
        index,
        label,
    )
    .execute(&mut *conn)
    .await
    .map_err(store("record verdict"))?;
    if let Some(finding) = finding {
        insert_finding(conn, &finding).await?;
        recorded.push(finding);
    }
    Ok(())
}

/// Records a verdict reached on a later check, unless another observer
/// recorded one first.
async fn decide(
    conn: &mut PgConnection,
    event: Uuid,
    index: i16,
    verdict: Verdict,
    place: impl FnOnce(Finding) -> Recorded,
    recorded: &mut Vec<Recorded>,
) -> Result<(), ObserverError> {
    let (label, finding) = match verdict {
        Verdict::Pending => return Ok(()),
        Verdict::Matched => ("matched", None),
        Verdict::Finding(finding) => ("finding", Some(place(finding))),
    };
    let inserted = sqlx::query!(
        r#"
        INSERT INTO pay_stellar.chain_event_checks (chain_event_id, entry_index, verdict)
        VALUES ($1, $2, $3)
        ON CONFLICT (chain_event_id, entry_index) DO NOTHING
        "#,
        event,
        index,
        label,
    )
    .execute(&mut *conn)
    .await
    .map_err(store("record verdict"))?;
    if inserted.rows_affected() == 1
        && let Some(finding) = finding
    {
        insert_finding(conn, &finding).await?;
        recorded.push(finding);
    }
    Ok(())
}

async fn charge_row(
    conn: &mut PgConnection,
    deployment: Uuid,
    owner: &ChainAddress,
    charge_id: &[u8; 32],
) -> Result<Option<ChargeRow>, ObserverError> {
    let row = sqlx::query!(
        r#"
        SELECT c.id, c.amount, c.state, c.outcome
        FROM pay_stellar.charges c
        JOIN pay_stellar.buyers b ON b.id = c.buyer_id
        WHERE c.seller_deployment_id = $1 AND b.wallet_address = $2 AND c.charge_id = $3
        "#,
        deployment,
        owner.to_string(),
        charge_id.as_slice(),
    )
    .fetch_optional(conn)
    .await
    .map_err(store("read charge"))?;
    Ok(row.map(|r| ChargeRow { id: r.id, amount: r.amount, state: r.state, outcome: r.outcome }))
}

async fn recurring_row(
    conn: &mut PgConnection,
    deployment: Uuid,
    owner: &ChainAddress,
    charge_id: &[u8; 32],
) -> Result<Option<RecurringRow>, ObserverError> {
    let row = sqlx::query!(
        r#"
        SELECT r.id, m.mandate_id, r.cycle, r.amount, r.state, r.outcome
        FROM pay_stellar.recurring_charges r
        JOIN pay_stellar.mandates m ON m.id = r.mandate_row_id
        JOIN pay_stellar.buyers b ON b.id = r.buyer_id
        WHERE r.seller_deployment_id = $1 AND b.wallet_address = $2 AND r.charge_id = $3
        "#,
        deployment,
        owner.to_string(),
        charge_id.as_slice(),
    )
    .fetch_optional(conn)
    .await
    .map_err(store("read recurring charge"))?;
    row.map(|r| {
        Ok(RecurringRow {
            id: r.id,
            mandate_id: r
                .mandate_id
                .as_slice()
                .try_into()
                .map_err(|_| ObserverError::Corrupt("mandate id is not 32 bytes"))?,
            cycle: r.cycle,
            amount: r.amount,
            state: r.state,
            outcome: r.outcome,
        })
    })
    .transpose()
}

async fn deposit_row(
    conn: &mut PgConnection,
    deployment: Uuid,
    owner: &ChainAddress,
    deposit_id: &[u8; 32],
) -> Result<Option<DepositRow>, ObserverError> {
    let row = sqlx::query!(
        r#"
        SELECT d.id, d.amount, d.state
        FROM pay_stellar.deposits d
        JOIN pay_stellar.buyers b ON b.id = d.buyer_id
        WHERE d.seller_deployment_id = $1 AND b.wallet_address = $2 AND d.deposit_id = $3
        "#,
        deployment,
        owner.to_string(),
        deposit_id.as_slice(),
    )
    .fetch_optional(conn)
    .await
    .map_err(store("read deposit"))?;
    Ok(row.map(|r| DepositRow { id: r.id, amount: r.amount, state: r.state }))
}

async fn withdrawal_row(
    conn: &mut PgConnection,
    deployment: Uuid,
    owner: &ChainAddress,
    withdrawal_id: &[u8; 32],
) -> Result<Option<WithdrawalRow>, ObserverError> {
    let row = sqlx::query!(
        r#"
        SELECT w.id, w.amount, w.destination_address, w.state
        FROM pay_stellar.withdrawals w
        JOIN pay_stellar.buyers b ON b.id = w.buyer_id
        WHERE w.seller_deployment_id = $1 AND b.wallet_address = $2 AND w.withdrawal_id = $3
        "#,
        deployment,
        owner.to_string(),
        withdrawal_id.as_slice(),
    )
    .fetch_optional(conn)
    .await
    .map_err(store("read withdrawal"))?;
    Ok(row.map(|r| WithdrawalRow {
        id: r.id,
        amount: r.amount,
        destination: r.destination_address,
        state: r.state,
    }))
}

/// The account the binding names for `role`, for the roles it names.
async fn bound_role(
    conn: &mut PgConnection,
    deployment: Uuid,
    role: Role,
) -> Result<Option<String>, ObserverError> {
    let row = sqlx::query!(
        r#"
        SELECT operator_address, treasury_address FROM pay_stellar.ledger_contracts
        WHERE seller_deployment_id = $1
        "#,
        deployment,
    )
    .fetch_one(conn)
    .await
    .map_err(store("read binding"))?;
    Ok(match role {
        Role::Operator => Some(row.operator_address),
        Role::Treasury => Some(row.treasury_address),
        Role::Admin | Role::Seller => None,
    })
}

fn chain_address(text: &str) -> Result<ChainAddress, ObserverError> {
    if let Ok(account) = text.parse::<AccountAddress>() {
        return Ok(ChainAddress::Account(account));
    }
    stellar_strkey::Contract::from_string(text)
        .map(|c| ChainAddress::Contract(c.0))
        .map_err(|_| ObserverError::Corrupt("stored address"))
}

fn outcome_of(token: &str) -> Result<fermah_pay_stellar_chain::prepaid::Outcome, ObserverError> {
    use fermah_pay_stellar_chain::prepaid::Outcome;
    // Every outcome the contract has, whatever it was when this was written.
    (0..)
        .map_while(Outcome::from_code)
        .find(|outcome| outcome.token() == token)
        .ok_or(ObserverError::Corrupt("stored outcome"))
}

fn recurring_outcome_of(
    token: &str,
) -> Result<fermah_pay_stellar_chain::prepaid::RecurringOutcome, ObserverError> {
    use fermah_pay_stellar_chain::prepaid::RecurringOutcome;
    (0..)
        .map_while(RecurringOutcome::from_code)
        .find(|outcome| outcome.token() == token)
        .ok_or(ObserverError::Corrupt("stored outcome"))
}

fn role_of(token: Option<&str>) -> Result<Role, ObserverError> {
    [Role::Admin, Role::Operator, Role::Seller, Role::Treasury]
        .into_iter()
        .find(|role| Some(role.token()) == token)
        .ok_or(ObserverError::Corrupt("stored role"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_position_parses_the_documented_forms() {
        assert_eq!("oldest".parse(), Ok(StartPosition::Oldest));
        assert_eq!("latest".parse(), Ok(StartPosition::Latest));
        assert_eq!("4924841".parse(), Ok(StartPosition::Ledger(4_924_841)));
        assert!("0".parse::<StartPosition>().is_err());
        assert!("yesterday".parse::<StartPosition>().is_err());
    }

    #[test]
    fn test_position_without_cursor_starts_at_its_ledger() {
        let position = Position { start_ledger: 100, cursor: None };
        assert_eq!(position.request(), EventsFrom::Ledger(100));
        assert!(position.covers(99) && !position.covers(100));
        assert!(position.precedes(EventCursor::parse("0000000429496729600-0000000000").unwrap()));
        assert!(!position.precedes(EventCursor::end_of_ledger(99)));
    }

    #[test]
    fn test_position_after_a_cursor() {
        let end = Position { start_ledger: 1, cursor: Some(EventCursor::end_of_ledger(100)) };
        assert!(end.covers(100) && !end.covers(101));
        let mid = EventCursor::parse("0000000429496729600-0000000003").unwrap();
        let inside = Position { start_ledger: 1, cursor: Some(mid) };
        assert_eq!(mid.ledger(), 100);
        assert!(inside.covers(99) && !inside.covers(100));
    }
}
