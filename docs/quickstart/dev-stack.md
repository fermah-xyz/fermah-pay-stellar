# Quickstart: the whole system on one machine

`deploy/dev/compose.yaml` runs every process against Stellar testnet, each on its own database login role, as a real deployment would:

- PostgreSQL;
- the gateway, with gRPC and the x402 interface;
- the settlement worker;
- the chain observer;
- optionally, Prometheus with the [alert rules](../self-hosting/monitoring.md).

It is for development, not a production deployment.

## Before

The stack settles against the ledger contract recorded in a testnet profile, with that profile's keys. Create the profile first with [the testnet quickstart](testnet.md) steps 1 to 3, then create two extra channel accounts for the stack's worker (step 8, `load-test --channels 3`, creates them). The stack's worker sends from `channel-2` and `channel-3`; the testnet CI run sends from `channel-1` and the submitter, so the two never race for a sequence number. Point the stack at the profile:

```bash
export PAY_STELLAR_TESTNET_PROFILE=~/.config/fermah-pay-stellar/testnet
```

The profile is mounted read-only. The key files keep their `0600` mode, and the services run as your user so they can read them.

## Up

```bash
just dev-up                         # `just dev-up --profile monitoring` adds Prometheus
just dev-logs gateway worker        # follow the logs
```

The first start runs three one-off jobs:

1. applies the migrations;
2. creates one login role per process;
3. provisions a product and a testnet deployment bound to the profile's contract, and issues an API key.

Later starts reuse them. The database lives in a Docker volume, so `just dev-down` keeps it; `docker compose -f deploy/dev/compose.yaml down -v` starts from scratch.

| Service | Address |
|---|---|
| gRPC API ([buyers](../api/buyer.md), [ledger](../api/ledger.md)) | `127.0.0.1:50051` |
| [x402 facilitator](../api/x402.md) | `http://127.0.0.1:8402` |
| Metrics: gateway, worker, observer | `http://127.0.0.1:9101/metrics`, `:9102`, `:9103` |
| PostgreSQL (owner `pay_stellar_owner` / `dev-owner`) | `127.0.0.1:55434` |
| Prometheus (monitoring profile) | `http://127.0.0.1:9090` |

## Calling the API

```bash
TOKEN=$(just dev-api-key)
grpcurl -plaintext -import-path proto -proto fermah/pay/stellar/v1/buyer.proto \
  -H "authorization: Bearer $TOKEN" \
  -d '{"external_ref":"alice","wallet_address":"G..."}' \
  127.0.0.1:50051 fermah.pay.stellar.v1.BuyerService/CreateBuyer
```

Without `grpcurl` installed, run its image instead:
`docker run --rm --network host -v "$PWD/proto:/proto:ro" fullstorydev/grpcurl:v1.9.3 -plaintext -import-path /proto ...`.

A deposit is then prepared with `LedgerService/PrepareDeposit`, signed by the buyer's wallet, and sent with `SubmitDeposit`. The worker settles it on testnet; the [testnet end-to-end run](testnet.md#6-end-to-end-through-the-api) shows the whole flow from code.

## Notes

- **Observer noise.** The testnet contract is shared with other runs. The observer starts from the latest ledger. Deposits and charges that other runs make after that are reported as `unknown_deposit` and `unknown_charge` warnings, because this database has no rows for them.
- **Changing the source accounts.** Set `DEV_SOURCE_KEYS` to a comma-separated list of key paths inside the profile, for example `/profile/channel-4.secret`. Any [key reference](../self-hosting/keys.md) works, including `aws-kms://...` with AWS credentials passed to the worker.
