//! The contract's event stream, read through `getEvents`, as evidence of
//! what the contract did in a range of ledgers.
//!
//! A charge's temporary record lives only until shortly after the charge's
//! last ledger. After that, the `charges` events are the only evidence left
//! of whether a charge was settled, and they are conclusive only when every
//! event of the range in question was read: the node must still retain the
//! range's first ledger, and have reached its last one.

use std::future::Future;

use fermah_pay_stellar_chain::prepaid::{
    ChainAddress, ChargeEntry, LedgerEvent, Outcome, RecurringEntry, RecurringOutcome, ledger_event,
};
use fermah_pay_stellar_chain::rpc::{
    EventCursor, EventPage, EventsFrom, Health, RpcClient, RpcError,
};
use fermah_pay_stellar_chain::stellar_xdr::ScVal;
use fermah_pay_stellar_domain::AccountAddress;

/// The event reads a node answers. `RpcClient` provides them; tests
/// substitute a scripted stream.
pub trait EventLog: Send + Sync {
    /// The ledgers the node retains.
    fn health(&self) -> impl Future<Output = Result<Health, RpcError>> + Send;

    fn events(
        &self,
        contract: &[u8; 32],
        from: &EventsFrom,
        limit: u32,
    ) -> impl Future<Output = Result<EventPage, RpcError>> + Send;
}

impl EventLog for RpcClient {
    async fn health(&self) -> Result<Health, RpcError> {
        self.get_health().await
    }

    async fn events(
        &self,
        contract: &[u8; 32],
        from: &EventsFrom,
        limit: u32,
    ) -> Result<EventPage, RpcError> {
        self.get_events(contract, from, limit).await
    }
}

/// An entry found for a charge, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found<T> {
    pub entry: T,
    pub event: EventCursor,
    pub ledger: u32,
    pub transaction_hash: [u8; 32],
}

/// A `charges` entry found for a prepaid charge.
pub type FoundEntry = Found<ChargeEntry>;

/// A `recurring` entry found for a recurring charge attempt.
pub type FoundRecurring = Found<RecurringEntry>;

impl Found<ChargeEntry> {
    /// Whether the contract settled the charge in this entry. A `duplicate`
    /// or `expired` answer debits nothing and records nothing, whoever sent
    /// it, so it is never the charge's settlement.
    #[must_use]
    pub const fn settles(&self) -> bool {
        !matches!(self.entry.outcome, Outcome::Duplicate | Outcome::Expired)
    }

    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        self.entry.outcome == Outcome::Duplicate
    }
}

impl Found<RecurringEntry> {
    /// As for prepaid charges: `duplicate` and `expired` settle nothing.
    #[must_use]
    pub const fn settles(&self) -> bool {
        !matches!(self.entry.outcome, RecurringOutcome::Duplicate | RecurringOutcome::Expired)
    }

    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        self.entry.outcome == RecurringOutcome::Duplicate
    }
}

/// What reading a range of the contract's events established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Search<T> {
    /// Every event of the range was read; these are all the entries for the
    /// charge in it, oldest first.
    Complete { entries: Vec<Found<T>>, oldest: u32 },
    /// The node no longer retains the first ledger of the range: events in
    /// ledgers before `oldest` cannot be read from it.
    Pruned { oldest: u32 },
    /// The node has not reached the last ledger of the range.
    Behind { latest: u32 },
    /// An event of the searched kind does not decode, so it could hold the
    /// charge unseen.
    Unreadable { event: EventCursor },
}

pub type ChargeSearch = Search<ChargeEntry>;
pub type RecurringSearch = Search<RecurringEntry>;

const PAGE: u32 = 1_000;

fn is_event(topics: &[ScVal], name: &[u8]) -> bool {
    matches!(topics.first(), Some(ScVal::Symbol(symbol)) if symbol.0.as_slice() == name)
}

/// Every entry `contract` emitted for `owner`'s `charge_id` in `charges`
/// events of ledgers `from..=to`.
pub async fn search_charge<L: EventLog>(
    log: &L,
    contract: &[u8; 32],
    owner: &AccountAddress,
    charge_id: &[u8; 32],
    from: u32,
    to: u32,
) -> Result<ChargeSearch, RpcError> {
    let owner = ChainAddress::Account(owner.clone());
    search(log, contract, b"charges", from, to, |event| match event {
        LedgerEvent::Charges(entries) => Some(
            entries
                .into_iter()
                .filter(|entry| entry.owner == owner && entry.charge_id == *charge_id)
                .collect(),
        ),
        _ => None,
    })
    .await
}

/// Every entry `contract` emitted for `owner`'s recurring charge attempt
/// `charge_id` in `recurring` events of ledgers `from..=to`.
pub async fn search_recurring<L: EventLog>(
    log: &L,
    contract: &[u8; 32],
    owner: &AccountAddress,
    charge_id: &[u8; 32],
    from: u32,
    to: u32,
) -> Result<RecurringSearch, RpcError> {
    let owner = ChainAddress::Account(owner.clone());
    search(log, contract, b"recurring", from, to, |event| match event {
        LedgerEvent::Recurring(entries) => Some(
            entries
                .into_iter()
                .filter(|entry| entry.owner == owner && entry.charge_id == *charge_id)
                .collect(),
        ),
        _ => None,
    })
    .await
}

/// The entries `pick` takes from `contract`'s `name` events in ledgers
/// `from..=to`. The first page starts at `from`, which the node refuses if it
/// no longer retains it, and later pages continue from cursors it refuses
/// likewise, so a `Complete` answer covers the whole range.
async fn search<L: EventLog, T>(
    log: &L,
    contract: &[u8; 32],
    name: &[u8],
    from: u32,
    to: u32,
    pick: impl Fn(LedgerEvent) -> Option<Vec<T>>,
) -> Result<Search<T>, RpcError> {
    let health = log.health().await?;
    if from < health.oldest_ledger {
        return Ok(Search::Pruned { oldest: health.oldest_ledger });
    }
    if to > health.latest_ledger {
        return Ok(Search::Behind { latest: health.latest_ledger });
    }
    let mut position = EventsFrom::Ledger(from);
    let mut entries = Vec::new();
    loop {
        let page = log.events(contract, &position, PAGE).await?;
        for event in &page.events {
            if event.ledger > to {
                return Ok(Search::Complete { entries, oldest: page.oldest_ledger });
            }
            if event.contract != *contract
                || !event.in_successful_contract_call
                || !is_event(&event.topics, name)
            {
                continue;
            }
            let Some(found) = ledger_event(&event.topics, &event.value).and_then(&pick) else {
                return Ok(Search::Unreadable { event: event.id });
            };
            entries.extend(found.into_iter().map(|entry| Found {
                entry,
                event: event.id,
                ledger: event.ledger,
                transaction_hash: event.transaction_hash,
            }));
        }
        if page.cursor.first_unread_ledger() > to {
            return Ok(Search::Complete { entries, oldest: page.oldest_ledger });
        }
        let next = EventsFrom::Cursor(page.cursor);
        // No progress: this node stops short of the range's end.
        if next == position {
            return Ok(Search::Behind { latest: page.latest_ledger });
        }
        position = next;
    }
}
