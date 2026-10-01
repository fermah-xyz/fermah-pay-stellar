-- The observer reports a contract running other code than the operator
-- expects (`code_changed`).
ALTER TABLE pay_stellar.reconciliation_findings
    DROP CONSTRAINT reconciliation_findings_kind_check,
    ADD CONSTRAINT reconciliation_findings_kind_check CHECK (kind IN
        ('event_gap', 'unrecognized_event',
         'unknown_charge', 'charge_amount_mismatch',
         'charge_outcome_mismatch', 'charge_unsettled',
         'unknown_deposit', 'deposit_amount_mismatch',
         'deposit_outcome_mismatch', 'deposit_unsettled',
         'unknown_withdrawal', 'withdrawal_mismatch', 'withdrawal_outcome_mismatch',
         'unknown_recurring_charge', 'recurring_charge_mismatch',
         'recurring_outcome_mismatch', 'recurring_charge_unsettled',
         'role_changed', 'admin_change', 'binding_out_of_date',
         'treasury_deficit', 'treasury_surplus',
         'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch',
         'code_changed'));

-- Like the other reconciliation checks, it is counted through consecutive
-- checks before it is recorded.
ALTER TABLE pay_stellar.reconciliation_streaks
    DROP CONSTRAINT reconciliation_streaks_kind_check,
    ADD CONSTRAINT reconciliation_streaks_kind_check CHECK (kind IN
        ('treasury_deficit', 'treasury_surplus', 'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch', 'code_changed'));
