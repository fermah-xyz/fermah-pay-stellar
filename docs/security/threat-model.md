# Threat model

What each party, key or piece of infrastructure could do if it turned
hostile or leaked, what stops it, how it is seen, and what is left. Buyers
and sellers are untrusted; so is any single key. On the prepaid ledger the
operator is trusted for one thing: paying buyers' withdrawals out of the
treasury ([below](#an-operator-that-does-not-pay-withdrawals)). The
[vault](#the-vault) removes that trust. The [monitoring
plan](../self-hosting/monitoring-plan.md) lists the signals below with their
alerts and dashboard panels, and [drills](../self-hosting/monitoring-drills.md)
raise them on testnet.

The assets are the buyers' USDC: in their wallets, deposited as prepaid
credit (held by the treasury account, owed by the contract as liabilities),
and approved to the contract under recurring mandates; the seller's earned
revenue; and the operator's XLM, which pays every fee.

## An operator that does not pay withdrawals

**Can.** Prepaid credit is custodial. The USDC sits in the treasury, and a
withdrawal needs the treasury's authorization as well as the buyer's, so an
operator that stops running its worker, loses the treasury's key, or refuses
can leave buyers unable to withdraw. No contract call lets a buyer withdraw
alone.

**Stopped by.** Nothing on-chain. What the design bounds and makes visible:
- The worker adds the treasury's authorization to every withdrawal a buyer signs through the API, with no person in the loop.
- The contract's liabilities and revenue are on-chain, and anyone can compare them with the USDC the treasury and its cold reserve hold.
- Withdrawals pay the buyer's own wallet by default, and each pays out at most once.

**Seen.**
- `PayStellarWithdrawalsWaiting` when a signed withdrawal has not gone out for ten minutes.
- `treasury_deficit` and `PayStellarTreasuryShort` when the treasury holds less than it owes.
- The buyer's own view: a withdrawal left in `SIGNED` that ends `EXPIRED`, with its amount back in the available balance.

**Response.** Run a worker with the treasury's key, top the treasury up from
the cold reserve, or rotate the treasury role to a key the operator holds.

**Left.** On the prepaid ledger, buyers rely on the operator to pay
withdrawals, as with any prepaid balance held by an issuer. The
[vault](#the-vault) gives buyers a way out that needs no operator.

## Compromised operator key

**Can.** Sign charge batches and recurring charge batches for the seller's
contract.

**Stopped by.** The contract, whatever the operator signs:
- each charge is at most `max_charge`;
- a buyer's prepaid credit and the seller's revenue grow by at most the [daily limits](../architecture/prepaid-contract.md#daily-limits) per UTC day;
- a recurring charge is at most the mandate's amount, once per period, from a buyer who signed that mandate; the buyer's daily limit does not apply to it, since the mandate already bounds the wallet, but the seller's does;
- a charge identifier is settled at most once.

The operator key moves no USDC out of the treasury (that needs the treasury's key) and cannot pay out revenue (that needs the seller's and the treasury's).

**Seen.** The observer matches every settled charge to the gateway's records. A charge the gateway never admitted is a critical `unknown_charge` or `unknown_recurring_charge` finding, and a different amount is `charge_amount_mismatch` or `recurring_charge_mismatch` ([observer](../self-hosting/observer.md)). An unusual charge volume raises `PayStellarChargeVolumeUnusual`.

**Response.** Pause the contract with the admin's keys, [rotate the operator](../self-hosting/rotation.md), and refund what the findings show was taken.

**Left.** Until the pause, the operator key can take up to the daily limits from buyers with prepaid credit (on a vault, up to three of each buyer's daily limits around UTC midnight, [as the vault contract explains](../architecture/vault-contract.md#the-buyers-spending-limit)), and each mandate's current period, as seller revenue: the seller can withdraw it, the operator cannot. Set the daily limits to what a day's legitimate charging needs. It can also charge a mandate's period a token amount, which uses the period up: buyers lose nothing, but the seller loses that period's revenue, since periods are not charged late.

## Compromised buyer key

**Can.** Spend the buyer's own credit at the seller: sign x402 commitments
and authorize deposits, withdrawals and mandates; move the USDC in the
buyer's wallet, which the key controls anyway.

**Stopped by.**
- A withdrawal needs the treasury's signature too. The worker gives it only to withdrawals prepared through the seller's API, which needs the seller deployment's API key.
- A withdrawal pays only the buyer's own wallet, unless the operator allows other accounts ([limits](../self-hosting/limits.md#withdrawal-destinations)).
- An x402 commitment pays only the seller it names, within the buyer's available balance.
- The key can also call the contract directly. A deposit made that way only moves the buyer's own USDC into the buyer's own credit; the observer reports it as `unknown_deposit`. A mandate made that way approves the contract for charges the worker never makes, since the worker charges only mandates it recorded.

**Seen.** `PayStellarWithdrawalsToOtherAccounts` when withdrawals to other accounts are allowed. The buyer's balance and charges are in the ledger API.

**Response.** The buyer moves the wallet's USDC to a new account and revokes any mandate. The seller stops admitting charges for that buyer.

**Left.** Whoever holds the key can pay the seller with the buyer's credit, as the buyer could.

## Hostile contract-account wallet

**Can.** A buyer's wallet may be a [contract account](../api/ledger.md#contract-accounts),
whose `__check_auth` is code its owner wrote. It runs when the gateway
simulates the buyer's entry and again in the worker's transaction, which the
operator pays for. Its owner can make it expensive, or accept an entry and
then refuse it once the transaction is sent.

**Stopped by.**
- A bound on the resource fee of a buyer's transaction and of a restore it needs (1 XLM by default). The gateway refuses an entry whose simulation costs more, and the worker does not send one that does.
- At most two restores per buyer request: a wallet whose own state keeps needing restores gets two, and its request then lapses unsent.
- An entry is accepted only after the network runs it in simulation, and the worker simulates again before sending.
- Deposits and withdrawals count against the same per-buyer and per-deployment quotas and fee-account floor as any other.
- Mandates and x402 do not accept contract accounts.

**Seen.** `PayStellarFeeBurnHigh`, `PayStellarSubmissionsNotLanding`, and
fees by kind on the dashboard.

**Response.** Remove the buyer, or lower the deployment's quotas.

**Left.** Up to the quotas a day of transactions at that bound. Each
submitted entry costs the RPC node a simulation, and a refused entry can be
submitted again: the seller's application, which forwards its buyers'
entries, is what limits how often. No USDC moves without the contract's
checks.

## SAC allowance manipulation

**Can.** A mandate approves the ledger contract, in the USDC contract, to move up to the mandate's total from the buyer's wallet until its last ledger.

**Stopped by.**
- Only the contract's code can spend that approval, and only for a period of a mandate the buyer signed, at most once per period and within its amount.
- A new mandate replaces the approval rather than adding to it.
- Revoking sets it to zero.
- The USDC contract itself stops honouring it after its last ledger.
- Changing it needs the buyer's signature.
- Replacing the contract's code needs two of the admin's three keys.

**Seen.**
- A mandate authorized or revoked outside the gateway is a `mandate_changed_elsewhere` finding as soon as the observer reads its event. A buyer lowering the approval in the USDC contract emits no event of this contract; the next charge is then refused as `allowance_short`. Both raise `PayStellarMandateChangedOutsideGateway`.
- A change of the contract's code raises `PayStellarContractCodeChanged` within a reconciliation, since an upgrade emits no event. When the observer is told the expected Wasm, it is also a critical `code_changed` finding and `PayStellarContractCodeUnexpected` for as long as it lasts.

**Response.** For a code change nobody planned, pause and treat the admin keys as exposed.

**Left.** Whoever controls the admin's keys controls the contract's code and so every approval to it; that is why the admin is a multisig.

## Nonce exhaustion

**Can.** Soroban authorization nonces are 64 random bits per entry, consumed only when the call succeeds. No counter can be exhausted. What can be consumed is a source account's sequence numbers, by anyone holding its key.

**Stopped by.**
- An envelope whose sequence someone else consumed is never retried blindly; its outcome is decided from the contract's state.
- The worker leaves that source out for 30 minutes while another source is free, and never leaves out the last free one.
- Source accounts hold no XLM and authorize nothing ([limits](../self-hosting/limits.md#source-accounts)).

**Seen.** `PayStellarSourceSequenceTaken`, and `PayStellarSubmissionsNotLanding` if envelopes keep failing.

**Response.** Rotate the source's key.

**Left.** While the key is out, each taken sequence costs one envelope a later decision, not money.

## Ledger-entry eviction

**Can.** Contract entries expire unless their time to live is extended. An
archived entry must be restored before use.

**Stopped by.**
- Every write extends what it touches to about 30 days once fewer than about 7 remain.
- The worker extends the contract's instance and code before they could expire.
- An archived buyer account or mandate is restored by the network inside the next call that touches it, or the worker sends the restore ([settlement](../architecture/transactions.md#settlement)).
- Charge records and approvals are temporary by design and need no extension.

**Seen.** `PayStellarContractLifeShort`, and the contract's life left on the dashboard.

**Response.** Check the worker, its source accounts and the fee account.

**Left.** Restoring an idle account costs the operator rent, paid inside the call.

## Denial of service via dust deposits

**Can.** Every deposit, withdrawal, mandate and revocation the worker sends
costs a fee whatever its amount.

**Stopped by.**
- The contract's minimum deposit.
- Per-buyer and per-deployment daily quotas, counted in the same database transaction as the insert.
- A minimum withdrawal.
- A floor on the fee account, below which nothing new is sent.

[Limits](../self-hosting/limits.md) sizes them; with the defaults a deployment's deposits cost at most about 130 XLM a day.

**Seen.** `PayStellarDeploymentQuotaReached`, `PayStellarFeeBurnHigh`, `PayStellarFeeAccountLow`, and fees by kind on the dashboard.

**Response.** Check the deployment's traffic; lower its quotas or revoke its API key.

**Left.** Up to the quotas a day.

## Other parties

| Party | Can | Stopped by | Seen |
|---|---|---|---|
| Treasury key | move everything the treasury holds | a [cold reserve](../self-hosting/treasury.md) that needs several signatures bounds it to the hot ceiling | `treasury_deficit`, `PayStellarTreasuryShort`, `unknown_withdrawal` |
| Admin keys | pause, change limits, rotate roles, replace the code | two of three keys for any change ([keys](../self-hosting/keys.md)) | `admin_change`, `role_changed` (`PayStellarWarningFinding`), `PayStellarContractCodeChanged`, `code_changed` |
| Seller API key | admit charges against the deployment's buyers' credit | the buyers' balances, the contract's daily limits, scope to one deployment | `PayStellarChargeVolumeUnusual` |
| Fee account key | spend its XLM | holds only XLM for fees; signs only fee bumps | `PayStellarFeeAccountLow` |
| RPC node | answer stale or wrong data, or stop answering | decisions only from ledgers the node reports, absence only after authorizations lapse, independent reads by the observer | `event_gap`, `PayStellarObserverLagging`, `PayStellarNodeStalled`, `PayStellarObserverStalled` |
| Database role | change rows its grants allow | the narrowest grants per process, final states that cannot be left, append-only audit tables ([database](../self-hosting/database.md)) | reconciliation findings against the contract |

## The vault

The [vault](../architecture/vault-contract.md) holds the buyers' USDC in
the contract itself. It changes what several keys can do:

| Party | Can on the vault | Stopped by | Seen |
|---|---|---|---|
| Operator, or an operator that stops paying | refuse to co-sign withdrawals; charge within each buyer's limits | each buyer's [daily spending limit](../architecture/vault-contract.md#the-buyers-spending-limit) on top of `max_charge` and the daily limits; the buyer's [exit](../architecture/vault-contract.md#leaving), which needs no operator and pays the destination the buyer signed | `unknown_charge` and the other per-event findings, as on the prepaid ledger |
| Admin keys | pause; change limits; propose new code | a pause never blocks exits, withdrawals or revenue payouts; new code installs only `UPGRADE_DELAY_LEDGERS` (about a week) after its proposal, longer than the exit notice, so a buyer who does not accept it can leave first, and no later than three days after that, so it cannot wait for buyers who never had the notice; the gateway takes no new deposit or mandate meanwhile; launch limits bound each buyer's deposits and mandate allowances together | `upgrade_proposed` (critical), and `PayStellarVaultUpgradeProposed` for as long as the code may be installed; `admin_change` for limits, cancellations and installs; `code_changed` if the code is anything but Wasm |
| Buyer's wallet, through another client | lower its limit or request an exit without the gateway | the notice: neither takes effect on-chain before every charge admitted earlier can settle; the gateway applies both to admission as soon as the worker reads them, and refuses charges while that reading lags | `vault_changed_elsewhere`, `PayStellarVaultChangedOutsideGateway` |
| Buyer's key | request an exit of the whole balance to any account; it cannot be cancelled, only replaced, and anyone can complete it once it unlocks | nothing: the key controls the buyer's money, on a vault as in the wallet | the `vault_changed_elsewhere` finding when the request was not made through this gateway |
| RPC node | omit a vault's limit or exit events from what the worker reads | the worker checks each page's order and contract, not completeness; the observer reads the same stream independently | charges admitted against an omitted change fail on-chain: the seller loses at most what that buyer held, and the observer reports the contract's totals against the database |
| USDC issuer | freeze the vault's balance; claw back, if it enables clawback | nothing in the contract; Circle's USDC has no clawback enabled today | `treasury_deauthorized` and `PayStellarTreasuryDeauthorized` while frozen; `treasury_deficit` after a clawback |
| Seller key | pay out the seller's revenue | the vault pays revenue to the seller alone; the seller role moves only with the current seller's authorization | the revenue events in reconciliation |

**Left.**
- A buyer's mandate allowance belongs to the vault's address, not its code. New code could spend what is left of it, so a buyer with a mandate revokes it before an upgrade's effective ledger as well as exiting.
- An admin pause longer than the margin between the notice and the charge window lets admitted charges expire before an exit unlocks. That costs the seller those charges, never a buyer's balance.
- A worker stopped for longer than its RPC node retains events cannot read the events it missed. It stops reading that vault and charges stay refused until an operator recovers it ([lost vault events](../self-hosting/vault.md#lost-vault-events)).
