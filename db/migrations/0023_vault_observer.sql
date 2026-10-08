-- What the observer reads of a vault's requests to judge its limit and exit
-- events, and its findings about them: a limit or exit the gateway did not
-- prepare (`vault_changed_elsewhere`) and a proposed code upgrade
-- (`upgrade_proposed`).
GRANT SELECT (buyer_id, seller_deployment_id, kind, cap, amount, destination, signed_at,
              expiration_ledger)
    ON pay_stellar.vault_requests TO pay_stellar_observer;
GRANT SELECT (cap, signed_at, expiration_ledger) ON pay_stellar.deposits TO pay_stellar_observer;
-- How far the worker has applied a vault's exits to `available`, for
-- reconciliation.
GRANT SELECT (seller_deployment_id, ledger) ON pay_stellar.vault_event_cursors
    TO pay_stellar_observer;

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
         'mandate_changed_elsewhere', 'vault_changed_elsewhere', 'upgrade_proposed',
         'role_changed', 'admin_change', 'binding_out_of_date',
         'treasury_deficit', 'treasury_surplus',
         'treasury_deauthorized',
         'event_totals_mismatch', 'ledger_totals_mismatch',
         'code_changed'));
