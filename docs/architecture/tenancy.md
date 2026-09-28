# Tenancy and authentication

## Scope

Every authenticated request acts within one scope:

```text
product ─┬─ seller deployment (network: stellar:testnet) ─┬─ API keys
         │                                                └─ buyers
         └─ seller deployment (network: stellar:pubnet)  ─┬─ API keys
                                                          └─ buyers
```

- A key belongs to one deployment; the deployment fixes the product and the
  network.
- Buyer rows repeat `(product_id, seller_deployment_id, network)`, and a
  composite foreign key to the deployment keeps the three values consistent.
- Buyer queries filter on all three columns. Because of that foreign key, the
  deployment ID alone determines the rows; the product and network
  predicates are kept so the queries stay scoped even if the schema changes.
- Buyers in different deployments are independent, even within one product:
  the same external reference or wallet may be registered in each, and
  neither deployment can see the other's buyers.

## API keys

A key is a prefix followed by 43 base64url characters encoding 32 bytes from
the operating system CSPRNG:

| Network of the key's deployment | Prefix |
|---|---|
| `stellar:testnet` | `fps_test_` |
| `stellar:pubnet` | `fps_live_` |

The prefix is derived from the deployment at issuance, never from the
operator's input. Clients send the key as `authorization: Bearer <key>`.

The database stores only the SHA-256 digest of the key. With 256 random
bits, the digest cannot be reversed by guessing, so a fast hash is
sufficient and authentication is a single indexed lookup rather than a
per-candidate password-hash check. A lost key cannot be recovered; issue a
new one and revoke the old one.

A gateway refuses, with `UNAUTHENTICATED` / `unauthenticated`:

- a missing or malformed `authorization` header;
- a key whose prefix names the other network, before any database access;
- a key that was never issued or has been revoked;
- a key whose stored row belongs to a deployment on the other network.

The standard gRPC health service (`grpc.health.v1.Health`) is the only
endpoint reachable without a key.

## Process separation

| Process | Database role | Can | Cannot |
|---|---|---|---|
| Gateway (`fermah-pay-stellar-gateway`) | member of `pay_stellar_api` | authenticate keys; create and read buyers | update or delete buyers; create or revoke keys; read key labels |
| Issuer (`fermah-pay-stellar-admin`) | member of `pay_stellar_issuer` | create products, deployments and keys; revoke keys | delete keys; touch buyers |
| Migrations (`fermah-pay-stellar-admin migrate`) | database owner | change the schema | (runs only during deployment) |

The gateway process therefore cannot mint credentials, and a wallet linked to
a buyer cannot be rewritten through it. Both properties are enforced by
database privileges and covered by tests that attempt the forbidden
statements under the real roles.
