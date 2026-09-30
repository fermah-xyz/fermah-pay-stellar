# Self-hosting: database

The gateway is tested with PostgreSQL 17. The schema lives in
`db/migrations/` and is applied, in order, by
`fermah-pay-stellar-admin migrate`, which records applied migrations in
`_sqlx_migrations`. Migrations are append-only: an applied migration is never
edited.

Migration `0007` moves charges from per-buyer sequence numbers to charge
identifiers, which needs the contract with the matching interface. An
installation with charges upgrades in this order: stop the gateway API so no
charge is admitted, let the worker settle every submitted charge, resolve any
quarantined charge, stop the worker, run `migrate`, upgrade the contract's
code with its admin, and start the new gateway and worker. The migration
refuses to run while any charge is admitted, submitted or quarantined.

## Roles

The migrations create group roles that cannot log in and grant them only
what their process needs:

| Group role | Granted to | Privileges |
|---|---|---|
| `pay_stellar_api` | the gateway's login role | read key digests and deployment scope; create buyers (never with a balance); create deposits, charges and withdrawals, only in their initial state; store a deposit's or withdrawal's verified signature; debit a buyer's available balance when admitting a charge or holding a withdrawal |
| `pay_stellar_issuer` | the provisioning login role | create products, deployments and keys; set `revoked_at` on keys; bind a deployment to its ledger contract |
| `pay_stellar_operator` | the login role of a person resolving quarantined charges | read charges; call `resolve_quarantined_charge`, the only way out of quarantine; see [resolving quarantined charges](quarantine.md). Also read the chain observer's events and findings, and record a reconciliation baseline, which no role can change or delete; for that it reads buyers' available balances and the state and amount of deposits and withdrawals |
| `pay_stellar_worker` | the login role of the process that submits transactions | record submissions and their outcomes; move deposits and charges to their outcomes; move withdrawals to their outcomes; credit confirmed deposits, refused charges and returned withdrawals back to the available balance. The signed envelope, hashes and sequence of a recorded submission cannot be changed. Take, renew and release the lease that picks which worker acts |
| `pay_stellar_observer` | the login role of the [chain observer](observer.md) | read ledger bindings, buyer wallets and available balances, and the amount, identifier and state of deposits, charges and withdrawals, and a withdrawal's destination; append observed events, verdicts and findings, which no role can update or delete; move its read position forward; keep the count of each discrepancy's consecutive checks; take, renew and release the lease that picks which observer acts. No write to any balance, deposit, charge or binding |

No role can change a buyer's wallet, an amount, a charge's identifier or
a ledger binding after the row is written, and no role can move a submission,
deposit or charge out of a final state: a confirmed deposit or refused charge
changes the available balance exactly once, on entering that state.

The database balance is an admission limit, not the authority: the ledger
contract refuses any charge above the on-chain balance and any charge
identifier or deposit ID it has already processed, whatever the database says.

Group roles are cluster-wide. If several databases on one server host this
schema, for example staging and production, they share the role names and
therefore the table privileges: a login role that is a member of
`pay_stellar_api` for one database holds the same privileges in the other.
The migrations revoke the default right of every role to connect to the
database, so a login role reaches only the databases it is explicitly allowed
to connect to. Prefer one server per environment all the same.

Create one login role per process, make it a member of its group role, and
allow it to connect to this database only:

```sql
CREATE ROLE pay_stellar_gateway    LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_api;
CREATE ROLE pay_stellar_admin      LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_issuer;
CREATE ROLE pay_stellar_settlement LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_worker;
CREATE ROLE pay_stellar_ops        LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_operator;
CREATE ROLE pay_stellar_watch      LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_observer;
GRANT CONNECT ON DATABASE pay_stellar
    TO pay_stellar_gateway, pay_stellar_admin, pay_stellar_settlement, pay_stellar_ops,
       pay_stellar_watch;
```

Run migrations with the owner of the database, not with any login role. The
first migration creates the group roles, so the owner needs the `CREATEROLE`
attribute, or a superuser must create `pay_stellar_api`, `pay_stellar_issuer`,
`pay_stellar_worker`, `pay_stellar_operator` and `pay_stellar_observer`
beforehand; the migrations skip roles that already exist.
Do not grant `pay_stellar_issuer` to the gateway's login role: the gateway
must not be able to issue keys.

