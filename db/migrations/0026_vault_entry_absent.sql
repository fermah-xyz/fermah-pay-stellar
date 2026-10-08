-- `vault_entry_absent`: the worker found no account entry for this vault
-- buyer when it read it. An archived entry the node no longer serves reads
-- the same way, with any limit and exit it holds; anything that brings it
-- back (a deposit, or an event of the buyer's) has the worker read it again.
ALTER TABLE pay_stellar.buyers
    ADD COLUMN vault_entry_absent BOOLEAN NOT NULL DEFAULT false;
GRANT SELECT (vault_entry_absent) ON pay_stellar.buyers
    TO pay_stellar_worker, pay_stellar_operator;
GRANT UPDATE (vault_entry_absent) ON pay_stellar.buyers TO pay_stellar_worker;
