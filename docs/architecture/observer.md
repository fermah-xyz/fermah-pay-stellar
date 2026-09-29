# Chain observer and reconciliation

The worker decides each deposit and charge from its own transactions and
from the contract's ledger entries. The observer
([`crates/gateway/src/observer`](../../crates/gateway/src/observer/mod.rs),
binary `fermah-pay-stellar-observer`) checks those decisions independently:
it reads every event the deployment's contract emits, matches each against
the gateway's records, and regularly compares the treasury's USDC, the
contract's totals, the event stream and the database. It holds no key and
cannot change a balance; what it finds is recorded as a finding and logged.
Running it and acting on its findings is described in
[self-hosting: observer](../self-hosting/observer.md).

## Reading the event stream

Events are read with Stellar RPC `getEvents`, filtered to the deployment's
contract. The request and paging rules below were checked against the RPC's
documentation and source and against a public testnet node:

- A read starts either at a ledger (`startLedger`) or right after a previous
  position (`pagination.cursor`); the node refuses both at once.
- A node retains a window of recent ledgers (120,960, about seven days, on
  the public endpoints), reported as `oldestLedger` and `latestLedger`. A
  start before the window, or after its latest ledger, is refused with
  `startLedger must be within the ledger range`. The start is never moved.
- One call scans at most 10,000 ledgers and returns at most `limit` events
  (up to 10,000). Its `cursor` is the last event's id when the page is full,
  and otherwise the end of the scanned window, so a page without events
  still advances the position.
- An event's id, and every cursor, is `<TOID>-<event index>`, where the TOID
  encodes ledger, transaction and operation; ids are unique on a network and
  order as the stream does. The end of a window is the largest position in
  its last ledger.

The observer keeps one position per deployment in
`pay_stellar.observer_cursors`. Each page is stored in one database
transaction that also moves the position: the events, their decoded entries,
the verdicts reached at once, and the new position either all commit or none
do. A second observer on the same deployment finds the position moved and
stores nothing; the position can only move forward. A crash therefore never
skips or repeats an event.

If the position is older than the node's oldest ledger, the events of the
ledgers in between can no longer be read. The observer records an
`event_gap` finding naming them, in the same transaction that moves the
position to the oldest ledger, and continues from there.

Every event is stored in `pay_stellar.chain_events`, `charges` entries also
in `pay_stellar.chain_charge_entries`. The event layouts are those of the
current contract, pinned by the contract's tests against the Soroban host;
an event of this contract that does not decode, for example after an
upgrade, is stored with its raw XDR and reported as `unrecognized_event`.
Pauses, unpauses and limit changes are reported as `admin_change`.

## Matching events against the records

| Event | Matched with | Match |
|---|---|---|
| `deposit` | the deposit with the same account and deposit ID | same amount, and `confirmed` |
| `charges` entry | the charge with the same account and charge identifier | same amount, and a final state that agrees with the outcome |
| `role` (operator, treasury) | the deployment's binding | the binding names the new account, or a later rotation of the same role superseded it |
| `role` (admin, seller) | nothing: the binding does not name them | always reported |
| `withdraw`, `revenue` | nothing: the gateway keeps no withdrawals | recorded for reconciliation |

A charge entry's outcome agrees with the charge when both debited
(`Charged` and `charged`) or both did not (a refusal and `refused` with the
same outcome); `UnknownAccount` also agrees with a charge the worker
quarantined for that answer. `Duplicate` and `Expired` entries debit nothing
and record nothing, and are not compared with the charge's outcome.

The worker may settle a row after the event is visible: a batch's outcome is
applied once its submission is final, and a deposit the buyer included
through their own transaction only after the buyer's authorization lapsed.
A row that is not yet final is therefore not a finding when the event is
observed. It is checked again on every round, and becomes
`charge_unsettled` or `deposit_unsettled` only if it is still not final once
the settlement grace after the event's ledger has passed. Every deposit,
charge entry and rotation ends with exactly one verdict in
`pay_stellar.chain_event_checks`.

## Reconciliation

On an interval, for each deployment, the observer reads the contract's
instance entry, which holds its configuration and its totals, and the
treasury's USDC trustline in one `getLedgerEntries` call. The node answers
that call from one ledger, so the treasury balance, the liabilities and the
revenue describe the same ledger `L`. If the contract names another treasury
than the one read, for example after a rotation, the read is repeated for
the treasury the contract names. The observer then reads events through `L`
and takes the database's sums in one repeatable-read transaction.

| Check | Compared | Finding |
|---|---|---|
| Solvency | treasury USDC against `liabilities + revenue` | `treasury_deficit` (critical) below, `treasury_surplus` (info) above |
| Treasury authorization | the trustline's `AUTHORIZED` flag | `treasury_deauthorized` (critical) without it, or without a trustline |
| Event stream | the contract's totals against the sums of the events through `L`: liabilities = deposits - charged - withdrawn, revenue = charged - revenue withdrawn | `event_totals_mismatch` |
| Database | the contract's totals against the bounds below | `ledger_totals_mismatch` |

The database and the contract legitimately disagree while work is in
flight: a charge admitted, submitted or quarantined may or may not have
debited on-chain, and a deposit awaiting signature, signed or submitted may
already be credited. With `W` and `R` the buyer and revenue withdrawals in
the event stream, and `D_out` and `C_out` the deposits and charges in the
stream that no database row explains (each also a finding of its own), the
contract's totals must lie within:

- liabilities: from `sum(available) + D_out - C_out - W` to that plus the
  pending charges and the pending deposits;
- revenue: from `sum(charged) + C_out - R` to that plus the pending charges.

These bounds follow from how `available` is kept: every confirmed deposit
adds to it, every admitted charge takes from it, and a refused charge gives
its amount back.

The database is read a moment after the chain, and a submission can settle
in between; a withdrawal can land between reads. A discrepancy is therefore
recorded only after it persisted through several consecutive checks
(`PAY_STELLAR_OBSERVER_CONFIRMATIONS`), and once until it clears. The count
is kept in memory, so after a restart a standing discrepancy is recorded
again once it is confirmed again.

The event and database checks rely on the event stream holding the
contract's whole history: they compare the contract's totals with sums over
the events the observer read. An observer started after the contract's first
deposit, or that recorded an `event_gap`, shows the missing history as a
standing `event_totals_mismatch`; start it at the contract's deployment
ledger while the node still retains it.

## What it cannot see

- Events older than every node available to it. The RPC's retention bounds
  how late an observer can start and how long it can be away.
- Money that never touches the contract or the treasury, such as USDC sent
  to a buyer's wallet.
- Anything between checks: a deficit that appears and is repaid within a few
  reconciliation intervals is not recorded.

## Treasury risks it watches

The contract holds no USDC; the treasury does, and two parties other than
the contract can leave it unable to pay:

- **The treasury key** can move USDC out without calling the contract:
  `treasury_deficit`.
- **USDC's issuer** can revoke the authorization of the treasury's trustline
  (Circle's USDC issuer has `AUTH_REVOCABLE` set). The balance still covers
  what the contract owes, but the asset contract then refuses every transfer
  into or out of the treasury, so every deposit and withdrawal fails:
  `treasury_deauthorized`.
