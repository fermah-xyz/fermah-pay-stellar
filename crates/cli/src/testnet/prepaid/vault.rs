//! The vault on testnet or a local network: deployment, a seller's flow
//! through the gateway API, and completing an exit once its notice passed.
//!
//! The vault holds the USDC itself, so a run checks the vault's own balance
//! in the USDC asset contract against what it owes, and every buyer figure
//! the gateway reports against the contract's account entry.

use anyhow::{Context as _, bail, ensure};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::prepaid::{
    AdminAction, Custody, PrepaidDeployment, VaultAccount, sac_balance, vault_account,
    vault_constructor_args,
};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::signer::LocalSigner;
use fermah_pay_stellar_chain::stellar_xdr::{
    ContractId, Hash, HostFunction, Limits, ReadXdr, ScAddress, ScVal, SorobanAuthorizationEntry,
    WriteXdr,
};
use fermah_pay_stellar_chain::{deploy, network_id, usdc};
use fermah_pay_stellar_domain::AccountAddress;
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    ChargeState, CreateBuyerRequest, CreateChargeRequest, DepositState, GetBalanceRequest,
    GetBalanceResponse, GetChargeRequest, GetDepositRequest, GetExitRequest, GetLimitChangeRequest,
    GetWithdrawalRequest, PrepareDepositRequest, PrepareExitRequest, PrepareLimitChangeRequest,
    PrepareWithdrawalRequest, SubmitDepositRequest, SubmitExitRequest, SubmitLimitChangeRequest,
    SubmitWithdrawalRequest, VaultRequestState, WithdrawalState,
};
use serde_json::{Value, json};
use tonic::Code;
use tonic::transport::Channel;

use super::e2e::{Bound, FundedBuyer, authed, until};
use super::{
    CHANNEL, Context, FEE_SOURCE, SUBMITTER, contract_bytes, random_bytes, receipt_json, unix_now,
};
use crate::testnet::evidence::{self, tx_url};
use crate::testnet::ledger::balances;
use crate::testnet::profile::VaultDeployment;

/// 0.1 USDC in, under a daily limit of 0.04 USDC.
const DEPOSIT: i64 = 1_000_000;
const DAILY_LIMIT: i64 = 400_000;
/// Within the limit together; a third charge would pass it.
const CHARGES: [i64; 2] = [100_000, 200_000];
const ABOVE_LIMIT: i64 = 150_000;
/// Returned to the wallet with the operator's co-signature.
const WITHDRAWAL: i64 = 100_000;
/// The lower limit the buyer then asks for, applied by the contract only
/// after its notice but by admission at once.
const LOWER_LIMIT: i64 = 50_000;
/// What the buyer asks to take without the operator.
const EXIT: i64 = 250_000;
/// The vault's `ExitLocked` error.
const EXIT_LOCKED: &str = "Error(Contract, #125)";

impl Context {
    pub(super) fn vault(&self) -> anyhow::Result<(VaultDeployment, PrepaidDeployment)> {
        let recorded = self.profile.vault_deployment()?;
        let pinned = PrepaidDeployment {
            contract: contract_bytes(&recorded.contract)?,
            usdc: contract_bytes(&recorded.usdc)?,
            custody: Custody::Vault,
        };
        Ok((recorded, pinned))
    }

