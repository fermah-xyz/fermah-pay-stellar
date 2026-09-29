# Evidence

Records of behaviour observed on a Stellar network, each produced by a tool
or test in this repository and checkable by anyone against a public
explorer or RPC endpoint. Unit tests and local fixtures are never recorded
here as network evidence.

## Record format

Every record is a JSON object with at least:

| Field | Meaning |
|---|---|
| `criterion` | what the record demonstrates |
| `network` | CAIP-2 network |
| `recorded_at` | when the tool wrote the record (UTC) |
| `transaction_hash` | hash of the transaction that produced the state |
| `ledger` | ledger the transaction was included in |
| `expected` | the property being demonstrated |
| `observed` | values read back from the ledger after inclusion |
| `public_explorer_url` | where to inspect the transaction independently |

## Records

### Buyer onboarding with sponsored reserves (testnet)

[`testnet/2026-09-28-buyer-onboarding-sponsored-reserves.json`](testnet/2026-09-28-buyer-onboarding-sponsored-reserves.json),
produced by `fermah-pay-stellar-testnet onboard-buyer`.

Demonstrates that a buyer account can be created holding 0 XLM, with a
Circle testnet USDC trustline, while a separate sponsor account pays the
transaction fee and all reserves. The buyer key was generated for this run
and never funded by Friendbot or any other account; only the sponsor was
funded by Friendbot.

To check it independently:

```bash
curl -s https://horizon-testnet.stellar.org/transactions/<transaction_hash>
# successful: true; fee_account: the sponsor
curl -s https://horizon-testnet.stellar.org/accounts/<buyer>
# sponsor: the sponsor; native balance 0; USDC trustline sponsored by the sponsor
```

The same property is re-checked by the `testnet` workflow whenever code that
reaches the network changes on main, and by `just testnet`.

### Prepaid ledger on testnet

Produced by the commands in the [testnet walkthrough](../quickstart/testnet.md)
against Circle testnet USDC. Every buyer, the treasury, the operator and the
seller hold 0 XLM throughout; a submitter account signs and sequences the
transactions and a separate fee account pays through fee bumps. Each record
lists the outer (fee-bump) and inner transaction hashes.

| Record | Shows |
|---|---|
| [`prepaid-deployment`](testnet/2026-09-29T020953-prepaid-deployment.json) | Wasm upload and contract creation; the deployed code hash equals the Wasm built in CI from the same source |
| [`buyers-funded-100`](testnet/2026-09-29T021058-buyers-funded-100.json) | 100 buyers created with 0 XLM and sponsored reserves, each funded with Circle USDC |
| [`deposits-1-100`](testnet/2026-09-29T022059-deposits-1-100.json) | 100 deposits: USDC moves from each buyer to the separate treasury and the ledger credits the buyer's account; the buyer only signs its authorization entry and pays no fee |
| [`charge-batch-100-seq1`](testnet/2026-09-29T022108-charge-batch-100-seq1.json) | **one transaction charging 100 distinct buyers**, all `Charged` |
| [`charge-buyer-1-seq2` (02:21:14)](testnet/2026-09-29T022114-charge-buyer-1-seq2.json) | a single charge |
| [`charge-buyer-1-seq2` (02:21:16)](testnet/2026-09-29T022116-charge-buyer-1-seq2.json) | the same charge again: refused as `DuplicateCharge` (contract error 110) before submission; the balance is unchanged |
| [`withdraw-buyer-2`](testnet/2026-09-29T022124-withdraw-buyer-2.json) | a withdrawal authorized by the buyer and the treasury: credit debited and USDC moved from the treasury back to the buyer in one invocation |
| [`charge-batch-100-seq2`](testnet/2026-09-29T022134-charge-batch-100-seq2.json) | one 100-entry transaction with mixed outcomes: 98 `Charged`, 1 `Duplicate`, 1 `InsufficientBalance` |
| [`treasury-solvency`](testnet/2026-09-29T022135-treasury-solvency.json) | the treasury's USDC equals buyer liabilities plus unwithdrawn revenue, read from the network: 2.985 USDC held, 0.505 owed to buyers and 2.48 earned by the seller |

Measured fees for a charge on testnet:

| Charges | Transactions | Fee charged | Per charge |
|---:|---:|---:|---:|
| 1 (`charge`) | 1 | 20,318 stroops | 20,318 stroops |
| 100 (`charge_batch`) | 1 | 479,601 stroops | 4,796 stroops |

Batching settles 100 charges in one transaction instead of 100, and costs
about a quarter of the per-charge fee of single charges: most of a charge's
fee is for the ledger entry it writes, which a batch cannot share.

### API end to end on testnet

[`api-end-to-end`](testnet/2026-09-29T022513-api-end-to-end.json), recorded by the `testnet` CI workflow
([run](https://github.com/fermah-xyz/fermah-pay-stellar/actions/runs/36512413007))
with `fermah-pay-stellar-testnet end-to-end` against the deployment above. A
seller's calls go only through the gateway's gRPC API; the gateway and the
settlement worker run under their production database roles.

| Step | Transaction |
|---|---|
| New buyer created with 0 XLM, reserves sponsored | [`c809fec0…`](https://stellar.expert/explorer/testnet/tx/c809fec00818df65b9ae3bde281952793b8cbec64021096cef760c226dfc0c9c) |
| Buyer sent 0.1 Circle USDC from the reserve | [`7a615c9b…`](https://stellar.expert/explorer/testnet/tx/7a615c9b0dfd5e506180fba5a6598dabbfde8014cbd43a925359cfbbf7dba80d) |
| Deposit of 0.1 USDC, authorized by one buyer signature, sent and paid by the worker | [`27655ba4…`](https://stellar.expert/explorer/testnet/tx/27655ba4bb602df3bc3d9658cbae709191a3e2c50bb3a4e5624f6e5f9b2a3b2c) |
| Three charges (0.01, 0.02, 0.03 USDC) settled in one batch | [`27851e05…`](https://stellar.expert/explorer/testnet/tx/27851e05dd1274a8f0a10cbd182b8ca9eeef241857fae78d6f3bdb71cd63668f) |
| The first charge's sequence sent straight to the contract | refused before submission as `DuplicateCharge` (contract error 110); no transaction |

Observed afterwards: the retried charge returned the original charge
(`created: false`), reusing its key with another amount was refused with
`idempotency_conflict`, the gateway's available balance and the contract's
account balance are both 0.04 USDC with charge
sequence 3, and the buyer held 0 XLM before and after.
