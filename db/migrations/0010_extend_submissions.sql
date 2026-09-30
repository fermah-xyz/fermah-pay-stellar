-- The settlement worker extends the life of each ledger contract's instance
-- and code before they would be archived, with a transaction recorded like
-- any other submission.

ALTER TABLE pay_stellar.submissions DROP CONSTRAINT submissions_kind_check;
ALTER TABLE pay_stellar.submissions ADD CONSTRAINT submissions_kind_check
    CHECK (kind IN ('deposit', 'charge_batch', 'withdrawal', 'restore', 'extend'));
