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
| [`prepaid-deployment`](testnet/2026-09-29T044603-prepaid-deployment.json) | Wasm upload and contract creation; the deployed code hash equals the Wasm built in CI from the same source |
| [`buyers-funded-100`](testnet/2026-09-29T045256-buyers-funded-100.json) | 100 buyers holding 0 XLM with sponsored reserves, each topped up with Circle USDC |
| [`deposits-1-100`](testnet/2026-09-29T050720-deposits-1-100.json) | 100 deposits: USDC moves from each buyer to the separate treasury and the ledger credits the buyer's account; the buyer only signs its authorization entry, pays no fee and holds 0 XLM before and after |
| [`charge-batch-98-first`](testnet/2026-09-29T050954-charge-batch-98-first.json) | **one transaction charging 98 distinct buyers**, all `Charged`, each under its own charge identifier |
| [`charge-buyer-1-single` (05:10:10)](testnet/2026-09-29T051010-charge-buyer-1-single.json) | a single charge |
| [`charge-buyer-1-single` (05:10:15)](testnet/2026-09-29T051015-charge-buyer-1-single.json) | the same charge identifier again: refused as `DuplicateCharge` (contract error 110) before submission; the balance is unchanged |
| [`withdraw-buyer-2`](testnet/2026-09-29T051027-withdraw-buyer-2.json) | a withdrawal authorized by the buyer and the treasury: credit debited and USDC moved from the treasury back to the buyer in one invocation |
| [`charge-batch-98-second`](testnet/2026-09-29T051043-charge-batch-98-second.json) | one 98-entry transaction with mixed outcomes: 96 `Charged`, 2 `InsufficientBalance` (buyer 1, already charged twice, and buyer 2, after its withdrawal) |
| [`treasury-solvency`](testnet/2026-09-29T051046-treasury-solvency.json) | the treasury's USDC equals buyer liabilities plus unwithdrawn revenue, read from the network: 2.985 USDC held, 0.555 owed to buyers and 2.43 earned by the seller |
| [`charge-batch-98-first-replayed`](testnet/2026-09-29T051328-charge-batch-98-first-replayed.json) | the first batch's 98 charges sent again, included on-chain: every entry `Duplicate`, nothing debited |

Measured fees on testnet:

| Operation | Transactions | Fee charged | Per charge |
|---|---:|---:|---:|
| 1 charge (`charge`) | 1 | 27,109 stroops | 27,109 stroops |
| 98 charges (`charge_batch`) | 1 | 1,154,114 stroops | 11,777 stroops |
| 98 replayed charges, all `Duplicate` | 1 | 262,594 stroops | 2,680 stroops |
| Deposit | 1 | about 649,100 stroops | |
| Withdrawal | 1 | 355,896 stroops | |

Batching settles 98 charges in one transaction instead of 98, at about 2.3
times lower fee per charge than single charges. Most of a charge's fee pays
for the two ledger entries it writes, the buyer's account and the charge's
record, which a batch cannot share. A replayed charge writes nothing.

### API end to end on testnet

[`api-end-to-end`](testnet/2026-09-29T051738-api-end-to-end.json), recorded by the `testnet` CI workflow
([run](https://github.com/fermah-xyz/fermah-pay-stellar/actions/runs/36525388773))
with `fermah-pay-stellar-testnet end-to-end` against the deployment above. A
seller's calls go only through the gateway's gRPC API; the gateway and the
settlement worker run under their production database roles.

| Step | Transaction |
|---|---|
| New buyer created with 0 XLM, reserves sponsored | [`5e0d079e…`](https://stellar.expert/explorer/testnet/tx/5e0d079e476c94994a7392e364007995d7e654deef27a2eab07c7ac8df169f86) |
| Buyer sent 0.1 Circle USDC from the reserve | [`3de86332…`](https://stellar.expert/explorer/testnet/tx/3de863327c8dd8814a42b8b17dd544110f8f08dbce27cf0f62502652731d60ed) |
| Deposit of 0.1 USDC, authorized by one buyer signature, sent and paid by the worker | [`dc747a0a…`](https://stellar.expert/explorer/testnet/tx/dc747a0a4e22722dbd7ade4be3a6b6e0006aac55a9704851eec6ae057ffb1701) |
| Three charges (0.01, 0.02, 0.03 USDC) settled in one batch | [`c4529f29…`](https://stellar.expert/explorer/testnet/tx/c4529f29472926930349a92ea62dba8b4f816856c4b11c8dd3dfde69bf23e984) |
| The first charge sent straight to the contract again | refused before submission as `DuplicateCharge` (contract error 110); no transaction |

Observed afterwards: the retried charge returned the original charge
(`created: false`), reusing its key with another amount was refused with
`idempotency_conflict`, each charge's record on the contract holds
`charged`, the gateway's available balance and the contract's account
balance are both 0.04 USDC, and the buyer held 0 XLM before and after.
