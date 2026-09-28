-- Tenancy and buyer accounts.
--
-- Scope model: an API key belongs to exactly one seller deployment; a
-- deployment belongs to one product and is pinned to one Stellar network.
-- Every buyer row repeats (product, deployment, network) and a composite
-- foreign key keeps the triple coherent, so a query that filters on all three
-- cannot match a row created under a different scope.
--
-- Privileges: migrations run as the database owner. Runtime processes log in
-- as roles that are members of these NOLOGIN group roles and receive only
-- the privileges below. See docs/self-hosting/database.md.

DO $$
BEGIN
    CREATE ROLE pay_stellar_api NOLOGIN;
EXCEPTION
    -- Roles are cluster-wide: another database on the same server, or a
    -- concurrent test database, may have created it already.
    WHEN duplicate_object OR unique_violation THEN NULL;
END
$$;

DO $$
BEGIN
    CREATE ROLE pay_stellar_issuer NOLOGIN;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
END
$$;

CREATE SCHEMA pay_stellar;
GRANT USAGE ON SCHEMA pay_stellar TO pay_stellar_api, pay_stellar_issuer;

CREATE TABLE pay_stellar.products (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE pay_stellar.seller_deployments (
    id          UUID PRIMARY KEY,
    product_id  UUID NOT NULL REFERENCES pay_stellar.products (id) ON DELETE RESTRICT,
    name        TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    network     TEXT NOT NULL CHECK (network IN ('stellar:testnet', 'stellar:pubnet')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (product_id, name),
    -- Targets of the composite foreign keys below.
    UNIQUE (id, network),
    UNIQUE (id, product_id, network)
);

CREATE TABLE pay_stellar.api_keys (
    id                    UUID PRIMARY KEY,
    seller_deployment_id  UUID NOT NULL,
    network               TEXT NOT NULL,
    -- SHA-256 of the full presented token. Tokens carry 256 bits from the
    -- OS CSPRNG, so a fast hash is not brute-forceable and lets the lookup
    -- be a unique-index probe instead of a per-candidate password hash.
    token_sha256          BYTEA NOT NULL UNIQUE CHECK (octet_length(token_sha256) = 32),
    label                 TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 128),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at            TIMESTAMPTZ,
    FOREIGN KEY (seller_deployment_id, network)
        REFERENCES pay_stellar.seller_deployments (id, network) ON DELETE RESTRICT
);

CREATE TABLE pay_stellar.buyers (
    id                    UUID PRIMARY KEY,
    product_id            UUID NOT NULL,
    seller_deployment_id  UUID NOT NULL,
    network               TEXT NOT NULL,
    external_ref          TEXT NOT NULL CHECK (external_ref ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    wallet_address        TEXT NOT NULL CHECK (wallet_address ~ '^G[A-Z2-7]{55}$'),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (seller_deployment_id, product_id, network)
        REFERENCES pay_stellar.seller_deployments (id, product_id, network) ON DELETE RESTRICT,
    UNIQUE (seller_deployment_id, external_ref),
    -- One wallet is one buyer within a deployment: the prepaid ledger binds
    -- an account to a single owner, and two buyers sharing a wallet could
    -- spend each other's deposits.
    UNIQUE (seller_deployment_id, wallet_address)
);

-- Runtime API: authenticate keys, create and read buyers. No UPDATE or
-- DELETE anywhere, so a wallet link cannot be rewritten by the API process.
REVOKE ALL ON ALL TABLES IN SCHEMA pay_stellar FROM pay_stellar_api, pay_stellar_issuer;
GRANT SELECT (id, seller_deployment_id, network, token_sha256, revoked_at)
    ON pay_stellar.api_keys TO pay_stellar_api;
GRANT SELECT (id, product_id, network) ON pay_stellar.seller_deployments TO pay_stellar_api;
GRANT SELECT, INSERT ON pay_stellar.buyers TO pay_stellar_api;

-- Issuer: provisions products, deployments and keys; may only revoke keys.
GRANT SELECT, INSERT ON pay_stellar.products TO pay_stellar_issuer;
GRANT SELECT, INSERT ON pay_stellar.seller_deployments TO pay_stellar_issuer;
GRANT SELECT (id, seller_deployment_id, network, label, created_at, revoked_at), INSERT
    ON pay_stellar.api_keys TO pay_stellar_issuer;
GRANT UPDATE (revoked_at) ON pay_stellar.api_keys TO pay_stellar_issuer;
