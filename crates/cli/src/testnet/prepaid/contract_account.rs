//! A buyer whose wallet is a contract account, through the gateway API on
//! testnet: the example account contract is deployed and funded with Circle
//! USDC, then registered, deposits, is charged and withdraws to itself. A
//! deposit signed by another key is refused by the gateway, which asks the
//! network.

use anyhow::{Context as _, ensure};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::prepaid::{
    ChainAddress, usdc_balance_call, usdc_transfer_authorization, usdc_transfer_call,
};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::signer::LocalSigner;
use fermah_pay_stellar_chain::stellar_xdr::{
    BytesM, ContractId, Hash, HostFunction, Limits, ReadXdr, ScAddress, ScBytes, ScVal,
    SorobanAuthorizationEntry, SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_chain::{deploy, usdc};
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    ChargeState, CreateBuyerRequest, CreateChargeRequest, DepositState, GetChargeRequest,
    GetDepositRequest, GetWithdrawalRequest, PrepareDepositRequest, PrepareWithdrawalRequest,
    SubmitDepositRequest, SubmitWithdrawalRequest, WithdrawalState,
};
use serde_json::json;
use tonic::transport::Channel;

use super::e2e::{authed, until};
use super::{
    CHANNEL, Context, FEE_SOURCE, SPONSOR, SUBMITTER, USDC_RESERVE, i128_of, random_bytes,
    receipt_json, unix_now,
};
use crate::testnet::evidence::{self, tx_url};

/// 0.5 USDC into the wallet, 0.3 deposited, 0.1 charged, 0.15 withdrawn.
const FUNDED: i128 = 5_000_000;
const DEPOSIT: i64 = 3_000_000;
const CHARGE: i64 = 1_000_000;
const WITHDRAWAL: i64 = 1_500_000;

