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

To move an existing deployment to a newer contract build, keeping its
address and balances, the admin replaces its code in place:

```bash
just contract-build
testnet upgrade-prepaid --wasm target/contract-wasm/fermah_pay_stellar_prepaid.wasm
```

The command checks on the ledger that the instance runs the new Wasm and
that the contract's totals are unchanged. The upgrade pays rent to keep the
new code alive for about 30 days: for the current 29 KB contract, the
upgrade transaction cost 28.2 XLM on testnet
([`admin-upgrade-two-signatures`](../evidence/README.md#deployed-contracts-testnet)),
plus the upload.

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
5. withdraws 0.01 USDC back to the buyer's wallet: the buyer signs the
   prepared entry, and the worker adds the treasury's signature;
6. calls the x402 interface's `/verify` and `/settle` with a commitment the
   buyer signs, and checks the charge settles;
7. checks that the gateway's balance equals the contract's and that the buyer
   still holds 0 XLM.

The command exits non-zero if any step does not hold, and writes an
`api-end-to-end` evidence record with every transaction hash.

Recurring charges have their own run, on a database of its own: each run
provisions a seller deployment bound to the contract, and a contract is bound
to one deployment per database.

```bash
just db-create pay_stellar_recurring
testnet recurring-end-to-end \
  --database-url postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar_recurring
```

With periods of two minutes, so it takes about five: a new buyer with 0 XLM
signs one mandate for two periods; the seller charges the first period and,
once it starts, the second; a charge sent straight to the contract before
the second period starts is refused (`not_due`), and after the last period
the API refuses a charge (`mandate_ended`) and so does the contract
(`mandate_expired`); finally the buyer revokes and the USDC approval is
zero. It writes a `recurring-end-to-end` evidence record.

The x402 interface has its own run, a [conformance harness](../api/x402.md#conformance-harness)
that calls it over HTTP as a third-party facilitator would, also on its own
database:

```bash
just db-create pay_stellar_x402
testnet x402-conformance \
  --database-url postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar_x402
```

Each of these commands also reads its database URL from
`PAY_STELLAR_E2E_DATABASE_URL`, and every `testnet` command its RPC endpoint
from `STELLAR_TESTNET_RPC_URL`.

The `testnet` workflow runs these commands in CI: on changes to main, on
demand, and on pull requests labelled `testnet`. Its job summary lists every
transaction with an explorer link, and the evidence record is attached to the
run.

## 7. Cold reserve

```bash
testnet cold-reserve
testnet sweep --amount 500000
testnet solvency
```

`cold-reserve` creates the treasury's reserve: a Circle USDC trustline, 0 XLM,
and two of three keys (its own and two co-signers kept in the profile) to move
anything. `sweep` moves USDC from the treasury to it with the call and
treasury authorization the worker's sweep builds, and `solvency` counts the
reserve with the treasury. Top the treasury up again with `propose-transfer`,
two `sign` calls and `submit`, as in
[the treasury guide](../self-hosting/treasury.md#topping-the-treasury-up).

## 8. Load

```bash
just db-create pay_stellar_load
testnet load-test \
  --database-url postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar_load \
  --buyers 10 --charges-per-buyer 100 --channels 4 --concurrency 32
```

This starts the same in-process gateway and worker as step 6, with the
worker sending from `channel-1` to `channel-4` (created sponsored, with 0
XLM, if missing) and the submitter. Then:

1. `--buyers` new zero-XLM buyers are created, each sent 0.1 USDC from the
   reserve, registered, and made to deposit once through the API;
2. `--buyers` × `--charges-per-buyer` charges of 0.0001 USDC are admitted
   through the API, `--concurrency` requests at a time;
3. the command waits until every charge has settled and checks that all of
   them are `charged`.

The `load-test` evidence record reports:

- the admission and settlement rates;
- settlement latency (p50, p95 and max, from admission to the charge's final
  state);
- the number of batches and charges per batch;
- how many source accounts sent;
- the fee per charge;
- every batch transaction.

Use a fresh database for each run. A minimal run, `--buyers 1
--charges-per-buyer 1 --channels 3`, is also the way to create the channel
accounts `channel-2` and `channel-3` that [the dev stack](dev-stack.md) sends
from.

## 9. The vault

The [vault](../architecture/vault-contract.md) holds the buyers' USDC itself
instead of a treasury. It is a separate deployment, recorded in the profile
next to the prepaid ledger:

```bash
just contract-build
testnet deploy-vault --wasm target/contract-wasm/fermah_pay_stellar_vault.wasm \
  --min-deposit 100000 --max-charge 10000000 \
  --max-balance 5000000000 --max-total 100000000000
testnet vault-end-to-end \
  --database-url postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar_vault
```

`--max-balance` and `--max-total` are the launch limits: the admin bounds
what one buyer and all buyers together may hold (500 and 10,000 USDC here).
The end-to-end run binds a new deployment to the vault and, through the gRPC
API:

1. a new buyer with 0 XLM deposits 0.1 USDC and sets a daily spending limit
   of 0.04 USDC with the same signature;
2. two charges within the limit settle, and a third that would pass it is
   refused at once (`above_spending_limit`);
3. a withdrawal is paid to the wallet: the buyer signs, and the worker adds
   the operator's signature;
4. the buyer lowers the limit, which admission applies at once and the
   contract only after its notice;
5. the buyer requests an exit of 0.025 USDC; a withdrawal that would take
   part of it is refused (`exit_requested`), and completing the exit before
   its notice is refused by the contract in simulation;
6. the run compares every figure `GetBalance` reports with the vault's
   account entry, and the vault's USDC with what it owes.

The exit unlocks about a day later (18,720 ledgers). Anyone may then
complete it; the recorded `vault-exit` evidence comes from the submitter
account, neither the buyer nor the operator:

```bash
testnet vault-exit --buyer G...     # the buyer address in the vault-end-to-end record
```

Admin changes to the vault go through the
[multi-signature flow](../self-hosting/keys.md) of
`fermah-pay-stellar-contract`, with the vault's actions `propose-upgrade`,
`cancel-upgrade`, `install-upgrade` and `set-launch-limits`.

## On a local network

Every command above also runs against a standalone network on this machine,
with `--network stellar:local` (or `PAY_STELLAR_TESTNET_NETWORK=stellar:local`):

```bash
docker run -d --name stellar -p 8000:8000 \
  stellar/quickstart@sha256:1d57fcdc3bc3775f841c4eed877cfc10e406ab7b879c0531844549aeff52ff0a \
  --local --enable core,rpc --limits testnet
# Wait for the RPC, Friendbot, and Soroban's settings, which exist only once
# the network has upgraded to its protocol a few ledgers after starting.
until curl -sf -X POST -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' http://localhost:8000/rpc | grep -q '"healthy"' \
   && curl -sf -o /dev/null 'http://localhost:8000/friendbot?addr=GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7' \
   && curl -sf -X POST -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"getLedgerEntries","params":{"keys":["AAAACAAAAAA="]}}' \
        http://localhost:8000/rpc | grep -q '"xdr"'; do
  sleep 5
done
localnet() { testnet --network stellar:local --rpc-url http://localhost:8000/rpc "$@"; }
localnet init-roles                    # Friendbot funds the roles and the stand-in USDC issuer
localnet mint-local-usdc --amount 1000000000
just contract-build
localnet deploy-prepaid --wasm target/contract-wasm/fermah_pay_stellar_prepaid.wasm \
  --min-deposit 100000 --max-charge 10000000
just db-create pay_stellar_local
localnet end-to-end --database-url postgres://pay_stellar_owner:local-development-only@127.0.0.1:55433/pay_stellar_local
```

The image is pinned to the digest the CI job uses. `recurring-end-to-end`,
`x402-conformance`, and `deploy-vault` followed by `vault-end-to-end` run the
same way, each on a database of its own, as in steps 6 and 9. For a disposable run, `--profile-dir` keeps the profile somewhere
else.

A local network has no Circle issuer. Its USDC is a stand-in issued by a key
derived from a public string, so anyone can issue it; `mint-local-usdc`
deploys its asset contract and funds the USDC reserve. The profile defaults
to `~/.config/fermah-pay-stellar/local` and evidence to
`target/local-evidence`. Each record is marked as a local test run: it is not
evidence of anything on a public network.

The `Local end-to-end` CI job runs these steps, with the recurring, x402 and
vault runs, on every change to the code, the contract or the schema. The `testnet` workflow remains the check against
Circle USDC on a public network.
