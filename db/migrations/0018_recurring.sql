-- Recurring charges: mandates, their revocation, and the charges made
-- under them.
--
-- A mandate is the buyer's standing authorization for the seller to charge
-- their wallet once per period. The buyer signs one authorization entry that
-- records the mandate in the contract and approves the contract, in the USDC
-- contract, to move up to the mandate's total until its last ledger. The
-- contract keeps one mandate per buyer: a new one replaces the old.
--
-- As with deposits, a submission that did not succeed is not a verdict: the
-- buyer's signed entry can be included by anyone until it lapses. So a
-- mandate is declared not authorized only once its entry has lapsed and the
-- contract does not hold it.
CREATE TABLE pay_stellar.mandates (
    id                        UUID PRIMARY KEY,
    buyer_id                  UUID NOT NULL,
    seller_deployment_id      UUID NOT NULL,
    network                   TEXT NOT NULL,
    idempotency_key           TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    -- The contract's mandate id: random, named by every charge for it.
    mandate_id                BYTEA NOT NULL CHECK (octet_length(mandate_id) = 32),
    -- Most that may be charged per period, in USDC base units.
    amount                    BIGINT NOT NULL CHECK (amount > 0),
    period_secs               BIGINT NOT NULL CHECK (period_secs > 0),
    cycles                    INTEGER NOT NULL CHECK (cycles > 0),
    -- Last ledger in which the contract and the USDC allowance honour it.
    live_until                BIGINT NOT NULL CHECK (live_until > 0),
    -- awaiting_signature: the buyer has the unsigned authorization entry.
    -- signed: a verified signed entry is stored; ready for submission.
    -- submitted: linked to the submission that carries it.
    -- active: the contract holds it; `starts_at` is its first period's
    --   start in ledger time.
    -- failed: the transaction was included but failed, and once the buyer's
    --   authorization lapsed the contract did not hold it.
    -- expired: the authorization lapsed and the contract does not hold it.
    -- replaced: the contract holds a newer mandate of the same buyer.
    -- revoked: the buyer revoked it.
    -- ended: past its last ledger or its last period.
    state                     TEXT NOT NULL DEFAULT 'awaiting_signature' CHECK (state IN
                                  ('awaiting_signature', 'signed', 'submitted', 'active',
                                   'failed', 'expired', 'replaced', 'revoked', 'ended')),
    authorization_xdr         TEXT NOT NULL,
    signed_authorization_xdr  TEXT,
    -- Last ledger in which the buyer's signed entry is valid.
    expiration_ledger         BIGINT NOT NULL CHECK (expiration_ledger > 0),
    submission_id             UUID REFERENCES pay_stellar.submissions (id) ON DELETE RESTRICT,
    -- Unix seconds of ledger time at which the first period starts.
    starts_at                 BIGINT CHECK (starts_at >= 0),
    last_error                TEXT,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    signed_at                 TIMESTAMPTZ,
    activated_at              TIMESTAMPTZ,
    closed_at                 TIMESTAMPTZ,
    FOREIGN KEY (buyer_id, seller_deployment_id, network)
        REFERENCES pay_stellar.buyers (id, seller_deployment_id, network) ON DELETE RESTRICT,
    CONSTRAINT mandates_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    UNIQUE (buyer_id, mandate_id),
    CHECK ((signed_authorization_xdr IS NULL) = (signed_at IS NULL)),
    CHECK (state <> 'awaiting_signature' OR signed_authorization_xdr IS NULL),
    CHECK (state NOT IN ('signed', 'submitted') OR signed_authorization_xdr IS NOT NULL),
    CHECK (state NOT IN ('awaiting_signature', 'signed') OR submission_id IS NULL),
    CHECK (state <> 'submitted' OR submission_id IS NOT NULL),
    CHECK (state <> 'active' OR (starts_at IS NOT NULL AND activated_at IS NOT NULL)),
    CHECK ((state IN ('failed', 'expired', 'replaced', 'revoked', 'ended')) = (closed_at IS NOT NULL))
);

