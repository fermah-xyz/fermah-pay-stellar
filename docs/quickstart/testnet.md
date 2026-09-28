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
testnet onboard-buyers --count 100 --usdc-each 1000000
```

Creates buyers 1 to 100 with zero XLM and sponsored reserves (19 per
transaction) and tops each up to 0.1 USDC from the reserve in one payment
transaction. The command fails if any buyer ends up holding XLM.

## 5. Deposit, charge, withdraw

```bash
testnet deposit --first 1 --last 100 --amount 500000
testnet charge-batch --first 1 --last 100 --seq 1 --amount 100000
testnet charge --buyer 1 --seq 2 --amount 100000
testnet charge --buyer 1 --seq 2 --amount 100000     # refused: duplicate
testnet withdraw --buyer 1 --amount 100000
```

`charge-batch` settles one charge for every buyer in a single transaction.
Charge sequences are per buyer account and must be consecutive; a repeated
sequence is refused as a duplicate and never debits again.
