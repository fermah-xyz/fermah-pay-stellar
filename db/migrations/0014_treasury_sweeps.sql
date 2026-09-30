-- The settlement worker moves the treasury's USDC above a ceiling to a cold
-- reserve, with a transaction recorded like any other submission.

ALTER TABLE pay_stellar.submissions DROP CONSTRAINT submissions_kind_check;
ALTER TABLE pay_stellar.submissions ADD CONSTRAINT submissions_kind_check
    CHECK (kind IN ('deposit', 'charge_batch', 'withdrawal', 'restore', 'extend', 'sweep'));
