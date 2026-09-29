# Architecture overview

## Roles

| Role | What it is | Holds |
|---|---|---|
| Buyer | A classic Stellar `G...` account owned by an end user | Its own key and USDC |
| Seller | A product that bills buyers, identified by a seller deployment | API keys for the gateway |
| Operator | Whoever runs the gateway | Sponsor key that pays onboarding fees and reserves; the keys that submit transactions, pay their fees and authorize charges |

Paying a transaction fee on someone's behalf and paying their account
reserves are different things on Stellar. The system keeps them separate,
and its evidence shows each one separately.

## Components

```text
seller ──gRPC + API key──> gateway ──> PostgreSQL <── worker ──> Stellar RPC
                              │                                     ▲
buyer wallet ── signs the deposit authorization the gateway prepared │
                                                                     │
operator tools ──────────────────────────────────────────────────────┘
```

- **Gateway** (`crates/gateway`, binary `fermah-pay-stellar-gateway`): the
  authenticated gRPC API. It resolves each API key to one tenancy scope and
  serves only data in that scope. It prepares the deposit authorizations
  buyers sign, verifies what they return, and admits charges against the
  buyer's available balance. It holds no Stellar signing keys.
- **Worker** (binary `fermah-pay-stellar-worker`): the only process that
  signs and sends transactions. It submits signed deposits and batches of
  admitted charges through the durable submission engine and applies each
  outcome to the rows it settles; see
  [settlement](transactions.md#settlement).
- **Stellar access** (`crates/stellar-chain`): key handling, transaction
  signing, the USDC asset identity per network, a typed Stellar RPC client,
  and sponsored buyer onboarding.
- **Operator tools** (`crates/cli`): schema migration and tenant provisioning
  (`fermah-pay-stellar-admin`), and testnet onboarding
  (`fermah-pay-stellar-testnet`).

## Tenancy

A **product** owns one or more **seller deployments**. Each deployment is
pinned to exactly one Stellar network (`stellar:testnet` or
`stellar:pubnet`). An **API key** belongs to exactly one deployment.

A gateway process serves one network. A request is authenticated by its key,
and the key determines the product, deployment and network the request acts
in; no request field can select or override them. Every buyer query binds
that scope, and a buyer of another deployment is reported exactly like a
buyer that does not exist. See [tenancy and authentication](tenancy.md).

## Deposits and charges

A buyer's balance lives in the deployment's
[prepaid ledger contract](prepaid-contract.md), and its USDC in the
deployment's treasury account. The gateway keeps an admission view of each
buyer:

- a **deposit** is prepared by the gateway, authorized by the buyer's wallet
  with one signature, and submitted and paid for by the worker; once the
  contract has processed it, the amount is added to the buyer's `available`
  balance;
- a **charge** is admitted by the gateway only if `available` covers it: it is
  debited at once and given the buyer's next contract sequence number, then
  settled on-chain in a batch of up to 100.

The database balance is an admission limit, not the authority. The contract
refuses any charge above the on-chain balance and accepts each account's
sequence numbers once and in order, so neither a gateway bug nor a
compromised API process can settle the same charge twice or overdraw a buyer
on-chain.
See the [ledger API](../api/ledger.md).

## Assets

USDC is identified by Circle's issuer on each network. The Stellar Asset
Contract address is derived from that asset and the network passphrase, not
read from configuration; the derivation is tested against the contract
addresses Horizon reports for these assets:

| Network | Issuer | Asset contract |
|---|---|---|
| `stellar:testnet` | `GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5` | `CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA` |
| `stellar:pubnet` | `GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN` | `CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75` |

USDC on Stellar has seven decimals: one USDC is 10,000,000 base units.

## Buyer onboarding without XLM

A new buyer account needs two base reserves for itself and one for its USDC
trustline. One transaction creates the buyer with a zero XLM balance while a
sponsor pays the fee and all three reserves:

1. `BeginSponsoringFutureReserves(buyer)`, source: sponsor
2. `CreateAccount(buyer, starting_balance = 0)`, source: sponsor
3. `ChangeTrust(USDC)`, source: buyer
4. `EndSponsoringFutureReserves`, source: buyer

The sponsor is the transaction source, so it pays the fee. Both the sponsor
and the buyer sign; the buyer's signature is its consent to the sponsorship
and the trustline. After inclusion, the tooling reads the buyer's account and
trustline entries back from the ledger and reports who pays each reserve.
The success check is on the ledger state, not on the transaction the tool
built.
