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

The treasury key can move USDC without calling the contract. The contract
cannot prevent that, so it cannot guarantee that the treasury holds at least
what it owes. It records `liabilities` (all buyer balances) and `revenue`
(charged, not yet paid out) so that the treasury's USDC balance can be
compared against them.

## Roles

| Role | Authorizes |
|---|---|
| Buyer (account owner) | `deposit`, and its side of `withdraw` |
| Treasury | its side of `withdraw` and `withdraw_revenue` |
| Operator | `charge`, `charge_batch` |
| Seller | its side of `withdraw_revenue` |
| Admin | `pause`, `unpause`, `set_limits`, `upgrade`, and every role rotation |

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
should be kept offline or in hardware.

## What each signer authorizes

Authorization binds a signer to the exact call and its sub-calls:

| Operation | Signer | Signed tree |
|---|---|---|
| `deposit(owner, amount, deposit_id)` | buyer | the call, and under it `usdc.transfer(buyer, treasury, amount)` |
| `withdraw(owner, amount, destination, id)` | buyer | the call only |
| `withdraw(...)` | treasury | the call, and under it `usdc.transfer(treasury, destination, amount)` |
| `charge_batch(charges)` | operator | the call only |

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
| CPU instructions | 62.6 M | 48.8 M | 400 M |
| Memory | 18.4 MB | 14.5 MB | 40 MB |
| Ledger-entry writes | 198 | 140 | 200 |
| Bytes written | 35.3 KB | 25.1 KB | 132 KB |
| Event bytes | 12,232 | 12,232 | 16,384 |

These are local VM measurements, reproduced by `just contract-resources` and
by the `Contract` CI job; they are not network evidence.

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

## Storage lifetime

Every entry the contract writes, and the contract instance on every call
that changes state, administrative calls included, has its time-to-live
extended to about 30 days once fewer than about
7 days remain, so an account that keeps being charged or funded is not
archived between uses. An account that is only read is not extended. An
account idle long enough to be archived is restored by the network inside
its next deposit or charge, which pays the restoration; see
[settlement](transactions.md#settlement).

## Building

`soroban-sdk` 28 requires the stellar CLI to build a contract:

```bash
just contract-build        # target/contract-wasm/fermah_pay_stellar_prepaid.wasm
just contract-resources    # full-batch resource measurement
```
