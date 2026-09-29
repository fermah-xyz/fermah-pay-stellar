//! The contract's event stream, read through `getEvents`, as evidence of
//! what the contract did in a range of ledgers.
//!
//! A charge's temporary record lives only until shortly after the charge's
//! last ledger. After that, the `charges` events are the only evidence left
//! of whether a charge was settled, and they are conclusive only when every
//! event of the range in question was read: the node must still retain the
//! range's first ledger, and have reached its last one.

use std::future::Future;

use fermah_pay_stellar_chain::prepaid::{ChainAddress, ChargeEntry, LedgerEvent, ledger_event};
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

/// A `charges` entry found for the charge, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundEntry {
    pub entry: ChargeEntry,
    pub event: EventCursor,
    pub ledger: u32,
    pub transaction_hash: [u8; 32],
}

/// What reading a range of the contract's events established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChargeSearch {
    /// Every event of the range was read; these are all the entries for the
    /// charge in it, oldest first.
    Complete { entries: Vec<FoundEntry>, oldest: u32 },
    /// The node no longer retains the first ledger of the range: events in
    /// ledgers before `oldest` cannot be read from it.
    Pruned { oldest: u32 },
    /// The node has not reached the last ledger of the range.
    Behind { latest: u32 },
    /// A `charges` event of the contract does not decode, so it could hold
    /// the charge unseen.
    Unreadable { event: EventCursor },
}

const PAGE: u32 = 1_000;

fn is_charges_event(topics: &[ScVal]) -> bool {
    matches!(topics.first(), Some(ScVal::Symbol(name)) if name.0.as_slice() == b"charges")
}

/// Every entry `contract` emitted for `owner`'s `charge_id` in ledgers
/// `from..=to`. The first page starts at `from`, which the node refuses if it
/// no longer retains it, and later pages continue from cursors it refuses
/// likewise, so a `Complete` answer covers the whole range.
pub async fn search_charge<L: EventLog>(
    log: &L,
    contract: &[u8; 32],
    owner: &AccountAddress,
    charge_id: &[u8; 32],
    from: u32,
    to: u32,
) -> Result<ChargeSearch, RpcError> {
    let health = log.health().await?;
    if from < health.oldest_ledger {
        return Ok(ChargeSearch::Pruned { oldest: health.oldest_ledger });
    }
    if to > health.latest_ledger {
        return Ok(ChargeSearch::Behind { latest: health.latest_ledger });
    }
    let owner = ChainAddress::Account(owner.clone());
    let mut position = EventsFrom::Ledger(from);
    let mut entries = Vec::new();
    loop {
        let page = log.events(contract, &position, PAGE).await?;
        for event in &page.events {
            if event.ledger > to {
                return Ok(ChargeSearch::Complete { entries, oldest: page.oldest_ledger });
            }
            if event.contract != *contract
                || !event.in_successful_contract_call
                || !is_charges_event(&event.topics)
            {
                continue;
            }
            let Some(LedgerEvent::Charges(settled)) = ledger_event(&event.topics, &event.value)
            else {
                return Ok(ChargeSearch::Unreadable { event: event.id });
            };
            entries.extend(
                settled
                    .into_iter()
                    .filter(|entry| entry.owner == owner && entry.charge_id == *charge_id)
                    .map(|entry| FoundEntry {
                        entry,
                        event: event.id,
                        ledger: event.ledger,
                        transaction_hash: event.transaction_hash,
                    }),
            );
        }
        if page.cursor.first_unread_ledger() > to {
            return Ok(ChargeSearch::Complete { entries, oldest: page.oldest_ledger });
        }
        let next = EventsFrom::Cursor(page.cursor);
        // No progress: this node stops short of the range's end.
        if next == position {
            return Ok(ChargeSearch::Behind { latest: page.latest_ledger });
        }
        position = next;
    }
}
