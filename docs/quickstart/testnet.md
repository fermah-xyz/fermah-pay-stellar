# Testnet walkthrough

Deploys the prepaid ledger on Stellar testnet with Circle testnet USDC and runs
deposits, charges and a withdrawal. Every command writes an evidence record
to `docs/evidence/testnet/` (override with `--evidence-dir`).

Requirements: the [local quickstart](local.md) prerequisites and the
stellar CLI (for building the contract Wasm).

## Keys

Keys live in a profile directory **outside the repository**, by default
`~/.config/fermah-pay-stellar/testnet/` (override with `--profile-dir` or
`PAY_STELLAR_TESTNET_PROFILE`). Files are created with mode `0600`. Never
copy them into the repository.

```bash
testnet() { cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-testnet -- "$@"; }
```

## 1. Roles

```bash
testnet init-roles
```

Creates any missing account:

| Role | Funding |
|---|---|
| `operator-sponsor` | Friendbot; pays reserves of every sponsored account |
| `submitter` | Friendbot; signs and sequences transactions |
| `fee-source` | Friendbot; pays fees through fee bumps |
| `admin`, `operator`, `seller`, `treasury`, `usdc-reserve` | sponsored: 0 XLM, USDC trustline |

## 2. USDC

Fund the `usdc-reserve` address printed above with Circle testnet USDC from
[faucet.circle.com](https://faucet.circle.com) (network: Stellar testnet).

## 3. Deploy

```bash
just contract-build
testnet deploy-prepaid --wasm target/contract-wasm/fermah_pay_stellar_prepaid.wasm \
  --min-deposit 100000 --max-charge 10000000
```

Amounts are USDC base units: `10000000` is 1 USDC. The command checks that the
network assigned the contract address derived locally from the deployer and
salt, and records the deployment in the profile.

## 4. Buyers

```bash
testnet onboard-buyers --count 100 --usdc-each 700000
```

Creates buyers 1 to 100 with zero XLM and sponsored reserves (19 per
transaction) and tops each up to 0.07 USDC from the reserve in one payment
transaction. The command fails if any buyer ends up holding XLM.

## 5. Deposit, charge, withdraw

```bash
testnet deposit --first 1 --last 100 --amount 300000
testnet charge-batch --first 1 --last 98 --tag first --amount 100000
testnet charge --buyer 1 --tag single --amount 100000
testnet charge --buyer 1 --tag single --amount 100000     # refused: duplicate
testnet withdraw --buyer 2 --amount 150000
testnet charge-batch --first 1 --last 98 --tag second --amount 150000
testnet solvency
```

`charge-batch` settles one charge for every buyer in a single transaction.
A charge's identifier is derived from its tag and the buyer's number, so
repeating a tag repeats the same charges, which the contract refuses as
duplicates without debiting again. Each charge may be settled for about an
hour.

## 6. End to end through the API

```bash
just db-up
testnet end-to-end \
  --database-url postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar
```

This runs the gateway and the settlement worker inside the command, each on
its production database role, against the deployment recorded in step 3, and
acts as a seller would, only through the gRPC API:

1. creates a new buyer account with 0 XLM (sponsored) and sends it 0.1 USDC
   from the reserve;
2. registers the buyer, prepares a deposit, signs the returned authorization
   entry with the buyer's key, and submits it; the worker sends and pays for
   the transaction;
3. creates three charges and waits until they settle on-chain;
4. repeats the first charge with the same idempotency key, which returns the
   original charge, and reuses the key with another amount, which is refused;
   then sends the first charge straight to the contract again, which
   refuses it as a duplicate before anything is submitted;
5. checks that the gateway's balance equals the contract's and that the buyer
   still holds 0 XLM.

The command exits non-zero if any step does not hold, and writes an
`api-end-to-end` evidence record with every transaction hash.

The `testnet` workflow runs the same command in CI: on changes to main, on
demand, and on pull requests labelled `testnet`. Its job summary lists every
transaction with an explorer link, and the evidence record is attached to the
run.
