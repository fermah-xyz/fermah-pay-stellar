-- At most one treasury sweep in flight per network. Workers take turns
-- through a lease, but one that lost its lease may finish its round while
-- the next holder starts: two sweeps computed from the same balance would
-- otherwise both go out and leave the treasury below what held withdrawals
-- need. Sweeps from different source accounts do not collide on the
-- one-in-flight-per-source index, so this one says it directly.
CREATE UNIQUE INDEX submissions_one_sweep_in_flight ON pay_stellar.submissions (network)
    WHERE kind = 'sweep' AND state = 'installed';
