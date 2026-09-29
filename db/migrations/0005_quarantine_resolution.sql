-- Resolving quarantined charges.
--
-- A charge is quarantined when the contract's answer contradicts the
-- gateway's records, or when its outcome cannot be established. It stays
-- debited, and its buyer gets no further batches, until an operator resolves
-- it with evidence. The only way out of quarantine is
-- `resolve_quarantined_charge`, which records who resolved it, how, and on
-- what evidence, in the same transaction as the state change.

DO $$
BEGIN
    CREATE ROLE pay_stellar_operator NOLOGIN;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
END
$$;

GRANT USAGE ON SCHEMA pay_stellar TO pay_stellar_operator;

CREATE TABLE pay_stellar.charge_resolutions (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    charge_id           UUID NOT NULL REFERENCES pay_stellar.charges (id) ON DELETE RESTRICT,
    -- charged / insufficient_balance / above_limit: the contract settled the
    --   charge's sequence with this outcome, as the evidence shows.
    -- readmitted: the charge's sequence is still unconsumed on the contract,
    --   so the charge goes back to the next batch.
    resolution          TEXT NOT NULL CHECK (resolution IN
                            ('charged', 'insufficient_balance', 'above_limit', 'readmitted')),
    evidence            TEXT NOT NULL CHECK (length(evidence) BETWEEN 1 AND 2000),
    quarantine_outcome  TEXT,
    quarantine_reason   TEXT,
    resolved_by         TEXT NOT NULL DEFAULT session_user,
    resolved_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Ties the record to the transaction that changed the charge; the charge
    -- guard below accepts leaving quarantine only in that transaction.
    transaction_id      xid8 NOT NULL DEFAULT pg_current_xact_id()
);

CREATE INDEX charge_resolutions_charge ON pay_stellar.charge_resolutions (charge_id);

-- The audit trail is append-only for every role, the owner included.
CREATE FUNCTION pay_stellar.charge_resolutions_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'charge resolutions are append-only'
        USING ERRCODE = 'check_violation';
END
$$;

CREATE TRIGGER charge_resolutions_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.charge_resolutions
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.charge_resolutions_guard();

-- `charged` and `refused` stay absorbing. `quarantined` may be left only in a
-- transaction that recorded a resolution for that charge, which only
-- `resolve_quarantined_charge` can insert. SECURITY DEFINER so that the
-- worker's updates, which trigger this check, may read the audit table.
CREATE OR REPLACE FUNCTION pay_stellar.charges_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pay_stellar AS $$
BEGIN
    IF OLD.state IN ('charged', 'refused') THEN
        RAISE EXCEPTION 'charge % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.state = 'quarantined' AND NOT EXISTS (
        SELECT 1 FROM pay_stellar.charge_resolutions r
        WHERE r.charge_id = OLD.id AND r.transaction_id = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION 'charge % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

-- Moves a quarantined charge to the state its evidence establishes. A
-- refused charge's amount returns to the buyer's available balance, in the
-- same transaction, exactly as when the worker applies a refusal.
CREATE FUNCTION pay_stellar.resolve_quarantined_charge(
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
        WHEN 'insufficient_balance', 'above_limit' THEN
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

REVOKE ALL ON FUNCTION pay_stellar.resolve_quarantined_charge(UUID, TEXT, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pay_stellar.resolve_quarantined_charge(UUID, TEXT, TEXT)
    TO pay_stellar_operator;

REVOKE ALL ON pay_stellar.charge_resolutions FROM pay_stellar_api, pay_stellar_issuer,
    pay_stellar_worker, pay_stellar_operator;
GRANT SELECT ON pay_stellar.charge_resolutions TO pay_stellar_operator;
GRANT SELECT ON pay_stellar.charges TO pay_stellar_operator;
GRANT SELECT (id, seller_deployment_id, network, wallet_address)
    ON pay_stellar.buyers TO pay_stellar_operator;
GRANT SELECT ON pay_stellar.ledger_contracts TO pay_stellar_operator;
GRANT SELECT (id, state, outer_hash, ledger, last_error)
    ON pay_stellar.submissions TO pay_stellar_operator;
