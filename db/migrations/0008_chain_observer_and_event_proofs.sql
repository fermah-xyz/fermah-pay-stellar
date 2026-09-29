-- Chain observer: the prepaid ledger contract's event stream, matched against
-- the gateway's records, and reconciliation of the treasury, the contract's
-- totals and the database.
--
-- The observer runs as its own process under `pay_stellar_observer`. It reads
-- what it matches and appends what it observes; it cannot write a balance, a
-- deposit, a charge or a binding, and it cannot rewrite what it recorded.

DO $$
BEGIN
    CREATE ROLE pay_stellar_observer NOLOGIN;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
END
$$;

GRANT USAGE ON SCHEMA pay_stellar TO pay_stellar_observer;

-- Where the observer continues reading a deployment's contract events. The
-- row is advanced in the same transaction that stores the events it covers,
-- so an event is stored exactly once whatever the process does in between.
CREATE TABLE pay_stellar.observer_cursors (
    seller_deployment_id  UUID PRIMARY KEY
                          REFERENCES pay_stellar.ledger_contracts (seller_deployment_id)
                          ON DELETE RESTRICT,
    -- The first ledger ever observed; events before it were never read.
    observed_from_ledger  BIGINT NOT NULL CHECK (observed_from_ledger > 0),
    -- Where reading continues while there is no cursor.
    start_ledger          BIGINT NOT NULL CHECK (start_ledger > 0),
    -- The RPC's position after the last event stored, or after the end of
    -- the last range it scanned: `<TOID, 19 digits>-<event index, 10 digits>`.
    cursor                TEXT CHECK (cursor ~ '^[0-9]{19}-[0-9]{10}$'),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A position as one number: TOID * 2^32 + event index for a cursor, and
-- just before the first event of the start ledger without one.
CREATE FUNCTION pay_stellar.observer_position(start_ledger BIGINT, cursor TEXT)
RETURNS NUMERIC
LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN cursor IS NULL THEN (start_ledger::numeric * 4294967296) * 4294967296 - 1
        ELSE substr(cursor, 1, 19)::numeric * 4294967296 + substr(cursor, 21, 10)::numeric
    END
$$;

-- Reading only moves forward: a position that went back would store events
-- a second time.
CREATE FUNCTION pay_stellar.observer_cursors_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.observed_from_ledger <> OLD.observed_from_ledger THEN
        RAISE EXCEPTION 'observed_from_ledger is fixed' USING ERRCODE = 'check_violation';
    END IF;
    IF pay_stellar.observer_position(NEW.start_ledger, NEW.cursor)
        < pay_stellar.observer_position(OLD.start_ledger, OLD.cursor) THEN
        RAISE EXCEPTION 'observer position of % cannot move back', OLD.seller_deployment_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER observer_cursors_forward_only
    BEFORE UPDATE ON pay_stellar.observer_cursors
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.observer_cursors_guard();

-- Every event the deployment's contract emitted, as the RPC reported it.
-- `owner`, `amount` and `reference` repeat the decoded fields that matching
-- and reconciliation query; `payload` holds every decoded field, or the raw
-- XDR of an event that does not decode.
CREATE TABLE pay_stellar.chain_events (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    seller_deployment_id  UUID NOT NULL
                          REFERENCES pay_stellar.ledger_contracts (seller_deployment_id)
                          ON DELETE RESTRICT,
    network               TEXT NOT NULL CHECK (network IN ('stellar:testnet', 'stellar:pubnet')),
    -- The RPC's event id, unique on a network.
    event_id              TEXT NOT NULL CHECK (event_id ~ '^[0-9]{19}-[0-9]{10}$'),
    ledger                BIGINT NOT NULL CHECK (ledger > 0),
    ledger_closed_at      TIMESTAMPTZ NOT NULL,
    transaction_hash      BYTEA NOT NULL CHECK (octet_length(transaction_hash) = 32),
    kind                  TEXT NOT NULL CHECK (kind IN
                              ('deposit', 'charges', 'withdrawal', 'revenue_withdrawal',
                               'role', 'unrecognized')),
    owner                 TEXT,
    amount                NUMERIC(40, 0),
    reference             BYTEA CHECK (octet_length(reference) = 32),
    payload               JSONB NOT NULL,
    observed_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (network, event_id)
);

CREATE INDEX chain_events_deployment_ledger
    ON pay_stellar.chain_events (seller_deployment_id, ledger);

-- The entries of each `charges` event, one row per charge the call settled
-- or refused.
CREATE TABLE pay_stellar.chain_charge_entries (
    chain_event_id  UUID NOT NULL REFERENCES pay_stellar.chain_events (id) ON DELETE RESTRICT,
    entry_index     SMALLINT NOT NULL CHECK (entry_index >= 0),
    owner           TEXT NOT NULL,
    charge_id       BYTEA NOT NULL CHECK (octet_length(charge_id) = 32),
    amount          NUMERIC(40, 0) NOT NULL,
    outcome         TEXT NOT NULL CHECK (outcome IN
                        ('charged', 'insufficient_balance', 'above_limit', 'duplicate',
                         'expired', 'unknown_account')),
    PRIMARY KEY (chain_event_id, entry_index)
);

CREATE INDEX chain_charge_entries_charge
    ON pay_stellar.chain_charge_entries (owner, charge_id);

-- An observed deposit, charge entry or role change that has been decided:
-- it matched the records, or a finding was recorded for it. One without a
-- verdict is still waiting for the records to catch up.
CREATE TABLE pay_stellar.chain_event_checks (
    chain_event_id  UUID NOT NULL REFERENCES pay_stellar.chain_events (id) ON DELETE RESTRICT,
    -- The charge entry's index; 0 for an event with one subject.
    entry_index     SMALLINT NOT NULL CHECK (entry_index >= 0),
    verdict         TEXT NOT NULL CHECK (verdict IN ('matched', 'finding')),
    checked_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_event_id, entry_index)
);

