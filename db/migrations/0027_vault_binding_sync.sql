-- A vault's operator can move to another account like a prepaid ledger's,
-- so its binding follows through the same audited function. A vault holds
-- its USDC itself and names no treasury: the binding keeps `treasury_address`
-- NULL, the audit records NULL, and the comparison treats two NULLs as the
-- same treasury so a sync with nothing to change records nothing.
ALTER TABLE pay_stellar.ledger_binding_changes
    ALTER COLUMN previous_treasury DROP NOT NULL,
    ALTER COLUMN current_treasury DROP NOT NULL;

CREATE OR REPLACE FUNCTION pay_stellar.sync_ledger_binding(
    deployment UUID,
    operator TEXT,
    treasury TEXT,
    evidence TEXT
) RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pay_stellar AS $$
DECLARE
    bound RECORD;
BEGIN
    SELECT operator_address, treasury_address INTO bound
    FROM pay_stellar.ledger_contracts WHERE seller_deployment_id = deployment FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'deployment % has no ledger binding', deployment
            USING ERRCODE = 'no_data_found';
    END IF;
    -- The contract's kind is fixed at deployment; a treasury appearing or
    -- disappearing means another contract is being described.
    IF (bound.treasury_address IS NULL) <> (treasury IS NULL) THEN
        RAISE EXCEPTION 'deployment % is bound to a % contract', deployment,
            CASE WHEN bound.treasury_address IS NULL THEN 'vault' ELSE 'prepaid ledger' END
            USING ERRCODE = 'check_violation';
    END IF;
    IF bound.operator_address = operator
       AND bound.treasury_address IS NOT DISTINCT FROM treasury THEN
        RETURN FALSE;
    END IF;
    INSERT INTO pay_stellar.ledger_binding_changes
        (seller_deployment_id, previous_operator, current_operator, previous_treasury,
         current_treasury, evidence)
    VALUES (deployment, bound.operator_address, operator, bound.treasury_address, treasury,
            evidence);
    UPDATE pay_stellar.ledger_contracts
    SET operator_address = operator, treasury_address = treasury
    WHERE seller_deployment_id = deployment;
    RETURN TRUE;
END
$$;

-- Every event up to a vault's cursor has been applied exactly once, and
-- applying an `exited` event debits the buyer, so a cursor moved back would
-- debit the same exit again. The worker's own advance is a compare-and-set
-- that only moves forward; this keeps any other write from moving it back.
-- Cursors are fixed-width digits, so text order is stream order. `ledger`
-- may fall: a reading taken as older only makes admission and the closing
-- of requests wait longer.
CREATE FUNCTION pay_stellar.vault_event_cursors_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.seller_deployment_id <> OLD.seller_deployment_id OR NEW.network <> OLD.network THEN
        RAISE EXCEPTION 'a vault event cursor belongs to one deployment'
            USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.cursor < OLD.cursor THEN
        RAISE EXCEPTION 'vault event cursor of % cannot move back', OLD.seller_deployment_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER vault_event_cursors_forward_only
    BEFORE UPDATE ON pay_stellar.vault_event_cursors
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.vault_event_cursors_guard();

-- A stored signature is final. The API stores it together with what it
-- commits (the hold on `available` for a withdrawal, the limit a deposit
-- carries), and the worker returns or applies that by whether the row was
-- signed; a row moved back to unsigned would strand it.
CREATE FUNCTION pay_stellar.signature_final_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.signed_at IS NOT NULL AND (NEW.signed_at IS DISTINCT FROM OLD.signed_at
        OR NEW.signed_authorization_xdr IS DISTINCT FROM OLD.signed_authorization_xdr) THEN
        RAISE EXCEPTION '% % is already signed', TG_TABLE_NAME, OLD.id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER deposits_signature_final
    BEFORE UPDATE ON pay_stellar.deposits
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.signature_final_guard();
CREATE TRIGGER withdrawals_signature_final
    BEFORE UPDATE ON pay_stellar.withdrawals
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.signature_final_guard();
CREATE TRIGGER mandates_signature_final
    BEFORE UPDATE ON pay_stellar.mandates
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.signature_final_guard();
CREATE TRIGGER revocations_signature_final
    BEFORE UPDATE ON pay_stellar.revocations
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.signature_final_guard();
CREATE TRIGGER vault_requests_signature_final
    BEFORE UPDATE ON pay_stellar.vault_requests
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.signature_final_guard();

-- The submission that last carried a buyer's unlocked exit. While it may
-- still land, or has landed and its event is not read yet, the worker does
-- not send another: a worker restarted mid-flight would otherwise pay the
-- fee of an exit the contract then refuses.
ALTER TABLE pay_stellar.buyers
    ADD COLUMN exit_submission_id UUID REFERENCES pay_stellar.submissions (id)
        ON DELETE RESTRICT;
GRANT SELECT (exit_submission_id), UPDATE (exit_submission_id) ON pay_stellar.buyers
    TO pay_stellar_worker;
