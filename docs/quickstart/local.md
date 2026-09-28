# Local quickstart

Runs the gateway against a local database, provisions a seller, onboards a
real buyer account on Stellar testnet, and registers it through the API.

Requirements: Rust via rustup, Docker, `just`, and
[`grpcurl`](https://github.com/fullstorydev/grpcurl) for the API calls.

## 1. Database and schema

```bash
just migrate
```

This starts PostgreSQL on `127.0.0.1:55433` and applies the migrations as the
database owner. Create the login roles the processes use (local passwords;
choose real secrets anywhere else):

```bash
docker compose -f deploy/local/compose.yaml exec -T postgres \
  psql -U pay_stellar_owner -d pay_stellar -v ON_ERROR_STOP=1 -c "
    CREATE ROLE pay_stellar_gateway LOGIN PASSWORD 'gateway-local' IN ROLE pay_stellar_api;
    CREATE ROLE pay_stellar_admin   LOGIN PASSWORD 'admin-local'   IN ROLE pay_stellar_issuer;"
```

## 2. Provision a seller and an API key

```bash
export PAY_STELLAR_ADMIN_DATABASE_URL=postgres://pay_stellar_admin:admin-local@127.0.0.1:55433/pay_stellar
admin() { cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-admin -- "$@"; }

admin create-product --name demo
# {"product_id":"<PRODUCT_ID>"}
admin create-deployment --product-id <PRODUCT_ID> --name demo-testnet --network stellar:testnet
# {"deployment_id":"<DEPLOYMENT_ID>","network":"stellar:testnet"}
admin issue-api-key --deployment-id <DEPLOYMENT_ID> --label quickstart
# {"key_id":"...","token":"fps_test_..."}
```

The token is shown once. Store it; the database keeps only its digest.

## 3. Start the gateway

```bash
PAY_STELLAR_DATABASE_URL=postgres://pay_stellar_gateway:gateway-local@127.0.0.1:55433/pay_stellar \
PAY_STELLAR_NETWORK=stellar:testnet \
PAY_STELLAR_LISTEN_ADDR=127.0.0.1:50051 \
cargo run -q -p fermah-pay-stellar-gateway
```

## 4. Onboard a buyer on testnet

```bash
cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-testnet -- \
  onboard-buyer --buyer-secret-out buyer.secret --sponsor-secret-out sponsor.secret
```

This generates a sponsor, funds it through Friendbot, generates a buyer that
is never funded, and submits one transaction in which the sponsor creates the
buyer with 0 XLM, pays the fee, and pays the account and USDC trustline
reserves. It prints an evidence record read back from the ledger and exits
non-zero unless the buyer holds 0 XLM and both reserves are paid by the
sponsor. Keys are written with mode `0600`; `*.secret` is git-ignored. Never
commit them.

## 5. Register the buyer

```bash
TOKEN=fps_test_...        # from step 2
BUYER=G...                # "buyer" from the evidence record
api() { grpcurl -plaintext -import-path proto -proto fermah/pay/stellar/v1/buyer.proto \
          -H "authorization: Bearer $TOKEN" "$@"; }

api -d "{\"external_ref\":\"viewer-42\",\"wallet_address\":\"$BUYER\"}" \
  127.0.0.1:50051 fermah.pay.stellar.v1.BuyerService/CreateBuyer
api -d '{"external_ref":"viewer-42"}' \
  127.0.0.1:50051 fermah.pay.stellar.v1.BuyerService/GetBuyer
```

Repeating `CreateBuyer` with the same input returns the same buyer with
`"created"` omitted (false). Calling without the header returns
`Unauthenticated`.