-- What the observer found that the records do not explain. See
-- docs/self-hosting/observer.md for each kind and what to do about it.
CREATE TABLE pay_stellar.reconciliation_findings (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    seller_deployment_id  UUID NOT NULL
                          REFERENCES pay_stellar.ledger_contracts (seller_deployment_id)
                          ON DELETE RESTRICT,
    kind                  TEXT NOT NULL CHECK (kind IN
                              ('event_gap', 'unrecognized_event',
                               'unknown_charge', 'charge_amount_mismatch',
                               'charge_outcome_mismatch', 'charge_unsettled',
                               'unknown_deposit', 'deposit_amount_mismatch',
                               'deposit_outcome_mismatch', 'deposit_unsettled',
                               'role_changed', 'binding_out_of_date',
                               'treasury_deficit', 'treasury_surplus',
                               'treasury_deauthorized',
                               'event_totals_mismatch', 'ledger_totals_mismatch')),
    severity              TEXT NOT NULL CHECK (severity IN ('critical', 'warning', 'info')),
    chain_event_id        UUID REFERENCES pay_stellar.chain_events (id) ON DELETE RESTRICT,
    entry_index           SMALLINT CHECK (entry_index >= 0),
    detail                JSONB NOT NULL,
    observed_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((chain_event_id IS NULL) = (entry_index IS NULL))
);

CREATE INDEX reconciliation_findings_deployment
    ON pay_stellar.reconciliation_findings (seller_deployment_id, observed_at);

-- Observations are evidence: append-only for every role, the owner included.
CREATE TRIGGER chain_events_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.chain_events
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();
CREATE TRIGGER chain_charge_entries_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.chain_charge_entries
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();
CREATE TRIGGER chain_event_checks_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.chain_event_checks
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();
CREATE TRIGGER reconciliation_findings_append_only
    BEFORE UPDATE OR DELETE ON pay_stellar.reconciliation_findings
    FOR EACH ROW EXECUTE FUNCTION pay_stellar.append_only_guard();

REVOKE ALL ON pay_stellar.observer_cursors, pay_stellar.chain_events,
    pay_stellar.chain_charge_entries, pay_stellar.chain_event_checks,
    pay_stellar.reconciliation_findings
    FROM pay_stellar_api, pay_stellar_issuer, pay_stellar_worker, pay_stellar_operator,
         pay_stellar_observer;

-- The observer: what it matches against, read-only; its own tables,
-- append-only; its position, forward-only.
GRANT SELECT (seller_deployment_id, network, contract_address, usdc_address, treasury_address,
              operator_address)
    ON pay_stellar.ledger_contracts TO pay_stellar_observer;
GRANT SELECT (id, seller_deployment_id, network, wallet_address, available)
    ON pay_stellar.buyers TO pay_stellar_observer;
GRANT SELECT (id, buyer_id, seller_deployment_id, network, amount, deposit_id, state)
    ON pay_stellar.deposits TO pay_stellar_observer;
GRANT SELECT (id, buyer_id, seller_deployment_id, network, amount, charge_id, state, outcome)
    ON pay_stellar.charges TO pay_stellar_observer;
GRANT SELECT, INSERT ON pay_stellar.observer_cursors TO pay_stellar_observer;
GRANT UPDATE (start_ledger, cursor, updated_at)
    ON pay_stellar.observer_cursors TO pay_stellar_observer;
GRANT SELECT, INSERT ON pay_stellar.chain_events, pay_stellar.chain_charge_entries,
    pay_stellar.chain_event_checks, pay_stellar.reconciliation_findings
    TO pay_stellar_observer;

-- An operator resolving a quarantined charge, or investigating a finding,
-- reads what the observer recorded.
GRANT SELECT ON pay_stellar.chain_events, pay_stellar.chain_charge_entries,
    pay_stellar.reconciliation_findings
    TO pay_stellar_operator;

-- A charge's record on the contract lapses shortly after the charge's last
-- ledger; after that, whether a batch settled it is shown only by the
-- contract's `charges` events. Searching them needs the first ledger in
-- which the batch could have been included: the latest ledger the worker
-- saw before it created the authorization the batch carries, which no
-- transaction could include earlier. Set when the submission is recorded
-- and never changed (the worker may update only outcome columns). Absent for
-- submissions that carry no charges, and for those recorded before this
-- column existed.
ALTER TABLE pay_stellar.submissions
    ADD COLUMN authorized_from_ledger INTEGER CHECK (authorized_from_ledger > 0);

GRANT SELECT (authorized_from_ledger) ON pay_stellar.submissions TO pay_stellar_operator;
