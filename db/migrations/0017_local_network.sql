-- A standalone network on one machine (`stellar:local`), for development and
-- continuous integration. Its rows never meet testnet or pubnet rows: every
-- process serves one network.
ALTER TABLE pay_stellar.seller_deployments
    DROP CONSTRAINT seller_deployments_network_check,
    ADD CONSTRAINT seller_deployments_network_check
        CHECK (network IN ('stellar:testnet', 'stellar:pubnet', 'stellar:local'));
ALTER TABLE pay_stellar.submissions
    DROP CONSTRAINT submissions_network_check,
    ADD CONSTRAINT submissions_network_check
        CHECK (network IN ('stellar:testnet', 'stellar:pubnet', 'stellar:local'));
ALTER TABLE pay_stellar.chain_events
    DROP CONSTRAINT chain_events_network_check,
    ADD CONSTRAINT chain_events_network_check
        CHECK (network IN ('stellar:testnet', 'stellar:pubnet', 'stellar:local'));
