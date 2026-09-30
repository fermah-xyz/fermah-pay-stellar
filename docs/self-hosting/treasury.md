# Self-hosting: the treasury and its cold reserve

Buyers' deposits land in the treasury, the account the ledger contract
names, and withdrawals are paid out of it. For withdrawals to go out without
waiting for a person, the settlement worker holds the treasury's key
(`PAY_STELLAR_TREASURY_KEY_FILE`). Anyone who takes that key can move
everything the treasury holds. A cold reserve bounds that loss:

- **Treasury (hot).** It keeps a working balance for withdrawals, and the
  worker signs with its key.
- **Cold reserve.** It is an account whose key the worker never holds, and it
  needs several signatures to move anything. The worker sweeps whatever the
  treasury holds above a ceiling into it.

A stolen treasury key then reaches at most the ceiling. The USDC in the
reserve still backs buyer balances: the observer counts the treasury and the
reserve together when it checks that what is held covers what the contract
owes.

## Sizing

| Setting | Meaning | Choose |
|---|---|---|
| `PAY_STELLAR_HOT_TREASURY_CEILING` | above it, the worker sweeps the surplus to the reserve | what you accept losing if the treasury key leaks |
| `PAY_STELLAR_HOT_TREASURY_TARGET` | what a sweep leaves in the treasury | about a day of withdrawals |
| `PAY_STELLAR_HOT_TREASURY_FLOOR` | below it, the treasury needs topping up; reported, never acted on | a few hours of withdrawals |

All three are USDC base units (1 USDC = 10,000,000), with
`0 <= floor <= target < ceiling`.

A sweep never leaves less than the withdrawals held in the database still
need, even if that is more than the target. The treasury's balance is read
at most once a minute. Only one sweep is in flight at a time, and a new one
waits until the previous one's treasury authorization has lapsed. That way a
copy of it included late cannot add to the next one; at worst, more than
intended would move to the reserve, which is still yours.

A withdrawal the treasury cannot pay waits, holding its amount, until the
treasury is topped up or the buyer's authorization lapses
([ledger API](../api/ledger.md#withdrawals)).

## Setting up the reserve

1. **Create the account.** Give it a Circle USDC trustline and the signers
   that will approve top-ups, with a medium threshold above any single
   signer's weight: two of three, for example. The account can hold no XLM
   if a sponsor pays its reserves. On testnet, `fermah-pay-stellar-testnet
   cold-reserve` does this, with the two co-signers kept in the profile.
2. **Configure the worker.** It needs the treasury's key and:

   ```sh
   export PAY_STELLAR_COLD_RESERVE=G...           # the reserve account
   export PAY_STELLAR_HOT_TREASURY_FLOOR=500000000     # 50 USDC
   export PAY_STELLAR_HOT_TREASURY_TARGET=2000000000   # 200 USDC
   export PAY_STELLAR_HOT_TREASURY_CEILING=5000000000  # 500 USDC
   ```

3. **Configure the observer.** Give it each treasury with its reserve, so
   solvency counts both:

   ```sh
   export PAY_STELLAR_COLD_RESERVES=G...TREASURY:G...RESERVE
   ```

   An observer that does not know the reserve reports a `treasury_deficit`
   as soon as the first sweep lands.

The reserve's address is set in the processes' configuration, not in the
database. Someone who can write to the database therefore cannot redirect
sweeps.

## Topping the treasury up

A top-up is a USDC transfer out of the reserve. It goes through the same
propose, sign and submit flow as the admin's changes, so no signer needs
another's key:

```sh
fermah-pay-stellar-contract propose-transfer \
  --network stellar:testnet --rpc-url https://soroban-testnet.stellar.org \
  --from G...RESERVE --to G...TREASURY --amount 2000000000 \
  --source-key submitter.secret --fee-key fee-source.secret --out top-up.json
fermah-pay-stellar-contract sign --proposal top-up.json --key signer-1.secret
fermah-pay-stellar-contract sign --proposal top-up.json --key signer-2.secret
fermah-pay-stellar-contract submit --network stellar:testnet \
  --rpc-url https://soroban-testnet.stellar.org --proposal top-up.json \
  --source-key submitter.secret --fee-key fee-source.secret
```

`sign` shows each signer the transfer they approve. `submit` checks the
signatures against the reserve's thresholds on the ledger and sends nothing
if they fall short.

## Alerts

- `PayStellarHotTreasuryLow`: the treasury has stayed below its floor. Top
  it up from the reserve before withdrawals start to wait.
- `PayStellarWithdrawalsWaiting`: a signed withdrawal has not gone out for
  ten minutes, typically because the treasury is short.
- `PayStellarTreasuryShort` and the observer's `treasury_deficit`: the
  treasury and the reserve together hold less than the contract owes.
