-- How many consecutive reconciliation checks each discrepancy has lasted,
-- and whether its finding was recorded. Kept in memory before, so a
-- restart confirmed, and recorded again, a discrepancy already on record.
-- The observer updates a streak in the transaction that records its
-- finding, so a crash leaves neither half.

CREATE TABLE pay_stellar.reconciliation_streaks (
    seller_deployment_id  UUID NOT NULL
                          REFERENCES pay_stellar.ledger_contracts (seller_deployment_id)
                          ON DELETE RESTRICT,
    kind                  TEXT NOT NULL CHECK (kind IN
                              ('treasury_deficit', 'treasury_surplus', 'treasury_deauthorized',
                               'event_totals_mismatch', 'ledger_totals_mismatch')),
    checks                INTEGER NOT NULL CHECK (checks > 0),
    recorded              BOOLEAN NOT NULL DEFAULT false,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (seller_deployment_id, kind)
);

REVOKE ALL ON pay_stellar.reconciliation_streaks FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON pay_stellar.reconciliation_streaks
    TO pay_stellar_observer;

-- A baseline is an operator's acknowledgement that, at `ledger`, the
-- contract's totals and the database were as they stood: what the observer
-- compares from then on is the change since. It lets reconciliation resume
-- after events were lost to an RPC node's retention, or when observation
-- began after the contract's first events, without hiding any change that
-- comes after. Taken only while nothing is in flight, so the offsets
-- between the contract and the database are exact.
CREATE TABLE pay_stellar.reconciliation_baselines (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    seller_deployment_id  UUID NOT NULL
                          REFERENCES pay_stellar.ledger_contracts (seller_deployment_id)
                          ON DELETE RESTRICT,
    ledger                BIGINT NOT NULL CHECK (ledger > 0),
    -- The contract's totals at `ledger`.
    liabilities           NUMERIC(39, 0) NOT NULL CHECK (liabilities >= 0),
    revenue               NUMERIC(39, 0) NOT NULL CHECK (revenue >= 0),
    -- The contract's totals less the database's available and charged sums
    -- at `ledger`: what earlier events (withdrawals, deposits and charges no
    -- row explains) account for.
    liabilities_offset    NUMERIC(39, 0) NOT NULL,
    revenue_offset        NUMERIC(39, 0) NOT NULL,
    note                  TEXT NOT NULL CHECK (length(note) BETWEEN 1 AND 500),
    recorded_by           TEXT NOT NULL DEFAULT session_user,
    recorded_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX reconciliation_baselines_deployment
    ON pay_stellar.reconciliation_baselines (seller_deployment_id, ledger);

CREATE FUNCTION pay_stellar.reconciliation_baselines_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'reconciliation baselines are append-only'
        USING ERRCODE = 'check_violation';
END
$$;

CREATE TRIGGER reconciliation_baselines_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.reconciliation_baselines
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.reconciliation_baselines_guard();

REVOKE ALL ON pay_stellar.reconciliation_baselines FROM PUBLIC;
GRANT SELECT ON pay_stellar.reconciliation_baselines TO pay_stellar_observer;
GRANT SELECT, INSERT (seller_deployment_id, ledger, liabilities, revenue, liabilities_offset,
                      revenue_offset, note)
    ON pay_stellar.reconciliation_baselines TO pay_stellar_operator;
-- What the operator's tool reads to take a baseline.
GRANT SELECT (available) ON pay_stellar.buyers TO pay_stellar_operator;
GRANT SELECT (seller_deployment_id, state, amount) ON pay_stellar.deposits TO pay_stellar_operator;