/// `entry` carrying `key`'s signature of `payload_hex`, the payload the API
/// reports, as the example account's `__check_auth` takes it.
fn signed_for(entry_xdr: &str, payload_hex: &str, key: &SecretKey) -> anyhow::Result<String> {
    let payload: [u8; 32] = (0..32)
        .map(|i| u8::from_str_radix(&payload_hex[2 * i..2 * i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("payload is not 32 bytes"))?;
    let mut entry = SorobanAuthorizationEntry::from_xdr_base64(entry_xdr, Limits::none())?;
    let (SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds)) =
        &mut entry.credentials
    else {
        anyhow::bail!("the API returned an entry without address credentials");
    };
    creds.signature = ScVal::Bytes(ScBytes(BytesM::try_from(key.sign_raw(&payload).to_vec())?));
    Ok(entry.to_xdr_base64(Limits::none())?)
}

impl Context {
    pub async fn contract_account_buyer(
        &self,
        database_url: &str,
        account_wasm: &[u8],
    ) -> anyhow::Result<()> {
        let (_, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SPONSOR)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);

        // The wallet: an example account answering to a new key, holding
        // Circle USDC sent from the reserve.
        let key = SecretKey::generate()?;
        let wasm_hash = deploy::wasm_hash(account_wasm);
        let upload = deploy::upload(account_wasm)?;
        let auth = submitter.record_source_authorization(&upload).await?;
        let uploaded = submitter.submit(upload, auth).await?;
        let salt: [u8; 32] = random_bytes()?;
        let id = deploy::contract_id(self.network, &source.address(), salt);
        let create = deploy::create(
            &source.address(),
            salt,
            wasm_hash,
            vec![ScVal::Bytes(ScBytes(BytesM::try_from(key.address().public_key().to_vec())?))],
        )?;
        let auth = submitter.record_source_authorization(&create).await?;
        let created = submitter.submit(create, auth).await?;
        ensure!(
            created.return_value == Some(ScVal::Address(ScAddress::Contract(ContractId(Hash(id))))),
            "the network created {:?}",
            created.return_value
        );
        let wallet = ChainAddress::Contract(id);
        let reserve = self.profile.key(USDC_RESERVE)?;
        let function = HostFunction::InvokeContract(usdc_transfer_call(
            pinned.usdc,
            &reserve.address(),
            &wallet,
            FUNDED,
        ));
        let auth = self
            .authorize(
                &submitter,
                &function,
                &[(
                    &reserve,
                    usdc_transfer_authorization(pinned.usdc, &reserve.address(), &wallet, FUNDED),
                )],
            )
            .await?;
        let funded = submitter.submit(function, auth).await?;
        let usdc_of = |owner: ChainAddress| {
            let submitter = &submitter;
            async move {
                let value = submitter
                    .read(HostFunction::InvokeContract(usdc_balance_call(pinned.usdc, &owner)))
                    .await?;
                value.as_ref().and_then(i128_of).context("balance returned no amount")
            }
        };
        let native = usdc::asset_contract_id(
            &fermah_pay_stellar_chain::stellar_xdr::Asset::Native,
            self.network,
        );
        let xlm_of = |owner: ChainAddress| {
            let submitter = &submitter;
            async move {
                let value = submitter
                    .read(HostFunction::InvokeContract(usdc_balance_call(native, &owner)))
                    .await?;
                value.as_ref().and_then(i128_of).context("balance returned no amount")
            }
        };
        let start = (usdc_of(wallet.clone()).await?, xlm_of(wallet.clone()).await?);
        ensure!(start == (FUNDED, 0), "the wallet holds {start:?} before depositing");

        let sources = vec![
            LocalSigner::arc(self.profile.key(CHANNEL)?),
            LocalSigner::arc(self.profile.key(SUBMITTER)?),
        ];
        let stack =
            self.stack(database_url, "contract-account", sources, Quotas::default()).await?;
        let outcome =
            self.contract_account_flow(&stack.endpoint, &stack.token, &wallet, &key).await;
        stack.stop().await;
        let flow = outcome?;
        let end = (usdc_of(wallet.clone()).await?, xlm_of(wallet.clone()).await?);
        let expected_usdc = FUNDED - i128::from(DEPOSIT) + i128::from(WITHDRAWAL);
        ensure!(end == (expected_usdc, 0), "the wallet holds {end:?} at the end");
        let credit = self.contract_balance(&submitter, &pinned, &wallet).await?;
        ensure!(
            credit == i128::from(DEPOSIT - CHARGE - WITHDRAWAL),
            "the contract credits the wallet {credit}"
        );

        let record = json!({
            "criterion": "contract-account-buyer",
            "expected": "a buyer whose wallet is a contract account deposits, is charged and withdraws through the gateway API, authorizing with its own __check_auth; a deposit signed by another key is refused by the gateway after the network refuses it in simulation",
            "wallet": {
                "address": wallet.to_string(),
                "kind": "contract account authorized by one Ed25519 key (contracts/example-account)",
                "wasm_sha256": hex_lower(&wasm_hash),
                "upload": receipt_json(&uploaded),
                "create": receipt_json(&created),
                "funded_with_circle_usdc": receipt_json(&funded),
            },
            "flow": flow,
            "observed": {
                "wallet_usdc_before": start.0.to_string(),
                "wallet_usdc_after": end.0.to_string(),
                "wallet_xlm_before_and_after": "0",
                "contract_credit_after": credit.to_string(),
            },
        });
        evidence::write(&self.evidence_dir, "contract-account-buyer", record)
    }

    async fn contract_account_flow(
        &self,
        endpoint: &str,
        token: &str,
        wallet: &ChainAddress,
        key: &SecretKey,
    ) -> anyhow::Result<serde_json::Value> {
        let channel = Channel::from_shared(endpoint.to_owned())?.connect().await?;
        let mut ledger = LedgerServiceClient::new(channel.clone());
        let buyer_id = BuyerServiceClient::new(channel)
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("contract-account-{}", unix_now()),
                    wallet_address: wallet.to_string(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?
            .buyer_id;

        let deposit = ledger
            .prepare_deposit(authed(
                PrepareDepositRequest {
                    buyer_id: buyer_id.clone(),
                    amount: DEPOSIT,
                    idempotency_key: format!("deposit-{}", unix_now()),
                },
                token,
            )?)
            .await?
            .into_inner()
            .deposit
            .context("no deposit returned")?;
        // Another key's signature: the gateway asks the network, which runs
        // the account's `__check_auth`, and refuses.
        let stranger = SecretKey::generate()?;
        let refused = ledger
            .submit_deposit(authed(
                SubmitDepositRequest {
                    deposit_id: deposit.deposit_id.clone(),
                    signed_authorization_entry_xdr: signed_for(
                        &deposit.authorization_entry_xdr,
                        &deposit.signature_payload,
                        &stranger,
                    )?,
                },
                token,
            )?)
            .await
            .expect_err("a deposit signed by another key was accepted");
        ensure!(
            refused.message() == "authorization_refused",
            "refused with {} rather than authorization_refused",
            refused.message()
        );
        ledger
            .submit_deposit(authed(
                SubmitDepositRequest {
                    deposit_id: deposit.deposit_id.clone(),
                    signed_authorization_entry_xdr: signed_for(
                        &deposit.authorization_entry_xdr,
                        &deposit.signature_payload,
                        key,
                    )?,
                },
                token,
            )?)
            .await?;
        let deposit = until(
            "deposit",
            || {
                let mut ledger = ledger.clone();
                let request = GetDepositRequest { deposit_id: deposit.deposit_id.clone() };
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

        let charge = ledger
            .create_charge(authed(
                CreateChargeRequest {
                    buyer_id: buyer_id.clone(),
                    amount: CHARGE,
                    idempotency_key: format!("charge-{}", unix_now()),
                },
                token,
            )?)
            .await?
            .into_inner()
            .charge
            .context("no charge returned")?;
        let charge = until(
            "charge",
            || {
                let mut ledger = ledger.clone();
                let request = GetChargeRequest { charge_id: charge.charge_id.clone() };
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

        let withdrawal = ledger
            .prepare_withdrawal(authed(
                PrepareWithdrawalRequest {
                    buyer_id,
                    amount: WITHDRAWAL,
                    destination: String::new(),
                    idempotency_key: format!("withdrawal-{}", unix_now()),
                },
                token,
            )?)
            .await?
            .into_inner()
            .withdrawal
            .context("no withdrawal returned")?;
        ensure!(
            withdrawal.destination == wallet.to_string(),
            "withdrawal to {}",
            withdrawal.destination
        );
        ledger
            .submit_withdrawal(authed(
                SubmitWithdrawalRequest {
                    withdrawal_id: withdrawal.withdrawal_id.clone(),
                    signed_authorization_entry_xdr: signed_for(
                        &withdrawal.authorization_entry_xdr,
                        &withdrawal.signature_payload,
                        key,
                    )?,
                },
                token,
            )?)
            .await?;
        let withdrawal = until(
            "withdrawal",
            || {
                let mut ledger = ledger.clone();
                let request =
                    GetWithdrawalRequest { withdrawal_id: withdrawal.withdrawal_id.clone() };
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

        let tx =
            |hash: &str| json!({ "transaction_hash": hash, "public_explorer_url": tx_url(hash) });
        Ok(json!({
            "refused_deposit": {
                "signed_by": "a key the account does not hold",
                "refusal": refused.message(),
            },
            "deposit": { "amount": DEPOSIT, "transaction": tx(&deposit.transaction_hash) },
            "charge": { "amount": CHARGE, "transaction": tx(&charge.transaction_hash) },
            "withdrawal": {
                "amount": WITHDRAWAL,
                "destination": withdrawal.destination,
                "transaction": tx(&withdrawal.transaction_hash),
            },
        }))
    }
}
