# Self-hosting: chain observer

`fermah-pay-stellar-observer` reads every event of each bound deployment's
ledger contract, matches it against the gateway's records, and reconciles the
treasury's USDC, the contract's totals and the database; see
[the design](../architecture/observer.md). Run one per network. It needs no
Stellar key.

## Database role

The observer logs in as a member of `pay_stellar_observer`, which can read
the ledger bindings, buyers' wallets and available balances, and the amount,
identifier and state of deposits and charges; append to its own tables; and
move its read position forward. It cannot write a balance, a deposit, a
charge or a binding, and nobody, the database owner included, can update or
delete an observed event or a finding.

```sql
CREATE ROLE pay_stellar_watch LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_observer;
GRANT CONNECT ON DATABASE pay_stellar TO pay_stellar_watch;
```

## Running it

```bash
export PAY_STELLAR_OBSERVER_DATABASE_URL=postgres://pay_stellar_watch:<secret>@<host>/pay_stellar
export PAY_STELLAR_NETWORK=stellar:testnet
export PAY_STELLAR_RPC_URL=https://soroban-testnet.stellar.org
cargo run -q -p fermah-pay-stellar-gateway --bin fermah-pay-stellar-observer
```

| Variable | Value |
|---|---|
| `PAY_STELLAR_OBSERVER_DATABASE_URL` | URL of the observer login role |
| `PAY_STELLAR_NETWORK`, `PAY_STELLAR_RPC_URL`, `PAY_STELLAR_RPC_TIMEOUT_SECS` | as for the gateway; the network is checked at startup |
| `PAY_STELLAR_OBSERVER_START` | where to start reading a deployment observed for the first time: `oldest` (default, the oldest ledger the RPC retains), `latest`, or a ledger number |
| `PAY_STELLAR_OBSERVER_SETTLE_WITHIN_SECS` | how long after an event's ledger the records may still lag it, default 7200. Keep it above the deposit authorization validity (`PAY_STELLAR_DEPOSIT_AUTHORIZATION_LEDGERS`, about an hour by default): a deposit the buyer includes themselves is confirmed only once that has lapsed |
| `PAY_STELLAR_OBSERVER_POLL_SECS` | seconds between reads of new events, default 5 |
| `PAY_STELLAR_OBSERVER_RECONCILE_SECS` | seconds between reconciliations, default 60 |
| `PAY_STELLAR_OBSERVER_CONFIRMATIONS` | consecutive reconciliations a discrepancy must persist through before it is recorded, default 3 |
| `PAY_STELLAR_OBSERVER_PAGE_SIZE` | events per RPC call, 1 to 10000, default 1000 |
| `PAY_STELLAR_OBSERVER_MAX_BACKOFF_SECS` | longest wait between retries after a failed round, default 300 |
| `PAY_STELLAR_LEASE_SECS` | how long an observer's lease lasts without renewal, default 15, at least 3. Several observers may run for one network; the one holding the lease observes and the others take over when it stops |

The start position applies once per deployment; afterwards the observer
continues from where it stopped. The reconciliation of the contract's totals
against the event stream and the database needs every event since the
contract was deployed, so start the observer while the RPC still retains
the deployment ledger, with `PAY_STELLAR_OBSERVER_START` set to that ledger
or to `oldest`. Public RPC nodes retain about seven days.

The observer stops on `SIGTERM` or Ctrl-C after its current round.

## Findings

Every finding is a row of `pay_stellar.reconciliation_findings` (kind,
severity, JSON detail, time), and a log line at `error` (critical), `warn`
(warning) or `info` level with the fields `seller_deployment_id`, `kind`,
`severity` and `detail`. Findings about an event carry its `event_id`,
`ledger` and `transaction_hash`; the event itself is in
`pay_stellar.chain_events`. Alert on `severity`. A login role that is a
member of `pay_stellar_operator` can read both tables.

