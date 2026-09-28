# Architecture overview

This page describes the target system and marks which parts exist. Anything
labelled *planned* is design, not a description of running code.

## Roles

| Role | What it is | Holds |
|---|---|---|
| Buyer | A classic Stellar `G...` account owned by an end user | Its own key; USDC before depositing |
| Seller | A product that bills buyers, identified by a seller deployment | API keys for the gateway |
| Operator | Whoever runs the gateway and its workers | Fee-paying and submission keys |
| Treasury | A separate `G...` account that holds deposited USDC (*planned*) | USDC |

Paying a transaction fee on someone's behalf, paying their account reserves,
and authorizing a contract call are three different things on Stellar. The
system keeps them separate, and its evidence shows each one separately.

## Components

```text
seller / facilitator ──gRPC + API key──> gateway ──> PostgreSQL
                                            │
                                            └──> Stellar RPC (reads, submission)
buyer wallet ── signs ──> onboarding / (planned) deposit authorization
```

- **Gateway** (`crates/gateway`): the authenticated gRPC API. It resolves each
  API key to one tenancy scope and serves only data in that scope. It holds
  no Stellar signing keys.
- **Stellar access** (`crates/stellar-chain`): key handling, transaction
  signing, the USDC asset identity per network, a typed Stellar RPC client,
  and sponsored buyer onboarding.
- **Operator tools** (`crates/cli`): schema migration and tenant provisioning
  (`fermah-pay-stellar-admin`), and testnet onboarding
  (`fermah-pay-stellar-testnet`).
- **Prepaid ledger contract**, **submission workers**, **event observer** and
  **reconciler**: *planned*.

## Tenancy

A **product** owns one or more **seller deployments**. Each deployment is
pinned to exactly one Stellar network (`stellar:testnet` or
`stellar:pubnet`). An **API key** belongs to exactly one deployment.

A gateway process serves one network. A request is authenticated by its key,
and the key determines the product, deployment and network the request acts
in; no request field can select or override them. Every buyer query binds
that scope, and a buyer of another deployment is reported exactly like a
buyer that does not exist. See [tenancy and authentication](tenancy.md).

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

## Custody (*planned*)

Deposited USDC moves to a separate treasury `G...` account; the prepaid
contract records each buyer's balance and holds no USDC. The treasury key
can move USDC without calling the contract, so solvency (treasury USDC
balance at least buyer balances plus unwithdrawn seller revenue) is a
monitored property, not one the contract can enforce. This will be
documented in the threat model before any deposit path ships.
