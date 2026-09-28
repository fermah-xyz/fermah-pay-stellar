# Self-hosting: database

The gateway is tested with PostgreSQL 17. The schema lives in
`db/migrations/` and is applied, in order, by
`fermah-pay-stellar-admin migrate`, which records applied migrations in
`_sqlx_migrations`. Migrations are append-only: an applied migration is never
edited.

## Roles

The first migration creates two group roles that cannot log in and grants
them only what their process needs:

| Group role | Granted to | Privileges |
|---|---|---|
| `pay_stellar_api` | the gateway's login role | read key digests and deployment scope; `SELECT`, `INSERT` on buyers |
| `pay_stellar_issuer` | the provisioning login role | create products, deployments and keys; set `revoked_at` on keys |

Group roles are cluster-wide. If several databases on one server host this
schema, they share the role names; the migration tolerates the roles already
existing.

Create one login role per process and make it a member of its group role:

```sql
CREATE ROLE pay_stellar_gateway LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_api;
CREATE ROLE pay_stellar_admin   LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_issuer;
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
| Admin | `PAY_STELLAR_ADMIN_DATABASE_URL` | owner URL for `migrate`, issuer URL otherwise |

Run one gateway process per network. A testnet gateway refuses live keys,
and a pubnet gateway refuses test keys.

The gateway serves plaintext gRPC. Terminate TLS in front of it (for example
at a load balancer or sidecar) whenever traffic leaves the host.
