-- Seller deployments whose ledger is the prepaid vault
-- (docs/architecture/vault-contract.md): the contract holds the buyers' USDC,
-- each buyer signs a daily spending limit, and a buyer can exit without the
-- operator once a notice has passed.

-- Who holds a deployment's USDC: a treasury account (the prepaid ledger) or
-- the vault contract itself, which has no treasury.
ALTER TABLE pay_stellar.ledger_contracts
    ADD COLUMN custody TEXT NOT NULL DEFAULT 'treasury' CHECK (custody IN ('treasury', 'vault')),
    ALTER COLUMN treasury_address DROP NOT NULL,
    ADD CONSTRAINT ledger_contracts_custody_treasury
        CHECK ((custody = 'treasury') = (treasury_address IS NOT NULL));
GRANT SELECT (custody) ON pay_stellar.ledger_contracts TO pay_stellar_observer;

-- A vault buyer's spending limit and exit as the contract's events last
-- showed them; only the worker, applying those events, writes them:
-- `cap`: the limit in force (zero until the buyer signs one).
-- `pending_cap`, `pending_cap_at`: a lower limit and the ledger it applies
--   from.
-- `exit_amount`, `exit_unlock_at`: an exit the buyer requested.
-- Admission also counts the buyer's limit changes, deposit limits and exit
-- requests signed through the API and not yet resolved, so it never waits
-- for the contract to become stricter.
ALTER TABLE pay_stellar.buyers
    ADD COLUMN cap BIGINT NOT NULL DEFAULT 0 CHECK (cap >= 0),
    ADD COLUMN pending_cap BIGINT CHECK (pending_cap >= 0),
    ADD COLUMN pending_cap_at BIGINT CHECK (pending_cap_at > 0),
    ADD COLUMN exit_amount BIGINT CHECK (exit_amount > 0),
    ADD COLUMN exit_unlock_at BIGINT CHECK (exit_unlock_at > 0),
    ADD CONSTRAINT buyers_pending_cap CHECK ((pending_cap IS NULL) = (pending_cap_at IS NULL)),
    ADD CONSTRAINT buyers_exit CHECK ((exit_amount IS NULL) = (exit_unlock_at IS NULL));
GRANT SELECT (cap, pending_cap, pending_cap_at, exit_amount, exit_unlock_at)
    ON pay_stellar.buyers TO pay_stellar_worker, pay_stellar_operator;
GRANT UPDATE (cap, pending_cap, pending_cap_at, exit_amount, exit_unlock_at)
    ON pay_stellar.buyers TO pay_stellar_worker;

-- The UTC day of ledger time a charge was admitted in. A vault counts the
-- charge against that day's share of the buyer's limit whenever it settles;
-- the prepaid ledger ignores it.
ALTER TABLE pay_stellar.charges ADD COLUMN day BIGINT NOT NULL DEFAULT 0 CHECK (day >= 0);
GRANT INSERT (day) ON pay_stellar.charges TO pay_stellar_api;

-- A vault deposit may carry the buyer's limit, signed with it.
ALTER TABLE pay_stellar.deposits ADD COLUMN cap BIGINT CHECK (cap >= 0);
GRANT INSERT (cap) ON pay_stellar.deposits TO pay_stellar_api;

-- A charge above what the buyer's limit leaves for its day is refused as
-- `above_cap`, like `above_daily_limit`.
ALTER TABLE pay_stellar.charges
    DROP CONSTRAINT charges_outcome_check,
    DROP CONSTRAINT charges_refused_outcome,
    ADD CONSTRAINT charges_outcome_check CHECK (outcome IN
        ('charged', 'insufficient_balance', 'above_limit', 'above_daily_limit', 'above_cap',
         'duplicate', 'expired', 'unknown_account')),
    ADD CONSTRAINT charges_refused_outcome CHECK (state <> 'refused'
        OR outcome IN ('insufficient_balance', 'above_limit', 'above_daily_limit', 'above_cap',
                       'expired'));

