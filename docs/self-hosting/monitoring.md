# Self-hosting: monitoring

Every process writes JSON logs to stdout, exports traces over OTLP, and serves Prometheus metrics.

| Setting | Process | Effect |
|---|---|---|
| `RUST_LOG` | all | log filter, default `info` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | all | export traces over OTLP/gRPC to this collector (for example `http://otel-collector:4317`); not exported when unset |
| `PAY_STELLAR_METRICS_ADDR` | gateway, worker, observer | serve Prometheus metrics at `/metrics` on this address (for example `0.0.0.0:9464`); not served when unset |

Scrape each process under a job named `pay-stellar-<process>`, and load the alert rules in [`deploy/monitoring/alerts.yml`](../../deploy/monitoring/alerts.yml). What each part of this watches, and why, is in the [monitoring plan](monitoring-plan.md).

## Dashboard and alert delivery

[`deploy/monitoring/`](../../deploy/monitoring) also holds a Grafana dashboard (`grafana/dashboards/pay-stellar.json`, with its provisioning) and an Alertmanager configuration. The dashboard has a row per threat the [threat model](../security/threat-model.md) names, plus health, solvency and charge patterns. The [dev stack](../quickstart/dev-stack.md)'s `monitoring` profile runs Prometheus, Alertmanager and Grafana with them.

Alertmanager as shipped holds alerts without sending them anywhere. To be told, give it a receiver and route the alerts to it, for example:

```yaml
route:
  receiver: operators
  group_by: [alertname, deployment, seller_deployment_id]
  routes:
    - matchers: [severity="critical"]
      receiver: operators
      repeat_interval: 1h
receivers:
  - name: operators
    webhook_configs:
      - url: https://alerts.example/hook
```

[Drills](monitoring-drills.md) provoke each threat's condition on testnet and wait for its alert. The `Monitoring` workflow checks that every rule and dashboard query names a metric the code emits, that each query parses on Prometheus, and that Grafana loads the dashboard (`deploy/monitoring/check.py`).

## Metrics

| Metric | Process | Meaning |
|---|---|---|
| `pay_stellar_submissions_closed_total{kind,state}` | worker | transactions that reached a final state: `succeeded`, `failed`, `expired`, `quarantined` |
| `pay_stellar_fees_charged_stroops_total{kind}` | worker | fees the network charged for included transactions, succeeded or failed, by kind |
| `pay_stellar_submission_seconds{kind,state}` | worker | time from building a transaction to its final state |
| `pay_stellar_submissions_in_flight` | worker | transactions whose outcome is still open |
| `pay_stellar_inclusion_bid_stroops` | worker | inclusion bid per operation of the last transaction built |
| `pay_stellar_charges_waiting` | worker | admitted charges not yet in a batch |
| `pay_stellar_oldest_waiting_charge_seconds` | worker | how long the oldest of them has waited |
| `pay_stellar_charges_settled_total{result}` | worker | charges settled by result: `charged`, a refusal outcome, `quarantined`, `requeued` |
| `pay_stellar_deposits_closed_total{state}` | worker | deposits closed: `confirmed`, `expired`, `failed` |
| `pay_stellar_hot_treasury_usdc`, `pay_stellar_hot_treasury_floor_usdc` | worker | the treasury's USDC and its floor, when a [cold reserve](treasury.md) is configured |
| `pay_stellar_withdrawals_closed_total{state}` | worker | withdrawals closed: `confirmed`, `expired`, `failed` |
| `pay_stellar_withdrawals_waiting` | worker | signed withdrawals not yet sent |
| `pay_stellar_oldest_waiting_withdrawal_seconds` | worker | how long the oldest of them has waited since it was signed |
| `pay_stellar_fee_source_spendable_stroops` | worker | XLM the fee account can spend above its reserve; below the fee floor no new transaction is built |
| `pay_stellar_contract_ttl_ledgers{deployment,entry}` | worker | ledgers of life left for a served contract's `instance` and `code`, read every `PAY_STELLAR_TTL_CHECK_SECS` |
| `pay_stellar_signing_failures_total{role}` | worker | signatures that failed or did not verify, by key: `operator`, `source`, `fee_source` |
| `pay_stellar_worker_step_failures_total` | worker | settlement rounds that failed |
| `pay_stellar_lease_held{role}` | worker, observer | 1 while this process holds its lease and acts, 0 while it stands by |
| `pay_stellar_findings_total{kind,severity}` | observer | findings recorded, as listed in [the observer guide](observer.md) |
| `pay_stellar_observer_lag_ledgers{deployment}` | observer | ledgers between the observer's position and the node's latest |
| `pay_stellar_treasury_usdc{deployment}` | observer | the treasury's USDC, in base units |
| `pay_stellar_cold_reserve_usdc{deployment}` | observer | the USDC of the treasury's cold reserve, 0 without one |
| `pay_stellar_contract_liabilities{deployment}`, `pay_stellar_contract_revenue{deployment}` | observer | what the contract owes buyers and the seller |
| `pay_stellar_api_refusals_total{reason}` | gateway | gRPC requests refused, by reason |
| `pay_stellar_charges_admitted_total{kind,seller_deployment_id}`, `pay_stellar_charges_admitted_usdc_total{kind,seller_deployment_id}` | gateway | charges admitted (`kind` is `charge` or `recurring`) and their amount, per deployment |
| `pay_stellar_withdrawals_prepared_total{destination}` | gateway | withdrawals prepared to the buyer's wallet (`own`) or another account (`other`) |
| `pay_stellar_recurring_settled_total{result}` | worker | recurring charges settled by result |
| `pay_stellar_source_sequence_taken_total{source}` | worker | times a transaction the worker did not send used a source's sequence |
| `pay_stellar_contract_code_expected{deployment}` | observer | 1 while the contract runs the Wasm `PAY_STELLAR_EXPECTED_WASM` names, 0 otherwise |
| `pay_stellar_x402_total{call,result}` | gateway | x402 verifications and settlements, by result |

