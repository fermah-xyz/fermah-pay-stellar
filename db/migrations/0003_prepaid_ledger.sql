-- Deposits and charges against a deployment's prepaid ledger contract.
--
-- The contract is the authority over balances: it refuses any charge above
-- the on-chain balance and any reuse of a charge sequence or deposit id. The
-- rows here are the gateway's admission view and the durable link between a
-- request and the transaction that settles it.

-- The ledger contract a seller deployment settles against. Immutable: buyer
-- balances belong to one contract, so moving a deployment to another contract
-- is a new deployment, not an update.
CREATE TABLE pay_stellar.ledger_contracts (
    seller_deployment_id  UUID PRIMARY KEY,
    network               TEXT NOT NULL,
    contract_address      TEXT NOT NULL CHECK (contract_address ~ '^C[A-Z2-7]{55}$'),
    usdc_address          TEXT NOT NULL CHECK (usdc_address ~ '^C[A-Z2-7]{55}$'),
    treasury_address      TEXT NOT NULL CHECK (treasury_address ~ '^G[A-Z2-7]{55}$'),
    operator_address      TEXT NOT NULL CHECK (operator_address ~ '^G[A-Z2-7]{55}$'),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (seller_deployment_id, network)
        REFERENCES pay_stellar.seller_deployments (id, network) ON DELETE RESTRICT,
    UNIQUE (network, contract_address)
);

-- `available` is what admission may still debit: confirmed deposits minus
-- every admitted charge not refused, debited when a charge is admitted and
-- before the contract settles it. It is not the authority: the contract
-- refuses a charge above the on-chain balance whatever this column says.
-- `next_charge_seq` mirrors the contract's per-account sequence: the contract
-- accepts exactly one charge per number, in order, starting at 1.
ALTER TABLE pay_stellar.buyers
    ADD COLUMN available BIGINT NOT NULL DEFAULT 0 CHECK (available >= 0),
    ADD COLUMN next_charge_seq BIGINT NOT NULL DEFAULT 1 CHECK (next_charge_seq >= 1),
    -- Target of the composite foreign keys below.
    ADD CONSTRAINT buyers_scope_key UNIQUE (id, seller_deployment_id, network);

CREATE TABLE pay_stellar.deposits (
    id                        UUID PRIMARY KEY,
    buyer_id                  UUID NOT NULL,
    seller_deployment_id      UUID NOT NULL,
    network                   TEXT NOT NULL,
    idempotency_key           TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    amount                    BIGINT NOT NULL CHECK (amount > 0),
    -- The contract's deposit id: random, and refused by the contract if
    -- reused for the same owner, so one deposit row can credit at most once.
    deposit_id                BYTEA NOT NULL CHECK (octet_length(deposit_id) = 32),
    -- awaiting_signature: the buyer has the unsigned authorization entry.
    -- signed: a verified signed entry is stored; ready for submission.
    -- submitted: linked to the submission that carries it.
    -- confirmed: the contract processed it (the transaction succeeded, or its
    --   deposit marker exists) and `buyers.available` was credited.
    -- failed: the transaction was included but failed, and once the buyer's
    --   authorization lapsed the contract had no deposit marker.
    -- expired: the authorization lapsed and the contract has no deposit
    --   marker, so nothing can credit this deposit any more.
    -- quarantined: the evidence contradicts itself; an operator must decide.
    state                     TEXT NOT NULL DEFAULT 'awaiting_signature' CHECK (state IN
                                  ('awaiting_signature', 'signed', 'submitted', 'confirmed',
                                   'failed', 'expired', 'quarantined')),
    authorization_xdr         TEXT NOT NULL,
    signed_authorization_xdr  TEXT,
    expiration_ledger         BIGINT NOT NULL CHECK (expiration_ledger > 0),
    submission_id             UUID REFERENCES pay_stellar.submissions (id) ON DELETE RESTRICT,
    last_error                TEXT,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    signed_at                 TIMESTAMPTZ,
    resolved_at               TIMESTAMPTZ,
    FOREIGN KEY (buyer_id, seller_deployment_id, network)
        REFERENCES pay_stellar.buyers (id, seller_deployment_id, network) ON DELETE RESTRICT,
    CONSTRAINT deposits_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    UNIQUE (buyer_id, deposit_id),
    CHECK ((signed_authorization_xdr IS NULL) = (signed_at IS NULL)),
    CHECK (state <> 'awaiting_signature' OR signed_authorization_xdr IS NULL),
    CHECK (state NOT IN ('signed', 'submitted') OR signed_authorization_xdr IS NOT NULL),
    CHECK (state NOT IN ('awaiting_signature', 'signed') OR submission_id IS NULL),
    -- A confirmed deposit may have no submission: a buyer can include the
    -- signed entry through a transaction of their own, and the contract's
    -- deposit marker is then the evidence.
    CHECK (state NOT IN ('submitted', 'quarantined') OR submission_id IS NOT NULL),
    CHECK ((state IN ('confirmed', 'failed', 'expired', 'quarantined')) = (resolved_at IS NOT NULL))
);

