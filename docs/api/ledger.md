# Ledger API

Service `fermah.pay.stellar.v1.LedgerService`, defined in
[`proto/fermah/pay/stellar/v1/ledger.proto`](../../proto/fermah/pay/stellar/v1/ledger.proto).

Authentication and scope work as in the [buyer API](buyer.md): the API key
selects the deployment, and a deposit, charge, withdrawal or buyer of another
deployment is reported exactly like one that does not exist.

Amounts are USDC base units: one USDC is `10000000`.

Recurring charges, which take USDC from the buyer's wallet once per period
under a mandate the buyer signs once, are part of the same service and
described in [recurring charges](recurring.md).

The deployment must be bound to its prepaid ledger contract before deposits
can be prepared (`fermah-pay-stellar-admin bind-ledger`). How the contract
holds balances is described in
[the prepaid ledger contract](../architecture/prepaid-contract.md).

## Deposits

A deposit moves USDC from the buyer's wallet to the deployment's treasury and
credits the buyer's balance on the ledger contract. The buyer signs one
authorization; the gateway submits the transaction and pays its fee, so the
buyer's account needs no XLM.

1. `PrepareDeposit(buyer_id, amount, idempotency_key)` returns a `Deposit` in
   state `AWAITING_SIGNATURE` with `authorization_entry_xdr`: an unsigned
   Soroban authorization entry for the buyer's wallet.
