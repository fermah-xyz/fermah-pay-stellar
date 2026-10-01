//! Recurring charges end to end, through the gateway API: a new buyer with no
//! XLM signs one mandate for monthly-style charges, the seller charges two
//! periods without the buyer signing again, the contract refuses a period
//! before it starts and every charge after the mandate is over, and the
//! buyer revokes. Periods last two minutes so a run takes minutes; the
//! contract applies a month the same way, with another number of seconds.
//!
//! The gateway and the settlement worker run inside this process, as for
//! the prepaid run. The two charges the API would refuse are sent straight
//! to the contract under the operator's key, so the refusals are on-chain
//! transactions with their own hashes.

use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{
    PrepaidDeployment, RecurringChargeRequest, RecurringOutcome, allowance_of, recurring_outcomes,
    stored_mandate,
};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::signer::LocalSigner;
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Limits, ReadXdr, SorobanAuthorizationEntry, WriteXdr,
};
use fermah_pay_stellar_chain::usdc;
use fermah_pay_stellar_domain::AccountAddress;
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, CreateRecurringChargeRequest, GetMandateRequest, GetRecurringChargeRequest,
    GetRevocationRequest, Mandate, MandateState, PrepareMandateRequest, PrepareRevocationRequest,
    RecurringCharge, RecurringChargeState, RevocationState, SubmitMandateRequest,
    SubmitRevocationRequest,
};
use serde_json::json;
use tonic::Code;
use tonic::transport::Channel;

use super::e2e::{FundedBuyer, authed, until};
use super::{
    CHANNEL, CHARGE_VALIDITY_LEDGERS, Context, FEE_SOURCE, SUBMITTER, receipt_json, unix_now,
};
use crate::testnet::evidence::{self, tx_url};
use crate::testnet::ledger::balances;

/// A compressed period, so a run shows several within minutes.
const PERIOD: u64 = 120;
const CYCLES: u32 = 2;
/// 0.01 USDC a period.
const AMOUNT: i64 = 100_000;
/// What the buyer's wallet holds before the mandate: three periods' worth.
const WALLET: i64 = 3 * AMOUNT;
const POLL: Duration = Duration::from_secs(2);

impl Context {
    pub async fn recurring_end_to_end(&self, database_url: &str) -> anyhow::Result<()> {
        // The channel account sends for the worker; the submitter is kept
        // for the charges sent straight to the contract, so the two never
        // compete for a sequence number.
        let sources = vec![LocalSigner::arc(self.profile.key(CHANNEL)?)];
        let stack = self.stack(database_url, "recurring", sources, Quotas::default()).await?;
        let outcome = self.recurring_flow(&stack.endpoint, &stack.token, &stack.pinned).await;
        let contract = stack.contract.clone();
        stack.stop().await;
        let mut record = outcome?;
        record["contract"] = json!(contract);
        evidence::write(&self.evidence_dir, "recurring-end-to-end", record)
    }

