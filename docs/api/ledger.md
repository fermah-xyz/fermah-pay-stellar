# Ledger API

Service `fermah.pay.stellar.v1.LedgerService`, defined in
[`proto/fermah/pay/stellar/v1/ledger.proto`](../../proto/fermah/pay/stellar/v1/ledger.proto).

Authentication and scope work as in the [buyer API](buyer.md): the API key
selects the deployment, and a deposit, charge or buyer of another deployment
is reported exactly like one that does not exist.

Amounts are USDC base units: one USDC is `10000000`.

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
database transaction it checks the buyer's available balance, debits it, and
assigns the charge the buyer's next contract sequence number. The worker then
settles admitted charges on-chain, up to 100 in one transaction.

The contract accepts each account's sequence numbers once and in order, and
refuses a charge above the on-chain balance, so the on-chain result cannot
double-charge or overdraw whatever the database holds.

| State | Final | Meaning |
|---|---|---|
| `ADMITTED` | no | debited from the available balance; waiting for a batch |
| `SUBMITTED` | no | in a batch sent to the network |
| `CHARGED` | yes | the contract debited the on-chain balance |
| `REFUSED` | yes | the contract refused it (`insufficient_balance` or `above_limit`); the amount is back in the available balance |
| `QUARANTINED` | no | the contract's answer contradicts the gateway's records (`duplicate`, `out_of_order`, `unknown_account`), or the outcome could not be established; the amount stays debited until an operator resolves it from on-chain evidence to `CHARGED`, `REFUSED` or back to `ADMITTED` |

`GetBalance(buyer_id)` returns `available`, what new charges may still
debit, and `pending_charges`, the sum of admitted and submitted charges.

## Idempotency

`PrepareDeposit` and `CreateCharge` take an `idempotency_key`: 1–128
characters of `[A-Za-z0-9._:@+-]`, unique per deployment and per operation
type.

- Repeating a request with the same key, buyer and amount returns the
  original deposit or charge with `created = false`, whatever happened in
  between. A repeated charge is never debited again, even if the balance is
  now too low for a new one.
- Reusing a key with a different buyer or amount is refused with
  `idempotency_conflict`.

`SubmitDeposit` with the entry already stored for that deposit returns the
deposit unchanged.

## Refusals

| Code | Message | Meaning | Caller action |
|---|---|---|---|
| `INVALID_ARGUMENT` | `invalid_amount` | amount is zero or negative | correct the input |
| `INVALID_ARGUMENT` | `invalid_idempotency_key` | key outside the allowed alphabet or length | correct the input |
| `INVALID_ARGUMENT` | `invalid_buyer_id`, `invalid_deposit_id`, `invalid_charge_id` | not a UUID | correct the input |
| `INVALID_ARGUMENT` | `invalid_authorization_entry` | not a base64 XDR authorization entry | send the entry as returned by the wallet |
| `INVALID_ARGUMENT` | `authorization_mismatch` | the entry differs from the prepared one in more than its signature, or is for another account | sign the prepared entry unchanged |
| `INVALID_ARGUMENT` | `invalid_signature` | the signature does not verify with the buyer's account key | sign with the buyer's wallet |
| `ALREADY_EXISTS` | `idempotency_conflict` | key already used with another buyer or amount | use a new key |
| `NOT_FOUND` | `buyer_not_found`, `deposit_not_found`, `charge_not_found` | no such resource in the caller's deployment | check the ID |
| `FAILED_PRECONDITION` | `ledger_not_configured` | the deployment has no ledger contract bound | bind one |
| `FAILED_PRECONDITION` | `insufficient_balance` | available balance below the amount | deposit first |
| `FAILED_PRECONDITION` | `deposit_expired` | the authorization's expiration ledger has passed | prepare a new deposit |
| `FAILED_PRECONDITION` | `deposit_already_signed` | the deposit holds a different signed entry | nothing to do; poll `GetDeposit` |
| `UNAVAILABLE` | `network_unavailable` | the Stellar RPC could not be reached; nothing was created | retry |
| `INTERNAL` | `internal` | server-side failure; details are logged, not returned | retry later |
