# Developer entry points. `just --list` shows them all.

# The local PostgreSQL's host port: set PAY_STELLAR_LOCAL_PG_PORT when 55433
# is taken. COMPOSE_PROJECT_NAME gives a second, separate instance.
pg_port := env_var_or_default("PAY_STELLAR_LOCAL_PG_PORT", "55433")
database_url := "postgres://pay_stellar_owner:local-development-only@127.0.0.1:" + pg_port + "/pay_stellar"

# Start the local PostgreSQL used by tests and the quickstart.
db-up:
    docker compose -f deploy/local/compose.yaml up -d --wait

db-down:
    docker compose -f deploy/local/compose.yaml down

# Create another database on the local PostgreSQL, for a run that needs its
# own (each end-to-end run binds the contract to a new deployment).
db-create name: db-up
    docker compose -f deploy/local/compose.yaml exec -T postgres createdb -U pay_stellar_owner {{name}}

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
    cargo test -p fermah-pay-stellar-chain --test testnet_onboarding --test testnet_events --locked -- --ignored

stellar_cli := "stellar"
contract_out := "target/contract-wasm"

# Build the prepaid ledger Wasm with the stellar CLI (required by soroban-sdk).
contract-build:
    {{stellar_cli}} contract build --package fermah-pay-stellar-prepaid --profile contract --locked --out-dir {{contract_out}}

# Build the example contract account's Wasm, a stand-in smart wallet.
example-account-build:
    {{stellar_cli}} contract build --package fermah-pay-stellar-example-account --profile contract --locked --out-dir {{contract_out}}

# Measure a full batch of distinct buyers in one invocation against the
# network's per-transaction limits, using the built Wasm.
contract-resources: contract-build
    PREPAID_WASM={{justfile_directory()}}/{{contract_out}}/fermah_pay_stellar_prepaid.wasm cargo test -p fermah-pay-stellar-prepaid --locked full_ -- --ignored --nocapture

# The whole system on this machine against testnet: PostgreSQL, gateway,
# worker and observer (see docs/quickstart/dev-stack.md). Needs
# PAY_STELLAR_TESTNET_PROFILE.
dev-up *args:
    DEV_UID=$(id -u) docker compose -f deploy/dev/compose.yaml {{args}} up -d --build

# The stack with monitoring and the drills' settings (docs/self-hosting/monitoring-drills.md).
dev-drills-up:
    DEV_UID=$(id -u) DRILL_CONTRACT=$(jq -r .contract "$PAY_STELLAR_TESTNET_PROFILE/deployment.json") \
        docker compose -f deploy/dev/compose.yaml -f deploy/dev/drills.yaml --profile monitoring up -d --build

# Stopping and reading logs mount nothing, so they need no testnet profile.
dev-down:
    PAY_STELLAR_TESTNET_PROFILE="${PAY_STELLAR_TESTNET_PROFILE:-/unused}" docker compose -f deploy/dev/compose.yaml --profile monitoring down

dev-logs *args:
    PAY_STELLAR_TESTNET_PROFILE="${PAY_STELLAR_TESTNET_PROFILE:-/unused}" docker compose -f deploy/dev/compose.yaml logs -f {{args}}

# The development API key the stack provisioned.
dev-api-key:
    docker compose -f deploy/dev/compose.yaml run --rm --no-deps --entrypoint jq provision -r .api_key /state/seller.json