-- The contract holds at most one mandate per buyer, so at most one row is
-- active: activating a mandate replaces the previous one in the same
-- statement.
CREATE UNIQUE INDEX mandates_one_active_per_buyer ON pay_stellar.mandates (buyer_id)
    WHERE state = 'active';
CREATE INDEX mandates_signed ON pay_stellar.mandates (created_at) WHERE state = 'signed';
CREATE INDEX mandates_submission ON pay_stellar.mandates (submission_id)
    WHERE submission_id IS NOT NULL;
CREATE INDEX mandates_open ON pay_stellar.mandates (expiration_ledger)
    WHERE state IN ('awaiting_signature', 'signed');

CREATE FUNCTION pay_stellar.mandates_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state IN ('failed', 'expired', 'replaced', 'revoked', 'ended') THEN
        RAISE EXCEPTION 'mandate % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER mandates_final_state
    BEFORE UPDATE ON pay_stellar.mandates
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.mandates_guard();

-- A revocation ends the buyer's mandate, whichever it is, and sets the USDC
-- allowance to zero. Its effect is what decides it: confirmed once the
-- contract holds no mandate for the buyer, after the transaction succeeded
-- or the buyer's authorization lapsed.
CREATE TABLE pay_stellar.revocations (
    id                        UUID PRIMARY KEY,
    buyer_id                  UUID NOT NULL,
    seller_deployment_id      UUID NOT NULL,
    network                   TEXT NOT NULL,
    idempotency_key           TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    -- awaiting_signature, signed, submitted: as for mandates.
    -- confirmed: the contract holds no mandate for the buyer.
    -- failed: included but failed, and the contract still holds a mandate
    --   once the buyer's authorization lapsed.
    -- expired: the authorization lapsed and the contract still holds a
    --   mandate.
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
    CONSTRAINT revocations_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    CHECK ((signed_authorization_xdr IS NULL) = (signed_at IS NULL)),
    CHECK (state <> 'awaiting_signature' OR signed_authorization_xdr IS NULL),
    CHECK (state NOT IN ('signed', 'submitted') OR signed_authorization_xdr IS NOT NULL),
    CHECK (state NOT IN ('awaiting_signature', 'signed') OR submission_id IS NULL),
    CHECK (state <> 'submitted' OR submission_id IS NOT NULL),
    CHECK ((state IN ('confirmed', 'failed', 'expired')) = (resolved_at IS NOT NULL))
);

CREATE INDEX revocations_signed ON pay_stellar.revocations (created_at) WHERE state = 'signed';
CREATE INDEX revocations_submission ON pay_stellar.revocations (submission_id)
    WHERE submission_id IS NOT NULL;
CREATE INDEX revocations_open ON pay_stellar.revocations (expiration_ledger)
    WHERE state IN ('awaiting_signature', 'signed');

CREATE FUNCTION pay_stellar.revocations_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state IN ('confirmed', 'failed', 'expired') THEN
        RAISE EXCEPTION 'revocation % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER revocations_final_state
    BEFORE UPDATE ON pay_stellar.revocations
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.revocations_guard();

-- The API creates both in their initial state and stores the buyer's
-- verified signature; only the worker moves them on.
GRANT SELECT ON pay_stellar.mandates, pay_stellar.revocations TO pay_stellar_api;
GRANT INSERT (id, buyer_id, seller_deployment_id, network, idempotency_key, mandate_id, amount,
              period_secs, cycles, live_until, authorization_xdr, expiration_ledger)
    ON pay_stellar.mandates TO pay_stellar_api;
GRANT INSERT (id, buyer_id, seller_deployment_id, network, idempotency_key, authorization_xdr,
              expiration_ledger)
    ON pay_stellar.revocations TO pay_stellar_api;
GRANT UPDATE (state, signed_authorization_xdr, signed_at)
    ON pay_stellar.mandates, pay_stellar.revocations TO pay_stellar_api;
GRANT SELECT ON pay_stellar.mandates, pay_stellar.revocations TO pay_stellar_worker;
GRANT UPDATE (state, submission_id, starts_at, last_error, activated_at, closed_at)
    ON pay_stellar.mandates TO pay_stellar_worker;
