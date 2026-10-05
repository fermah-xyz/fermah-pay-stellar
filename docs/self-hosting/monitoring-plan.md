# Monitoring plan

What is watched, by which part, and what it raises, for the risks in the
[threat model](../security/threat-model.md). The metrics and alert rules
are listed in [monitoring](monitoring.md); the dashboard has a row for each
heading below.

## Contract balances against the gateway and the treasury

The [chain observer](observer.md) reads every event of each contract and,
at each reconciliation, the contract's totals and the treasury's (and cold
reserve's) USDC in one read, from one ledger.

| Check | Finding or alert |
|---|---|
| The treasury and its cold reserve hold at least what the contract owes buyers and the seller | `treasury_deficit` (critical), `PayStellarTreasuryShort` |
| The contract's totals equal the sums of its events since the last baseline | `event_totals_mismatch` |
| The contract's totals lie within what the gateway's database allows, counting work in flight | `ledger_totals_mismatch` |
| Every settled charge, deposit, withdrawal and recurring charge matches a gateway record | the per-event findings in [the observer guide](observer.md#findings) |
| The contract runs the code the operator expects | `code_changed` (critical) and `PayStellarContractCodeUnexpected` for as long as it does; `PayStellarContractCodeChanged` for any change, expected code or not |
| The observer itself is reading the chain | `PayStellarObserverStalled`, `PayStellarNodeStalled`, `PayStellarObserverFailing` |

A reconciliation finding is recorded only after it persists through
consecutive checks, so a settlement landing between the two reads is not
reported.

## Spending ceilings

The contract enforces `max_charge` and the daily limits per buyer and per
seller whatever the operator signs. Refusals are counted
(`pay_stellar_charges_settled_total{result="above_limit"|"above_daily_limit"}`),
and every change of a limit is an `admin_change` finding. The gateway adds
quotas on requests that cost the operator fees ([limits](limits.md)).

## Failed transactions

Every transaction the worker sends ends `succeeded`, `failed`, `expired` or
`quarantined` (`pay_stellar_submissions_closed_total`).
`PayStellarSubmissionsNotLanding` fires when they keep failing or expiring;
`PayStellarChargeQuarantined` and `PayStellarRecurringChargeQuarantined`
when a charge needs an operator; `PayStellarWorkerFailing` when settlement
rounds fail; `PayStellarSigningFailing` when a key does not sign.

## The account that pays

The fee account pays every transaction through a fee bump. Its spendable
XLM is measured every round (`pay_stellar_fee_source_spendable_stroops`):
`PayStellarFeeAccountLow` warns before the floor, and
`PayStellarFeeAccountAtFloor` fires when nothing new is sent.
`PayStellarFeeBurnHigh` fires when an hour's fees exceed 50 XLM. Buyer
accounts are created, with their reserves sponsored, by onboarding tooling
outside the gateway; fund that sponsor separately.

## Charge patterns

Admitted charges and their amount are counted per deployment.
`PayStellarChargeVolumeUnusual` fires when an hour's amount is more than
three times the deployment's hourly average over the last week, and above
1 USDC; `PayStellarDeploymentQuotaReached` when a deployment hits a daily
quota. The dashboard shows the last hour against the weekly average.

## Health

`PayStellarProcessDown`, `PayStellarNoLeader` (processes run but none holds
the lease), `PayStellarObserverLagging`, and `PayStellarContractLifeShort`.