    /// Uploads the vault Wasm, creates an instance whose constructor pins
    /// the profile's roles, the network's USDC and the limits, and, with
    /// `launch_limits`, has the admin bound each buyer's balance and the
    /// total.
    pub async fn deploy_vault(
        &self,
        wasm: &[u8],
        min_deposit: i128,
        max_charge: i128,
        launch_limits: Option<(i128, i128)>,
    ) -> anyhow::Result<()> {
        self.rpc.verify_network(self.network).await?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let usdc_contract = usdc::asset_contract_id(&usdc::circle_usdc(self.network), self.network);
        let admin = self.profile.key("admin")?.address();
        let operator = self.profile.key("operator")?.address();
        let seller = self.profile.key("seller")?.address();

        let wasm_hash = deploy::wasm_hash(wasm);
        let upload = deploy::upload(wasm)?;
        let auth = submitter.record_source_authorization(&upload).await?;
        let uploaded = submitter.submit(upload, auth).await?;

        let salt: [u8; 32] = random_bytes()?;
        let expected = deploy::contract_id(self.network, &source.address(), salt);
        let create = deploy::create(
            &source.address(),
            salt,
            wasm_hash,
            vault_constructor_args(
                &admin,
                &operator,
                &seller,
                usdc_contract,
                min_deposit,
                max_charge,
            ),
        )?;
        let auth = submitter.record_source_authorization(&create).await?;
        let created = submitter.submit(create, auth).await?;
        ensure!(
            created.return_value
                == Some(ScVal::Address(ScAddress::Contract(ContractId(Hash(expected))))),
            "network created {:?}, expected {}",
            created.return_value,
            usdc::contract_strkey(expected)
        );
        let limited = match launch_limits {
            None => None,
            Some(limits) => {
                let action = AdminAction::SetLaunchLimits { limits: Some(limits) };
                let function = HostFunction::InvokeContract(action.call(expected));
                let (_, tree) = action
                    .authorizations(expected, &admin)
                    .into_iter()
                    .next()
                    .context("no admin authorization")?;
                let auth = self.admin_authorization(&submitter, &function, tree).await?;
                Some(submitter.submit(function, vec![auth]).await?)
            }
        };

        let deployment = VaultDeployment {
            contract: usdc::contract_strkey(expected),
            wasm_sha256: hex_lower(&wasm_hash),
            usdc: usdc::contract_strkey(usdc_contract),
            admin: admin.to_string(),
            operator: operator.to_string(),
            seller: seller.to_string(),
            min_deposit,
            max_charge,
            launch_limits,
        };
        self.profile.save_vault_deployment(&deployment)?;
        evidence::write(
            &self.evidence_dir,
            "vault-deployment",
            json!({
                "criterion": "vault-deployment",
                "expected": "vault created from the recorded Wasm with its constructor pinning the roles, the network's USDC and limits, and the admin's launch limits set",
                "deployment": serde_json::to_value(&deployment)?,
                "upload": receipt_json(&uploaded),
                "create": receipt_json(&created),
                "launch_limits": limited.as_ref().map(receipt_json),
            }),
        )
    }

