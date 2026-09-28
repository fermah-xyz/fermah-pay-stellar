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
