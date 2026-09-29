# Developer entry points. `just --list` shows them all.

database_url := "postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar"

# Start the local PostgreSQL used by tests and the quickstart.
db-up:
    docker compose -f deploy/local/compose.yaml up -d --wait

db-down:
    docker compose -f deploy/local/compose.yaml down

# Apply migrations to the local database.
migrate: db-up
    SQLX_OFFLINE=true cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-admin -- --database-url {{database_url}} migrate

# Refresh the offline query cache after changing SQL or migrations.
sqlx-prepare: migrate
    DATABASE_URL={{database_url}} cargo sqlx prepare --workspace

# Formatting, lints, unused dependencies, supply-chain policy and the full
# test suite: the same verdicts the Rust and Supply chain workflows give.
gate: db-up
    cargo fmt --all -- --check
    SQLX_OFFLINE=true cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo machete
    cargo deny check
    SQLX_OFFLINE=true DATABASE_URL={{database_url}} cargo test --workspace --all-features --locked

# Checks against the live Stellar testnet (needs network access and Friendbot).
testnet:
    cargo test -p fermah-pay-stellar-chain --test testnet_onboarding --locked -- --ignored

stellar_cli := "stellar"
contract_out := "target/contract-wasm"

# Build the prepaid ledger Wasm with the stellar CLI (required by soroban-sdk).
contract-build:
    {{stellar_cli}} contract build --package fermah-pay-stellar-prepaid --profile contract --locked --out-dir {{contract_out}}

# Measure a full batch of distinct buyers in one invocation against the
# network's per-transaction limits, using the built Wasm.
contract-resources: contract-build
    PREPAID_WASM={{justfile_directory()}}/{{contract_out}}/fermah_pay_stellar_prepaid.wasm cargo test -p fermah-pay-stellar-prepaid --locked full_ -- --ignored --nocapture