Counters count each event once: a transition is counted only by the call that made it, after its transaction committed.

## Alerts

| Alert | Severity | What to do |
|---|---|---|
| `PayStellarCriticalFinding` | critical | Read the finding in `pay_stellar.reconciliation_findings` and act as [the observer guide](observer.md) says for its kind |
| `PayStellarTreasuryShort` | critical | The treasury and its cold reserve hold less than the contract owes. Stop admitting deposits and charges, and find where the USDC went |
| `PayStellarChargeQuarantined` | critical | A charge's amount is held from its buyer until resolved; see [quarantine](quarantine.md) |
| `PayStellarRecurringChargeQuarantined` | critical | A recurring charge's period stays taken until resolved; see [quarantine](quarantine.md#recurring-charges) |
| `PayStellarChargeVolumeUnusual` | warning | A deployment admitted more than three times its usual hourly charge amount. Confirm with the seller; if unexpected, revoke its API key and lower its daily limits ([limits](limits.md)) |
| `PayStellarWithdrawalsToOtherAccounts` | warning | A withdrawal to an account other than the buyer's wallet was prepared; confirm it was intended |
| `PayStellarDeploymentQuotaReached` | warning | A deployment reached a daily quota; check its traffic before raising the quota ([limits](limits.md)) |
| `PayStellarMandateChangedOutsideGateway` | warning | A recurring charge found the buyer's USDC approval lowered (`allowance_short`) or the mandate revoked (`no_mandate`) by a call outside this gateway. Confirm with the buyer; a change nobody made means the buyer's key is exposed |
| `PayStellarFeeBurnHigh` | warning | The worker paid more than 50 XLM of fees in an hour. See fees by kind on the dashboard: a burst of small deposits or mandates, or bids far above the network's, burns the fee account ([limits](limits.md)) |
| `PayStellarSourceSequenceTaken` | critical | A transaction the worker did not send used a source account's sequence: its key is exposed. Rotate it ([limits](limits.md#source-accounts)) |
| `PayStellarSigningFailing` | critical | A key could not sign, or signed for another account. Check the key reference and the key service; nothing was sent with a bad signature |
| `PayStellarFeeAccountLow` | warning | Fund the fee account before it reaches the floor. The rule assumes the default floor of 10 XLM; adjust it if `PAY_STELLAR_FEE_FLOOR_STROOPS` differs |
| `PayStellarFeeAccountAtFloor` | critical | No new deposits or batches go out, while work in flight finishes. Fund the fee account; settlement resumes on its own |
| `PayStellarContractLifeShort` | critical | The worker extends a contract below about 7 days of life; at half that it has failed to. Check its logs, its source accounts and the fee account |
| `PayStellarChargesWaiting` | warning | Batches are not going out. Check the worker's logs, its source accounts and the fee account's balance |
| `PayStellarSubmissionsNotLanding` | warning | Transactions keep expiring or failing. Check the inclusion bid against network fees and the RPC node's health |
| `PayStellarHotTreasuryLow` | warning | The treasury has been below its floor for ten minutes. Top it up from the [cold reserve](treasury.md#topping-the-treasury-up) before withdrawals wait |
| `PayStellarWithdrawalsWaiting` | warning | A signed withdrawal has not gone out for ten minutes while its amount is held. Check that a worker holds the treasury key, that the treasury holds enough USDC, and the withdrawal's `last_error` |
| `PayStellarWorkerFailing` | warning | Settlement rounds keep failing; read the worker's error logs |
| `PayStellarObserverLagging` | warning | The observer is more than an hour behind. Fix it before the events it has not read leave the RPC node's retention |
| `PayStellarNoLeader` | critical | Worker or observer processes are running but none holds the lease, so nothing is settled or observed. Check their logs for database errors |
| `PayStellarProcessDown` | critical | Restart the process; the worker and the observer resume from the database |

## Traces

Gateway requests, settlement rounds and the submission engine's steps are spans. A charge can be followed from the API call that admitted it to the batch and transaction that settled it. Spans carry the seller deployment, and the submission and charge identifiers, never keys or buyer data beyond account addresses.
