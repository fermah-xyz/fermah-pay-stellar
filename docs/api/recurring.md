# Recurring charges

Part of `fermah.pay.stellar.v1.LedgerService`
([`ledger.proto`](../../proto/fermah/pay/stellar/v1/ledger.proto)).
Authentication, scope, amounts and idempotency work as in the
[ledger API](ledger.md).

A buyer authorizes the seller once; the seller then charges the buyer's
wallet once per period, up to an amount per period, without the buyer
signing again. The USDC moves from the buyer's wallet straight to the
treasury as the seller's revenue: it does not touch the buyer's prepaid
balance. How the contract enforces each rule is in
[the prepaid ledger contract](../architecture/prepaid-contract.md#recurring-charges).

## Mandates

A mandate is up to `amount` per period of `period_secs`, for `cycles`
periods. The first period starts when the contract records the mandate.

1. `PrepareMandate { buyer_id, amount, period_secs, cycles, idempotency_key }`
   returns the mandate in `MANDATE_STATE_AWAITING_SIGNATURE` with an
   authorization entry for the buyer's wallet to sign. The entry authorizes
   exactly two calls: the mandate on the ledger contract and, beneath it, the
   USDC contract's `approve` of the ledger contract for `amount × cycles`
   until `live_until_ledger`. Only the ledger contract can use that approval,
   and only within the mandate.
2. `SubmitMandate { mandate_id, signed_authorization_entry_xdr }` stores the
   signed entry after checking it is the prepared one, signed by the buyer's
   key. The worker submits it; the buyer pays no fee and needs no XLM.
3. `GetMandate { mandate_id }` reports its state:

| State | Meaning |
|---|---|
| `AWAITING_SIGNATURE` | waiting for the signed entry |
| `SIGNED`, `SUBMITTED` | on its way to the network |
| `ACTIVE` | the contract holds it; `starts_at` is the first period's start, in Unix seconds of ledger time |
| `FAILED`, `EXPIRED` | the contract never held it, and the buyer's signature has lapsed |
| `REPLACED` | a newer mandate of the same buyer became active; the contract keeps one per buyer |
| `REVOKED` | the buyer revoked it, through this API or any other client |
| `ENDED` | past its last period or its last ledger |

The gateway sets `live_until_ledger` so that every period has ended by then
even if ledgers close every four seconds, rather than every five. The network
refuses an approval beyond its longest entry lifetime (3,110,400 ledgers,
about six months, on testnet in October 2026), so the gateway refuses a
mandate whose periods would need more than `PAY_STELLAR_MAX_MANDATE_LEDGERS`
(3,000,000 by default) with `mandate_too_long`. A longer subscription is a new
mandate before the old one ends. Periods shorter than
`PAY_STELLAR_MIN_MANDATE_PERIOD_SECS` (a day by default) are refused with
`invalid_period`.

A new mandate of a buyer with an active one replaces it once active,
together with its approval: whatever was left of the old approval can no
longer be spent.

## Charging a period

`CreateRecurringCharge { mandate_id, amount, idempotency_key }` charges the
mandate's current period, up to its amount per period. The gateway decides
the current period by the latest ledger's close time, as the contract does;
the worker then settles the charge on-chain in a batch. `GetRecurringCharge`
reports it:

| State | Meaning |
|---|---|
| `ADMITTED`, `SUBMITTED` | on its way to the network |
| `CHARGED` | the USDC moved from the buyer's wallet to the treasury |
| `REFUSED` | nothing moved; `outcome` says why |
| `QUARANTINED` | the evidence contradicts itself; an operator resolves it ([quarantine](../self-hosting/quarantine.md)) |

A refused charge's `outcome` is one of the contract's answers:

| Outcome | Meaning | Seller action |
|---|---|---|
| `wallet_short` | the buyer's wallet holds less USDC than the amount | ask the buyer to top up, then charge the period again |
| `allowance_short` | the buyer lowered the approval in the USDC contract | ask the buyer for a new mandate |
| `transfer_refused` | the USDC contract refused for another reason, e.g. a frozen wallet | contact the buyer |
| `no_mandate` | the contract no longer holds this mandate: the buyer revoked or replaced it outside this API; the mandate is now `REVOKED` | ask the buyer for a new mandate |
| `mandate_expired` | the mandate's last period or last ledger has passed; the mandate is now `ENDED` | ask the buyer for a new mandate |
| `not_due`, `period_over` | the charge reached the contract before or after its period, at a period's edge | charge the current period |
| `above_mandate`, `above_limit`, `above_daily_limit` | above the amount per period, the contract's largest charge, or the seller's daily limit | charge less, or later |
| `expired` | not settled before its last ledger | charge the period again |

A period is charged at most once. While a charge for the period is admitted,
submitted, charged or quarantined, another is refused with
`period_already_charged`; after a refusal the period can be charged again
while it lasts. A period nobody charged is not charged later.

## Revoking

`PrepareRevocation { buyer_id, idempotency_key }` returns an entry for the
buyer's wallet to sign; it ends the buyer's mandate, whichever is current,
and sets the USDC approval to zero. `SubmitRevocation` and `GetRevocation`
work as for mandates. A revocation is `CONFIRMED` once the contract holds no
mandate for the buyer. It works even while the contract is paused. The buyer
can also lower the approval in the USDC contract from any wallet; charges
are then refused with `allowance_short`.

## Quotas

Each mandate and revocation the worker sends costs the operator a fee and
the approval's rent. A buyer may prepare
`PAY_STELLAR_MAX_MANDATE_CHANGES_PER_BUYER_PER_DAY` (5 by default) mandates
and revocations together per 24 hours; beyond that the request is refused
with `mandate_quota_exceeded`. All buyers of a deployment together may
prepare `PAY_STELLAR_MAX_MANDATE_CHANGES_PER_DEPLOYMENT_PER_DAY` (2000 by
default); beyond that the request is refused with
`deployment_mandate_quota_exceeded`.

## Refusals

| Code | Message | Meaning |
|---|---|---|
| `INVALID_ARGUMENT` | `invalid_mandate_id`, `invalid_revocation_id`, `invalid_recurring_charge_id` | not a UUID |
| `INVALID_ARGUMENT` | `invalid_period` | the period is shorter than the gateway allows |
| `INVALID_ARGUMENT` | `invalid_cycles` | no periods |
| `INVALID_ARGUMENT` | `mandate_too_long` | the periods end beyond the furthest last ledger the gateway allows |
| `INVALID_ARGUMENT` | `above_mandate` | the charge is above the mandate's amount per period |
| `ALREADY_EXISTS` | `period_already_charged` | the current period has a charge in flight, charged or quarantined |
| `NOT_FOUND` | `mandate_not_found`, `revocation_not_found`, `recurring_charge_not_found` | no such resource in the caller's deployment |
| `FAILED_PRECONDITION` | `mandate_not_active` | the mandate is not active (not yet, or replaced or revoked) |
| `FAILED_PRECONDITION` | `mandate_ended` | every period of the mandate has passed |
| `FAILED_PRECONDITION` | `mandate_authorization_expired`, `revocation_expired` | the buyer's signature has lapsed |
| `FAILED_PRECONDITION` | `mandate_already_signed`, `revocation_already_signed` | another signed entry is already stored |
| `RESOURCE_EXHAUSTED` | `mandate_quota_exceeded` | the buyer prepared its quota of mandates and revocations in the last 24 hours |
| `RESOURCE_EXHAUSTED` | `deployment_mandate_quota_exceeded` | the deployment's buyers prepared its quota of mandates and revocations in the last 24 hours |

The other refusals (`invalid_amount`, `invalid_signature`,
`authorization_mismatch`, `idempotency_conflict`, `network_unavailable`, …)
mean what they mean in the [ledger API](ledger.md#refusals).
