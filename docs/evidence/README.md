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

The same property is re-checked by the weekly `testnet` workflow and by
`just testnet`.

### Prepaid ledger on testnet

Produced by the commands in the [testnet walkthrough](../quickstart/testnet.md)
against Circle testnet USDC. Every buyer, the treasury, the operator and the
seller hold 0 XLM throughout; a submitter account signs and sequences the
transactions and a separate fee account pays through fee bumps. Each record
lists the outer (fee-bump) and inner transaction hashes.

| Record | Shows |
|---|---|
| [`prepaid-deployment`](testnet/2026-09-28-prepaid-deployment.json) | Wasm upload and contract creation; the deployed code hash equals the Wasm built in CI from the same source |
| [`buyers-funded-100`](testnet/2026-09-28-buyers-funded-100.json) | 100 buyers created with 0 XLM and sponsored reserves, each funded with Circle USDC |
| [`deposits-1-100`](testnet/2026-09-28-deposits-1-100.json) | 100 deposits: USDC moves from each buyer to the separate treasury and the ledger credits the buyer's account; the buyer only signs its authorization entry and pays no fee |
| [`charge-batch-100-seq1`](testnet/2026-09-28T234208-charge-batch-100-seq1.json) | **one transaction charging 100 distinct buyers**, all `Charged` |
| [`charge-buyer-1-seq2` (23:42:13)](testnet/2026-09-28T234213-charge-buyer-1-seq2.json) | a single charge |
| [`charge-buyer-1-seq2` (23:42:15)](testnet/2026-09-28T234215-charge-buyer-1-seq2.json) | the same charge again: refused as `DuplicateCharge` (contract error 110) before submission; the balance is unchanged |
| [`withdraw-buyer-2`](testnet/2026-09-28T234223-withdraw-buyer-2.json) | a withdrawal authorized by the buyer and the treasury: credit debited and USDC moved from the treasury back to the buyer in one invocation |
| [`charge-batch-100-seq2`](testnet/2026-09-28T234233-charge-batch-100-seq2.json) | one 100-entry transaction with mixed outcomes: 98 `Charged`, 1 `Duplicate`, 1 `InsufficientBalance` |
| [`treasury-solvency`](testnet/2026-09-28T234234-treasury-solvency.json) | the treasury's USDC covers buyer liabilities plus unwithdrawn revenue, read from the network; the surplus of 4.965 USDC is exactly what the treasury owes under an earlier deployment of the contract that shares the same treasury account |

Measured fees for a charge on testnet:

| Charges | Transactions | Fee charged | Per charge |
|---:|---:|---:|---:|
| 1 (`charge`) | 1 | 20,305 stroops | 20,305 stroops |
| 100 (`charge_batch`) | 1 | 479,589 stroops | 4,796 stroops |

Batching settles 100 charges in one transaction instead of 100, and costs
about a quarter of the per-charge fee of single charges: most of a charge's
fee is for the ledger entry it writes, which a batch cannot share.

### API end to end on testnet

[`api-end-to-end`](testnet/2026-09-29T011650-api-end-to-end.json), recorded
by `fermah-pay-stellar-testnet end-to-end` against the deployment above. A
seller's calls go only through the gateway's gRPC API; the gateway and the
settlement worker run under their production database roles.

| Step | Transaction |
|---|---|
| New buyer created with 0 XLM, reserves sponsored | [`f7eaad0c…`](https://stellar.expert/explorer/testnet/tx/f7eaad0cbf0257e5976ab7930ed01824cf2cf5556a08baeae40a9a25ae6a6ba2) |
| Buyer sent 0.1 Circle USDC from the reserve | [`c70392b2…`](https://stellar.expert/explorer/testnet/tx/c70392b265e5cf2969be9e9d957bb8acdca5798823cebe0967b32be0d7b69a38) |
| Deposit of 0.1 USDC, authorized by one buyer signature, sent and paid by the worker | [`83457b8a…`](https://stellar.expert/explorer/testnet/tx/83457b8ac1bd6c221e552e82324e9c0b6df050f0fca9f872ec829c2c2f3a7990) |
| Three charges (0.01, 0.02, 0.03 USDC), settled in one batch | [`92f08932…`](https://stellar.expert/explorer/testnet/tx/92f0893285459622fdf470bfe37edb1705497787591452a45c794d6fa7f5bad7) |

Observed afterwards: the retried charge returned the original charge
(`created: false`), reusing its key with another amount was refused with
`idempotency_conflict`, the gateway's available balance and the contract's
account balance are both 0.04 USDC with charge sequence 3, and the buyer
held 0 XLM before and after.

The same flow runs in the `testnet` CI workflow, which adds one check: the
first charge's sequence is sent straight to the contract and refused as
`DuplicateCharge` (contract error 110) before submission, with the account
unchanged. Record
[`api-end-to-end` from CI](testnet/2026-09-29T013349-api-end-to-end.json)
([workflow run](https://github.com/fermah-xyz/fermah-pay-stellar/actions/runs/36508242951)):

| Step | Transaction |
|---|---|
| New buyer created with 0 XLM, reserves sponsored | [`8e5dc41f…`](https://stellar.expert/explorer/testnet/tx/8e5dc41f01d64b401cae23b225ded59817e923c3185190a408cb1c13189f9056) |
| Buyer sent 0.1 Circle USDC from the reserve | [`c67778ce…`](https://stellar.expert/explorer/testnet/tx/c67778ce73032a461da75cd5560433f96955b8e4527e02ff2300ffd6ccf4a0a6) |
| Deposit of 0.1 USDC, authorized by one buyer signature | [`d403b269…`](https://stellar.expert/explorer/testnet/tx/d403b26904e54e4ef94f7a00dfd587cc7e4446a0dad4867774827df10ffb8aa4) |
| Three charges settled in one batch | [`22719a20…`](https://stellar.expert/explorer/testnet/tx/22719a20c32281e4b77a446cee244db5a84c51d2a7091fd0b3077d3ca973d8be) |
| Replayed charge sequence sent to the contract | refused before submission; no transaction |
