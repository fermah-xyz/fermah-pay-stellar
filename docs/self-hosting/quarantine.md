# Self-hosting: resolving quarantined charges

A charge is quarantined when the contract's answer contradicts the gateway's
records (`unknown_account`), when the network reports a batch applied but the
contract holds no record of its charges, or when a charge's fate was not
established before its record lapsed and the contract's events could not
establish it either (the node no longer held them all, or the batch was
recorded before the gateway kept the ledger it was authorized at). A
quarantined charge stays debited from the buyer's available balance until it
is resolved.

Resolution is done by an operator under a login role that is a member of
`pay_stellar_operator`:

```sql
CREATE ROLE pay_stellar_ops LOGIN PASSWORD '<secret>' IN ROLE pay_stellar_operator;
```

That role can read charges and call one database function,
`resolve_quarantined_charge`, and nothing else. The function is the only way
out of quarantine for any role: it records who resolved the charge, how, and
on what evidence, in an append-only table, in the same transaction as the
change.

## Commands

```bash
export PAY_STELLAR_ADMIN_DATABASE_URL=postgres://pay_stellar_ops:<secret>@<host>/pay_stellar
admin() { cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-admin -- "$@"; }

admin quarantined-charges
```

lists each quarantined charge with its account, contract charge identifier,
last ledger, amount, the contract's answer, the reason, and the hash of the
batch that carried it. `<CHARGE>` below is the charge's `id` from that list.

The resolution is never typed in by the operator; the tool derives it from
the network and refuses when the evidence does not establish it:

- While the charge's record can still exist (until about an hour after its
  last ledger), the record decides:

  ```bash
  admin resolve-charge --charge-id <CHARGE> --record \
    --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org
  ```

  The tool refuses while the node is not past the last ledger in which the
  batch that carried the charge could still land, since until then an absent
  record proves nothing. A record holding `charged` leaves the amount debited; `insufficient_balance`
  or `above_limit` return it to the available balance. With no record, a
  charge still within its last ledger goes back to the next batch, and one
  past it is refused as `expired` and its amount returned.

- After that, the transaction in which the contract settled the charge
  decides:

  ```bash
  admin resolve-charge --charge-id <CHARGE> --transaction <HASH> \
    --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org
  ```

  The tool reads the contract's `charges` event in that transaction and
  requires an entry for the same account, charge identifier and amount with
  an outcome that settled it.

  Without a known transaction, the contract's events over the whole period
  in which the charge could have been settled decide:

  ```bash
  admin resolve-charge --charge-id <CHARGE> --events \
    --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org
  ```

  The period runs from the ledger at which the worker signed the batch that
  carried the charge, before which no transaction could include it, to the
  charge's last ledger, after which the contract refuses it. The tool reads
  every `charges` event of the contract in that period: an entry for the
  charge, with the same amount and an outcome that settled it, resolves it
  as that outcome; no entry resolves it as `expired` and returns its amount.
  It refuses when the node no longer retains the period's first ledger (use
  an RPC provider with longer history), when the node has not yet passed the
  charge's last ledger (use `--record`), or when the batch's signing ledger
  was not recorded. The evidence names the ledgers read and the node's oldest
  retained ledger.

Every resolution is kept in `pay_stellar.charge_resolutions`, which no role
can update or delete.
