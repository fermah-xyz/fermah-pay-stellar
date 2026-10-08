-- A vault buyer's limit and exit are first read from the contract's account
-- entry, then kept current from the contract's events after that read, so a
-- limit or exit the wallet set before the deployment served it, or before
-- the buyer was registered, is known too.
--
-- `vault_synced_ledger`: the ledger of that read. Events up to it are
-- already reflected in `cap`, `pending_cap` and `exit_*`. NULL until the
-- worker has read it; admission refuses the buyer's charges and withdrawals
-- until then. Always NULL for a prepaid ledger's buyer.
ALTER TABLE pay_stellar.buyers
    ADD COLUMN vault_synced_ledger BIGINT CHECK (vault_synced_ledger > 0);
GRANT SELECT (vault_synced_ledger) ON pay_stellar.buyers
    TO pay_stellar_worker, pay_stellar_operator;
GRANT UPDATE (vault_synced_ledger) ON pay_stellar.buyers TO pay_stellar_worker;

-- An exit pays from the vault's balance, which also backs the charges and
-- withdrawals still held for the buyer. When the contract pays more than
-- `available`, the difference was held for them: `available` then goes below
-- zero by at most what they hold, and their refunds, if they never settle,
-- bring it back to what the contract holds for the buyer.
ALTER TABLE pay_stellar.buyers
    DROP CONSTRAINT buyers_available_check,
    ADD CONSTRAINT buyers_available_check
        CHECK (available >= 0 OR vault_synced_ledger IS NOT NULL);

-- The observer matches a limit or exit event only with a request that
-- existed when the event's ledger closed, and counts an exit as applied by
-- the worker by its exact position in the event stream.
GRANT SELECT (created_at) ON pay_stellar.vault_requests TO pay_stellar_observer;
GRANT SELECT (created_at) ON pay_stellar.deposits TO pay_stellar_observer;
GRANT SELECT (cursor) ON pay_stellar.vault_event_cursors TO pay_stellar_observer;
