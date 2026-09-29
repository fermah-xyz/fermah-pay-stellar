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
| Admin | `pause`, `unpause`, `set_operator`, `set_limits`, `upgrade` |

All roles and the USDC contract are fixed by the constructor, which runs
atomically with deployment. The admin can replace the contract code through
`upgrade` and therefore holds power over what every balance means; it is a
separate key from the operator and the treasury.

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

A charge is `(owner, sequence, amount)`. Each account stores the last
sequence it consumed, and a charge must carry exactly the next one:

| Outcome | Meaning | Sequence consumed |
|---|---|---|
| `Charged` | balance debited, revenue credited | yes |
| `InsufficientBalance` | refused | yes |
| `AboveLimit` | above the per-charge limit | yes |
| `Duplicate` | sequence already consumed | no |
| `OutOfOrder` | a later sequence than the next one | no |
| `UnknownAccount` | no such account | no |

Because refusals consume their sequence, retrying a charge, even after the
buyer adds funds, never debits it: it reports `Duplicate`.

`charge_batch` settles up to 100 charges in one call and returns one outcome
per entry; refused entries do not affect the others. Only a malformed batch
(empty, more than 100 entries, or a non-positive amount) reverts the whole
call. `charge` settles a single charge and reverts on any refusal, consuming
nothing. Every call emits one `charges` event listing each entry with its
outcome.

### Why a sequence per account

The replay state lives inside the account entry, so a charge writes exactly
one ledger entry. Stellar limits a transaction to 200 ledger-entry writes and
16 KiB of events. A separate record per charge identifier would need about 201
writes for 100 charges; measured in the Soroban VM, that variant exceeds the
limits for a batch of 100.

Measured in the Soroban VM with the built Wasm, one `charge_batch` call for
100 distinct buyers uses:

| Resource | All charged | Mixed (20 refused, 20 duplicates) | Per-transaction limit |
|---|---:|---:|---:|
| CPU instructions | 23.0 M | 20.1 M | 400 M |
| Memory | 6.1 MB | 5.3 MB | 40 MB |
| Ledger-entry writes | 102 | 82 | 200 |
| Bytes written | 21.6 KB | 17.4 KB | 132 KB |
| Footprint entries | 104 | 104 | 400 |
| Event bytes | 9,680 | 9,680 | 16,384 |

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
| 111 | `OutOfOrderCharge` |
| 112 | `EmptyBatch` |
| 113 | `BatchTooLarge` |
| 114 | `WithdrawalAlreadyProcessed` |
| 115 | `InsufficientRevenue` |
| 116 | `Overflow` |

## Storage lifetime

Every entry the contract writes, and the contract instance on every state
change, has its time-to-live extended to about 30 days once fewer than about
7 days remain, so an account that keeps being charged or funded is not
archived between uses. An account that is only read is not extended. An
account idle long enough to be archived is restored by the settlement worker
before its next deposit or charge; see
[settlement](transactions.md#settlement).

## Building

`soroban-sdk` 28 requires the stellar CLI to build a contract:

```bash
just contract-build        # target/contract-wasm/fermah_pay_stellar_prepaid.wasm
just contract-resources    # full-batch resource measurement
```