GRANT UPDATE (state, submission_id, last_error, resolved_at)
    ON pay_stellar.revocations TO pay_stellar_worker;
GRANT SELECT (id, buyer_id, seller_deployment_id, network, mandate_id, amount, period_secs,
              cycles, live_until, state, starts_at)
    ON pay_stellar.mandates TO pay_stellar_observer;
GRANT SELECT (seller_deployment_id, state)
    ON pay_stellar.mandates, pay_stellar.revocations TO pay_stellar_operator;

-- A charge of one period of an active mandate. The seller asks for it; the
-- worker settles it on-chain in a batch, moving the USDC from the buyer's
-- wallet to the treasury as revenue. Nothing is held from the buyer's
-- prepaid balance. The contract charges a period at most once, so at most
-- one attempt per period is live or charged here; a refused attempt leaves
-- the period free for another while it lasts.
CREATE TABLE pay_stellar.recurring_charges (
    id                    UUID PRIMARY KEY,
    mandate_row_id        UUID NOT NULL REFERENCES pay_stellar.mandates (id) ON DELETE RESTRICT,
    buyer_id              UUID NOT NULL,
    seller_deployment_id  UUID NOT NULL,
    network               TEXT NOT NULL,
    idempotency_key       TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    cycle                 INTEGER NOT NULL CHECK (cycle >= 0),
    -- The contract's identifier of this attempt: random, so a retry of a
    -- refused period is a new attempt.
    charge_id             BYTEA NOT NULL CHECK (octet_length(charge_id) = 32),
    amount                BIGINT NOT NULL CHECK (amount > 0),
    last_ledger           BIGINT NOT NULL CHECK (last_ledger > 0),
    -- admitted: waiting for a batch.
    -- submitted: in the batch of `submission_id`, at `batch_index`.
    -- charged: the contract moved the USDC.
    -- refused: the contract refused it, or it expired unsent; `outcome`
    --   says why.
    -- quarantined: the evidence contradicts itself; an operator decides.
    state                 TEXT NOT NULL DEFAULT 'admitted' CHECK (state IN
                              ('admitted', 'submitted', 'charged', 'refused', 'quarantined')),
    outcome               TEXT CHECK (outcome IN
                              ('charged', 'duplicate', 'expired', 'no_mandate', 'mandate_expired',
                               'already_charged', 'not_due', 'period_over', 'above_mandate',
                               'above_limit', 'above_daily_limit', 'allowance_short',
                               'wallet_short', 'transfer_refused')),
    submission_id         UUID REFERENCES pay_stellar.submissions (id) ON DELETE RESTRICT,
    batch_index           SMALLINT CHECK (batch_index BETWEEN 0 AND 99),
    last_error            TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    settled_at            TIMESTAMPTZ,
    FOREIGN KEY (buyer_id, seller_deployment_id, network)
        REFERENCES pay_stellar.buyers (id, seller_deployment_id, network) ON DELETE RESTRICT,
    CONSTRAINT recurring_charges_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    UNIQUE (buyer_id, charge_id),
    CHECK ((submission_id IS NULL) = (batch_index IS NULL)),
    CHECK (state <> 'admitted' OR submission_id IS NULL),
    CHECK (state NOT IN ('submitted', 'charged') OR submission_id IS NOT NULL),
    CHECK ((state = 'charged') = (outcome IS NOT DISTINCT FROM 'charged')),
    CHECK (state <> 'refused' OR outcome NOT IN ('charged', 'duplicate')),
    CHECK (state NOT IN ('admitted', 'submitted') OR outcome IS NULL),
    CHECK ((state IN ('charged', 'refused', 'quarantined')) = (settled_at IS NOT NULL))
);

CREATE UNIQUE INDEX recurring_charges_one_per_period
    ON pay_stellar.recurring_charges (mandate_row_id, cycle)
    WHERE state IN ('admitted', 'submitted', 'charged', 'quarantined');
CREATE INDEX recurring_charges_admitted
    ON pay_stellar.recurring_charges (seller_deployment_id, created_at) WHERE state = 'admitted';
