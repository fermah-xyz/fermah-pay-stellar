# Serving a vault

A deployment bound to a [vault](../architecture/vault-contract.md) with
`fermah-pay-stellar-admin bind-ledger --vault` is served by the same
gateway, worker and observer as a prepaid ledger. The vault holds the
buyers' USDC, so no treasury key is involved: the worker co-signs
withdrawals with the operator's key. The [ledger API](../api/ledger.md#vault-deployments)
describes what sellers see.

## What the worker keeps current

Each vault buyer's daily limit and exit live on the contract. The gateway
must know of one the buyer's wallet set without it, or a charge it admits
could fail on-chain after the seller delivered. For each vault it serves,
every round, the worker:

1. reads the account entry of each buyer it has not read yet, such as one
   registered after its wallet already used the vault, and stores the limit
   and exit it holds;
2. applies every new limit and exit event, page by page. Each page's effects
   commit with the new position in one transaction, so no event applies
   twice or is skipped. A buyer's limit and exit change only for events
   after the ledger its entry was read at, which the entry already reflects.

Until a buyer's entry has been read, and whenever the worker's reading lags
the network by more than `PAY_STELLAR_VAULT_EVENTS_STALE_LEDGERS` (2880 by
default), the gateway refuses that buyer's charges and withdrawals with
`network_unavailable`.

A vault that cannot be read is logged, counted in
`pay_stellar_vault_read_failures_total` and left behind. The rest of the
round, including other deployments, goes on. `PayStellarVaultEventsBehind`
fires when the reading is an hour behind.

## Exits and the available balance

An exit pays from the vault's balance whenever someone sends it, even while
the worker is down. The balance also backs charges the gateway admitted and
withdrawals it held but that have not settled yet. When an exit pays more
than the buyer's `available`, the difference was held for those. `available`
then goes below zero by at most what they hold. They can no longer settle,
and as they are refused or expire, their refunds bring `available` back to
what the vault holds for the buyer. `GetBalance` reports zero meanwhile.

The gateway keeps an exit's amount free from every charge and withdrawal it
admits once it knows of the request. So this happens only when the worker
was down for longer than the exit's notice.

## Upgrades

While code the admin proposed may still be installed, from the proposal
until it is cancelled, installed or its window closes, the gateway refuses
new deposits and mandates with `upgrade_pending`. Money deposited then would
run under the new code without the notice existing buyers have to leave
first. The worker learns of a proposal from the vault's events, and from
the contract instance when it starts serving a vault.
`PayStellarVaultUpgradeProposed` stays active for that whole time.

## Lost vault events

The worker reads the vault's events from the RPC node, which retains only
recent ledgers. If the worker stops for longer than that, the node no longer
holds the events from where it stopped. The worker then refuses to read on,
since skipping them could leave out a limit or an exit. The log names the
first missing ledger and the node's oldest, and charges stay refused.

1. If an RPC node with a longer history is available, point the worker at it
   (`PAY_STELLAR_RPC_URL`). It continues from where it stopped, and nothing
   else is needed.
2. Otherwise, rebuild the buyers from the contract.
   - Exits paid during the lost span were not applied to `available`. For
     each buyer, compare `available` plus its open charges and held
     withdrawals with the balance in its account entry on the vault. The
     observer's `ledger_totals_mismatch` gives the total difference.
   - Correct the differences with the seller.
   - Then, as the database owner:

     ```sql
     BEGIN;
     DELETE FROM pay_stellar.vault_event_cursors WHERE seller_deployment_id = '<deployment>';
     UPDATE pay_stellar.buyers SET vault_synced_ledger = NULL
     WHERE seller_deployment_id = '<deployment>';
     COMMIT;
     ```

   The worker then starts at the latest ledger and reads every buyer's
   account entry again before their charges are admitted.

## Settings

| Variable | Process | Value |
|---|---|---|
| `PAY_STELLAR_VAULT_EVENTS_STALE_LEDGERS` | gateway | how far the worker's reading may lag before vault charges and withdrawals are refused, default 2880 (about four hours). With `PAY_STELLAR_CHARGE_VALIDITY_LEDGERS` it must stay below the vault's notice of 18720 ledgers, or the gateway refuses to start |
| `PAY_STELLAR_CHARGE_VALIDITY_LEDGERS` | gateway | a vault refuses as `expired` a charge settled two UTC days after the day it was admitted in. At the default of 720 ledgers, about an hour, no charge stays valid that long. Near the maximum of 17280 ledgers, a charge admitted late in a day and sent at the end of its window can reach that point when ledgers close slower than five seconds; it is then refused and refunded |
