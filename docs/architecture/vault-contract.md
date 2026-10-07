# Prepaid vault contract

Source: [`contracts/vault`](../../contracts/vault/src/lib.rs). One contract
instance serves one seller deployment. It is the successor of the
[prepaid ledger contract](prepaid-contract.md) for mainnet. The gateway and
the worker serve a deployment bound to either (see
[the ledger API](../api/ledger.md#vault-deployments)).

The prepaid ledger keeps the books while a separate treasury account holds
the USDC, so the treasury's key can move every deposit and the operator's
key can charge any buyer up to the admin's limits. The vault removes both.

## Guarantees

For the buyer: no key the operator or the admin holds can move a buyer's
credit except within a daily spending limit the buyer signed, and a buyer
can always leave without the operator.

For the seller: every prepaid charge admitted before a buyer acts alone can
still settle.

| | Prepaid ledger | Vault |
|---|---|---|
| Where the USDC is | the treasury account | the contract's own USDC balance |
| Who can move it | the treasury key | only the contract's code |
| Charge limits | per charge and per day, set by the admin | the same, plus a daily limit each buyer signs |
| Buyer withdrawal | buyer and treasury | buyer and operator, at once; or the buyer alone, after a notice |
| Seller revenue | seller and treasury | seller alone |
| Code upgrade | admin, at once | admin, after a delay longer than the notice |
| Pause | stops every money movement | stops deposits, charges and new mandates; never a way out |

## Custody

The contract holds the USDC. The USDC contract accepts a spend from a
contract address only when that contract is the direct invoker, and the
vault exports no `__check_auth`, so only the vault's own code moves its
balance. Every payout is debited from the ledger in the same invocation as
the transfer, so the USDC the vault holds always equals
`liabilities + revenue` (all buyer balances plus revenue not paid out),
plus anything sent to it directly, which is never credited.

The USDC issuer remains outside the contract's control. Circle's USDC
issuer has `AUTH_REVOCABLE` set and can freeze the vault's balance; it does
not have clawback enabled on testnet or mainnet today. Check the issuer's
flags before deploying.

## Time

Every delay is a number of ledgers, compared with the ledger sequence, as a
charge's last ledger is:

| Constant | Ledgers | About |
|---|---:|---|
| `MAX_CHARGE_WINDOW` | 17,280 | 1 day: the furthest ahead a charge's last ledger may be |
| `CHARGE_RECORD_GRACE` | 720 | 1 hour: how long a charge record outlives that ledger |
| `NOTICE_LEDGERS` | 18,720 | 26 hours: from a buyer's lower limit or exit request to its effect |
| `UPGRADE_DELAY_LEDGERS` | 120,960 | 7 days: from proposing new code to installing it |
| `EXIT_MARGIN_LEDGERS` | 86,400 | 5 days: how long after a proposal an exit request still unlocks first |

The contract does not compile unless `NOTICE_LEDGERS` exceeds
`MAX_CHARGE_WINDOW + CHARGE_RECORD_GRACE` and `UPGRADE_DELAY_LEDGERS`
exceeds `NOTICE_LEDGERS + EXIT_MARGIN_LEDGERS`. Only the daily limits' day is
ledger time (`timestamp / 86400`).

## The buyer's spending limit

Each account has a `cap`: the most the seller may charge it per UTC day of
ledger time. An account starts with none and cannot be charged until the
buyer signs one, with `set_cap(owner, cap)` or with the first deposit
(`deposit(owner, amount, deposit_id, Some(cap))`, one signature).

- A limit at least the one in force applies at once and drops any pending
  lower limit.
- A lower limit applies `NOTICE_LEDGERS` later. Another lower limit replaces
  it: one no lower than the pending one keeps its effective ledger (it can
  only help the seller), anything lower waits a full notice from then.
- Each charge names the UTC day it was admitted in, today or yesterday, and
  counts against that day's share of the limit whenever it settles, so a
  charge admitted within the limit late in a day still settles after
  midnight. A charge naming an older day is refused as `Expired`; one naming
  a later day reverts its batch (`InvalidDay`).
- A charge above what the limit leaves for its day is refused as
  `AboveCap`.

`get_cap` returns the limit in force; the stored `cap` folds in a pending
lower limit only when the account is next written.

Because each day has its own share, a compromised operator key can take up
to the limit for yesterday and again for today: up to twice the limit in 24
hours, from each buyer.

A charge's refusals are checked in this order: a malformed batch reverts
whole (empty, too large, a non-positive amount, a later day, a last ledger
beyond the window); then `Duplicate` (identifier already recorded),
`Expired` (past its last ledger, or naming a day before yesterday),
`UnknownAccount`, `AboveLimit` (above `max_charge`), `InsufficientBalance`,
`AboveCap`, `AboveDailyLimit`.

## Leaving

- **Cooperative withdrawal**, `withdraw(owner, amount, destination, id)`:
  the buyer and the operator authorize it, and it pays at once. The
  operator's signature stands for the gateway having first held the charges
  it already admitted for this buyer.
- **Exit**, `request_exit(owner, amount, destination)` then `exit(owner)`:
  the buyer alone requests it; from `NOTICE_LEDGERS` later anyone may send
  `exit`, which pays the requested amount, or the whole balance if less is
  left, to the destination the buyer signed. A buyer holding no XLM can have
  any wallet or relayer send it. With nothing left to pay, `exit` is refused
  (`NothingToExit`) and the request stays. A new request replaces the
  previous one: one for no more than the pending amount keeps its unlock
  ledger, so a buyer whose destination stopped accepting USDC can name
  another without waiting again; a larger one restarts the notice. Nothing
  but the buyer's own request can change or clear it.

An exit request does not lower the spending limit: until the exit is paid,
charges within the limit still settle. A wallet that means to leave entirely
signs `set_cap(0)` with the request; both take effect after the same
notice.

Exits, withdrawals, limit changes, mandate revocations and revenue payouts
all work while the contract is paused.

## Why the notice keeps the seller whole

The gateway admits a charge at once and the worker settles it later, no
later than its last ledger, at most `MAX_CHARGE_WINDOW` after admission. A
lower limit or an exit takes effect `NOTICE_LEDGERS` after the buyer asks,
which is later than any charge admitted before the request can settle.
Every such charge has settled, or expired, by then.

That covers what the contract decides. The services cover the rest:

- The worker reads the vault's events from a position stored per
  deployment, applying each page's limit and exit events to the buyers'
  rows in the same database transaction that moves the position, so no
  event applies twice.
- Admission counts the lowest limit known, including one the buyer signed
  through the API that is not resolved yet, and keeps an exit's amount free
  from every charge and withdrawal, so the exit pays in full and nothing
  admitted is left without funds. A request stays counted until the events
  past the ledger that included it, or past its authorization's last
  ledger, have been applied.
- Charges are refused while the worker's reading lags the network by more
  than a set number of ledgers, which with the charge window must stay below
  the notice: a limit change or exit made outside the gateway is then known
  before any charge admitted without it could still settle.
- A cooperative withdrawal is co-signed by the operator only for amounts the
  gateway has already held from the available balance.

Recurring mandates are outside this guarantee: a buyer can revoke one at
once, as with the prepaid ledger.

The admin can still cost the seller admitted charges, though never move a
buyer's money: a pause longer than the margin between `NOTICE_LEDGERS` and
the charges' windows lets admitted charges expire before an exit unlocks,
and a lower `max_charge` or daily limit refuses charges admitted under the
higher one, with that outcome recorded.

## Upgrades

`propose_upgrade(wasm_hash)` records the hash, installable
`UPGRADE_DELAY_LEDGERS` later; proposing again restarts the delay, and
`cancel_upgrade` drops it. `upgrade()` takes no argument: it installs
exactly the proposed hash, as Wasm code, once the delay has passed. A buyer
who requests an exit within `EXIT_MARGIN_LEDGERS` of a proposal can exit
before the new code runs.

Soroban also allows a contract's code to be a reference to code another
address manages, which that address can change with no call to the
contract. The vault only ever installs Wasm, and monitoring should alert if
its executable is anything else.

A recurring mandate's allowance belongs to the vault's address, not to its
code, so new code could spend what is left of it from the buyer's wallet.
A buyer with a mandate who wants to be safe from an upgrade revokes it
before the proposal's effective ledger, as well as exiting.

## Roles

| Role | Authorizes |
|---|---|
| Buyer | its deposits, limits, withdrawals (with the operator), exit requests, mandates and revocations |
| Operator | charges, recurring charges, its side of cooperative withdrawals |
| Seller | revenue payouts, and handing the seller role on |
| Admin | pause, limits, daily and launch limits, operator and admin rotation, upgrade proposals |

Admin, operator and seller must be three distinct addresses. The seller
role moves only with the current seller's and the new seller's
authorization, so no other key can redirect revenue.

## Launch limits

`set_launch_limits(Some({max_balance, max_total}))` bounds the balance one
buyer may hold and the total all buyers may hold; deposits past either are
refused as `AboveLaunchLimit`. They only refuse deposits, so they apply at
once, and `None` removes them.

## Charges, records and recurring charges

As in the [prepaid ledger](prepaid-contract.md#charges-and-replay-protection):
charges carry an identifier and a last ledger, batches settle up to 98, and
each settled identifier is recorded in temporary storage until shortly
after its last ledger. A recurring charge moves USDC from the buyer's wallet
into the vault as revenue.

A full batch of 98 distinct buyers, each account carrying a pending lower
limit and an exit request and each charge at the longest window, measured
with the built Wasm (`just contract-resources`): 92.4 M instructions,
26.6 MB of memory, 198 ledger-entry writes against the network's 200,
79.6 KB written and 11,840 event bytes. A charge record's rent grows with
its window, so most of that batch's estimated fee (about 5.4 M stroops, of
which 3.9 M is temporary rent) comes from the day-long windows; with the
gateway's default window of about an hour it is close to the prepaid
ledger's.

## Throughput

Every call writes the contract instance (its totals and the seller's daily
count), so the network applies a deployment's transactions one after
another. A full batch leaves one ledger-entry write of margin under the
network's 200, counting the operator's authorization nonce.

## Events

Besides those of the prepaid ledger: `cap_raised` (owner, limit),
`cap_lowered` (owner, limit, ledger it applies from), `exit_requested`
(owner, amount, destination, unlock ledger), `exit` (owner, destination,
amount paid), `launch` (previous and new launch limits),
`upgrade_proposed` (hash, ledger it is installable from) and
`upgrade_cancelled` (hash).
