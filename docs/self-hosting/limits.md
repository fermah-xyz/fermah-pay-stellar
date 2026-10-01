# Self-hosting: limits against untrusted buyers and sellers

Buyers and sellers are not trusted. Every deposit, withdrawal, mandate and
revocation the worker sends costs the operator a network fee whatever its
amount, and a stolen key of a buyer, of a seller's API key or of a source
account must not be able to cost more than a bounded amount. The limits
below, enforced by the contract or by the gateway, bound each case.

## The contract's minimum deposit

The contract refuses deposits below its `min_deposit`, which the admin sets
with `set-limits` ([keys](keys.md#an-admin-that-needs-several-signatures)).
A deposit into a new account cost about 0.065 XLM on testnet
([observed costs](../architecture/transactions.md#observed-costs-on-testnet)).
Choose a minimum at which that fee is a small share of the deposit: at a
price of `P` dollars per XLM, a minimum of `0.065 × P × 20` USDC keeps the fee
under 5%. At 0.40 dollars per XLM that is about 0.52 USDC, so 1 USDC is a
reasonable minimum on mainnet.

The testnet deployment's minimum is 0.1 USDC, raised from 0.01 by two of
the admin's three keys
([evidence](../evidence/testnet/2026-10-01T0420-admin-set-limits-two-signatures.json)).

## Daily quotas

The gateway counts requests that cost the operator a fee, over the last 24
hours, in the same database transaction as the insert:

| Setting | Default | Counts |
|---|---:|---|
| `PAY_STELLAR_MAX_NEW_BUYERS_PER_DAY` | 1000 | new buyers per deployment |
| `PAY_STELLAR_MAX_DEPOSITS_PER_BUYER_PER_DAY` | 10 | deposits per buyer |
| `PAY_STELLAR_MAX_DEPOSITS_PER_DEPLOYMENT_PER_DAY` | 2000 | deposits across a deployment |
| `PAY_STELLAR_MAX_WITHDRAWALS_PER_BUYER_PER_DAY` | 5 | withdrawals per buyer |
| `PAY_STELLAR_MAX_MANDATE_CHANGES_PER_BUYER_PER_DAY` | 5 | mandates and revocations per buyer |
| `PAY_STELLAR_MAX_MANDATE_CHANGES_PER_DEPLOYMENT_PER_DAY` | 2000 | mandates and revocations across a deployment |

The per-deployment quotas stop a flood of new buyers from multiplying the
per-buyer ones: with the defaults, at most 2000 deposits a day at about
0.065 XLM each, about 130 XLM, whatever the number of buyers. A request over
a quota is refused (`*_quota_exceeded`), and the alert
`PayStellarDeploymentQuotaReached` fires.

## Withdrawal destinations

A withdrawal pays only the buyer's own wallet unless
`PAY_STELLAR_WITHDRAWALS_TO_OTHER_ACCOUNTS` is on. Someone who steals a
buyer's key can then only return that buyer's credit to the buyer's wallet,
which the same key controls anyway, rather than send it elsewhere. A request
naming another account is refused with `destination_not_allowed`. With the
setting on, each withdrawal to another account raises the alert
`PayStellarWithdrawalsToOtherAccounts`.

## Charges and daily limits

The contract's daily limits bound what one buyer, and all the seller's
buyers together, can be charged per day, whatever the operator key signs
([daily limits](../architecture/prepaid-contract.md#daily-limits)). The
gateway counts admitted charges and their amount per deployment
(`pay_stellar_charges_admitted_total`, `pay_stellar_charges_admitted_usdc_total`);
`PayStellarChargeVolumeUnusual` fires when an hour's amount is more than three
times the deployment's hourly average over the last week, and above 1 USDC.

## Source accounts

Whoever holds a source account's key can consume its sequence numbers. The
worker notices when a transaction it did not send used the sequence of an
envelope it sent; that envelope's fate is then decided from the contract, as
for any envelope not found. The source is left out for 30 minutes while
another source is free, never when it is the last free one, and
`PayStellarSourceSequenceTaken` fires. Rotate that source's key.

## The contract's code

The admin can replace the contract's code. Give the observer the Wasm hash
each contract must run (`PAY_STELLAR_EXPECTED_WASM`, see
[the observer guide](observer.md)); any other code is a critical
`code_changed` finding.
