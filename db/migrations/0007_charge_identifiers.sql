-- Charges carry an identifier and a last ledger instead of a per-buyer
-- sequence number.
--
-- The contract now records each settled charge's identifier, with its
-- outcome, until shortly after the charge's last ledger, and refuses a
-- charge past that ledger. The identifier is derived from the seller's
-- idempotency key, so a retried request names the same charge on-chain; the
-- last ledger bounds how long a charge may wait before it is refunded.
--
-- A contract with the new interface cannot settle a charge made under the
-- sequence scheme, so an existing installation upgrades with every charge
-- final: stop the API, let the worker settle what it sent, resolve any
-- quarantined charge, then migrate and upgrade the contract.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pay_stellar.charges
               WHERE state IN ('admitted', 'submitted', 'quarantined')) THEN
        RAISE EXCEPTION 'charges are still admitted, submitted or quarantined; settle or resolve every charge before this upgrade'
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
END
$$;

ALTER TABLE pay_stellar.charges
    ADD COLUMN charge_id BYTEA CHECK (octet_length(charge_id) = 32),
    ADD COLUMN last_ledger BIGINT CHECK (last_ledger > 0);

-- Only final charges remain, and `charges_final_state` refuses any change to
-- them, the owner's included. The backfill is the one change this migration
-- makes to them, inside the migration's transaction. Their last ledger is
-- never read again: nothing is sent or decided for a final charge.
ALTER TABLE pay_stellar.charges DISABLE TRIGGER charges_final_state;
UPDATE pay_stellar.charges
SET charge_id = sha256(convert_to(idempotency_key, 'UTF8')),
    last_ledger = 1;
ALTER TABLE pay_stellar.charges ENABLE TRIGGER charges_final_state;

ALTER TABLE pay_stellar.charges
    ALTER COLUMN charge_id SET NOT NULL,
    ALTER COLUMN last_ledger SET NOT NULL,
    ADD CONSTRAINT charges_charge_id UNIQUE (buyer_id, charge_id),
    DROP COLUMN sequence;

ALTER TABLE pay_stellar.buyers DROP COLUMN next_charge_seq;

-- `expired`: the contract refused the charge past its last ledger, or it was
-- never sent before then; either way nothing was debited and the amount
-- returns to the buyer. `out_of_order` no longer exists.
ALTER TABLE pay_stellar.charges
    DROP CONSTRAINT charges_outcome_check,
    DROP CONSTRAINT charges_check3,
    DROP CONSTRAINT charges_check4,
    ADD CONSTRAINT charges_outcome_check CHECK (outcome IN
        ('charged', 'insufficient_balance', 'above_limit', 'duplicate', 'expired',
         'unknown_account')),
    ADD CONSTRAINT charges_refused_outcome CHECK (state <> 'refused'
        OR outcome IN ('insufficient_balance', 'above_limit', 'expired')),
    ADD CONSTRAINT charges_quarantined_outcome CHECK (state <> 'quarantined'
        OR outcome IS NULL OR outcome IN ('duplicate', 'unknown_account')),
    -- A charge that expires before it is ever sent is refunded without a
    -- submission; every other settled charge names the submission that
    -- settled it.
    DROP CONSTRAINT charges_check,
    ADD CONSTRAINT charges_admitted_unlinked CHECK (state <> 'admitted' OR submission_id IS NULL),
    ADD CONSTRAINT charges_linked CHECK (state NOT IN ('submitted', 'charged', 'quarantined')
        OR submission_id IS NOT NULL),
    ADD CONSTRAINT charges_unlinked_only_if_expired CHECK (state IN ('admitted', 'refused')
        OR submission_id IS NOT NULL),
    -- Batches now hold at most 98 charges. Final charges from larger batches
    -- keep their index: NOT VALID applies the bound to new and changed rows
    -- only, and final rows never change.
    DROP CONSTRAINT charges_batch_index_check,
    ADD CONSTRAINT charges_batch_index_check CHECK (batch_index BETWEEN 0 AND 97) NOT VALID;

GRANT INSERT (charge_id, last_ledger) ON pay_stellar.charges TO pay_stellar_api;

-- Resolving a quarantined charge from its record needs the ledger up to which
-- the batch that carried it could still be included, which only the stored
-- envelope's authorizations tell. The envelope was broadcast, so it holds
-- nothing secret.
GRANT SELECT (envelope_xdr) ON pay_stellar.submissions TO pay_stellar_operator;

-- A quarantined charge may also be resolved as expired: the contract has no
-- record of it and it is past its last ledger, so nothing was debited.
ALTER TABLE pay_stellar.charge_resolutions
    DROP CONSTRAINT charge_resolutions_resolution_check,
    ADD CONSTRAINT charge_resolutions_resolution_check CHECK (resolution IN
        ('charged', 'insufficient_balance', 'above_limit', 'expired', 'readmitted'));

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
        WHEN 'insufficient_balance', 'above_limit', 'expired' THEN
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