## Configuration

| Process | Variable | Value |
|---|---|---|
| Gateway | `PAY_STELLAR_DATABASE_URL` | URL of the gateway login role |
| Gateway | `PAY_STELLAR_NETWORK` | `stellar:testnet` or `stellar:pubnet` |
| Gateway | `PAY_STELLAR_LISTEN_ADDR` | listen address, default `127.0.0.1:50051` |
| Gateway | `PAY_STELLAR_X402_LISTEN_ADDR` | listen address of the [x402 facilitator interface](../api/x402.md) (HTTP); not served when unset |
| Gateway | `PAY_STELLAR_DATABASE_MAX_CONNECTIONS` | pool size, default 16, must be at least 1 |
| Gateway | `PAY_STELLAR_MAX_CONCURRENT_REQUESTS` | requests processed at once across all connections, default 64; the rest wait |
| Gateway | `PAY_STELLAR_REQUEST_TIMEOUT_SECS` | a request still running after this is cancelled, default 30 |
| Gateway | `PAY_STELLAR_RPC_URL` | Stellar RPC endpoint of the network; checked at startup (network passphrase, and protocol 27 or later). Must be `https` unless it points at this host |
| Gateway | `PAY_STELLAR_RPC_TIMEOUT_SECS` | per-request RPC timeout, default 10 |
| Gateway | `PAY_STELLAR_CHARGE_VALIDITY_LEDGERS` | ledgers a charge may wait for settlement before it is refunded, default 720 (about an hour), at most 17280 |
| Gateway | `PAY_STELLAR_MAX_NEW_BUYERS_PER_DAY`, `PAY_STELLAR_MAX_DEPOSITS_PER_BUYER_PER_DAY`, `PAY_STELLAR_MAX_WITHDRAWALS_PER_BUYER_PER_DAY`, `PAY_STELLAR_MIN_WITHDRAWAL` | [quotas](../api/ledger.md#quotas) against requests that each cost the operator fees: new buyers per deployment (default 1000), deposits (10) and withdrawals (5) per buyer, each per 24 hours, and the smallest withdrawal (100000, 0.01 USDC) |
| Gateway | `PAY_STELLAR_DEPOSIT_AUTHORIZATION_LEDGERS` | ledgers a buyer's deposit or withdrawal signature stays valid, default 720 (about an hour). A withdrawal the treasury cannot pay stays held this long before its amount is returned |
| Admin | `PAY_STELLAR_ADMIN_DATABASE_URL` | owner URL for `migrate`, issuer URL otherwise |
| Worker | `PAY_STELLAR_WORKER_DATABASE_URL` | URL of the worker login role |
| Worker | `PAY_STELLAR_NETWORK`, `PAY_STELLAR_RPC_URL` | as for the gateway |
| Worker | `PAY_STELLAR_SOURCE_KEY_FILE` | [key references](keys.md), comma-separated, of the accounts that sequence transactions; each has at most one transaction in flight, so several keep sending while one waits. The accounts may hold no XLM (the fee account pays), and nothing but these workers may submit from them |
| Worker | `PAY_STELLAR_LEASE_SECS` | how long a worker's lease lasts without renewal, default 15, at least 3: a standby takes over this long after a worker stops without releasing it |
| Worker | `PAY_STELLAR_FEE_SOURCE_KEY_FILE` | [key reference](keys.md) of the account that pays fees |
| Worker | `PAY_STELLAR_OPERATOR_KEY_FILE` | [key reference](keys.md) of the contracts' operator; the worker serves the deployments bound with this operator |
| Worker | `PAY_STELLAR_TREASURY_KEY_FILE` | optional [key reference](keys.md) of the treasury; the worker pays the withdrawals of the deployments bound with this treasury. Without it, withdrawals wait until they lapse |
| Worker | `PAY_STELLAR_COLD_RESERVE`, `PAY_STELLAR_HOT_TREASURY_FLOOR`, `…_TARGET`, `…_CEILING` | optional [cold reserve](treasury.md): the treasury's USDC above the ceiling is swept there, down to the target; below the floor it needs topping up. Needs the treasury key |
| Worker | `PAY_STELLAR_FEE_FLOOR_STROOPS` | spendable XLM, in stroops, below which the fee account pays only for finishing work in flight; default `100000000` (10 XLM) |
| Worker | `PAY_STELLAR_TTL_THRESHOLD_LEDGERS`, `PAY_STELLAR_TTL_EXTEND_TO_LEDGERS`, `PAY_STELLAR_TTL_CHECK_SECS` | a served contract's instance and code are extended to `…EXTEND_TO…` ledgers of life (default 518400, about 30 days) once fewer than `…THRESHOLD…` remain (default 120960, about 7 days), checked every `…CHECK_SECS…` (default 600) |
| Worker | `PAY_STELLAR_TRANSACTION_VALIDITY_SECS` | a transaction's inclusion window, default 60 |
| Worker | `PAY_STELLAR_MAX_CLOCK_SKEW_SECS` | largest difference between the host clock and the latest ledger's close time at which transactions are still built, default 20; must be below the validity |
| Worker | `PAY_STELLAR_INCLUSION_FEE` | lowest inclusion bid per operation, in stroops, default 10000; at least 100 |
| Worker | `PAY_STELLAR_MAX_INCLUSION_FEE` | highest inclusion bid per operation, in stroops, default 1000000 (0.1 XLM); equal to the lowest for a fixed bid |
| Worker | `PAY_STELLAR_INCLUSION_FEE_PERCENTILE` | percentile of recent Soroban inclusion fees to bid at: `10` to `90` in steps of 10, `95`, `99` or `max`; default `90` |
| Worker | `PAY_STELLAR_RESOURCE_FEE_MARGIN_PERCENT` | headroom over the simulated resource fee, default 20; the unused part is refunded |
| Worker | `PAY_STELLAR_OPERATOR_AUTHORIZATION_LEDGERS` | how long the operator's authorization of a batch stays valid, default 24 ledgers |
| Worker | `PAY_STELLAR_MAX_BATCH` | charges per batch, 1 to 98, default 98 |
| Observer | `PAY_STELLAR_OBSERVER_DATABASE_URL` | URL of the observer login role; the other settings are in [chain observer](observer.md#running-it) |

Key files must not be readable by other users; the worker refuses to start
otherwise.

Several workers may run with the same settings, on different hosts, for
availability. They share a lease per network and operator: the one holding
it settles, renewing it every third of its life, and the others stand by.
A worker that shuts down releases the lease and a standby takes over at
once; one that dies is replaced once its lease lapses. The lease only
decides who does the work. Settlement stays correct if two workers act at
once, for instance a worker that lost its lease finishing its round: a
source account still has one transaction in flight, and a deposit or
charge still moves to its outcome once. The observer uses the same scheme
with one lease per network.

Both processes refuse to start unless the RPC endpoint serves the configured
network at protocol 27 or later: authorizations use `AddressV2` credentials.

Each transaction bids the market percentile of recent inclusion fees, never
less than the lowest bid nor more than the highest, and twice the previous
bid after a transaction expired unincluded; each bid is logged. The fee
source pays at most twice the highest bid per transaction in inclusion fees
(the call and its fee bump), besides the resource fee. See
[inclusion fees](../architecture/transactions.md#inclusion-fees). Keep the
host clock synchronized: the worker stops building transactions while it
disagrees with the network by more than `PAY_STELLAR_MAX_CLOCK_SKEW_SECS`
([local clock](../architecture/transactions.md#local-clock)).

Run one gateway process per network. A testnet gateway refuses live keys,
and a pubnet gateway refuses test keys.

The gateway serves plaintext gRPC. Terminate TLS in front of it (for example
at a load balancer or sidecar) whenever traffic leaves the host.

Every request, authenticated or not, costs the gateway a database lookup of
its key. The gateway bounds how many it processes at once and for how long,
but it does not limit requests per client: put a rate limit per client in the
proxy or load balancer in front of it.