ALTER TABLE pay_stellar.chain_charge_entries
    DROP CONSTRAINT chain_charge_entries_outcome_check,
    ADD CONSTRAINT chain_charge_entries_outcome_check CHECK (outcome IN
        ('charged', 'insufficient_balance', 'above_limit', 'above_daily_limit', 'above_cap',
         'duplicate', 'expired', 'unknown_account'));

ALTER TABLE pay_stellar.charge_resolutions
    DROP CONSTRAINT charge_resolutions_resolution_check,
    ADD CONSTRAINT charge_resolutions_resolution_check CHECK (resolution IN
        ('charged', 'insufficient_balance', 'above_limit', 'above_daily_limit', 'above_cap',
         'expired', 'readmitted'));

CREATE OR REPLACE FUNCTION pay_stellar.resolve_quarantined_charge(
    charge UUID,
    resolution TEXT,
    evidence TEXT
) RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pay_stellar AS $$
DECLARE
    quarantined RECORD;
BEGIN
    SELECT id, buyer_id, amount, state, outcome, last_error INTO quarantined
    FROM pay_stellar.charges WHERE id = charge FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no charge %', charge USING ERRCODE = 'no_data_found';
    END IF;
    IF quarantined.state <> 'quarantined' THEN
        RAISE EXCEPTION 'charge % is %, not quarantined', charge, quarantined.state
            USING ERRCODE = 'check_violation';
    END IF;

    INSERT INTO pay_stellar.charge_resolutions
        (charge_id, resolution, evidence, quarantine_outcome, quarantine_reason)
    VALUES (charge, resolution, evidence, quarantined.outcome, quarantined.last_error);

    CASE resolution
        WHEN 'charged' THEN
            UPDATE pay_stellar.charges SET state = 'charged', outcome = 'charged'
            WHERE id = charge;
        WHEN 'insufficient_balance', 'above_limit', 'above_daily_limit', 'above_cap',
             'expired' THEN
            UPDATE pay_stellar.charges SET state = 'refused', outcome = resolution
            WHERE id = charge;
            UPDATE pay_stellar.buyers SET available = available + quarantined.amount
            WHERE id = quarantined.buyer_id;
        WHEN 'readmitted' THEN
            UPDATE pay_stellar.charges
            SET state = 'admitted', outcome = NULL, submission_id = NULL, batch_index = NULL,
                settled_at = NULL
            WHERE id = charge;
    END CASE;
    RETURN resolution;
END
$$;

-- A buyer's change of limit (`set_cap`) or request to exit (`request_exit`),
-- signed through the API and sent by the worker like a revocation. The
-- contract's account entry decides it: confirmed once it holds the limit or
-- the exit, after the transaction succeeded or the buyer's authorization
-- lapsed.
CREATE TABLE pay_stellar.vault_requests (
    id                        UUID PRIMARY KEY,
    buyer_id                  UUID NOT NULL,
    seller_deployment_id      UUID NOT NULL,
    network                   TEXT NOT NULL,
    idempotency_key           TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    kind                      TEXT NOT NULL CHECK (kind IN ('set_cap', 'request_exit')),
    -- set_cap: the limit.
    cap                       BIGINT CHECK (cap >= 0),
    -- request_exit: the amount and the account that receives it.
    amount                    BIGINT CHECK (amount > 0),
    destination               TEXT CHECK (destination ~ '^[GC][A-Z2-7]{55}$'),
    -- awaiting_signature, signed, submitted, confirmed, failed, expired: as
    -- for revocations.
    state                     TEXT NOT NULL DEFAULT 'awaiting_signature' CHECK (state IN
                                  ('awaiting_signature', 'signed', 'submitted', 'confirmed',
                                   'failed', 'expired')),
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
    CONSTRAINT vault_requests_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    CHECK ((kind = 'set_cap') = (cap IS NOT NULL)),
    CHECK ((kind = 'request_exit') = (amount IS NOT NULL AND destination IS NOT NULL)),
    CHECK ((signed_authorization_xdr IS NULL) = (signed_at IS NULL)),
    CHECK (state <> 'awaiting_signature' OR signed_authorization_xdr IS NULL),
    CHECK (state NOT IN ('signed', 'submitted') OR signed_authorization_xdr IS NOT NULL),
    CHECK (state NOT IN ('awaiting_signature', 'signed') OR submission_id IS NULL),
    CHECK (state <> 'submitted' OR submission_id IS NOT NULL),
    CHECK ((state IN ('confirmed', 'failed', 'expired')) = (resolved_at IS NOT NULL))
);

