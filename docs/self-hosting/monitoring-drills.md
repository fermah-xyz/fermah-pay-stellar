# Monitoring drills

A drill provokes on testnet the condition one threat in the [threat
model](../security/threat-model.md) leaves behind, then waits until the
alert that must catch it is active in Alertmanager. It shows the whole
path works: the contract or the gateway, the observer or the worker, the
metric, the Prometheus rule and Alertmanager.

No drill moves USDC out of the treasury or changes the contract's own
state.

## Running them

The drills run against the [development stack](../quickstart/dev-stack.md)
with its monitoring profile and the drill settings in
[`deploy/dev/drills.yaml`](../../deploy/dev/drills.yaml):

```sh
export PAY_STELLAR_TESTNET_PROFILE=~/.config/fermah-pay-stellar/testnet
just dev-drills-up
PAY_STELLAR_DEV_API_KEY=$(just dev-api-key) \
  cargo run -p fermah-pay-stellar-cli --bin fermah-pay-stellar-testnet -- drill all
```

`drill <vector>` runs one drill. Before acting, each drill checks that its
alert is not already active, so the alert it then waits for is one it
raised. Each drill waits at most `--timeout-secs` (default 600) and writes a
`drill-<vector>` record under `docs/evidence/testnet`.

The drill settings make a condition visible in minutes, not hours. None of
them belongs in a real deployment.
- Withdrawals to other accounts are allowed.
- The deployment's daily deposit quota is 4.
- The observer is told to expect code the contract does not run.

The dust drill uses up the deposit quota for the day; run it last, as
`all` does.

## The drills

| Vector | What the drill does | Alert |
|---|---|---|
| `operator-key` | A new buyer deposits straight to the contract, and the operator's key charges that buyer outside the gateway, as someone holding the key could | `PayStellarCriticalFinding{kind="unknown_charge"}` |
| `buyer-key` | A new buyer deposits through the API. A withdrawal of the buyer's credit to another account is then prepared, as someone holding the buyer's key could ask for; it is never signed, so nothing is held or moved | `PayStellarWithdrawalsToOtherAccounts` |
| `allowance` | A new buyer authorizes a mandate through the API, then sets the contract's USDC approval to zero in the USDC contract directly. The seller charges the period, and the contract refuses it | `PayStellarMandateChangedOutsideGateway{result="allowance_short"}` |
| `admin-code` | The observer, told to expect other code, compares it with the code the contract runs, as after an upgrade nobody planned. The condition stands from the stack's start, so this drill checks the alert that lasts while it does, not the moment it began | `PayStellarContractCodeUnexpected` |
| `dust` | Deposits are prepared for one buyer until the deployment's daily quota refuses one; none is signed or sent | `PayStellarDeploymentQuotaReached{reason="deployment_deposit_quota_exceeded"}` |

## Covered by tests instead

| Vector | Why there is no drill | Where it is checked |
|---|---|---|
| A source account's sequence taken by someone else | The sequence must be used while the worker's envelope is in flight, and testnet includes it within seconds | `test_a_source_whose_sequence_was_taken_is_left_out_while_another_is_free` (`crates/gateway/tests/submission.rs`) |
| Ledger-entry eviction | The contract's life cannot be shortened on demand. The worker measures it each round (`pay_stellar_contract_ttl_ledgers`, on the dashboard) and extends it below the threshold | `test_an_idle_contract_is_extended_before_it_could_be_archived` (`crates/gateway/tests/worker.rs`) |
