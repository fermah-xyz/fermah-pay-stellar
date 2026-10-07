//! The x402 conformance harness against the deployment recorded in the
//! profile: the gateway and the worker run in this process, a new buyer with
//! no XLM deposits through the gRPC API, and the harness then calls the x402
//! interface over HTTP as a third-party facilitator would.

use anyhow::{Context as _, bail, ensure};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::signer::LocalSigner;
use fermah_pay_stellar_chain::stellar_xdr::{Limits, ReadXdr, SorobanAuthorizationEntry, WriteXdr};
use fermah_pay_stellar_chain::usdc;
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, DepositState, GetDepositRequest, PrepareDepositRequest,
    SubmitDepositRequest,
};
use tonic::transport::Channel;

use super::e2e::{FundedBuyer, Stack, authed, until};
use super::{CHANNEL, Context, unix_now};
use crate::testnet::evidence;
use crate::x402_conformance::{Target, run};

/// The buyer's deposit: 0.1 USDC, the contract's minimum on testnet, enough
/// for the few payments of 0.005 USDC the harness makes.
const DEPOSIT: i64 = 1_000_000;

impl Context {
    pub async fn x402_conformance(&self, database_url: &str) -> anyhow::Result<()> {
        let sources = vec![LocalSigner::arc(self.profile.key(CHANNEL)?)];
        let stack = self.stack(database_url, "x402", sources, Quotas::default()).await?;
        let outcome = self.conformance_flow(&stack).await;
        stack.stop().await;
        let record = outcome?;
        let passed = record["passed"] == true;
        let failed = record["failed_cases"].clone();
        evidence::write(&self.evidence_dir, "x402-conformance", record)?;
        if !passed {
            bail!("x402 conformance failed: {failed}");
        }
        Ok(())
    }

    async fn conformance_flow(&self, stack: &Stack) -> anyhow::Result<serde_json::Value> {
        let token: &str = &stack.token;
        let FundedBuyer { key: buyer, .. } = self.funded_buyer(DEPOSIT).await?;
        let channel = Channel::from_shared(stack.endpoint.clone())?.connect().await?;
        let buyer_id = BuyerServiceClient::new(channel.clone())
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("x402-{}", unix_now()),
                    wallet_address: buyer.address().to_string(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?
            .buyer_id;
        let mut ledger = LedgerServiceClient::new(channel);
        let prepared = ledger
            .prepare_deposit(authed(
                PrepareDepositRequest {
                    buyer_id,
                    amount: DEPOSIT,
                    idempotency_key: "deposit-1".to_owned(),
                    daily_limit: None,
                },
                token,
            )?)
            .await?
            .into_inner()
            .deposit
            .context("no deposit returned")?;
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &prepared.authorization_entry_xdr,
            Limits::none(),
        )?;
        let signed = sign_entry(&entry, network_id(self.network), &[&buyer])?;
        ledger
            .submit_deposit(authed(
                SubmitDepositRequest {
                    deposit_id: prepared.deposit_id.clone(),
                    signed_authorization_entry_xdr: signed.to_xdr_base64(Limits::none())?,
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

        let target = Target {
            endpoint: stack.x402_endpoint.clone(),
            api_key: zeroize::Zeroizing::new(token.to_owned()),
            network: self.network,
            pay_to: stack.contract.clone(),
            asset: usdc::contract_strkey(stack.pinned.usdc),
            settlement_timeout: std::time::Duration::from_secs(300),
        };
        let mut record = run(&target, &buyer, &self.rpc).await?;
        // The interface ran on this machine for the run; the address it
        // listened on says nothing about the deployment.
        record["endpoint"] = serde_json::json!("in-process gateway");
        record["deposit"] = serde_json::json!({
            "amount": DEPOSIT,
            "transaction_hash": deposit.transaction_hash,
            "ledger": deposit.ledger,
        });
        Ok(record)
    }
}
