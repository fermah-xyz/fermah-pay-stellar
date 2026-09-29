-- Durable transaction submission.
--
-- A submission row is written with the complete signed envelope before the
-- envelope is ever broadcast. From then on the row, not process memory, is
-- the record of what may be in flight: after a crash, only these exact bytes
-- are resent, and an outcome is established only through the envelope's hash.

DO $$
BEGIN
    CREATE ROLE pay_stellar_worker NOLOGIN;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
END
$$;

GRANT USAGE ON SCHEMA pay_stellar TO pay_stellar_worker;

CREATE TABLE pay_stellar.submissions (
    id                  UUID PRIMARY KEY,
    network             TEXT NOT NULL CHECK (network IN ('stellar:testnet', 'stellar:pubnet')),
    kind                TEXT NOT NULL CHECK (kind IN ('deposit', 'charge_batch', 'withdrawal')),
    -- installed: the envelope may be in flight; its outcome is not yet known.
    -- succeeded / failed: the network included it, with this result.
    -- expired: proven never included (validity window over and the source's
    --   sequence never reached this envelope's sequence).
    -- quarantined: the evidence contradicts itself; an operator must decide.
    state               TEXT NOT NULL
                        CHECK (state IN ('installed', 'succeeded', 'failed', 'expired', 'quarantined')),
    source_address      TEXT NOT NULL CHECK (source_address ~ '^G[A-Z2-7]{55}$'),
    fee_source_address  TEXT NOT NULL CHECK (fee_source_address ~ '^G[A-Z2-7]{55}$'),
    sequence            BIGINT NOT NULL CHECK (sequence > 0),
    valid_until         TIMESTAMPTZ NOT NULL,
    inner_hash          BYTEA NOT NULL CHECK (octet_length(inner_hash) = 32),
    outer_hash          BYTEA NOT NULL UNIQUE CHECK (octet_length(outer_hash) = 32),
    envelope_xdr        TEXT NOT NULL,
    ledger              INTEGER,
    fee_charged         BIGINT,
    result_xdr          TEXT,
    return_value_xdr    TEXT,
    last_error          TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at         TIMESTAMPTZ,
    -- Deliberately no uniqueness on (source, sequence): after an envelope is
    -- proven never included, the source's sequence has not moved and the next
    -- envelope must carry the same number.
    CHECK (
        (state IN ('succeeded', 'failed')) = (ledger IS NOT NULL AND result_xdr IS NOT NULL)
    ),
    CHECK ((state = 'installed') = (resolved_at IS NULL))
);

-- One source account signs at most one envelope that may still land. Stellar
-- accepts only the next sequence number, so a second in-flight envelope from
-- the same source would either fail or displace the first; this index makes
-- the single-flight rule a property of the schema.
CREATE UNIQUE INDEX submissions_one_in_flight_per_source
    ON pay_stellar.submissions (network, source_address)
    WHERE state = 'installed';

-- An outcome, once recorded, is final: a later RPC answer can never rewrite
-- it, and the installed envelope and identities can never change.
CREATE FUNCTION pay_stellar.submissions_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state <> 'installed' THEN
        RAISE EXCEPTION 'submission % is already %', OLD.id, OLD.state
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER submissions_final_state
    BEFORE UPDATE ON pay_stellar.submissions
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.submissions_guard();

REVOKE ALL ON pay_stellar.submissions FROM pay_stellar_api, pay_stellar_issuer, pay_stellar_worker;
GRANT SELECT, INSERT ON pay_stellar.submissions TO pay_stellar_worker;
GRANT UPDATE (state, ledger, fee_charged, result_xdr, return_value_xdr, last_error, resolved_at)
    ON pay_stellar.submissions TO pay_stellar_worker;
