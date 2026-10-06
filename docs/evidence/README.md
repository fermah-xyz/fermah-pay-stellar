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

## Deployed contracts (testnet)

Every contract the records below were produced on, with the code it runs
today. Each contract's code can be read back from the network and hashed
(`stellar contract fetch --id <contract> --network testnet`), and compared
with the Wasm the `Contract` job of the Rust workflow builds from the same
commit.

| Contract | What it is | Running Wasm (SHA-256) | Records |
|---|---|---|---|
| [`CD3GESMYMJ3MNWNSKS6P7TEDHL5HYEWSGTFX7A3ENDB5MXTQ5TED7PSI`](https://stellar.expert/explorer/testnet/contract/CD3GESMYMJ3MNWNSKS6P7TEDHL5HYEWSGTFX7A3ENDB5MXTQ5TED7PSI) | the prepaid ledger with recurring charges; the deployment the workflows use | `38dfa7d755a3d89e1147633c0adf8f5ebdbd8354082ae21cb6ae6acdcca4d66e` (deployed as `fcdd05c4…`, then upgraded) | from `2026-10-01T022132-prepaid-deployment` on |
| [`CDDFMUZ7RT7RYBLL4MGC3WTR7YEFSCLCZATIJN57XGKV45QCEJE5GNPV`](https://stellar.expert/explorer/testnet/contract/CDDFMUZ7RT7RYBLL4MGC3WTR7YEFSCLCZATIJN57XGKV45QCEJE5GNPV) | the prepaid ledger before recurring charges | `02e59475f67a8e9204c2aa6029051ff9ba70254ef69be76b97230d6cedfb3d95` (deployed as `02d80b2f…`, upgraded to `78e874a9…`, then to this with daily limits) | from `2026-09-29T044603-prepaid-deployment` to `2026-09-30T180500-daily-limits` |
| [`CCDBGAESDWKXV6NVQT6N25YNFFVSYVAWAOIZO57UYMG7BZ4V7C36GZKW`](https://stellar.expert/explorer/testnet/contract/CCDBGAESDWKXV6NVQT6N25YNFFVSYVAWAOIZO57UYMG7BZ4V7C36GZKW) | a buyer's wallet: the example contract account (`contracts/example-account`) | `2b35859f9267d85acd0ac3c51543b5d9fda2e92b2ff9c8f5ef9993dab7111678` | `contract-account-buyer` |
| [`CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA`](https://stellar.expert/explorer/testnet/contract/CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA) | Circle's testnet USDC (Stellar Asset Contract), not ours | | all |

Both ledger contracts share their roles:

| Role | Account |
|---|---|
| Treasury, which holds the USDC | [`GAQ7QT4OSH5I25YEJZ27LJZDTN4LWQULUOD7XQQMPXGE5DFZBGC3PWAN`](https://stellar.expert/explorer/testnet/account/GAQ7QT4OSH5I25YEJZ27LJZDTN4LWQULUOD7XQQMPXGE5DFZBGC3PWAN) |
| Treasury's cold reserve | [`GA33ZSMJ6ASRM3PXQ63ZTYKOU7ZIE4CWHGQ77HXZJX2XLRGBZYTRDFXC`](https://stellar.expert/explorer/testnet/account/GA33ZSMJ6ASRM3PXQ63ZTYKOU7ZIE4CWHGQ77HXZJX2XLRGBZYTRDFXC) |
| Admin (two of three keys) | [`GBM3GVZUVGSNJKQHGEDGCNCWURSM6B7242PR544C4CM34AY2YEDQ2YE3`](https://stellar.expert/explorer/testnet/account/GBM3GVZUVGSNJKQHGEDGCNCWURSM6B7242PR544C4CM34AY2YEDQ2YE3) |
| Operator | [`GCPXGYYNF547BXJ2NRZKMZWDAANHSLKK7FYMBPGIHI4MW4Q234I6YSNG`](https://stellar.expert/explorer/testnet/account/GCPXGYYNF547BXJ2NRZKMZWDAANHSLKK7FYMBPGIHI4MW4Q234I6YSNG) |
| Seller | [`GDLT7M7IAMPMGMDOQ2C6XOKTBLZ7Q7AXEZ6WFWRFKIMTGPOCPAN4EVYN`](https://stellar.expert/explorer/testnet/account/GDLT7M7IAMPMGMDOQ2C6XOKTBLZ7Q7AXEZ6WFWRFKIMTGPOCPAN4EVYN) |

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
| [`prepaid-deployment`](testnet/2026-09-29T044603-prepaid-deployment.json) | Wasm upload and contract creation of `CDDFMUZ7…GNPV`, the contract every other record in this table was produced on |
| [`admin-multisig`](testnet/2026-09-30T041524-admin-multisig.json) | the admin account given two co-signers and a medium threshold of two: any two of its three keys, and never one, authorize an admin change; it still holds 0 XLM |
| [`admin-pause-two-signatures`](testnet/2026-09-30T0416-admin-pause-two-signatures.json), [`admin-unpause-two-signatures`](testnet/2026-09-30T0416-admin-unpause-two-signatures.json) | a pause signed by the two co-signers and an unpause signed by the master key and one co-signer, each proposed, signed on its own and submitted with `fermah-pay-stellar-contract`; the same proposal with one signature was refused for missing weight before anything was sent |
| [`prepaid-upgrade`](testnet/2026-09-29T143109-prepaid-upgrade.json) | the admin replaces the contract's code in place (Wasm `02d80b2f…` to `78e874a9…`, which announces pauses and limit changes): same contract address, totals unchanged; the running code hash equals the Wasm built in CI from the same source |
| [`buyers-funded-100`](testnet/2026-09-29T045256-buyers-funded-100.json) | 100 buyers holding 0 XLM with sponsored reserves, each topped up with Circle USDC |
| [`deposits-1-100`](testnet/2026-09-29T050720-deposits-1-100.json) | 100 deposits: USDC moves from each buyer to the separate treasury and the ledger credits the buyer's account; the buyer only signs its authorization entry, pays no fee and holds 0 XLM before and after |
| [`charge-batch-98-first`](testnet/2026-09-29T050954-charge-batch-98-first.json) | **one transaction charging 98 distinct buyers**, all `Charged`, each under its own charge identifier |
| [`charge-buyer-1-single` (05:10:10)](testnet/2026-09-29T051010-charge-buyer-1-single.json) | a single charge |
| [`charge-buyer-1-single` (05:10:15)](testnet/2026-09-29T051015-charge-buyer-1-single.json) | the same charge identifier again: refused as `DuplicateCharge` (contract error 110) before submission; the balance is unchanged |
| [`withdraw-buyer-2`](testnet/2026-09-29T051027-withdraw-buyer-2.json) | a withdrawal authorized by the buyer and the treasury: credit debited and USDC moved from the treasury back to the buyer in one invocation |
| [`charge-batch-98-second`](testnet/2026-09-29T051043-charge-batch-98-second.json) | one 98-entry transaction with mixed outcomes: 96 `Charged`, 2 `InsufficientBalance` (buyer 1, already charged twice, and buyer 2, after its withdrawal) |
| [`treasury-solvency`](testnet/2026-09-29T051046-treasury-solvency.json) | the treasury's USDC equals buyer liabilities plus unwithdrawn revenue, read from the network: 2.985 USDC held, 0.555 owed to buyers and 2.43 earned by the seller |
| [`charge-batch-98-first-replayed`](testnet/2026-09-29T051328-charge-batch-98-first-replayed.json) | the first batch's 98 charges sent again, included on-chain: every entry `Duplicate`, nothing debited |
| [`api-end-to-end` (2026-09-30)](testnet/2026-09-30T053005-api-end-to-end.json) | the same API end-to-end flow, recorded again: a buyer with 0 XLM deposits with one signature, three charges settle, a retried charge returns the original, a replayed charge is refused on-chain, the buyer withdraws to the same wallet, and the gateway's balance equals the contract's |
| [`cold-reserve`](testnet/2026-09-30T054043-cold-reserve.json) | the treasury's cold reserve: a Circle USDC trustline, two of its three keys needed to move anything, no XLM |
| [`treasury-sweep`](testnet/2026-09-30T054057-treasury-sweep.json), [again](testnet/2026-09-30T054143-treasury-sweep.json) | the treasury's key alone moves USDC from the treasury to its cold reserve |
| [`treasury-solvency` (2026-09-30)](testnet/2026-09-30T054144-treasury-solvency.json), [again](testnet/2026-09-30T054155-treasury-solvency.json) | the USDC of the treasury and its cold reserve together equals buyer liabilities plus unwithdrawn revenue, after the sweeps |
| [`cold-reserve-refill`](testnet/2026-09-30T054200-cold-reserve-refill.json) | USDC moves back from the cold reserve to the treasury only with two of the reserve's three signatures; with one, nothing is sent |
| [`load-test`](testnet/2026-09-30T055844-load-test.json) | 1,000 charges for 10 buyers admitted through the API at about 84 per second and all settled on-chain as charged, in 11 batches sent from 5 source accounts at once; settlement latency p50 24 s, p95 32 s; 8,573 stroops of fees per charge |
| [`key-signature`](testnet/2026-09-30T164338-key-signature.json) | a transaction signed through an `aws-kms://` key reference, the key never leaving AWS KMS, is accepted by testnet for the account the key's public key encodes |
| [`daily-limits`](testnet/2026-09-30T180500-daily-limits.json) | `CDDFMUZ7…GNPV` upgraded to Wasm `02e59475…` and given daily limits, each change signed by two of the admin's three keys; a charge that would take a buyer past its daily limit is refused as `AboveDailyLimit` without debiting it |
| [`prepaid-deployment` (2026-10-01)](testnet/2026-10-01T022132-prepaid-deployment.json) | a fresh contract, `CD3GESMY…7PSI`, built with recurring charges; the deployment the workflows use from then on. The earlier contract stays on the network with its records |
| [`admin-set-limits-two-signatures`](testnet/2026-10-01T0420-admin-set-limits-two-signatures.json) | the minimum deposit on `CD3GESMY…7PSI` raised from 0.01 to 0.1 USDC, proposed, signed by two of the admin's three keys on their own and submitted with `fermah-pay-stellar-contract` |
| [`admin-upgrade-two-signatures`](testnet/2026-10-01T0905-admin-upgrade-two-signatures.json) | `CD3GESMY…7PSI` upgraded in place from Wasm `fcdd05c4…` to `38dfa7d7…` (mandates kept until their last ledger; unusable mandates refused), proposed, signed by two of the admin's three keys and submitted with `fermah-pay-stellar-contract`. One signature was refused before anything was sent. The code fetched back from the network hashes to the Wasm CI built from `main` |
| [`x402-conformance`](testnet/2026-10-01T054928-x402-conformance.json) | the x402 facilitator interface called as a black box over HTTP by the conformance harness: 25 cases (discovery, authentication, each documented refusal from a request differing in one property, verify, settle with the commitment as `transaction`, an idempotent retry, reuse refused) with every request and response, and the settlement then confirmed on-chain through an independent RPC read of its `charges` event |
| [`recurring-end-to-end`](testnet/2026-10-01T034500-recurring-end-to-end.json) | recurring charges on `CD3GESMY…7PSI` through the gateway API, with two-minute periods: (a) a buyer holding 0 XLM authorizes a mandate of two periods of 0.01 USDC with one signature, which also approves the contract in the USDC contract for 0.02 USDC; (b) the first period is charged and (c) the second, each moving 0.01 USDC from the wallet to the treasury without the buyer signing again; a charge sent straight to the contract before the second period started is refused on-chain as `not_due`; (d) after the last period the API refuses a charge (`mandate_ended`) and a charge sent straight to the contract is refused on-chain as `mandate_expired`; the buyer then revokes and the approval is zero. The buyer holds 0 XLM throughout |
| [`contract-account-buyer`](testnet/2026-10-01T173346-contract-account-buyer.json) | a buyer whose wallet is a contract account (the example account in `contracts/example-account`, `CCDBGAES…GZKW`), funded with Circle USDC, through the gateway API: a deposit signed by a key the account does not hold refused as `authorization_refused` after the network refused it in simulation; then a deposit of 0.3 USDC authorized by the account's `__check_auth`, a 0.1 USDC charge, and a withdrawal of 0.15 USDC back to the account. The wallet holds 0 XLM throughout; it ends with 0.35 USDC, and 0.05 USDC of credit on the contract |
| [`drill-buyer-key`](testnet/2026-10-06T161915-drill-buyer-key.json), [`drill-allowance`](testnet/2026-10-06T162011-drill-allowance.json), [`drill-operator-key`](testnet/2026-10-06T162043-drill-operator-key.json), [`drill-admin-code`](testnet/2026-10-06T162343-drill-admin-code.json), [`drill-dust`](testnet/2026-10-06T162414-drill-dust.json) | the [monitoring drills](../self-hosting/monitoring-drills.md) against the development stack on testnet, each with the alert Alertmanager then held active: a withdrawal of a buyer's credit prepared to another account raised `PayStellarWithdrawalsToOtherAccounts`; a buyer setting the contract's USDC approval to zero outside the gateway made the next period's charge `allowance_short` on-chain and raised `PayStellarMandateChangedOutsideGateway`; the operator's key charging a buyer outside the gateway raised `PayStellarCriticalFinding{kind="unknown_charge"}` within a minute, the drill's own deposit and charge being the only unmatched events; the observer expecting other code than the contract runs raised the standing `PayStellarContractCodeUnexpected`; deposits prepared until the deployment's quota refused one raised `PayStellarDeploymentQuotaReached` |

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