    async fn recurring_flow(
        &self,
        endpoint: &str,
        token: &str,
        pinned: &PrepaidDeployment,
    ) -> anyhow::Result<serde_json::Value> {
        let asset = usdc::circle_usdc(self.network);
        let channel = Channel::from_shared(endpoint.to_owned())?.connect().await?;
        let mut ledger = LedgerServiceClient::new(channel.clone());
        let FundedBuyer { key: buyer, onboarding, funding } = self.funded_buyer(WALLET).await?;
        let wallet = buyer.address();
        let before = balances(&self.rpc, &wallet, &asset).await?;
        ensure!(before.xlm_stroops == Some(0), "buyer holds XLM: {:?}", before.xlm_stroops);
        let treasury = pinned.treasury.clone();
        let treasury_before = balances(&self.rpc, &treasury, &asset).await?.usdc.unwrap_or(0);
        let buyer_id = BuyerServiceClient::new(channel)
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("recurring-{}", unix_now()),
                    wallet_address: wallet.to_string(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?
            .buyer_id;

        // (a) The buyer authorizes the mandate with one signature.
        let prepared = ledger
            .prepare_mandate(authed(
                PrepareMandateRequest {
                    buyer_id: buyer_id.clone(),
                    amount: AMOUNT,
                    period_secs: PERIOD,
                    cycles: CYCLES,
                    idempotency_key: "mandate-1".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .mandate
            .context("no mandate returned")?;
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &prepared.authorization_entry_xdr,
            Limits::none(),
        )?;
        let signed = sign_entry(&entry, network_id(self.network), &[&buyer])?;
        ledger
            .submit_mandate(authed(
                SubmitMandateRequest {
                    mandate_id: prepared.mandate_id.clone(),
                    signed_authorization_entry_xdr: signed.to_xdr_base64(Limits::none())?,
                },
                token,
            )?)
            .await?;
        let mandate = until(
            "mandate",
            || {
                let mut ledger = ledger.clone();
                let request = GetMandateRequest { mandate_id: prepared.mandate_id.clone() };
                async move {
                    ledger
                        .get_mandate(authed(request, token)?)
                        .await?
                        .into_inner()
                        .mandate
                        .context("no mandate")
                }
            },
            |m: &Mandate| !matches!(m.state(), MandateState::Signed | MandateState::Submitted),
        )
        .await?;
        ensure!(mandate.state() == MandateState::Active, "mandate ended {:?}", mandate.state());
        let held = self
            .rpc
            .get_ledger_entries(&[pinned.mandate_key(&wallet), pinned.allowance_key(&wallet)])
            .await?;
        let terms = held
            .iter()
            .find_map(|record| stored_mandate(&record.data))
            .context("the contract holds no mandate for the buyer")?;
        let allowance = held
            .iter()
            .find_map(|record| allowance_of(&record.data))
            .context("the USDC contract holds no allowance for the ledger contract")?;
        ensure!(
            allowance == (i128::from(AMOUNT) * i128::from(CYCLES), mandate.live_until_ledger),
            "allowance {allowance:?}"
        );

        // (b) The first period's charge, as the seller asks for it.
        let first = self.charge_and_settle(&mut ledger, token, &mandate, "period-0").await?;
        ensure!(first.cycle == 0 && first.state() == RecurringChargeState::Charged, "{first:?}");
        // The next period has not started: the contract refuses it on-chain.
        let early = self.charge_directly(pinned, &wallet, &terms.mandate_id, 1, "early").await?;
        ensure!(early.0 == RecurringOutcome::NotDue, "an early charge was answered {:?}", early.0);

        // (c) The second period's charge, once ledger time reaches it.
        self.wait_for_ledger_time(terms.start + PERIOD).await?;
        let second = self.charge_and_settle(&mut ledger, token, &mandate, "period-1").await?;
        ensure!(second.cycle == 1 && second.state() == RecurringChargeState::Charged, "{second:?}");

        // (d) Every period is over: the API refuses, and so does the
        // contract for a charge sent straight to it.
        self.wait_for_ledger_time(terms.start + u64::from(CYCLES) * PERIOD).await?;
        let refused = ledger
            .create_recurring_charge(authed(
                CreateRecurringChargeRequest {
                    mandate_id: mandate.mandate_id.clone(),
                    amount: AMOUNT,
                    idempotency_key: "period-2".to_owned(),
                },
                token,
            )?)
            .await
            .err()
            .context("a charge after the last period was admitted")?;
        ensure!(
            (refused.code(), refused.message()) == (Code::FailedPrecondition, "mandate_ended"),
            "unexpected refusal {refused:?}"
        );
        let late = self.charge_directly(pinned, &wallet, &terms.mandate_id, 2, "late").await?;
        ensure!(
            late.0 == RecurringOutcome::MandateExpired,
            "a late charge was answered {:?}",
            late.0
        );
        let after_charges = balances(&self.rpc, &wallet, &asset).await?;
        let treasury_after = balances(&self.rpc, &treasury, &asset).await?.usdc.unwrap_or(0);
        ensure!(
            after_charges.usdc == Some(WALLET - 2 * AMOUNT),
            "wallet holds {:?} after two periods",
            after_charges.usdc
        );

        // The buyer revokes: the mandate ends and the allowance is zero.
        let revocation = ledger
            .prepare_revocation(authed(
                PrepareRevocationRequest {
                    buyer_id: buyer_id.clone(),
                    idempotency_key: "revocation-1".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .revocation
            .context("no revocation returned")?;
        let entry = SorobanAuthorizationEntry::from_xdr_base64(
            &revocation.authorization_entry_xdr,
            Limits::none(),
        )?;
        let signed = sign_entry(&entry, network_id(self.network), &[&buyer])?;
        ledger
            .submit_revocation(authed(
                SubmitRevocationRequest {
                    revocation_id: revocation.revocation_id.clone(),
                    signed_authorization_entry_xdr: signed.to_xdr_base64(Limits::none())?,
                },
                token,
            )?)
            .await?;
        let revoked = until(
            "revocation",
            || {
                let mut ledger = ledger.clone();
                let request =
                    GetRevocationRequest { revocation_id: revocation.revocation_id.clone() };
                async move {
                    ledger
                        .get_revocation(authed(request, token)?)
                        .await?
                        .into_inner()
                        .revocation
                        .context("no revocation")
                }
            },
            |r| !matches!(r.state(), RevocationState::Signed | RevocationState::Submitted),
        )
        .await?;
        ensure!(revoked.state() == RevocationState::Confirmed, "revocation {:?}", revoked.state());
        let after_revocation = self
            .rpc
            .get_ledger_entries(&[pinned.mandate_key(&wallet), pinned.allowance_key(&wallet)])
            .await?;
        ensure!(
            after_revocation.iter().all(|r| stored_mandate(&r.data).is_none()),
            "the contract still holds a mandate"
        );
        let allowance_after = after_revocation.iter().find_map(|r| allowance_of(&r.data));
        ensure!(
            allowance_after.is_none_or(|(amount, _)| amount == 0),
            "allowance left after revoking: {allowance_after:?}"
        );
        let after = balances(&self.rpc, &wallet, &asset).await?;
        ensure!(after.xlm_stroops == Some(0), "buyer holds XLM after: {:?}", after.xlm_stroops);

        let charge = |c: &RecurringCharge| {
            json!({
                "cycle": c.cycle,
                "amount": c.amount,
                "outcome": c.outcome,
                "transaction_hash": c.transaction_hash,
                "ledger": c.ledger,
                "public_explorer_url": tx_url(&c.transaction_hash),
            })
        };
        Ok(json!({
            "criterion": "recurring-end-to-end",
            "expected": "a buyer with 0 XLM authorizes a mandate with one signature; the seller charges the first and the second period without the buyer signing again; the contract refuses a charge before its period and a charge after the mandate's last period, on-chain; the buyer revokes and the allowance is zero",
            "period_compressed": {
                "period_secs": PERIOD,
                "note": "periods last two minutes so the run shows them within minutes; the contract measures a period in ledger time, so a monthly mandate is the same code with period_secs = 2592000",
            },
            "buyer": wallet.to_string(),
            "onboarding_transaction": hex_lower(&onboarding.transaction_hash),
            "funding_transaction": hex_lower(&funding),
            "mandate": {
                "amount_per_period": AMOUNT,
                "period_secs": PERIOD,
                "cycles": CYCLES,
                "live_until_ledger": mandate.live_until_ledger,
                "starts_at": terms.start,
                "transaction_hash": mandate.transaction_hash,
                "ledger": mandate.ledger,
                "public_explorer_url": tx_url(&mandate.transaction_hash),
                "allowance": { "amount": allowance.0.to_string(), "live_until_ledger": allowance.1 },
            },
            "first_period": charge(&first),
            "early_charge": early.1,
            "second_period": charge(&second),
            "api_refusal_after_last_period": refused.message(),
            "charge_after_last_period": late.1,
            "revocation": {
                "transaction_hash": revoked.transaction_hash,
                "ledger": revoked.ledger,
                "public_explorer_url": tx_url(&revoked.transaction_hash),
                "allowance_after": allowance_after.map(|(a, l)| json!({ "amount": a.to_string(), "live_until_ledger": l })),
            },
            "observed": {
                "buyer_xlm_stroops_before": before.xlm_stroops,
                "buyer_xlm_stroops_after": after.xlm_stroops,
                "buyer_usdc_before": before.usdc,
                "buyer_usdc_after": after.usdc,
                "treasury_usdc_gained": treasury_after - treasury_before,
            },
        }))
    }

    /// Asks the API to charge the mandate's current period and waits for the
    /// worker to settle it.
    async fn charge_and_settle(
        &self,
        ledger: &mut LedgerServiceClient<Channel>,
        token: &str,
        mandate: &Mandate,
        key: &str,
    ) -> anyhow::Result<RecurringCharge> {
        let created = ledger
            .create_recurring_charge(authed(
                CreateRecurringChargeRequest {
                    mandate_id: mandate.mandate_id.clone(),
                    amount: AMOUNT,
                    idempotency_key: key.to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .recurring_charge
            .context("no recurring charge returned")?;
        until(
            "recurring charge",
            || {
                let mut ledger = ledger.clone();
                let request = GetRecurringChargeRequest {
                    recurring_charge_id: created.recurring_charge_id.clone(),
                };
                async move {
                    ledger
                        .get_recurring_charge(authed(request, token)?)
                        .await?
                        .into_inner()
                        .recurring_charge
                        .context("no recurring charge")
                }
            },
            |c| {
                !matches!(
                    c.state(),
                    RecurringChargeState::Admitted | RecurringChargeState::Submitted
                )
            },
        )
        .await
    }

    /// Sends one recurring charge straight to the contract under the
    /// operator's key and returns the contract's answer and the receipt.
    async fn charge_directly(
        &self,
        pinned: &PrepaidDeployment,
        owner: &AccountAddress,
        mandate_id: &[u8; 32],
        cycle: u32,
        tag: &str,
    ) -> anyhow::Result<(RecurringOutcome, serde_json::Value)> {
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let operator = self.profile.key("operator")?;
        let submitter = self.submitter(&source, &fee_source);
        let mut charge_id = [0_u8; 32];
        getrandom::fill(&mut charge_id)?;
        let charges = [RecurringChargeRequest {
            owner: owner.clone(),
            charge_id,
            mandate_id: *mandate_id,
            cycle,
            amount: i128::from(AMOUNT),
            last_ledger: self.rpc.get_latest_ledger().await? + CHARGE_VALIDITY_LEDGERS,
        }];
        let function = HostFunction::InvokeContract(pinned.charge_recurring_batch_call(&charges));
        let auth = self
            .authorize(
                &submitter,
                &function,
                &[(&operator, pinned.charge_recurring_batch_authorization(&charges))],
            )
            .await?;
        let receipt = submitter.submit(function, auth).await?;
        let outcome = receipt
            .return_value
            .as_ref()
            .and_then(recurring_outcomes)
            .and_then(|outcomes| outcomes.first().copied())
            .context("the batch returned no outcome")?;
        let mut record = receipt_json(&receipt);
        record["attempt"] = json!(tag);
        record["cycle"] = json!(cycle);
        record["outcome"] = json!(outcome.token());
        record["charge_id"] = json!(hex_lower(&charge_id));
        Ok((outcome, record))
    }

    /// Waits until the network's latest ledger closed at or after `at`.
    async fn wait_for_ledger_time(&self, at: u64) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(PERIOD * 3);
        loop {
            let info = self.rpc.get_latest_ledger_info().await?;
            if u64::try_from(info.close_time).unwrap_or(0) >= at {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("ledger time did not reach {at}");
            }
            tokio::time::sleep(POLL).await;
        }
    }
}
