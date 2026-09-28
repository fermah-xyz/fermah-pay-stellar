# Fermah Pay for Stellar

Prepaid USDC accounts and seller-side billing on Stellar.

This repository is self-contained: it builds, tests and runs from public
sources only, under the Apache License 2.0.

It provides:

- **Tenancy**: products, seller deployments pinned to one Stellar network,
  and API keys scoped to a single deployment.
- **Buyer API**: a gRPC service to register buyers and link their Stellar
  wallets ([reference](docs/api/buyer.md)).
- **Prepaid ledger contract** (Soroban): buyer credit and seller revenue over
  USDC held in a separate treasury, with per-account replay protection and
  batches of up to 100 charges in one call
  ([design](docs/architecture/prepaid-contract.md)).
- **Sponsored submission**: buyers sign only an authorization entry for the
  exact call; a submitter signs the transaction and a separate account pays
  through a fee bump
  ([design](docs/architecture/transactions.md)). Deposits, a 100-buyer batch,
  withdrawals and treasury solvency are recorded on testnet with Circle USDC
  ([evidence](docs/evidence/README.md#prepaid-ledger-on-testnet)).
- **Buyer onboarding without XLM**: one transaction creates a buyer account
  with a Circle USDC trustline and a zero XLM balance, while a sponsor pays
  the fee and every reserve ([testnet evidence](docs/evidence/README.md)).

## Layout

```text
contracts/prepaid     Soroban prepaid ledger contract
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