CREATE INDEX recurring_charges_submission ON pay_stellar.recurring_charges (submission_id)
    WHERE submission_id IS NOT NULL;

-- An operator's decision on a quarantined recurring charge, with its
-- evidence, recorded in the same transaction as the change; append-only.
CREATE TABLE pay_stellar.recurring_charge_resolutions (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    recurring_charge_id UUID NOT NULL REFERENCES pay_stellar.recurring_charges (id)
                            ON DELETE RESTRICT,
    -- The outcome the contract's events show for the charge.
    resolution          TEXT NOT NULL CHECK (resolution IN
                            ('charged', 'expired', 'no_mandate', 'mandate_expired',
                             'already_charged', 'not_due', 'period_over', 'above_mandate',
                             'above_limit', 'above_daily_limit', 'allowance_short',
                             'wallet_short', 'transfer_refused')),
    evidence            TEXT NOT NULL CHECK (length(evidence) BETWEEN 1 AND 2000),
    quarantine_reason   TEXT,
    resolved_by         TEXT NOT NULL DEFAULT session_user,
    resolved_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    transaction_id      xid8 NOT NULL DEFAULT pg_current_xact_id()
);

CREATE INDEX recurring_charge_resolutions_charge
    ON pay_stellar.recurring_charge_resolutions (recurring_charge_id);

CREATE TRIGGER recurring_charge_resolutions_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.recurring_charge_resolutions
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();

-- `charged` and `refused` are absorbing. `quarantined` may be left only in a
-- transaction that recorded a resolution for that charge, which only
-- `resolve_quarantined_recurring_charge` can insert. SECURITY DEFINER so
-- that the worker's updates, which trigger this check, may read the audit
-- table.
CREATE FUNCTION pay_stellar.recurring_charges_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pay_stellar AS $$
BEGIN
    IF OLD.state IN ('charged', 'refused') THEN
        RAISE EXCEPTION 'recurring charge % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.state = 'quarantined' AND NOT EXISTS (
        SELECT 1 FROM pay_stellar.recurring_charge_resolutions r
        WHERE r.recurring_charge_id = OLD.id AND r.transaction_id = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION 'recurring charge % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER recurring_charges_final_state
    BEFORE UPDATE ON pay_stellar.recurring_charges
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.recurring_charges_guard();

-- Moves a quarantined recurring charge to the outcome its evidence
-- establishes. No balance changes: the USDC moved, or did not, between the
-- buyer's wallet and the treasury.
CREATE FUNCTION pay_stellar.resolve_quarantined_recurring_charge(
    charge UUID,
    resolution TEXT,
    evidence TEXT
) RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pay_stellar AS $$
DECLARE
    quarantined RECORD;
BEGIN
    SELECT id, state, last_error INTO quarantined
    FROM pay_stellar.recurring_charges WHERE id = charge FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no recurring charge %', charge USING ERRCODE = 'no_data_found';
    END IF;
    IF quarantined.state <> 'quarantined' THEN
        RAISE EXCEPTION 'recurring charge % is %, not quarantined', charge, quarantined.state
            USING ERRCODE = 'check_violation';
    END IF;
    INSERT INTO pay_stellar.recurring_charge_resolutions
        (recurring_charge_id, resolution, evidence, quarantine_reason)
    VALUES (charge, resolution, evidence, quarantined.last_error);
    UPDATE pay_stellar.recurring_charges
    SET state = CASE WHEN resolution = 'charged' THEN 'charged' ELSE 'refused' END,
        outcome = resolution
    WHERE id = charge;
    RETURN resolution;
END
$$;

REVOKE ALL ON FUNCTION pay_stellar.resolve_quarantined_recurring_charge(UUID, TEXT, TEXT)
    FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pay_stellar.resolve_quarantined_recurring_charge(UUID, TEXT, TEXT)
    TO pay_stellar_operator;
GRANT SELECT ON pay_stellar.recurring_charge_resolutions TO pay_stellar_operator;

GRANT SELECT ON pay_stellar.recurring_charges TO pay_stellar_api;
GRANT INSERT (id, mandate_row_id, buyer_id, seller_deployment_id, network, idempotency_key, cycle,
              charge_id, amount, last_ledger)
    ON pay_stellar.recurring_charges TO pay_stellar_api;
