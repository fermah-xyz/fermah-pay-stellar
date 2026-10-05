# Prepaid ledger contract

Source: [`contracts/prepaid`](../../contracts/prepaid/src/lib.rs). One contract
instance serves one seller deployment.

## Custody

The contract records each buyer's prepaid credit and the seller's earned
revenue. It holds no USDC:

- `deposit` moves USDC from the buyer to a separate **treasury** account
  through the USDC Stellar Asset Contract and credits the buyer, in the same
  invocation.
- `withdraw` debits unused credit and moves the USDC from the treasury to the
  destination, in the same invocation.

Either the transfer and the ledger change both happen or neither does: a
deposit the buyer cannot fund, or a withdrawal the treasury cannot fund,
leaves the ledger unchanged.

A withdrawal therefore needs two authorizations: the buyer's, for the amount
and destination, and the treasury's, for the transfer out of the treasury.
A buyer cannot withdraw alone. Prepaid credit is custodial, as on a prepaid
card: the buyer relies on the operator, who holds the treasury's key, to
pay withdrawals out. The [threat model](../security/threat-model.md#an-operator-that-does-not-pay-withdrawals)
lists what bounds and shows that reliance.

Keeping the USDC out of the contract is deliberate. A defect in the
contract's logic cannot move USDC it does not hold, and the treasury can keep
only what operations need while the rest sits in a
[cold reserve](../self-hosting/treasury.md) that needs several signatures.

The treasury key can move USDC without calling the contract. The contract
cannot prevent that, so it cannot guarantee that the treasury holds at least
what it owes. It records `liabilities` (all buyer balances) and `revenue`
(charged, not yet paid out) so that the treasury's USDC balance can be
compared against them, which the [chain observer](observer.md) does.

USDC's issuer can also revoke the authorization of the treasury's trustline
(Circle's USDC issuer has `AUTH_REVOCABLE` set on both networks). The treasury
then still holds the USDC, but the asset contract refuses every transfer to
or from it, so every deposit and withdrawal fails until the authorization is
restored or the treasury is rotated to another account.

## Roles

| Role | Authorizes |
|---|---|
| Buyer (account owner) | `deposit`, and its side of `withdraw` |
| Treasury | its side of `withdraw` and `withdraw_revenue` |
| Operator | `charge`, `charge_batch` |
| Seller | its side of `withdraw_revenue` |
| Admin | `pause`, `unpause`, `set_limits`, `set_daily_limits`, `upgrade`, and every role rotation |

The constructor, which runs atomically with deployment, sets the four roles
and the USDC contract, and refuses to run unless admin, operator, seller and
treasury are four distinct addresses: each authorizes something the others
must not be able to do alone.

Each role can be moved with `set_admin`, `set_operator`, `set_seller` or
`set_treasury`. A rotation needs the admin's authorization and that of the
new holder, so a role cannot be handed to an address nobody controls; it is
refused if it would make two roles share an address or names the current
holder, and it emits a `role` event with the previous and new address. After
`set_operator` the previous operator can no longer charge. The contract holds
no USDC, so `set_treasury` moves nothing: the previous treasury must transfer
its USDC to the new one, which needs a USDC trustline, for the liabilities to
stay covered.

The admin can replace the contract code through `upgrade`, and so holds power
over what every balance means: it is the deployment's root of trust and
should be kept offline or in hardware. Every administrative change is
visible in the contract's events (see [events](#events)), so a pause, a
limit change, a rotation or an upgrade nobody expected can be detected.

## What each signer authorizes

Authorization binds a signer to the exact call and its sub-calls:

| Operation | Signer | Signed tree |
|---|---|---|
| `deposit(owner, amount, deposit_id)` | buyer | the call, and under it `usdc.transfer(buyer, treasury, amount)` |
| `withdraw(owner, amount, destination, id)` | buyer | the call only |
| `withdraw(...)` | treasury | the call, and under it `usdc.transfer(treasury, destination, amount)` |
| `charge_batch(charges)` | operator | the call only |
| `authorize_recurring(owner, mandate_id, amount, period_secs, cycles, live_until)` | buyer | the call, and under it `usdc.approve(buyer, contract, amount × cycles, live_until)` |
| `revoke_recurring(owner)` | buyer | the call, and under it `usdc.approve(buyer, contract, 0, 0)` |
| `charge_recurring_batch(charges)` | operator | the call only |

A deposit signed for another deployment or treasury, a changed amount or a
changed withdrawal destination does not match the signed tree and is refused
before any USDC moves. The tree builders in
[`crates/stellar-chain/src/prepaid.rs`](../../crates/stellar-chain/src/prepaid.rs)
produce exactly these trees; the contract tests sign them with real Ed25519
keys and have the Soroban host verify them.

## Accounts

An account is keyed by its owner's address: one account per owner per
contract, created by the owner's first deposit. Only a deposit the owner
authorizes can create or credit it, so no one can claim a buyer's account
ahead of the buyer. Deposit and withdrawal identifiers are likewise scoped to
their owner: reusing another owner's identifier affects nothing. Amounts are
USDC base units (seven decimals). Deposits and withdrawals are limited to the
range a classic trustline can hold (at most `i64::MAX`).

## Charges and replay protection

A charge is `(owner, charge_id, amount, last_ledger)`: the seller's
32-byte identifier for the charge and the last ledger in which it may be
settled. The contract records every charge it settles, by owner and
identifier, together with the outcome, in temporary storage that lives until
shortly after the charge's last ledger:

| Outcome | Meaning | Recorded |
|---|---|---|
| `Charged` | balance debited, revenue credited | yes |
| `InsufficientBalance` | refused | yes |
| `AboveLimit` | above the per-charge limit | yes |
| `AboveDailyLimit` | would take the account or the seller past a [daily limit](#daily-limits) | yes |
| `UnknownAccount` | no such account | yes |
| `Duplicate` | the identifier is already recorded | no |
| `Expired` | past its last ledger | no |

A charge whose identifier is recorded is a duplicate and never debits again,
even if the buyer has since added funds. A record lives until
`CHARGE_RECORD_GRACE` (720 ledgers, about an hour) after the charge's last
ledger, and a charge may name a last ledger at most `MAX_CHARGE_WINDOW`
(17,280 ledgers, about a day) ahead; by the time a record expires, the charge
it records is past its last ledger and is refused as `Expired`. A replay is
therefore refused at any time. Until then the record also answers, from the
contract itself, what happened to a charge whose transaction's fate is
unknown.

`charge_batch` settles up to 98 charges in one call and returns one outcome
per entry; refused entries do not affect the others. Only a malformed batch
(empty, more than 98 entries, a non-positive amount, or a last ledger beyond
the window) reverts the whole call. `charge` settles a single charge and
reverts on any refusal, recording nothing. Every call emits one `charges`
event listing each entry (owner, identifier, amount, outcome).

### Daily limits

Once the admin calls `set_daily_limits({per_buyer, per_seller})`, a charge is refused as `AboveDailyLimit` if it would take one account's charges, or all accounts' charges together, past that limit within one UTC day of ledger time (`timestamp / 86400`). Both limits must be positive; until the first call there are none, so a contract upgraded from a version without them behaves as before. `get_daily_limits` returns them. Only `Charged` entries count, and the counts start again each day.

The limits are enforced by the contract whatever the gateway admits. They bound what a leaked operator key can move in a day, and what one seller deployment can charge before a person looks.

Each account's count lives in its own entry, which a charge already writes: `Account {balance, day, charged}`. The seller's count lives in the contract instance. A full batch therefore writes no more entries than before.

### Batch size

Stellar limits a transaction to 200 ledger-entry writes and 400 footprint
entries. A charge to a distinct buyer writes two entries, the account and the
record, so 98 such charges write 196 entries, plus the contract instance and
the operator's authorization nonce: 198 writes. At 100 charges the host
refuses the transaction (202 writes). The footprint, about 200 entries, stays
well within its limit of 400: the write limit is the one that binds.

Measured in the Soroban VM with the built Wasm, one `charge_batch` call for
98 distinct buyers uses:

| Resource | All charged | Mixed (20 refused, 19 duplicates) | Per-transaction limit |
|---|---:|---:|---:|
| CPU instructions | 64.2 M | 50.1 M | 400 M |
| Memory | 18.5 MB | 14.6 MB | 40 MB |
| Ledger-entry writes | 198 | 140 | 200 |
| Bytes written | 41.2 KB | 28.7 KB | 132 KB |
| Event bytes | 12,232 | 12,232 | 16,384 |

These are local VM measurements, reproduced by `just contract-resources` and
by the `Contract` CI job; they are not network evidence.

## Recurring charges

A buyer can let the seller charge their wallet on a schedule without signing
each charge. The buyer signs one authorization for
`authorize_recurring(owner, mandate_id, amount, period_secs, cycles,
live_until)`, which records a **mandate** (up to `amount` per period of
`period_secs` of ledger time, for `cycles` periods, never after ledger
`live_until`) and, under the same signature, approves this contract in the
USDC contract to move up to `amount × cycles` of the buyer's USDC until
`live_until`. The first period starts when the mandate is recorded. The
operator then settles each period's charge with `charge_recurring_batch`;
the USDC moves from the buyer's wallet straight to the treasury as seller
revenue. A recurring charge does not touch the buyer's prepaid account or
the contract's liabilities.

The allowance names this contract as spender, so only this contract's code
can use it, and only within a mandate. The USDC contract stops honouring it
after `live_until` by itself. A buyer has at most one mandate per contract,
because the USDC contract keeps one allowance per buyer and spender: a new
`authorize_recurring` replaces the mandate and sets the allowance to the new
mandate's total, so what was left of the old one cannot be spent. The
network bounds how far ahead `live_until` can be (about six months on
testnet and mainnet today); a longer subscription needs a new mandate before
then.

`authorize_recurring` refuses, as `InvalidMandate`, a mandate whose amount
is above the largest charge (no period could be charged in full), and one
with the identifier of the buyer's current mandate: that would start its
periods over under the same identifier, indistinguishable in the events from
charging a period twice. A changed mandate takes a new identifier. The
mandate is kept until its `live_until`, not only the usual 30 days a write
extends an entry by, so a monthly mandate is not archived between charges.

A recurring charge is `{owner, charge_id, mandate_id, cycle, amount,
last_ledger}`: an identifier for this attempt, the mandate and the period it
charges for. Each entry's outcome:

| Outcome | Meaning | Recorded |
|---|---|---|
| `Charged` | USDC moved from the wallet to the treasury; the period is used | yes |
| `NoMandate` | the buyer has no mandate, or a different one | yes |
| `MandateExpired` | past the mandate's `live_until` or its last period | yes |
| `AlreadyCharged` | the period was already charged | yes |
| `NotDue` | the period has not started | yes |
| `PeriodOver` | the period ended without a charge; it is not charged late | yes |
| `AboveMandate` | above the mandate's amount per period | yes |
| `AboveLimit` | above the per-charge limit, which the admin lowered below the mandate's amount after it was recorded | yes |
| `AboveDailyLimit` | would take the seller past its daily limit | yes |
| `AllowanceShort` | the allowance no longer covers it, e.g. the buyer lowered it in the USDC contract | yes |
| `WalletShort` | the wallet holds less USDC than the amount | yes |
| `TransferRefused` | the USDC contract refused for another reason, e.g. a frozen trustline | yes |
| `Duplicate` | this attempt's identifier is already recorded | no |
| `Expired` | past the attempt's last ledger | no |

A period is charged at most once, whatever the attempt identifier; a refused
period can be attempted again under a new identifier while it lasts.
Attempt records use the same lifetime and window as prepaid charge records,
in their own key space. The seller's daily limit counts recurring charges;
the buyer's does not, because it counts what leaves the prepaid account and
the mandate already bounds the wallet.

`revoke_recurring` ends the buyer's mandate and sets the allowance to zero.
It works while the contract is paused, so stopping charges never depends on
the admin. The buyer can also lower the allowance in the USDC contract from
any wallet; charges are then refused as `AllowanceShort`.

`charge_recurring_batch` settles up to 35 charges. The binding limit is the
16,384 bytes of contract events per transaction: each charge adds about 416
bytes between this contract's `recurring` event and the USDC contract's
`transfer` event, so 39 fit. Writes would allow 49 (four per charge: the
mandate, the record, the allowance and the buyer's USDC balance). Measured
for 35 distinct buyers: 30.4 M instructions, 7.3 MB of memory, 143 writes,
34.9 KB written and 14,644 event bytes.

## Events

After construction, every call that changes state emits an event, all but
the upgrade from the contract itself. Topics and data, as the host records
them:

| Call | Topics | Data |
|---|---|---|
| `deposit` | `"deposit"`, owner | `[amount, deposit_id]` |
| `charge`, `charge_batch` | `"charges"` | `[[owner, charge_id, amount, outcome], ...]` |
| `withdraw` | `"withdraw"`, owner | `[destination, amount, withdrawal_id]` |
| `withdraw_revenue` | `"revenue"` | `[destination, amount, withdrawal_id]` |
| `set_admin`, `set_operator`, `set_seller`, `set_treasury` | `"role"`, role name | `[previous, current]` |
| `pause`, `unpause` | `"pause"` | `paused` after the call, even when it did not change |
| `set_limits` | `"limits"` | `[previous, current]`, each `{max_charge, min_deposit}` |
| `set_daily_limits` | `"daily"` | `[previous, current]`, each `{per_buyer, per_seller}`; `previous` is void the first time |
| `authorize_recurring` | `"mandate"`, owner | the mandate `{amount, cycles, live_until, mandate_id, next_cycle, period_secs, start}` |
| `revoke_recurring` | `"revoke"`, owner | the revoked mandate's identifier, or void if there was none |
| `charge_recurring_batch` | `"recurring"` | `[[owner, charge_id, mandate_id, cycle, amount, outcome], ...]` |
| `upgrade` | `"executable_update"`, previous code, new code (system event) | an empty vector |

`upgrade` publishes no event of its own: the Soroban host emits a system
event (type `System`, attributed to the contract) whenever a contract's code
is replaced, naming the previous and the new executable (for Wasm code,
`["Wasm", hash]`). A refused call emits nothing.
[`ledger_event`](../../crates/stellar-chain/src/prepaid.rs) decodes each of
these; the contract tests pin the decoding against the events the host
records.

## Errors

Refusals revert with a contract error; codes start at 101 so they never
coincide with the USDC contract's own codes, which pass through calls into
this contract.

| Code | Error |
|---:|---|
| 101 | `InvalidLimits` |
| 102 | `Paused` |
| 103 | `InvalidAmount` |
| 104 | `BelowMinimumDeposit` |
| 105 | `DepositAlreadyProcessed` |
| 107 | `UnknownAccount` |
| 108 | `InsufficientBalance` |
| 109 | `ChargeAboveLimit` |
| 110 | `DuplicateCharge` |
| 111 | `ChargeExpired` |
| 112 | `EmptyBatch` |
| 113 | `BatchTooLarge` |
| 114 | `WithdrawalAlreadyProcessed` |
| 115 | `InsufficientRevenue` |
| 116 | `Overflow` |
| 117 | `DuplicateRole` |
| 118 | `ChargeWindowTooLong` |
| 119 | `ChargeAboveDailyLimit` |
| 120 | `InvalidMandate` |

## Storage lifetime

Every entry the contract writes, and the contract instance on every call
that changes state, administrative calls included, has its time-to-live
extended to about 30 days once fewer than about
7 days remain, so an account that keeps being charged or funded is not
archived between uses. An account that is only read is not extended. An
account idle long enough to be archived is restored by the network inside
its next deposit or charge, which pays the restoration; see
[settlement](transactions.md#settlement). A contract that goes idle does
not extend itself, so the settlement worker reads its instance's and code's
remaining life and extends both before they could be archived.

## Building

`soroban-sdk` 28 requires the stellar CLI to build a contract:

```bash
just contract-build        # target/contract-wasm/fermah_pay_stellar_prepaid.wasm
just contract-resources    # full-batch resource measurement
```