    /// One seller's flow against the recorded vault, through the gateway API
    /// run in this process on the database at `database_url`.
    pub async fn vault_end_to_end(&self, database_url: &str) -> anyhow::Result<()> {
        let (recorded, pinned) = self.vault()?;
        let sources = vec![
            LocalSigner::arc(self.profile.key(CHANNEL)?),
            LocalSigner::arc(self.profile.key(SUBMITTER)?),
        ];
        let bound = Bound {
            contract: recorded.contract.clone(),
            operator: recorded.operator.clone(),
            treasury: None,
            pinned: pinned.clone(),
        };
        let stack = self.stack_on(database_url, "vault", sources, Quotas::default(), bound).await?;
        // The gateway takes the vault's money only once the worker reads it.
        let reading = until(
            "the worker's first reading of the vault",
            || async {
                Ok(sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM pay_stellar.vault_event_cursors",
                )
                .fetch_one(&stack.records)
                .await?)
            },
            |rows| *rows > 0,
        )
        .await;
        let outcome = match reading {
            Ok(_) => self.vault_flow(&stack.endpoint, &stack.token, &pinned, &recorded).await,
            Err(error) => Err(error),
        };
        stack.stop().await;
        evidence::write(&self.evidence_dir, "vault-end-to-end", outcome?)
    }

    #[allow(clippy::too_many_lines)]
    async fn vault_flow(
        &self,
        endpoint: &str,
        token: &str,
        pinned: &PrepaidDeployment,
        recorded: &VaultDeployment,
    ) -> anyhow::Result<Value> {
        let asset = usdc::circle_usdc(self.network);
        let channel = Channel::from_shared(endpoint.to_owned())?.connect().await?;
        let mut buyers = BuyerServiceClient::new(channel.clone());
        let mut ledger = LedgerServiceClient::new(channel);
        let FundedBuyer { key: buyer, funding, .. } = self.funded_buyer(DEPOSIT).await?;
        let sign = |xdr: &str| -> anyhow::Result<String> {
            let entry = SorobanAuthorizationEntry::from_xdr_base64(xdr, Limits::none())?;
            Ok(sign_entry(&entry, network_id(self.network), &[&buyer])?
                .to_xdr_base64(Limits::none())?)
        };
        let buyer_id = buyers
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("vault-buyer-{}", unix_now()),
                    wallet_address: buyer.address().to_string(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?
            .buyer_id;
        let balance = |ledger: &LedgerServiceClient<Channel>| {
            let mut ledger = ledger.clone();
            let request = GetBalanceRequest { buyer_id: buyer_id.clone() };
            async move { anyhow::Ok(ledger.get_balance(authed(request, token)?).await?.into_inner()) }
        };

        // The first deposit carries the daily limit, under the same
        // signature.
        let prepared = ledger
            .prepare_deposit(authed(
                PrepareDepositRequest {
                    buyer_id: buyer_id.clone(),
                    amount: DEPOSIT,
                    idempotency_key: "deposit-1".to_owned(),
                    daily_limit: Some(DAILY_LIMIT),
                },
                token,
            )?)
            .await?
            .into_inner()
            .deposit
            .context("no deposit returned")?;
        ledger
            .submit_deposit(authed(
                SubmitDepositRequest {
                    deposit_id: prepared.deposit_id.clone(),
                    signed_authorization_entry_xdr: sign(&prepared.authorization_entry_xdr)?,
                },
                token,
            )?)
            .await?;
        let deposit = until(
            "deposit",
            || {
                let mut ledger = ledger.clone();
                let request = GetDepositRequest { deposit_id: prepared.deposit_id.clone() };
                async move {
                    ledger
                        .get_deposit(authed(request, token)?)
                        .await?
                        .into_inner()
                        .deposit
                        .context("no deposit")
                }
            },
            |d| !matches!(d.state(), DepositState::Signed | DepositState::Submitted),
        )
        .await?;
        ensure!(deposit.state() == DepositState::Confirmed, "deposit ended {:?}", deposit.state());
        // The gateway learns the limit from the contract's event.
        until("the daily limit", || balance(&ledger), |b| b.daily_limit == DAILY_LIMIT).await?;

        // Charges within the limit settle; one past it is refused at once.
        let mut charges = Vec::new();
        for (i, amount) in CHARGES.into_iter().enumerate() {
            let created = ledger
                .create_charge(authed(
                    CreateChargeRequest {
                        buyer_id: buyer_id.clone(),
                        amount,
                        idempotency_key: format!("charge-{}", i + 1),
                    },
                    token,
                )?)
                .await?
                .into_inner()
                .charge
                .context("no charge")?;
            charges.push(created.charge_id);
        }
        let above = refusal(
            ledger
                .create_charge(authed(
                    CreateChargeRequest {
                        buyer_id: buyer_id.clone(),
                        amount: ABOVE_LIMIT,
                        idempotency_key: "charge-above".to_owned(),
                    },
                    token,
                )?)
                .await,
        )?;
        ensure!(
            above == (Code::FailedPrecondition, "above_spending_limit".to_owned()),
            "a charge past the daily limit ended {above:?}"
        );
        let mut settled = Vec::new();
        for id in &charges {
            let charge = until(
                "charge",
                || {
                    let mut ledger = ledger.clone();
                    let request = GetChargeRequest { charge_id: id.clone() };
                    async move {
                        ledger
                            .get_charge(authed(request, token)?)
                            .await?
                            .into_inner()
                            .charge
                            .context("no charge")
                    }
                },
                |c| !matches!(c.state(), ChargeState::Admitted | ChargeState::Submitted),
            )
            .await?;
            ensure!(charge.state() == ChargeState::Charged, "charge ended {:?}", charge.state());
            settled.push(charge);
        }

        // A withdrawal: the wallet signs, the worker adds the operator's
        // signature, and the vault pays the wallet.
        let wallet_before = balances(&self.rpc, &buyer.address(), &asset).await?;
        let prepared = ledger
            .prepare_withdrawal(authed(
                PrepareWithdrawalRequest {
                    buyer_id: buyer_id.clone(),
                    amount: WITHDRAWAL,
                    destination: String::new(),
                    idempotency_key: "withdrawal-1".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .withdrawal
            .context("no withdrawal returned")?;
        ledger
            .submit_withdrawal(authed(
                SubmitWithdrawalRequest {
                    withdrawal_id: prepared.withdrawal_id.clone(),
                    signed_authorization_entry_xdr: sign(&prepared.authorization_entry_xdr)?,
                },
                token,
            )?)
            .await?;
        let withdrawal = until(
            "withdrawal",
            || {
                let mut ledger = ledger.clone();
                let request =
                    GetWithdrawalRequest { withdrawal_id: prepared.withdrawal_id.clone() };
                async move {
                    ledger
                        .get_withdrawal(authed(request, token)?)
                        .await?
                        .into_inner()
                        .withdrawal
                        .context("no withdrawal")
                }
            },
            |w| !matches!(w.state(), WithdrawalState::Signed | WithdrawalState::Submitted),
        )
        .await?;
        ensure!(
            withdrawal.state() == WithdrawalState::Confirmed,
            "withdrawal ended {:?}",
            withdrawal.state()
        );
        let wallet_after = balances(&self.rpc, &buyer.address(), &asset).await?;
        ensure!(
            wallet_after.usdc.zip(wallet_before.usdc).map(|(a, b)| a - b) == Some(WITHDRAWAL),
            "the wallet's USDC went from {:?} to {:?}, expected {WITHDRAWAL} more",
            wallet_before.usdc,
            wallet_after.usdc
        );

        // A lower limit: the contract applies it after its notice, admission
        // at once.
        let prepared = ledger
            .prepare_limit_change(authed(
                PrepareLimitChangeRequest {
                    buyer_id: buyer_id.clone(),
                    daily_limit: LOWER_LIMIT,
                    idempotency_key: "limit-1".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .limit_change
            .context("no limit change returned")?;
        ledger
            .submit_limit_change(authed(
                SubmitLimitChangeRequest {
                    limit_change_id: prepared.limit_change_id.clone(),
                    signed_authorization_entry_xdr: sign(&prepared.authorization_entry_xdr)?,
                },
                token,
            )?)
            .await?;
        let limit_change = until(
            "limit change",
            || {
                let mut ledger = ledger.clone();
                let request =
                    GetLimitChangeRequest { limit_change_id: prepared.limit_change_id.clone() };
                async move {
                    ledger
                        .get_limit_change(authed(request, token)?)
                        .await?
                        .into_inner()
                        .limit_change
                        .context("no limit change")
                }
            },
            |c| !matches!(c.state(), VaultRequestState::Signed | VaultRequestState::Submitted),
        )
        .await?;
        ensure!(
            limit_change.state() == VaultRequestState::Confirmed,
            "limit change ended {:?}",
            limit_change.state()
        );

        // An exit to the wallet: payable by anyone once the notice passes,
        // and held from charges and withdrawals until then.
        let prepared = ledger
            .prepare_exit(authed(
                PrepareExitRequest {
                    buyer_id: buyer_id.clone(),
                    amount: EXIT,
                    destination: String::new(),
                    idempotency_key: "exit-1".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .exit
            .context("no exit returned")?;
        ledger
            .submit_exit(authed(
                SubmitExitRequest {
                    exit_id: prepared.exit_id.clone(),
                    signed_authorization_entry_xdr: sign(&prepared.authorization_entry_xdr)?,
                },
                token,
            )?)
            .await?;
        let exit = until(
            "exit request",
            || {
                let mut ledger = ledger.clone();
                let request = GetExitRequest { exit_id: prepared.exit_id.clone() };
                async move {
                    ledger
                        .get_exit(authed(request, token)?)
                        .await?
                        .into_inner()
                        .exit
                        .context("no exit")
                }
            },
            |e| !matches!(e.state(), VaultRequestState::Signed | VaultRequestState::Submitted),
        )
        .await?;
        ensure!(exit.state() == VaultRequestState::Confirmed, "exit ended {:?}", exit.state());

        // What the exit leaves free: a withdrawal into it is refused, as is
        // a charge past the lower limit.
        let available = DEPOSIT - CHARGES.iter().sum::<i64>() - WITHDRAWAL;
        // The hold is taken when the signed withdrawal is submitted.
        let into = ledger
            .prepare_withdrawal(authed(
                PrepareWithdrawalRequest {
                    buyer_id: buyer_id.clone(),
                    amount: available - EXIT + 1,
                    destination: String::new(),
                    idempotency_key: "withdrawal-into-exit".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .withdrawal
            .context("no withdrawal returned")?;
        let into_exit = refusal(
            ledger
                .submit_withdrawal(authed(
                    SubmitWithdrawalRequest {
                        withdrawal_id: into.withdrawal_id.clone(),
                        signed_authorization_entry_xdr: sign(&into.authorization_entry_xdr)?,
                    },
                    token,
                )?)
                .await,
        )?;
        ensure!(
            into_exit == (Code::FailedPrecondition, "exit_requested".to_owned()),
            "a withdrawal into the exit ended {into_exit:?}"
        );
        let past_lower = refusal(
            ledger
                .create_charge(authed(
                    CreateChargeRequest {
                        buyer_id: buyer_id.clone(),
                        amount: 1,
                        idempotency_key: "charge-past-lower".to_owned(),
                    },
                    token,
                )?)
                .await,
        )?;
        ensure!(
            past_lower == (Code::FailedPrecondition, "above_spending_limit".to_owned()),
            "a charge past the lower limit ended {past_lower:?}"
        );

        // The exit cannot be completed before its notice: only simulated.
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let early = match submitter
            .read(HostFunction::InvokeContract(pinned.exit_call(&buyer.address())))
            .await
        {
            Ok(_) => bail!("the vault paid an exit before its notice"),
            Err(error) => format!("{error:#}"),
        };
        ensure!(
            early.contains(EXIT_LOCKED),
            "an early exit was refused for another reason: {early}"
        );

        // The gateway's figures against the contract's account entry. The
        // limit change and the exit are applied by the worker from events.
        let account = self.vault_account_of(pinned, &buyer.address()).await?;
        let (lower, effective_at) = account.pending_cap.context("no lower limit on the vault")?;
        let (exit_amount, _, unlock_at) = account.exit.clone().context("no exit on the vault")?;
        let shown = until(
            "the gateway's vault figures",
            || balance(&ledger),
            |b| b.exit_amount.is_some() && b.pending_daily_limit.is_some(),
        )
        .await?;
        let expected = GetBalanceResponse {
            available,
            pending_charges: 0,
            pending_withdrawals: 0,
            daily_limit: DAILY_LIMIT,
            pending_daily_limit: Some(LOWER_LIMIT),
            pending_daily_limit_ledger: effective_at,
            exit_amount: Some(EXIT),
            exit_unlock_ledger: unlock_at,
            admitted_daily_limit: LOWER_LIMIT,
            reserved_for_exit: EXIT,
        };
        ensure!(shown == expected, "the gateway shows {shown:?}, expected {expected:?}");
        ensure!(
            (account.balance, account.cap, lower, exit_amount)
                == (
                    i128::from(available),
                    i128::from(DAILY_LIMIT),
                    i128::from(LOWER_LIMIT),
                    i128::from(EXIT)
                ),
            "the vault holds {account:?}"
        );
        let solvency = self.vault_solvency(&submitter, pinned).await?;

        let after = balances(&self.rpc, &buyer.address(), &asset).await?;
        ensure!(after.xlm_stroops == Some(0), "buyer holds XLM: {:?}", after.xlm_stroops);
        Ok(json!({
            "criterion": "vault-end-to-end",
            "expected": "through the gateway API on the vault: a new buyer with 0 XLM deposits and sets a daily limit with one signature; charges within the limit settle and one past it is refused; a withdrawal is paid with the operator's co-signature; a lower limit is admitted at once and applied by the contract after its notice; an exit request holds its amount from withdrawals and cannot be completed before its notice; the gateway's figures equal the vault's and the vault holds what it owes",
            "contract": recorded.contract,
            "buyer": buyer.address().to_string(),
            "funding_transaction": hex_lower(&funding),
            "deposit": {
                "amount": DEPOSIT,
                "daily_limit": DAILY_LIMIT,
                "transaction_hash": deposit.transaction_hash,
                "public_explorer_url": tx_url(&deposit.transaction_hash),
            },
            "charges": settled.iter().map(|c| json!({
                "amount": c.amount,
                "transaction_hash": c.transaction_hash,
                "ledger": c.ledger,
            })).collect::<Vec<_>>(),
            "charge_past_limit": { "amount": ABOVE_LIMIT, "refusal": above.1 },
            "withdrawal": {
                "amount": WITHDRAWAL,
                "transaction_hash": withdrawal.transaction_hash,
                "public_explorer_url": tx_url(&withdrawal.transaction_hash),
            },
            "lower_limit": {
                "daily_limit": LOWER_LIMIT,
                "applies_from_ledger": effective_at,
                "transaction_hash": limit_change.transaction_hash,
                "public_explorer_url": tx_url(&limit_change.transaction_hash),
            },
            "exit": {
                "amount": EXIT,
                "destination": exit.destination,
                "unlocks_at_ledger": unlock_at,
                "transaction_hash": exit.transaction_hash,
                "public_explorer_url": tx_url(&exit.transaction_hash),
                "withdrawal_into_it": into_exit.1,
                "completion_before_notice": "refused in simulation as ExitLocked (contract error 125)",
            },
            "charge_past_lower_limit": past_lower.1,
            "observed": {
                "gateway": {
                    "available": shown.available,
                    "daily_limit": shown.daily_limit,
                    "pending_daily_limit": shown.pending_daily_limit,
                    "admitted_daily_limit": shown.admitted_daily_limit,
                    "exit_amount": shown.exit_amount,
                    "reserved_for_exit": shown.reserved_for_exit,
                },
                "vault_account": {
                    "balance": account.balance.to_string(),
                    "daily_limit": account.cap.to_string(),
                    "pending_daily_limit": lower.to_string(),
                    "exit_amount": exit_amount.to_string(),
                },
                "solvency": solvency,
                "buyer_xlm_stroops": after.xlm_stroops,
            },
        }))
    }

    /// Completes `owner`'s exit once its notice has passed, on the recorded
    /// vault or on `contract`, an earlier vault of the same network. Anyone
    /// may send it; it pays the destination the buyer signed.
    pub async fn vault_exit(
        &self,
        owner: &AccountAddress,
        contract: Option<&str>,
    ) -> anyhow::Result<()> {
        self.rpc.verify_network(self.network).await?;
        let (mut recorded, mut pinned) = self.vault()?;
        if let Some(contract) = contract {
            pinned.contract = contract_bytes(contract)?;
            contract.clone_into(&mut recorded.contract);
        }
        let asset = usdc::circle_usdc(self.network);
        let before = self.vault_account_of(&pinned, owner).await?;
        let (requested, destination, unlock_at) =
            before.exit.clone().context("the buyer has no exit request")?;
        let destination: AccountAddress = destination
            .to_string()
            .parse()
            .context("the exit pays a contract; only an account's balance is checked here")?;
        let wallet_before = balances(&self.rpc, &destination, &asset).await?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let function = HostFunction::InvokeContract(pinned.exit_call(owner));
        let auth = submitter.record_source_authorization(&function).await?;
        let paid = submitter.submit(function, auth).await?;
        let after = self.vault_account_of(&pinned, owner).await?;
        let wallet_after = balances(&self.rpc, &destination, &asset).await?;
        let amount = requested.min(before.balance);
        ensure!(
            after.exit.is_none() && after.balance == before.balance - amount,
            "the vault account went from {before:?} to {after:?}"
        );
        ensure!(
            wallet_after.usdc.zip(wallet_before.usdc).map(|(a, b)| i128::from(a - b))
                == Some(amount),
            "the destination's USDC went from {:?} to {:?}, expected {amount} more",
            wallet_before.usdc,
            wallet_after.usdc
        );
        let solvency = self.vault_solvency(&submitter, &pinned).await?;
        evidence::write(
            &self.evidence_dir,
            "vault-exit",
            json!({
                "criterion": "vault-exit",
                "expected": "after the notice, an account other than the buyer and the operator completes the buyer's exit; the vault pays the destination the buyer signed and still holds what it owes",
                "contract": recorded.contract,
                "buyer": owner.to_string(),
                "sent_by": source.address().to_string(),
                "destination": destination.to_string(),
                "requested": requested.to_string(),
                "paid": amount.to_string(),
                "unlocked_at_ledger": unlock_at,
                "exit": receipt_json(&paid),
                "public_explorer_url": tx_url(&hex_lower(&paid.outer_hash)),
                "observed": {
                    "vault_balance_before": before.balance.to_string(),
                    "vault_balance_after": after.balance.to_string(),
                    "solvency": solvency,
                },
            }),
        )
    }

    async fn vault_account_of(
        &self,
        pinned: &PrepaidDeployment,
        owner: &AccountAddress,
    ) -> anyhow::Result<VaultAccount> {
        self.rpc
            .get_ledger_entries(&[pinned.account_key(owner)])
            .await?
            .first()
            .and_then(|record| vault_account(&record.data))
            .with_context(|| format!("no vault account for {owner}"))
    }

    /// The vault's USDC against what it owes; fails if it holds less.
    async fn vault_solvency(
        &self,
        submitter: &fermah_pay_stellar_chain::sponsored::Submitter<'_>,
        pinned: &PrepaidDeployment,
    ) -> anyhow::Result<Value> {
        let totals = self.totals(submitter, pinned).await?;
        let held = self
            .rpc
            .get_ledger_entries(&[pinned.vault_balance_key()])
            .await?
            .first()
            .and_then(|record| sac_balance(&record.data))
            .context("the vault holds no USDC balance entry")?;
        let owed = totals.liabilities + totals.revenue;
        ensure!(held.authorized && held.amount >= owed, "the vault holds {held:?} and owes {owed}");
        Ok(json!({
            "vault_usdc": held.amount.to_string(),
            "liabilities": totals.liabilities.to_string(),
            "revenue": totals.revenue.to_string(),
        }))
    }
}

/// The code and reason of a refused call; fails if it was not refused.
fn refusal<T: std::fmt::Debug>(
    result: Result<tonic::Response<T>, tonic::Status>,
) -> anyhow::Result<(Code, String)> {
    match result {
        Ok(response) => bail!("accepted: {:?}", response.into_inner()),
        Err(status) => Ok((status.code(), status.message().to_owned())),
    }
}
