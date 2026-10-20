# Sandbox: integrate against a hosted gateway

A hosted Fermah Pay gateway on Stellar testnet, with Circle's testnet USDC,
for sellers who want to integrate before running their own. Access is per
seller: you receive

- the endpoint, for example `https://pay-sandbox.fermah.xyz`;
- an API key for your own seller deployment, bound to your own
  [vault](../architecture/vault-contract.md) contract on testnet;
- the secret key of a test buyer whose wallet already holds testnet USDC and
  no XLM.

Everything runs on testnet: no real funds move, and the sandbox may be reset.

## 1. Run the walkthrough

```bash
export FERMAH_PAY_ENDPOINT=https://pay-sandbox.fermah.xyz
export FERMAH_PAY_API_KEY=...          # your API key
export FERMAH_PAY_BUYER_SECRET=S...    # the test buyer's secret key
docker run --rm -e FERMAH_PAY_ENDPOINT -e FERMAH_PAY_API_KEY -e FERMAH_PAY_BUYER_SECRET \
  ghcr.io/fermah-xyz/fermah-pay-stellar:main
```

It takes about a minute. Each step prints the call it makes, the
identifiers and amounts that come back, and the transaction on the Stellar
explorer:

1. **`CreateBuyer`** registers the buyer with your deployment. The same
   reference and wallet return the same buyer, so you can run it again.
2. **`PrepareDeposit`** returns an authorization entry for the exact deposit,
   which carries the buyer's daily spending limit.
3. **The buyer signs the entry**, after checking it is the deposit asked for:
   the amount, the limit, and Circle's USDC moving from the buyer into your
   vault. In your product, the buyer's wallet does this; you never hold the
   buyer's key. The buyer needs no XLM.
4. **`SubmitDeposit`** hands the signed entry back. The gateway sends the
   transaction and pays its fee; `GetDeposit` reports it confirmed.
5. **`CreateCharge`** is admitted at once against the balance and settles
   on-chain in a batch shortly after; `GetCharge` reports it charged.
6. **`GetBalance`** shows the available balance and the limit.

Each run deposits 0.1 USDC and charges 0.01 USDC against a daily limit of
0.05 USDC, which the buyer signs with the deposit. The test buyer starts with
2 USDC. The sixth run on the same UTC day is refused as
`above_spending_limit`: the limit is doing its job. Run again the next day, or
with a smaller `--charge`.

Without Docker, the same walkthrough runs from a clone of this repository:
`cargo run -q -p fermah-pay-stellar-cli --bin fermah-pay-stellar-try`. It
reads the API key and the buyer's secret from the environment only, and
refuses a plain `http://` endpoint other than one on your own machine.

## 2. Make the same calls from your code

The API is gRPC. The `.proto` files in [`proto/`](../../proto) are the
reference; generate a client in your language with your usual tooling. Every
call carries `authorization: Bearer <API key>`. From a shell, with
[grpcurl](https://github.com/fullstorydev/grpcurl) and a clone of this
repository:

```bash
call() {
  grpcurl -import-path proto -proto fermah/pay/stellar/v1/buyer.proto \
    -proto fermah/pay/stellar/v1/ledger.proto \
    -H "authorization: Bearer $FERMAH_PAY_API_KEY" -d "$2" \
    pay-sandbox.fermah.xyz:443 "fermah.pay.stellar.v1.$1"
}
call BuyerService/CreateBuyer '{"external_ref":"user-42","wallet_address":"G..."}'
call LedgerService/PrepareDeposit \
  '{"buyer_id":"...","amount":1000000,"daily_limit":500000,"idempotency_key":"deposit-1"}'
call LedgerService/CreateCharge '{"buyer_id":"...","amount":100000,"idempotency_key":"charge-1"}'
call LedgerService/GetBalance '{"buyer_id":"..."}'
```

The buyer's wallet signs the `authorization_entry_xdr` that `PrepareDeposit`
returns, for example with the Stellar SDK's `authorizeEntry`, and you pass
the signed entry to `SubmitDeposit`. The API reference covers every call,
state and refusal: [buyers](../api/buyer.md), [the ledger](../api/ledger.md)
and its [vault deployments](../api/ledger.md#vault-deployments), and
[recurring charges](../api/recurring.md).

Every request that creates something takes an idempotency key: repeating it
returns the original, so a retry can never charge twice.

## 3. x402

The same host serves the [x402 facilitator interface](../api/x402.md) over
HTTPS: `GET /supported`, which needs no key, and `POST /verify`,
`POST /settle` and `GET /settlements/{commitment}`, with the same API key.

```bash
curl https://pay-sandbox.fermah.xyz/supported
```

Buyers whose wallet is a contract account, such as a passkey smart wallet,
pay by authorizing the commitment against the verifier `/supported` names
([contract-account payers](../api/x402.md#contract-account-payers)).

## Sandbox rules

- **Testnet only.** USDC is Circle's testnet USDC; more is available from
  [faucet.circle.com](https://faucet.circle.com).
- **Quotas apply** per buyer and per deployment, as in production
  ([limits](../self-hosting/limits.md)), alongside each buyer's own daily
  limit; a refusal names the quota or limit reached.
- **Your deployment is yours.** Your API key reaches only your buyers and
  your vault; other sellers' data is reported exactly like data that does
  not exist.
- **The sandbox may be reset**, with notice. Testnet itself is reset by the
  Stellar network from time to time.
