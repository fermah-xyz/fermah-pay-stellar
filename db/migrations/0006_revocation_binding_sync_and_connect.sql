-- Final revocation, contract-verified binding updates, and per-database
-- connection rights.

-- A revoked key stays revoked. The issuer role may set `revoked_at`; without
-- this guard it could also clear it and bring a leaked key back.
CREATE FUNCTION pay_stellar.api_keys_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.revoked_at IS NOT NULL THEN
        RAISE EXCEPTION 'api key % is revoked', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER api_keys_revocation_final
    BEFORE UPDATE ON pay_stellar.api_keys
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.api_keys_guard();

-- The ledger contract can move its operator and treasury to other accounts.
-- The deployment's binding follows through `sync_ledger_binding`, the only
-- way to change it, which records every change. The provisioning tool reads
-- the new accounts from the contract itself before calling it.
CREATE TABLE pay_stellar.ledger_binding_changes (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    seller_deployment_id  UUID NOT NULL
                          REFERENCES pay_stellar.ledger_contracts (seller_deployment_id)
                          ON DELETE RESTRICT,
    previous_operator     TEXT NOT NULL,
    current_operator      TEXT NOT NULL,
    previous_treasury     TEXT NOT NULL,
    current_treasury      TEXT NOT NULL,
    evidence              TEXT NOT NULL CHECK (length(evidence) BETWEEN 1 AND 2000),
    changed_by            TEXT NOT NULL DEFAULT session_user,
    changed_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE FUNCTION pay_stellar.append_only_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME USING ERRCODE = 'check_violation';
END
$$;

CREATE TRIGGER ledger_binding_changes_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.ledger_binding_changes
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();

CREATE FUNCTION pay_stellar.sync_ledger_binding(
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
    IF bound.operator_address = operator AND bound.treasury_address = treasury THEN
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

REVOKE ALL ON FUNCTION pay_stellar.sync_ledger_binding(UUID, TEXT, TEXT, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pay_stellar.sync_ledger_binding(UUID, TEXT, TEXT, TEXT)
    TO pay_stellar_issuer;
REVOKE ALL ON pay_stellar.ledger_binding_changes FROM pay_stellar_api, pay_stellar_issuer,
    pay_stellar_worker, pay_stellar_operator;
GRANT SELECT ON pay_stellar.ledger_binding_changes TO pay_stellar_issuer;

-- The group roles are shared by every database on the server, so a login
-- role that belongs to one for another database, e.g. a staging one, would
-- otherwise reach this database through PUBLIC's default CONNECT right.
-- Connecting is granted per login role instead (docs/self-hosting).
DO $$
BEGIN
    EXECUTE format('REVOKE CONNECT ON DATABASE %I FROM PUBLIC', current_database());
END
$$;