CREATE TABLE pay_stellar.charges (
    id                    UUID PRIMARY KEY,
    buyer_id              UUID NOT NULL,
    seller_deployment_id  UUID NOT NULL,
    network               TEXT NOT NULL,
    idempotency_key       TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    amount                BIGINT NOT NULL CHECK (amount > 0),
    sequence              BIGINT NOT NULL CHECK (sequence >= 1),
    -- admitted: debited from `available`, waiting for a batch.
    -- submitted: in the batch of `submission_id`, at `batch_index`.
    -- charged: the contract debited the on-chain balance.
    -- refused: the contract consumed the sequence without debiting; the
    --   amount was returned to `available`.
    -- quarantined: the contract's answer contradicts the gateway's view
    --   (e.g. duplicate or out of order), or the outcome cannot be
    --   established; the amount stays debited until an operator decides.
    state                 TEXT NOT NULL DEFAULT 'admitted' CHECK (state IN
                              ('admitted', 'submitted', 'charged', 'refused', 'quarantined')),
    outcome               TEXT CHECK (outcome IN
                              ('charged', 'insufficient_balance', 'above_limit',
                               'duplicate', 'out_of_order', 'unknown_account')),
    submission_id         UUID REFERENCES pay_stellar.submissions (id) ON DELETE RESTRICT,
    batch_index           SMALLINT CHECK (batch_index BETWEEN 0 AND 99),
    last_error            TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    settled_at            TIMESTAMPTZ,
    FOREIGN KEY (buyer_id, seller_deployment_id, network)
        REFERENCES pay_stellar.buyers (id, seller_deployment_id, network) ON DELETE RESTRICT,
    CONSTRAINT charges_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    -- One charge per contract sequence number: two rows sharing a number
    -- would each count as settled by the one on-chain charge.
    UNIQUE (buyer_id, sequence),
    CHECK ((state = 'admitted') = (submission_id IS NULL)),
    CHECK ((submission_id IS NULL) = (batch_index IS NULL)),
    CHECK ((state = 'charged') = (outcome IS NOT DISTINCT FROM 'charged')),
    CHECK (state <> 'refused' OR outcome IN ('insufficient_balance', 'above_limit')),
    CHECK (state <> 'quarantined' OR outcome IS NULL
           OR outcome IN ('duplicate', 'out_of_order', 'unknown_account')),
    CHECK (state NOT IN ('admitted', 'submitted') OR outcome IS NULL),
    CHECK ((state IN ('charged', 'refused', 'quarantined')) = (settled_at IS NOT NULL))
);

CREATE INDEX charges_admitted ON pay_stellar.charges (seller_deployment_id, created_at)
    WHERE state = 'admitted';
CREATE INDEX deposits_signed ON pay_stellar.deposits (created_at) WHERE state = 'signed';
CREATE INDEX charges_submission ON pay_stellar.charges (submission_id)
    WHERE submission_id IS NOT NULL;
CREATE INDEX deposits_submission ON pay_stellar.deposits (submission_id)
    WHERE submission_id IS NOT NULL;

-- Final states are absorbing. A confirmed deposit credits `available` once,
-- on the transition into `confirmed`, and a refused charge returns its
-- amount once, on the transition into `refused`; if a row could leave a
-- final state it could re-enter it and repeat that effect.
CREATE FUNCTION pay_stellar.deposits_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state IN ('confirmed', 'failed', 'expired', 'quarantined') THEN
        RAISE EXCEPTION 'deposit % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER deposits_final_state
    BEFORE UPDATE ON pay_stellar.deposits
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.deposits_guard();

CREATE FUNCTION pay_stellar.charges_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state IN ('charged', 'refused', 'quarantined') THEN
        RAISE EXCEPTION 'charge % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER charges_final_state
    BEFORE UPDATE ON pay_stellar.charges
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.charges_guard();

-- Buyer rows: the API inserts buyers without balance columns, and no role may
-- rewrite the wallet link. The API debits `available` and allocates sequence
-- numbers when admitting a charge; the worker credits confirmed deposits and
-- returns refused charges.
REVOKE INSERT ON pay_stellar.buyers FROM pay_stellar_api;
GRANT INSERT (id, product_id, seller_deployment_id, network, external_ref, wallet_address)
    ON pay_stellar.buyers TO pay_stellar_api;
GRANT UPDATE (available, next_charge_seq) ON pay_stellar.buyers TO pay_stellar_api;
GRANT SELECT (id, seller_deployment_id, network, wallet_address, available)
    ON pay_stellar.buyers TO pay_stellar_worker;
GRANT UPDATE (available) ON pay_stellar.buyers TO pay_stellar_worker;

GRANT SELECT, INSERT ON pay_stellar.ledger_contracts TO pay_stellar_issuer;
GRANT SELECT ON pay_stellar.ledger_contracts TO pay_stellar_api, pay_stellar_worker;

-- The API creates deposits and charges only in their initial state (the
-- column default), stores the buyer's verified signature, and nothing else;
-- only the worker moves them on.
GRANT SELECT ON pay_stellar.deposits TO pay_stellar_api;
GRANT INSERT (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, deposit_id,
              authorization_xdr, expiration_ledger)
    ON pay_stellar.deposits TO pay_stellar_api;
GRANT UPDATE (state, signed_authorization_xdr, signed_at) ON pay_stellar.deposits TO pay_stellar_api;
GRANT SELECT ON pay_stellar.deposits TO pay_stellar_worker;
GRANT UPDATE (state, submission_id, last_error, resolved_at)
    ON pay_stellar.deposits TO pay_stellar_worker;

GRANT SELECT ON pay_stellar.charges TO pay_stellar_api;
GRANT INSERT (id, buyer_id, seller_deployment_id, network, idempotency_key, amount, sequence)
    ON pay_stellar.charges TO pay_stellar_api;
GRANT SELECT ON pay_stellar.charges TO pay_stellar_worker;
GRANT UPDATE (state, outcome, submission_id, batch_index, last_error, settled_at)
    ON pay_stellar.charges TO pay_stellar_worker;

-- The API reports each request's transaction hash and ledger.
GRANT SELECT (id, state, outer_hash, ledger) ON pay_stellar.submissions TO pay_stellar_api;
