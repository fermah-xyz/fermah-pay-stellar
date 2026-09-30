-- Login roles for the development stack, one per process, each a member of
-- the group role the migrations created and allowed to connect to this
-- database only. Development passwords; see docs/self-hosting/database.md
-- for a real deployment.
DO $$
DECLARE
    login record;
BEGIN
    FOR login IN
        SELECT * FROM (VALUES
            ('pay_stellar_gateway', 'pay_stellar_api'),
            ('pay_stellar_admin', 'pay_stellar_issuer'),
            ('pay_stellar_settlement', 'pay_stellar_worker'),
            ('pay_stellar_ops', 'pay_stellar_operator'),
            ('pay_stellar_watch', 'pay_stellar_observer')
        ) AS r(name, grp)
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = login.name) THEN
            EXECUTE format('CREATE ROLE %I LOGIN PASSWORD %L IN ROLE %I',
                           login.name, 'dev-' || login.name, login.grp);
        END IF;
        EXECUTE format('GRANT CONNECT ON DATABASE pay_stellar TO %I', login.name);
    END LOOP;
END
$$;
