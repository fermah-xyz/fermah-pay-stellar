-- The contract announces pause, unpause and limit changes. The observer
-- stores them as their own kinds of event and reports each as an
-- `admin_change` finding, since only the admin can make them.

ALTER TABLE pay_stellar.chain_events
    DROP CONSTRAINT chain_events_kind_check,
    ADD CONSTRAINT chain_events_kind_check CHECK (kind IN
        ('deposit', 'charges', 'withdrawal', 'revenue_withdrawal', 'role', 'pause', 'limits',
         'unrecognized'));

ALTER TABLE pay_stellar.reconciliation_findings
    DROP CONSTRAINT reconciliation_findings_kind_check,
    ADD CONSTRAINT reconciliation_findings_kind_check CHECK (kind IN
        ('event_gap', 'unrecognized_event',
         'unknown_charge', 'charge_amount_mismatch',
         'charge_outcome_mismatch', 'charge_unsettled',
         'unknown_deposit', 'deposit_amount_mismatch',
         'deposit_outcome_mismatch', 'deposit_unsettled',
         'role_changed', 'admin_change', 'binding_out_of_date',
         'treasury_deficit', 'treasury_surplus',
         'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch'));
