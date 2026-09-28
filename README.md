# Fermah Pay for Stellar

Prepaid USDC accounts and seller-side billing on Stellar. The target design:
a buyer funds a prepaid balance once; a seller charges it per use through an
authenticated API; an operator submits the transactions and pays the XLM
fees. USDC is held by a separate treasury account, and a Soroban contract
records each buyer's balance. The [status](#status) table below separates
what is implemented from what is planned.

This repository is self-contained: it builds, tests and runs from public
sources only, under the Apache License 2.0.

## Status

Work is delivered in vertical slices. What exists today:

| Capability | State |
|---|---|
| Tenancy: products, seller deployments, network-pinned API keys | Implemented and tested |
| Buyer registration API (`BuyerService`, gRPC) | Implemented and tested |
| Buyer onboarding on Stellar with sponsored reserves (buyer holds 0 XLM) | Implemented; verified on testnet ([evidence](docs/evidence/README.md)) |
| Prepaid ledger contract (`deposit`, `charge`, `charge_batch`, `get_balance`, `withdraw`) | Planned |
| Buyer-signed authorization with operator fee bump | Planned |
| Charge settlement, event observation and reconciliation | Planned |
| Recurring charges with bounded SAC allowances | Planned |
| TypeScript SDK | Planned |

Nothing in the "Planned" rows should be read as available.

## Layout

```text
crates/domain         validated values (network, account address, external reference)
crates/stellar-chain  keys, transaction signing, USDC identity, Stellar RPC client, onboarding
crates/gateway        gRPC API daemon, authentication, PostgreSQL store, key issuance
crates/proto          generated gRPC types
crates/cli            operator tools: fermah-pay-stellar-admin, fermah-pay-stellar-testnet
proto/                gRPC API definitions
db/migrations/        PostgreSQL schema
deploy/local/         local development database
docs/                 architecture, API, quickstart, self-hosting, evidence
```

## Build and test

Requirements: Rust (the pinned toolchain in `rust-toolchain.toml` installs
automatically through rustup), Docker, and [`just`](https://github.com/casey/just).
No `protoc` or other system tools are needed.

```bash
just db-up     # local PostgreSQL on 127.0.0.1:55433
just gate      # fmt, clippy, unused deps, cargo-deny, full test suite
just testnet   # optional: live checks against Stellar testnet
```

Continue with the [quickstart](docs/quickstart/local.md).

## Documentation

- [Architecture](docs/architecture/overview.md)
- [Buyer API](docs/api/buyer.md)
- [Local quickstart](docs/quickstart/local.md)
- [Self-hosting: database](docs/self-hosting/database.md)
- [Evidence](docs/evidence/README.md)

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