| Kind | Severity | Meaning | What to do |
|---|---|---|---|
| `event_gap` | warning | Ledgers `from_ledger` to `to_ledger` were pruned by the RPC before the observer read them: their events are unknown. | Read that range from a node or archive with longer history and check it by hand. Keep the observer running, or restart it well within the RPC's retention. |
| `unrecognized_event` | warning | The contract emitted an event this version does not decode, for example after a contract upgrade. The raw XDR is in `detail`. | Upgrade the gateway to the version that matches the contract before trusting its reconciliation. |
| `unknown_charge` | critical if it debited, otherwise warning | The operator key settled a charge the gateway never admitted. | Treat the operator key as exposed: pause the contract with the admin key and [rotate the operator](rotation.md). Refund the buyer if the charge debited. |
| `charge_amount_mismatch` | critical | A settled charge's amount differs from the gateway's row. | Investigate as a compromised operator key or altered database. |
| `charge_outcome_mismatch` | critical if one side debited and the other did not, otherwise warning | The contract settled the charge differently from how the gateway recorded it. | Compare the event's transaction with the charge; correct the buyer's balance with the seller. |
| `charge_unsettled` | warning | The contract settled the charge, but the gateway's row is still not final after the settlement grace. | Check that the worker runs; resolve a quarantined charge with the transaction in `detail` ([quarantine](quarantine.md)). |
| `unknown_deposit` | warning | A deposit to the contract that the gateway did not prepare: the account holds that credit on-chain, but it is not in the buyer's available balance. | Expected if buyers deposit directly; otherwise investigate who called the contract. |
| `deposit_amount_mismatch` | critical | A deposit's amount differs from the gateway's row. | Investigate the database; the buyer's signature fixes the amount. |
| `deposit_outcome_mismatch` | critical | The contract credited a deposit the gateway closed as failed or expired: the buyer's credit is missing from the available balance. | Credit the buyer through the seller; report the case. |
| `deposit_unsettled` | warning | The contract credited a deposit whose row is still not final after the grace. | Check that the worker runs. |
| `role_changed` | warning | The admin or seller role was rotated on the contract. | Confirm the rotation was intended; if not, the admin key is compromised. |
| `admin_change` | warning | The admin paused or unpaused the contract, or changed its deposit and charge limits; `detail` holds the new state. | Confirm the change was intended; if not, the admin key is compromised. |
| `binding_out_of_date` | warning | The operator or treasury was rotated on the contract but the deployment's binding still names the previous account after the grace. | Run `admin sync-ledger` ([rotation](rotation.md)); if the rotation was not intended, the admin key is compromised. |
| `treasury_deficit` | critical | The treasury holds less USDC than the contract owes buyers and the seller. | Pause the contract with the admin key and find where the USDC went; top up the treasury. After a treasury rotation, move the previous treasury's USDC. |
| `treasury_surplus` | info | The treasury holds more USDC than the contract owes. | Expected if it holds other funds; otherwise find the source. |
| `treasury_deauthorized` | critical | The treasury's USDC trustline is missing or no longer authorized by the USDC issuer. Its balance may still cover what is owed, but every deposit and withdrawal fails. | Contact the issuer; consider rotating to another treasury and moving the USDC. |
| `event_totals_mismatch` | warning | The contract's totals differ from the sums of the events the observer read (since the last baseline, if any). | If `coverage` shows a gap or a start after deployment, the history is incomplete: check the books by hand, then record a baseline (below). Otherwise events were missed or the contract changed behaviour: investigate. |
| `ledger_totals_mismatch` | warning | The contract's totals fall outside what the database allows for, even counting charges and deposits in flight. | Look for the per-event findings that explain it; otherwise compare buyer balances on-chain with `available`. |

Reconciliation findings (`treasury_*`, `*_totals_mismatch`) are recorded once
when a discrepancy has persisted through the configured confirmations, and
again only after it cleared and came back. Restarting the observer changes
neither.

## Resuming reconciliation after lost history

When events were lost (an `event_gap`), or the observer began after the
contract's first events, the event and database checks cannot be
reconciled from events alone. After checking the books by hand, an operator
acknowledges them as they stand:

```bash
admin observer-baseline --deployment-id <DEPLOYMENT_ID> --note "<why>" \
  --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org
```

The command runs under the operator role and prints the baseline's ledger.
From that ledger on, reconciliation compares only what changes, so any later
discrepancy is still found. It is refused while any charge or deposit is in
flight; retry once they are final. Baselines are kept in
`pay_stellar.reconciliation_baselines`, which no role can change or delete.
