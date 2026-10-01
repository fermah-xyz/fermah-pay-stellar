-- The observer reports a mandate authorized or revoked on-chain that the
-- gateway did not prepare (`mandate_changed_elsewhere`).
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
         'mandate_changed_elsewhere',
         'role_changed', 'admin_change', 'binding_out_of_date',
         'treasury_deficit', 'treasury_surplus',
         'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch',
         'code_changed'));

-- What the observer matches a `revoke` event against: a revocation the buyer
-- signed whose signature was still valid at the event's ledger.
GRANT SELECT (buyer_id, seller_deployment_id, signed_at, expiration_ledger)
    ON pay_stellar.revocations TO pay_stellar_observer;

-- A quarantined recurring charge resolved as refused because the contract no
-- longer holds its mandate (`no_mandate`) or the mandate is over
-- (`mandate_expired`) closes the mandate, as the worker does when it
-- settles such an answer itself: otherwise the API would keep admitting
-- charges the contract refuses.
CREATE OR REPLACE FUNCTION pay_stellar.resolve_quarantined_recurring_charge(
    charge UUID,
    resolution TEXT,
    evidence TEXT
) RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pay_stellar AS $$
DECLARE
    quarantined RECORD;
BEGIN
    SELECT id, state, last_error, mandate_row_id INTO quarantined
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
    IF resolution IN ('no_mandate', 'mandate_expired') THEN
        UPDATE pay_stellar.mandates
        SET state = CASE WHEN resolution = 'mandate_expired' THEN 'ended' ELSE 'revoked' END,
            closed_at = now(),
            last_error = CASE WHEN resolution = 'mandate_expired'
                              THEN 'the contract answered mandate_expired'
                              ELSE 'the contract no longer holds this mandate' END
        WHERE id = quarantined.mandate_row_id AND state = 'active';
    END IF;
    RETURN resolution;
END
$$;
