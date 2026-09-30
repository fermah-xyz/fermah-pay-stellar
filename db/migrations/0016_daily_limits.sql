-- The ledger contract can refuse a charge that would pass a daily limit
-- (per account or per seller) with the outcome `above_daily_limit`. Like
-- `above_limit`, it debits nothing and records the charge, so the charge is
-- refused and its amount returned.

ALTER TABLE pay_stellar.charges
    DROP CONSTRAINT charges_outcome_check,
    DROP CONSTRAINT charges_refused_outcome,
    ADD CONSTRAINT charges_outcome_check CHECK (outcome IN
        ('charged', 'insufficient_balance', 'above_limit', 'above_daily_limit', 'duplicate',
         'expired', 'unknown_account')),
    ADD CONSTRAINT charges_refused_outcome CHECK (state <> 'refused'
        OR outcome IN ('insufficient_balance', 'above_limit', 'above_daily_limit', 'expired'));

ALTER TABLE pay_stellar.chain_charge_entries
    DROP CONSTRAINT chain_charge_entries_outcome_check,
    ADD CONSTRAINT chain_charge_entries_outcome_check CHECK (outcome IN
        ('charged', 'insufficient_balance', 'above_limit', 'above_daily_limit', 'duplicate',
         'expired', 'unknown_account'));

ALTER TABLE pay_stellar.charge_resolutions
    DROP CONSTRAINT charge_resolutions_resolution_check,
    ADD CONSTRAINT charge_resolutions_resolution_check CHECK (resolution IN
        ('charged', 'insufficient_balance', 'above_limit', 'above_daily_limit', 'expired',
         'readmitted'));

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
        WHEN 'insufficient_balance', 'above_limit', 'above_daily_limit', 'expired' THEN
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

-- The admin's setting of the daily limits is stored as its own kind of
-- event and reported like the other admin changes.
ALTER TABLE pay_stellar.chain_events
    DROP CONSTRAINT chain_events_kind_check,
    ADD CONSTRAINT chain_events_kind_check CHECK (kind IN
        ('deposit', 'charges', 'withdrawal', 'revenue_withdrawal', 'role', 'pause', 'limits',
         'daily_limits', 'unrecognized'));
