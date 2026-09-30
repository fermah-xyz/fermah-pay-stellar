# Documentation

| Section | Contents |
|---|---|
| [architecture/](architecture/overview.md) | system overview, [prepaid ledger contract](architecture/prepaid-contract.md), [sponsored transactions](architecture/transactions.md), [chain observer and reconciliation](architecture/observer.md), [tenancy and authentication](architecture/tenancy.md) |
| [api/](api/buyer.md) | gRPC API reference and refusal reasons: [buyers](api/buyer.md), [deposits, charges and balances](api/ledger.md); the [x402 facilitator interface](api/x402.md) over HTTP |
| [quickstart/](quickstart/local.md) | run the gateway locally; [the whole system on one machine](quickstart/dev-stack.md) with Docker Compose; [testnet walkthrough](quickstart/testnet.md) for the ledger contract |
| [self-hosting/](self-hosting/database.md) | database roles, migrations and configuration; [running the chain observer and acting on its findings](self-hosting/observer.md); [resolving quarantined charges](self-hosting/quarantine.md); [rotating a contract role](self-hosting/rotation.md); [keys and key references](self-hosting/keys.md); [the treasury and its cold reserve](self-hosting/treasury.md); [metrics, traces and alerts](self-hosting/monitoring.md) |
| [evidence/](evidence/README.md) | recorded observations on Stellar networks |
