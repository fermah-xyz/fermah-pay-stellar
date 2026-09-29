-- Restores of archived contract state are submitted like any other
-- transaction, so they are recorded before they are sent.
--
-- A deposit is always decided from the contract's own deposit marker, read
-- after the buyer's authorization has lapsed: the marker either exists or can
-- no longer come to exist, so a deposit's outcome is never left for an
-- operator to establish.

ALTER TABLE pay_stellar.submissions DROP CONSTRAINT submissions_kind_check;
ALTER TABLE pay_stellar.submissions ADD CONSTRAINT submissions_kind_check
    CHECK (kind IN ('deposit', 'charge_batch', 'withdrawal', 'restore'));

ALTER TABLE pay_stellar.deposits DROP CONSTRAINT deposits_state_check;
ALTER TABLE pay_stellar.deposits ADD CONSTRAINT deposits_state_check
    CHECK (state IN ('awaiting_signature', 'signed', 'submitted', 'confirmed', 'failed',
                     'expired'));
