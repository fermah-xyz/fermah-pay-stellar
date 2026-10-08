-- While the admin's proposed code may still be installed, new buyers' money
-- would run under it without the notice the vault gives existing buyers: the
-- gateway refuses deposits and mandates until the proposal is cancelled,
-- installed or lapses. `upgrade_pending_until`: the last ledger the proposed
-- code may be installed in, as the vault's events or its instance showed;
-- the worker writes it.
ALTER TABLE pay_stellar.vault_event_cursors
    ADD COLUMN upgrade_pending_until BIGINT CHECK (upgrade_pending_until > 0);

-- The vault announces an installed upgrade.
ALTER TABLE pay_stellar.chain_events
    DROP CONSTRAINT chain_events_kind_check,
    ADD CONSTRAINT chain_events_kind_check CHECK (kind IN
        ('deposit', 'charges', 'withdrawal', 'revenue_withdrawal', 'role', 'pause', 'limits',
         'daily_limits', 'mandate', 'revoke', 'recurring', 'cap_raised', 'cap_lowered',
         'exit_requested', 'exit', 'launch', 'upgrade_proposed', 'upgrade_cancelled',
         'upgrade_installed', 'unrecognized'));
