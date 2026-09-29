# Self-hosting: database

The gateway is tested with PostgreSQL 17. The schema lives in
`db/migrations/` and is applied, in order, by
`fermah-pay-stellar-admin migrate`, which records applied migrations in
`_sqlx_migrations`. Migrations are append-only: an applied migration is never
edited.

## Roles

The migrations create group roles that cannot log in and grant them only
what their process needs:

| Group role | Granted to | Privileges |
|---|---|---|
| `pay_stellar_api` | the gateway's login role | read key digests and deployment scope; create buyers (never with a balance); create deposits and charges, only in their initial state; store a deposit's verified signature; debit a buyer's available balance and allocate charge sequence numbers when admitting a charge |
| `pay_stellar_issuer` | the provisioning login role | create products, deployments and keys; set `revoked_at` on keys; bind a deployment to its ledger contract |
| `pay_stellar_worker` | the login role of the process that submits transactions | record submissions and their outcomes; move deposits and charges to their outcomes; credit confirmed deposits and refused charges back to the available balance. The signed envelope, hashes and sequence of a recorded submission cannot be changed |

No role can change a buyer's wallet, an amount, a charge's sequence number or
a ledger binding after the row is written, and no role can move a submission,
deposit or charge out of a final state: a confirmed deposit or refused charge
changes the available balance exactly once, on entering that state.

The database balance is an admission limit, not the authority: the ledger
contract refuses any charge above the on-chain balance and any reused charge
sequence or deposit ID, whatever the database says.

Group roles are cluster-wide. If several databases on one server host this
schema, they share the role names; the migration tolerates the roles already
existing.

Create one login role per process and make it a member of its group role:

```sql
CREATE ROLE pay_stellar_gateway   LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_api;
CREATE ROLE pay_stellar_admin     LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_issuer;
CREATE ROLE pay_stellar_settlement LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_worker;
```

Run migrations with the owner of the database, not with either login role.
Do not grant `pay_stellar_issuer` to the gateway's login role: the gateway
must not be able to issue keys.

## Configuration

| Process | Variable | Value |
|---|---|---|
| Gateway | `PAY_STELLAR_DATABASE_URL` | URL of the gateway login role |
| Gateway | `PAY_STELLAR_NETWORK` | `stellar:testnet` or `stellar:pubnet` |
| Gateway | `PAY_STELLAR_LISTEN_ADDR` | listen address, default `127.0.0.1:50051` |
| Gateway | `PAY_STELLAR_DATABASE_MAX_CONNECTIONS` | pool size, default 16, must be at least 1 |
| Gateway | `PAY_STELLAR_RPC_URL` | Stellar RPC endpoint of the network; checked at startup |
| Gateway | `PAY_STELLAR_RPC_TIMEOUT_SECS` | per-request RPC timeout, default 10 |
| Gateway | `PAY_STELLAR_DEPOSIT_AUTHORIZATION_LEDGERS` | ledgers a buyer's deposit signature stays valid, default 720 (about an hour) |
| Admin | `PAY_STELLAR_ADMIN_DATABASE_URL` | owner URL for `migrate`, issuer URL otherwise |
| Worker | `PAY_STELLAR_WORKER_DATABASE_URL` | URL of the worker login role |
| Worker | `PAY_STELLAR_NETWORK`, `PAY_STELLAR_RPC_URL` | as for the gateway |
| Worker | `PAY_STELLAR_SOURCE_KEY_FILE` | seed file of the account that sequences every transaction; no other process may submit from it |
| Worker | `PAY_STELLAR_FEE_SOURCE_KEY_FILE` | seed file of the account that pays fees |
| Worker | `PAY_STELLAR_OPERATOR_KEY_FILE` | seed file of the contracts' operator; the worker serves the deployments bound with this operator |
| Worker | `PAY_STELLAR_TRANSACTION_VALIDITY_SECS`, `PAY_STELLAR_INGESTION_MARGIN_SECS` | a transaction's inclusion window and the wait after it, defaults 60 and 30 |
| Worker | `PAY_STELLAR_OPERATOR_AUTHORIZATION_LEDGERS` | how long the operator's authorization of a batch stays valid, default 24 ledgers |
| Worker | `PAY_STELLAR_MAX_BATCH` | charges per batch, 1 to 100, default 100 |

Key files must not be readable by other users; the worker refuses to start
otherwise. Run one worker per source account.

Run one gateway process per network. A testnet gateway refuses live keys,
and a pubnet gateway refuses test keys.

The gateway serves plaintext gRPC. Terminate TLS in front of it (for example
at a load balancer or sidecar) whenever traffic leaves the host.
