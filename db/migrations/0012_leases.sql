-- Leases let several worker or observer processes run against one database
-- while only one of them acts at a time; the others stand by and take over
-- when the holder's lease lapses or is released. Expiry is judged by the
-- database clock alone, so the processes' clocks need not agree.
--
-- A lease keeps processes from racing for the same work; it is not what
-- keeps money correct. The unique in-flight index and the conditional
-- state changes still hold if a process that lost its lease finishes a
-- round it had started.
CREATE TABLE pay_stellar.leases (
    name        TEXT PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 200),
    holder      UUID NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    CHECK (expires_at > acquired_at)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON pay_stellar.leases
    TO pay_stellar_worker, pay_stellar_observer;