2. The wallet signs the entry, either as an entry (for example with a Stellar
   SDK's `authorizeEntry`) or by signing the 32-byte `signature_payload` with
   the account key and placing the signature in the entry.
3. `SubmitDeposit(deposit_id, signed_authorization_entry_xdr)` verifies the
   entry and stores it; the deposit is `SIGNED`.
4. The worker submits it. Poll `GetDeposit` until the state is final.

What the buyer signs is exactly one call and one sub-call: `deposit(owner,
amount, deposit_id)` on the bound contract, and the USDC `transfer` of the
same amount from the buyer to the bound treasury. The gateway rebuilds this
from the binding and the request; it never accepts a call from the caller.

`SubmitDeposit` accepts the entry only if, with its signature removed, it is
identical to the prepared entry (same account, nonce, expiration ledger and
invocation), and if it carries one valid Ed25519 signature by the buyer's own
account key. Accounts that authorize through additional signers or
thresholds are not supported.

The entry carries `AddressV2` credentials (network protocol 27 and later),
whose signed payload also commits to the buyer's address. The wallet's SDK
must support them: one that knows only legacy `Address` credentials cannot
decode the entry, or signs the legacy payload, which is refused as
`invalid_signature`. `signature_payload` is the `AddressV2` payload, so
signing it directly works with any Ed25519 signer.

The entry is valid until `expiration_ledger`, about an hour after
preparation by default. A deposit whose signature lapses before the network
includes it ends `EXPIRED`; prepare a new one.

| State | Final | Meaning |
|---|---|---|
| `AWAITING_SIGNATURE` | no | waiting for the signed entry |
| `SIGNED` | no | verified; waiting to be submitted |
| `SUBMITTED` | no | in a transaction sent to the network |
| `CONFIRMED` | yes | processed by the contract; `amount` added to the available balance |
| `FAILED` | yes | included as failed, and the contract never processed it |
| `EXPIRED` | yes | the authorization lapsed and the contract never processed it |

A deposit whose transaction fails or is not included stays `SUBMITTED` (or
returns to `SIGNED` to be sent again) until its outcome is certain: the
signed entry could still be included by another transaction until its
expiration ledger, so `FAILED` and `EXPIRED` are only decided afterwards, from
the contract's own record of the deposit. A deposit the buyer includes
through a transaction of their own is confirmed the same way.

## Charges

`CreateCharge(buyer_id, amount, idempotency_key)` admits a charge: in one
database transaction it checks the buyer's available balance and debits it.
The charge's identifier on the contract, `contract_charge_id`, is derived from
the idempotency key, so every retry of the request names the same charge; its
`last_ledger` is the last ledger in which the contract will accept it, about
an hour after admission by default. The worker settles admitted charges
on-chain, up to 98 in one transaction.

The contract records every charge identifier it settles and refuses it
again, and refuses a charge above the on-chain balance or past its last
ledger, so the on-chain result cannot double-charge or overdraw whatever the
database holds. A charge not settled by its last ledger is refused as
`expired` and its amount returned.

| State | Final | Meaning |
|---|---|---|
| `ADMITTED` | no | debited from the available balance; waiting for a batch |
| `SUBMITTED` | no | in a batch sent to the network |
| `CHARGED` | yes | the contract debited the on-chain balance |
| `REFUSED` | yes | nothing was debited: the contract refused it (`insufficient_balance`, `above_limit`, or `above_daily_limit` when the charge would pass the buyer's or the seller's daily limit on the contract) or it expired (`expired`); the amount is back in the available balance |
| `QUARANTINED` | no | the contract's answer contradicts the gateway's records (`unknown_account`), or the outcome could not be established from the contract's records; the amount stays debited until an operator resolves it from on-chain evidence to `CHARGED`, `REFUSED` or back to `ADMITTED` |

`GetBalance(buyer_id)` returns `available`, what new charges may still
debit; `pending_charges`, the sum of admitted and submitted charges; and
`pending_withdrawals`, the sum held for signed withdrawals not yet final.

## Withdrawals

A withdrawal returns unused credit from the treasury to the buyer's wallet,
or to another `G...` account the buyer names; that account needs a USDC
trustline. It follows the same two steps as a deposit:

1. `PrepareWithdrawal(buyer_id, amount, destination, idempotency_key)`
   returns the authorization entry the buyer signs. It covers exactly this
   amount, destination and withdrawal identifier. Nothing is held yet, but a
   request above the available balance is refused at once.
2. `SubmitWithdrawal(withdrawal_id, signed_authorization_entry_xdr)`
   verifies the signed entry. In the same database transaction it holds the
   amount from the available balance, so charges admitted afterwards cannot
   spend it. It is refused with `insufficient_balance` if charges admitted
   since the prepare step left too little.

The settlement worker adds the treasury's authorization and sends the
withdrawal. The contract records every withdrawal identifier it processes
and refuses it again, so a withdrawal pays out at most once. If the treasury
cannot pay yet (for example, it is short of USDC until it is topped up),
the withdrawal waits in `SIGNED` and is retried until the buyer's
authorization lapses.

The held amount returns to the available balance only once the withdrawal is
`FAILED` or `EXPIRED`. Like a deposit's outcome, that is decided after the
buyer's authorization has lapsed, from the contract's own record of the
withdrawal. Until then a copy of the signed entry could still be included.

| State | Final | Meaning |
|---|---|---|
| `AWAITING_SIGNATURE` | no | waiting for the signed entry; nothing held |
| `SIGNED` | no | verified and held; waiting to be sent |
| `SUBMITTED` | no | in a transaction sent to the network |
| `CONFIRMED` | yes | processed by the contract; the USDC went to `destination` |
| `FAILED` | yes | included as failed, and the contract never processed it; the amount is back in the available balance |
| `EXPIRED` | yes | the authorization lapsed and the contract never processed it; any held amount is back in the available balance |

## Quotas

Each deposit or withdrawal the worker sends costs the operator a network fee,
and a buyer's first deposit creates contract state the operator pays rent
for, whatever the amount. So the gateway limits, per buyer and per 24 hours,
how many deposits (`PAY_STELLAR_MAX_DEPOSITS_PER_BUYER_PER_DAY`, 10 by
default) and withdrawals (`PAY_STELLAR_MAX_WITHDRAWALS_PER_BUYER_PER_DAY`, 5
by default) may be prepared. It also refuses withdrawals below
`PAY_STELLAR_MIN_WITHDRAWAL` (0.01 USDC by default). Across all buyers of a
deployment it limits deposits too
(`PAY_STELLAR_MAX_DEPOSITS_PER_DEPLOYMENT_PER_DAY`, 2000 by default), so a
flood of new buyers cannot multiply the per-buyer quota; see
[limits](../self-hosting/limits.md). The contract itself
refuses deposits below its `min_deposit`.

Each count is taken in the same transaction as the insert, with the buyer's
row locked, so concurrent requests cannot pass a quota together. A repeated
request with the same idempotency key is answered from its row and counts
nothing.

## Idempotency

`PrepareDeposit`, `CreateCharge` and `PrepareWithdrawal` take an
`idempotency_key`: 1–128 characters of `[A-Za-z0-9._:@+-]`, unique per
deployment and per operation type.

- Repeating a request with the same key, buyer and amount (and, for a
  withdrawal, destination) returns the original deposit, charge or withdrawal
  with `created = false`, whatever happened in between. A repeated charge is
  never debited again, even if the balance is now too low for a new one.
- Reusing a key with a different buyer, amount or destination is refused
  with `idempotency_conflict`.

`SubmitDeposit` or `SubmitWithdrawal` with the entry already stored returns
the deposit or withdrawal unchanged, and holds nothing again.

## Refusals

| Code | Message | Meaning | Caller action |
|---|---|---|---|
| `INVALID_ARGUMENT` | `invalid_amount` | amount is zero or negative | correct the input |
| `INVALID_ARGUMENT` | `invalid_idempotency_key` | key outside the allowed alphabet or length | correct the input |
| `INVALID_ARGUMENT` | `invalid_buyer_id`, `invalid_deposit_id`, `invalid_charge_id`, `invalid_withdrawal_id` | not a UUID | correct the input |
| `INVALID_ARGUMENT` | `invalid_destination` | the withdrawal destination is not a `G...` account address | correct the input |
| `INVALID_ARGUMENT` | `withdrawal_below_minimum` | the withdrawal is below the gateway's minimum | withdraw more |
| `INVALID_ARGUMENT` | `invalid_authorization_entry` | not a base64 XDR authorization entry | send the entry as returned by the wallet |
| `INVALID_ARGUMENT` | `authorization_mismatch` | the entry differs from the prepared one in more than its signature, or is for another account | sign the prepared entry unchanged |
| `INVALID_ARGUMENT` | `invalid_signature` | the signature does not verify with the buyer's account key | sign with the buyer's wallet |
| `ALREADY_EXISTS` | `idempotency_conflict` | key already used with another buyer, amount or destination | use a new key |
| `NOT_FOUND` | `buyer_not_found`, `deposit_not_found`, `charge_not_found`, `withdrawal_not_found` | no such resource in the caller's deployment | check the ID |
| `FAILED_PRECONDITION` | `ledger_not_configured` | the deployment has no ledger contract bound | bind one |
| `FAILED_PRECONDITION` | `insufficient_balance` | available balance below the amount | deposit first, or withdraw less |
| `FAILED_PRECONDITION` | `deposit_expired` | the authorization's expiration ledger has passed | prepare a new deposit |
| `FAILED_PRECONDITION` | `deposit_already_signed` | the deposit holds a different signed entry | nothing to do; poll `GetDeposit` |
| `FAILED_PRECONDITION` | `withdrawal_expired` | the withdrawal's authorization expiration ledger has passed | prepare a new withdrawal |
| `FAILED_PRECONDITION` | `withdrawal_already_signed` | the withdrawal holds a different signed entry | nothing to do; poll `GetWithdrawal` |
| `RESOURCE_EXHAUSTED` | `deposit_quota_exceeded`, `withdrawal_quota_exceeded` | the buyer prepared its quota of deposits or withdrawals in the last 24 hours | retry later |
| `RESOURCE_EXHAUSTED` | `deployment_deposit_quota_exceeded` | the deployment's buyers prepared its quota of deposits in the last 24 hours | retry later |
| `FAILED_PRECONDITION` | `destination_not_allowed` | the withdrawal names an account other than the buyer's wallet, which this gateway does not allow | withdraw to the buyer's wallet |
| `UNAVAILABLE` | `network_unavailable` | the Stellar RPC could not be reached; nothing was created | retry |
| `INTERNAL` | `internal` | server-side failure; details are logged, not returned | retry later |
