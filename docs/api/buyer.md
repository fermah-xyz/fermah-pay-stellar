# Buyer API

Service `fermah.pay.stellar.v1.BuyerService`, defined in
[`proto/fermah/pay/stellar/v1/buyer.proto`](../../proto/fermah/pay/stellar/v1/buyer.proto).

Every call needs `authorization: Bearer <api key>` (see
[tenancy and authentication](../architecture/tenancy.md)). The key
determines the product, seller deployment and network; they are not request
fields.

## `CreateBuyer`

Registers a buyer of the caller's deployment and links its Stellar wallet.

| Field | Rule |
|---|---|
| `external_ref` | 1–128 characters of `[A-Za-z0-9._:@+-]`; unique within the deployment |
| `wallet_address` | canonical classic account (`G...`) or [contract account](ledger.md#contract-accounts) (`C...`) address, unique within the deployment |

Behaviour:

- New reference and new wallet: creates the buyer; `created = true`.
- Same reference with the same wallet as an earlier registration: returns the
  original buyer unchanged; `created = false`. This makes the call safe to
  retry with identical input.
- Same reference with a different wallet, or a wallet already linked to
  another reference: refused with `buyer_conflict`. The existing link is not
  changed.
- A deployment registers at most `PAY_STELLAR_MAX_NEW_BUYERS_PER_DAY` new
  buyers in any 24 hours (1000 by default); beyond that a new buyer is
  refused with `buyer_quota_exceeded`. A repeated registration counts nothing.

A buyer's wallet link cannot be changed after creation.

## `GetBuyer`

Looks up a buyer of the caller's deployment by `buyer_id` or by
`external_ref`. A buyer of any other deployment is reported as
`buyer_not_found`, exactly like an unknown ID.

## `Buyer`

| Field | Meaning |
|---|---|
| `buyer_id` | gateway-assigned UUID |
| `external_ref` | the seller's reference |
| `wallet_address` | canonical `G...` or `C...` address |
| `network` | CAIP-2 network of the deployment |
| `created_at` | RFC 3339 UTC timestamp |

## Refusals

A refusal is a gRPC status whose message is exactly one of these tokens.
Branch on the token, not on free text.

| Code | Message | Meaning | Caller action |
|---|---|---|---|
| `UNAUTHENTICATED` | `unauthenticated` | missing, malformed, unknown, revoked or wrong-network key | fix the credential |
| `INVALID_ARGUMENT` | `invalid_external_ref` | reference outside the allowed alphabet or length | correct the input |
| `INVALID_ARGUMENT` | `invalid_wallet_address` | not a valid account address | correct the input |
| `INVALID_ARGUMENT` | `unsupported_wallet_address` | muxed (`M...`) address | use the underlying `G...` account |
| `INVALID_ARGUMENT` | `invalid_buyer_id` | `buyer_id` is not a UUID | correct the input |
| `INVALID_ARGUMENT` | `missing_lookup` | neither `buyer_id` nor `external_ref` set | set one |
| `ALREADY_EXISTS` | `buyer_conflict` | reference or wallet already bound differently | do not retry with the same input |
| `RESOURCE_EXHAUSTED` | `buyer_quota_exceeded` | the deployment registered its quota of new buyers in the last 24 hours | retry later, or ask the operator for a higher quota |
| `NOT_FOUND` | `buyer_not_found` | no such buyer in the caller's deployment | create it, or check the reference |
| `INTERNAL` | `internal` | server-side failure; details are logged, not returned | retry later |