CREATE INDEX vault_requests_signed ON pay_stellar.vault_requests (created_at)
    WHERE state = 'signed';
CREATE INDEX vault_requests_submission ON pay_stellar.vault_requests (submission_id)
    WHERE submission_id IS NOT NULL;
CREATE INDEX vault_requests_open ON pay_stellar.vault_requests (expiration_ledger)
    WHERE state IN ('awaiting_signature', 'signed');
CREATE INDEX vault_requests_recent ON pay_stellar.vault_requests (buyer_id, created_at);

CREATE FUNCTION pay_stellar.vault_requests_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state IN ('confirmed', 'failed', 'expired') THEN
        RAISE EXCEPTION 'vault request % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER vault_requests_final_state
    BEFORE UPDATE ON pay_stellar.vault_requests
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.vault_requests_guard();

GRANT SELECT ON pay_stellar.vault_requests TO pay_stellar_api, pay_stellar_worker;
GRANT INSERT (id, buyer_id, seller_deployment_id, network, idempotency_key, kind, cap, amount,
              destination, authorization_xdr, expiration_ledger)
    ON pay_stellar.vault_requests TO pay_stellar_api;
GRANT UPDATE (state, signed_authorization_xdr, signed_at)
    ON pay_stellar.vault_requests TO pay_stellar_api;
GRANT UPDATE (state, submission_id, last_error, resolved_at)
    ON pay_stellar.vault_requests TO pay_stellar_worker;
GRANT SELECT (seller_deployment_id, kind, state)
    ON pay_stellar.vault_requests TO pay_stellar_operator;

-- How far the worker has read a vault deployment's events: limit changes
-- and exits made outside the gateway reach admission only through it, so
-- admission refuses while it lags too far behind.
CREATE TABLE pay_stellar.vault_event_cursors (
    seller_deployment_id  UUID PRIMARY KEY,
    network               TEXT NOT NULL,
    -- Every event up to and including this position in the node's stream
    -- has been applied, each exactly once.
    cursor                TEXT NOT NULL CHECK (cursor ~ '^[0-9]{19}-[0-9]{10}$'),
    -- Every event up to and including this ledger has been applied.
    ledger                BIGINT NOT NULL CHECK (ledger > 0),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (seller_deployment_id, network)
        REFERENCES pay_stellar.seller_deployments (id, network) ON DELETE RESTRICT
);
GRANT SELECT ON pay_stellar.vault_event_cursors TO pay_stellar_api, pay_stellar_operator;
GRANT SELECT, INSERT, UPDATE ON pay_stellar.vault_event_cursors TO pay_stellar_worker;

ALTER TABLE pay_stellar.submissions DROP CONSTRAINT submissions_kind_check;
ALTER TABLE pay_stellar.submissions ADD CONSTRAINT submissions_kind_check
    CHECK (kind IN ('deposit', 'charge_batch', 'withdrawal', 'restore', 'extend', 'sweep',
                    'mandate', 'revocation', 'recurring_batch', 'set_cap', 'request_exit',
                    'exit'));

-- The vault's own events.
ALTER TABLE pay_stellar.chain_events
    DROP CONSTRAINT chain_events_kind_check,
    ADD CONSTRAINT chain_events_kind_check CHECK (kind IN
        ('deposit', 'charges', 'withdrawal', 'revenue_withdrawal', 'role', 'pause', 'limits',
         'daily_limits', 'mandate', 'revoke', 'recurring', 'cap_raised', 'cap_lowered',
         'exit_requested', 'exit', 'launch', 'upgrade_proposed', 'upgrade_cancelled',
         'unrecognized'));
