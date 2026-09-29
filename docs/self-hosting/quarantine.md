# Self-hosting: resolving quarantined charges

A charge is quarantined when the contract's answer contradicts the gateway's
records (`duplicate`, `out_of_order`, `unknown_account`), when a batch's
answer cannot be read, or when the account's charge sequence was consumed by
a transaction the gateway did not send. A quarantined charge stays debited
from the buyer's available balance, and the buyer gets no further batches
until it is resolved.

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

lists each quarantined charge with its account, sequence, amount, the
contract's answer, the reason, and the hash of the batch that carried it.

The resolution is never typed in by the operator; the tool derives it from
the network and refuses when the evidence does not establish it:

- The contract settled the charge's sequence in some transaction (for
  example the batch whose answer could not be read, or a transaction that
  included a copy of the operator's authorization):

  ```bash
  admin resolve-charge --charge-id <ID> --transaction <HASH> \
    --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org
  ```

  The tool reads the contract's `charges` event in that transaction and
  requires an entry for the same account, sequence and amount with an outcome
  that consumed the sequence. `charged` leaves the amount debited;
  `insufficient_balance` or `above_limit` return it to the available balance.

- The charge's sequence is still unconsumed on the contract:

  ```bash
  admin resolve-charge --charge-id <ID> --readmit \
    --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org
  ```

  The tool reads the account's last consumed sequence and requires it to be
  below the charge's. The charge goes back to the next batch with the same
  sequence.

Every resolution is kept in `pay_stellar.charge_resolutions`, which no role
can update or delete.
