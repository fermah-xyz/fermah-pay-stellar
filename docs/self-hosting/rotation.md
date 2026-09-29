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
`PAY_STELLAR_OPERATOR_KEY_FILE`. Batches the previous operator authorized and
that are still in flight settle normally; a batch sent after the rotation with
the previous key is refused in simulation and retried.

For a treasury rotation, the contract moves no USDC: before rotating, give the
new treasury a USDC trustline, and after rotating, transfer the previous
treasury's USDC to it so that it covers the contract's liabilities and unpaid
revenue. `fermah-pay-stellar-testnet solvency` compares the two on testnet.
Deposits prepared before the rotation were signed for the previous treasury;
the contract refuses them, and the buyer prepares a new deposit.

## Admin or seller

Neither is part of the gateway's binding; rotate them on the contract only.
The admin can replace the contract's code and every role, so keep it offline
or in hardware and rotate it first if any other key is suspected.
