-- A buyer's wallet may be a contract account (`C...`), such as a smart
-- wallet, as well as a classic account (`G...`); a withdrawal may pay
-- either. The contract treats both alike as owners and destinations.
ALTER TABLE pay_stellar.buyers
    DROP CONSTRAINT buyers_wallet_address_check,
    ADD CONSTRAINT buyers_wallet_address_check
        CHECK (wallet_address ~ '^[GC][A-Z2-7]{55}$');

ALTER TABLE pay_stellar.withdrawals
    DROP CONSTRAINT withdrawals_destination_address_check,
    ADD CONSTRAINT withdrawals_destination_address_check
        CHECK (destination_address ~ '^[GC][A-Z2-7]{55}$');