GRANT SELECT ON pay_stellar.recurring_charges TO pay_stellar_worker;
GRANT UPDATE (state, outcome, submission_id, batch_index, last_error, settled_at)
    ON pay_stellar.recurring_charges TO pay_stellar_worker;
GRANT SELECT (id, buyer_id, seller_deployment_id, network, charge_id, mandate_row_id, cycle,
              amount, state, outcome)
    ON pay_stellar.recurring_charges TO pay_stellar_observer;
GRANT SELECT (id, seller_deployment_id, state, amount, outcome, last_error)
    ON pay_stellar.recurring_charges TO pay_stellar_operator;

-- Each entry of an observed `recurring` event, as `chain_charge_entries`
-- holds those of `charges` events.
CREATE TABLE pay_stellar.chain_recurring_entries (
    chain_event_id  UUID NOT NULL REFERENCES pay_stellar.chain_events (id) ON DELETE RESTRICT,
    entry_index     SMALLINT NOT NULL CHECK (entry_index >= 0),
    owner           TEXT NOT NULL,
    charge_id       BYTEA NOT NULL CHECK (octet_length(charge_id) = 32),
    mandate_id      BYTEA NOT NULL CHECK (octet_length(mandate_id) = 32),
    cycle           BIGINT NOT NULL CHECK (cycle >= 0),
    amount          NUMERIC(40, 0) NOT NULL,
    outcome         TEXT NOT NULL CHECK (outcome IN
                        ('charged', 'duplicate', 'expired', 'no_mandate', 'mandate_expired',
                         'already_charged', 'not_due', 'period_over', 'above_mandate',
                         'above_limit', 'above_daily_limit', 'allowance_short',
                         'wallet_short', 'transfer_refused')),
    PRIMARY KEY (chain_event_id, entry_index)
);

CREATE INDEX chain_recurring_entries_charge
    ON pay_stellar.chain_recurring_entries (owner, charge_id);

CREATE TRIGGER chain_recurring_entries_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.chain_recurring_entries
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();

GRANT SELECT, INSERT ON pay_stellar.chain_recurring_entries TO pay_stellar_observer;
GRANT SELECT ON pay_stellar.chain_recurring_entries TO pay_stellar_operator;

ALTER TABLE pay_stellar.reconciliation_findings
    DROP CONSTRAINT reconciliation_findings_kind_check,
    ADD CONSTRAINT reconciliation_findings_kind_check CHECK (kind IN
        ('event_gap', 'unrecognized_event',
         'unknown_charge', 'charge_amount_mismatch',
         'charge_outcome_mismatch', 'charge_unsettled',
         'unknown_deposit', 'deposit_amount_mismatch',
         'deposit_outcome_mismatch', 'deposit_unsettled',
         'unknown_withdrawal', 'withdrawal_mismatch', 'withdrawal_outcome_mismatch',
         'unknown_recurring_charge', 'recurring_charge_mismatch',
         'recurring_outcome_mismatch', 'recurring_charge_unsettled',
         'role_changed', 'admin_change', 'binding_out_of_date',
         'treasury_deficit', 'treasury_surplus',
         'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch'));

ALTER TABLE pay_stellar.submissions DROP CONSTRAINT submissions_kind_check;
ALTER TABLE pay_stellar.submissions ADD CONSTRAINT submissions_kind_check
    CHECK (kind IN ('deposit', 'charge_batch', 'withdrawal', 'restore', 'extend', 'sweep',
                    'mandate', 'revocation', 'recurring_batch'));

-- The contract's mandate, revocation and recurring charge events.
ALTER TABLE pay_stellar.chain_events
    DROP CONSTRAINT chain_events_kind_check,
    ADD CONSTRAINT chain_events_kind_check CHECK (kind IN
        ('deposit', 'charges', 'withdrawal', 'revenue_withdrawal', 'role', 'pause', 'limits',
         'daily_limits', 'mandate', 'revoke', 'recurring', 'unrecognized'));
