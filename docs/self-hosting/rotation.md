# Self-hosting: rotating a ledger contract role

The prepaid ledger contract has four roles: admin, operator, seller and
treasury (see [the contract](../architecture/prepaid-contract.md#roles)).
Each can be moved to a new account, for example after a key is exposed. A
rotation is a contract call authorized by the admin and by the new account;
the contract refuses it if two roles would share an account, and emits a
`role` event.

## Operator or treasury

The gateway's binding of a deployment names the operator, which selects the
worker that settles its charges, and the treasury, which every deposit
authorization pays into. After rotating either on the contract, move the
binding with a login role that is a member of `pay_stellar_issuer`:

```bash
admin sync-ledger --deployment-id <DEPLOYMENT_ID> \
  --rpc-url https://soroban-testnet.stellar.org
```

The tool reads the contract's current operator and treasury from
`get_config`, refuses if the contract's USDC is not the bound one, and records
the previous and new accounts in `pay_stellar.ledger_binding_changes`, which
no role can update or delete. The accounts are never typed in.

For an operator rotation, restart the worker with the new key in
`PAY_STELLAR_OPERATOR_KEY_FILE`. The contract checks the operator when a
batch is applied: a batch the previous operator authorized that lands before
the rotation settles normally; one that lands after it fails, and once its
authorization has lapsed its charges go into a new batch under the new key,
or are refunded if their last ledger has passed.

For a treasury rotation, the contract moves no USDC: before rotating, give the
new treasury a USDC trustline, and after rotating, transfer the previous
treasury's USDC to it so that it covers the contract's liabilities and unpaid
revenue. `fermah-pay-stellar-testnet solvency` compares the two on testnet.
Deposits prepared before the rotation were signed for the previous treasury;
the contract refuses them, and the buyer prepares a new deposit.

The [chain observer](observer.md) reads every `role` event: it reports an
operator or treasury rotation the binding has not followed within its
settlement grace as `binding_out_of_date`, and every admin or seller
rotation as `role_changed`.

## Admin or seller

Neither is part of the gateway's binding; rotate them on the contract only.
The admin can replace the contract's code and every role, so keep it offline
or in hardware and rotate it first if any other key is suspected.
