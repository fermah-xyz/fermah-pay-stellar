-- Buyer withdrawals: returning unused credit from the treasury.
--
-- A withdrawal needs two authorizations: the buyer's, for the exact amount,
-- destination and withdrawal id, and the treasury's, which the worker signs
-- for each attempt. The contract writes a marker for the withdrawal id when
-- it processes it and refuses the id again, so a withdrawal row can move
-- USDC at most once.
--
-- The amount is held from `buyers.available` in the statement that stores
-- the buyer's signature, so charges admitted afterwards cannot spend it. It
-- is returned in the statement that moves the row to `failed` or `expired`,
-- which happens only once the buyer's authorization has lapsed and the
-- contract has no marker: from then on nothing can include it.
CREATE TABLE pay_stellar.withdrawals (
    id                        UUID PRIMARY KEY,
    buyer_id                  UUID NOT NULL,
    seller_deployment_id      UUID NOT NULL,
    network                   TEXT NOT NULL,
    idempotency_key           TEXT NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    amount                    BIGINT NOT NULL CHECK (amount > 0),
    destination_address       TEXT NOT NULL CHECK (destination_address ~ '^G[A-Z2-7]{55}$'),
    -- The contract's withdrawal id: random, and refused by the contract if
    -- reused for the same owner.
    withdrawal_id             BYTEA NOT NULL CHECK (octet_length(withdrawal_id) = 32),
    -- awaiting_signature: the buyer has the unsigned authorization entry;
    --   nothing is held.
    -- signed: a verified signed entry is stored and the amount is held.
    -- submitted: linked to the submission that carries it.
    -- confirmed: the contract processed it (the transaction succeeded, or
    --   its withdrawal marker exists); the held amount has left.
    -- failed: the transaction was included but failed, and once the buyer's
    --   authorization lapsed the contract had no marker; the amount is
    --   returned.
    -- expired: the authorization lapsed and the contract has no marker; the
    --   amount, if held, is returned.
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
    CONSTRAINT withdrawals_idempotency_key UNIQUE (seller_deployment_id, idempotency_key),
    UNIQUE (buyer_id, withdrawal_id),
    CHECK ((signed_authorization_xdr IS NULL) = (signed_at IS NULL)),
    CHECK (state <> 'awaiting_signature' OR signed_authorization_xdr IS NULL),
    CHECK (state IN ('awaiting_signature', 'expired') OR signed_authorization_xdr IS NOT NULL),
    CHECK (state NOT IN ('awaiting_signature', 'signed') OR submission_id IS NULL),
    CHECK (state <> 'submitted' OR submission_id IS NOT NULL),
    CHECK ((state IN ('confirmed', 'failed', 'expired')) = (resolved_at IS NOT NULL))
);

CREATE INDEX withdrawals_signed ON pay_stellar.withdrawals (created_at) WHERE state = 'signed';
CREATE INDEX withdrawals_submission ON pay_stellar.withdrawals (submission_id)
    WHERE submission_id IS NOT NULL;
CREATE INDEX withdrawals_open ON pay_stellar.withdrawals (expiration_ledger)
    WHERE state IN ('awaiting_signature', 'signed');

-- Final states are absorbing: the amount is returned once, on the move
-- into `failed` or `expired`.
CREATE FUNCTION pay_stellar.withdrawals_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state IN ('confirmed', 'failed', 'expired') THEN
        RAISE EXCEPTION 'withdrawal % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER withdrawals_final_state
    BEFORE UPDATE ON pay_stellar.withdrawals
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.withdrawals_guard();

-- The API creates withdrawals in their initial state and stores the buyer's
-- verified signature together with the hold on `available`; only the worker
-- moves them on.
GRANT SELECT ON pay_stellar.withdrawals TO pay_stellar_api;
GRANT INSERT (id, buyer_id, seller_deployment_id, network, idempotency_key, amount,
              destination_address, withdrawal_id, authorization_xdr, expiration_ledger)
    ON pay_stellar.withdrawals TO pay_stellar_api;
GRANT UPDATE (state, signed_authorization_xdr, signed_at)
    ON pay_stellar.withdrawals TO pay_stellar_api;
GRANT SELECT ON pay_stellar.withdrawals TO pay_stellar_worker;
GRANT UPDATE (state, submission_id, last_error, resolved_at)
    ON pay_stellar.withdrawals TO pay_stellar_worker;
GRANT SELECT (id, buyer_id, seller_deployment_id, network, amount, destination_address,
              withdrawal_id, state)
    ON pay_stellar.withdrawals TO pay_stellar_observer;
GRANT SELECT (seller_deployment_id, state, amount)
    ON pay_stellar.withdrawals TO pay_stellar_operator;

-- The observer matches each `withdraw` event against its row.
ALTER TABLE pay_stellar.reconciliation_findings
    DROP CONSTRAINT reconciliation_findings_kind_check,
    ADD CONSTRAINT reconciliation_findings_kind_check CHECK (kind IN
        ('event_gap', 'unrecognized_event',
         'unknown_charge', 'charge_amount_mismatch',
         'charge_outcome_mismatch', 'charge_unsettled',
         'unknown_deposit', 'deposit_amount_mismatch',
         'deposit_outcome_mismatch', 'deposit_unsettled',
         'unknown_withdrawal', 'withdrawal_mismatch', 'withdrawal_outcome_mismatch',
         'role_changed', 'admin_change', 'binding_out_of_date',
         'treasury_deficit', 'treasury_surplus',
         'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch'));
