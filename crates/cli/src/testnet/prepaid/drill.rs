//! Monitoring drills: each provokes on testnet the condition one threat
//! leaves behind, against the development stack running with the
//! `deploy/dev/drills.yaml` settings and its monitoring profile, then waits
//! for the alert that condition must raise to be active in Alertmanager, and
//! records what it did and the alert.
//!
//! No drill moves USDC out of the treasury or changes the contract's own
//! state: the shared testnet contract stays as the workflows expect it.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{ChargeRequest, DepositIntent, instance_wasm};
use fermah_pay_stellar_chain::rpc::{hex_lower, http_client};
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Limits, ReadXdr, SorobanAuthorizationEntry, WriteXdr,
};
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, CreateRecurringChargeRequest, DepositState, GetDepositRequest,
    GetMandateRequest, GetRecurringChargeRequest, MandateState, PrepareDepositRequest,
    PrepareMandateRequest, PrepareWithdrawalRequest, RecurringChargeState, SubmitDepositRequest,
    SubmitMandateRequest,
};
use serde_json::{Value, json};
use tonic::transport::Channel;

use super::e2e::{FundedBuyer, authed, until};
use super::{CHARGE_VALIDITY_LEDGERS, Context, FEE_SOURCE, SPONSOR, receipt_json, unix_now};
use crate::testnet::evidence::{self, tx_url};

/// 0.1 USDC: the testnet contract's minimum deposit.
const DEPOSIT: i64 = 1_000_000;
/// 0.01 USDC.
const SMALL: i64 = 100_000;

/// Where the development stack listens, and how long to wait for an alert.
pub struct Stack {
    pub gateway: String,
    pub api_key: zeroize::Zeroizing<String>,
    pub alertmanager: String,
    pub timeout: Duration,
}

/// One threat the drills cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Vector {
    /// The operator key charges a buyer outside the gateway.
    OperatorKey,
    /// A buyer key withdraws to an account other than the buyer's wallet.
    BuyerKey,
    /// A buyer lowers the contract's USDC approval outside the gateway.
    Allowance,
    /// The contract runs other code than the operator expects.
    AdminCode,
    /// A flood of deposits reaches the deployment's daily quota.
    Dust,
    /// Every drill above, in an order that keeps the quota for the last.
    All,
}

impl Vector {
    const fn name(self) -> &'static str {
        match self {
            Self::OperatorKey => "operator-key",
            Self::BuyerKey => "buyer-key",
            Self::Allowance => "allowance",
            Self::AdminCode => "admin-code",
            Self::Dust => "dust",
            Self::All => "all",
        }
    }
}

impl Context {
    pub async fn drill(&self, stack: &Stack, vector: Vector) -> anyhow::Result<()> {
        let vectors = match vector {
            Vector::All => vec![
                Vector::BuyerKey,
                Vector::Allowance,
                Vector::OperatorKey,
                Vector::AdminCode,
                Vector::Dust,
            ],
            one => vec![one],
        };
        for vector in vectors {
            // Control: the alert this drill expects is not already active, so
            // the drill is what raises it. The admin-code alert is raised by
            // the stack's settings from its start.
            if vector != Vector::AdminCode {
                let (alert, labels) = expected(vector);
                if let Some(active) = active_alert(stack, alert, labels).await? {
                    bail!(
                        "{alert} {labels:?} is already active (since {}); wait for it to resolve before this drill",
                        active["startsAt"]
                    );
                }
            }
            let started = Instant::now();
            let (alert, labels) = expected(vector);
            let (threat, action) = match vector {
                Vector::OperatorKey => self.drill_operator_key(stack).await?,
                Vector::BuyerKey => self.drill_buyer_key(stack).await?,
                Vector::Allowance => self.drill_allowance(stack).await?,
                Vector::AdminCode => self.drill_admin_code().await?,
                Vector::Dust => self.drill_dust(stack).await?,
                Vector::All => unreachable!("expanded above"),
            };
            let fired = wait_for_alert(stack, alert, labels).await?;
            let record = json!({
                "criterion": "monitoring-drill",
                "vector": vector.name(),
                "threat": threat,
                "expected": format!("the alert {alert} becomes active in Alertmanager"),
                "action": action,
                "alert": {
                    "name": alert,
                    "labels": fired["labels"],
                    "annotations": fired["annotations"],
                    "starts_at": fired["startsAt"],
                    "state": fired["status"]["state"],
                },
                "observed_after_secs": started.elapsed().as_secs(),
            });
            evidence::write(&self.evidence_dir, &format!("drill-{}", vector.name()), record)?;
            println!("{}: {alert} active after {} s", vector.name(), started.elapsed().as_secs());
        }
        Ok(())
    }

    async fn clients(
        &self,
        stack: &Stack,
    ) -> anyhow::Result<(BuyerServiceClient<Channel>, LedgerServiceClient<Channel>)> {
        let channel = Channel::from_shared(stack.gateway.clone())?.connect().await?;
        Ok((BuyerServiceClient::new(channel.clone()), LedgerServiceClient::new(channel)))
    }

    /// A new wallet holding `usdc` and no XLM, registered as a buyer.
    async fn registered_buyer(
        &self,
        stack: &Stack,
        usdc: i64,
    ) -> anyhow::Result<(SecretKey, String)> {
        let FundedBuyer { key, .. } = self.funded_buyer(usdc).await?;
        let (mut buyers, _) = self.clients(stack).await?;
        let id = buyers
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("drill-{}", unix_now()),
                    wallet_address: key.address().to_string(),
                },
                &stack.api_key,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?
            .buyer_id;
        Ok((key, id))
    }

    async fn drill_operator_key(&self, _stack: &Stack) -> Alerted {
        let (_, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SPONSOR)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        // A buyer with credit the gateway knows nothing of: deposited straight
        // to the contract.
        let FundedBuyer { key: buyer, .. } = self.funded_buyer(DEPOSIT).await?;
        let mut deposit_id = [0_u8; 32];
        getrandom::fill(&mut deposit_id)?;
        let intent = DepositIntent {
            owner: buyer.address().into(),
            amount: i128::from(DEPOSIT),
            deposit_id,
            cap: None,
        };
        let function = HostFunction::InvokeContract(pinned.deposit_call(&intent));
        let auth = self
            .authorize(&submitter, &function, &[(&buyer, pinned.deposit_authorization(&intent))])
            .await?;
        let deposited = submitter.submit(function, auth).await?;
        // The operator's key charges it, as someone holding it would.
        let operator = self.profile.key("operator")?;
        let mut charge_id = [0_u8; 32];
        getrandom::fill(&mut charge_id)?;
        let charges = [ChargeRequest {
            owner: buyer.address().into(),
            charge_id,
            amount: i128::from(SMALL),
            last_ledger: self.rpc.get_latest_ledger().await? + CHARGE_VALIDITY_LEDGERS,
            day: 0,
        }];
        let function = HostFunction::InvokeContract(pinned.charge_batch_call(&charges));
        let auth = self
            .authorize(
                &submitter,
                &function,
                &[(&operator, pinned.charge_batch_authorization(&charges))],
            )
            .await?;
        let charged = submitter.submit(function, auth).await?;
        Ok((
            "compromised operator key",
            json!({
                "description": "a buyer deposited straight to the contract, and the operator key charged it outside the gateway",
                "deposit": receipt_json(&deposited),
                "charge": receipt_json(&charged),
            }),
        ))
    }

    async fn drill_buyer_key(&self, stack: &Stack) -> Alerted {
        let (buyer, buyer_id) = self.registered_buyer(stack, DEPOSIT).await?;
        let (_, mut ledger) = self.clients(stack).await?;
        let token: &str = &stack.api_key;
        // Credit through the API, so a withdrawal can be prepared.
        let prepared = ledger
            .prepare_deposit(authed(
                PrepareDepositRequest {
                    buyer_id: buyer_id.clone(),
                    amount: DEPOSIT,
                    idempotency_key: format!("drill-deposit-{}", unix_now()),
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
        // Someone holding the buyer's key asks for the credit to go elsewhere.
        // Nothing is signed, so nothing is held and nothing can leave.
        let elsewhere = SecretKey::generate()?.address();
        let withdrawal = ledger
            .prepare_withdrawal(authed(
                PrepareWithdrawalRequest {
                    buyer_id,
                    amount: SMALL,
                    destination: elsewhere.to_string(),
                    idempotency_key: format!("drill-withdrawal-{}", unix_now()),
                },
                token,
            )?)
            .await
            .context("the gateway refused a withdrawal to another account; start the stack with deploy/dev/drills.yaml")?
            .into_inner()
            .withdrawal
            .context("no withdrawal returned")?;
        Ok((
            "compromised buyer key",
            json!({
                "description": "a withdrawal of the buyer's credit to an account other than the buyer's wallet was prepared (never signed, so nothing was held or moved)",
                "deposit_transaction": deposit.transaction_hash,
                "deposit_explorer_url": tx_url(&deposit.transaction_hash),
                "withdrawal_id": withdrawal.withdrawal_id,
                "destination": withdrawal.destination,
            }),
        ))
    }

    async fn drill_allowance(&self, stack: &Stack) -> Alerted {
        let (_, pinned) = self.deployment()?;
        let (buyer, buyer_id) = self.registered_buyer(stack, DEPOSIT).await?;
        let (_, mut ledger) = self.clients(stack).await?;
        let token: &str = &stack.api_key;
        let prepared = ledger
            .prepare_mandate(authed(
                PrepareMandateRequest {
                    buyer_id,
                    amount: SMALL,
                    period_secs: 86_400,
                    cycles: 1,
                    idempotency_key: format!("drill-mandate-{}", unix_now()),
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
            |m| !matches!(m.state(), MandateState::Signed | MandateState::Submitted),
        )
        .await?;
        ensure!(mandate.state() == MandateState::Active, "mandate ended {:?}", mandate.state());
        // The buyer sets the approval to zero in the USDC contract itself,
        // through a wallet other than this gateway.
        let (source, fee_source) = (self.profile.key(SPONSOR)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let (call, tree) = pinned.buyer_approval_authorization(&buyer.address(), 0, 0);
        let function = HostFunction::InvokeContract(call);
        let auth = self.authorize(&submitter, &function, &[(&buyer, tree)]).await?;
        let lowered = submitter.submit(function, auth).await?;
        // The seller charges the period as usual.
        let charge = ledger
            .create_recurring_charge(authed(
                CreateRecurringChargeRequest {
                    mandate_id: mandate.mandate_id.clone(),
                    amount: SMALL,
                    idempotency_key: format!("drill-recurring-{}", unix_now()),
                },
                token,
            )?)
            .await?
            .into_inner()
            .recurring_charge
            .context("no recurring charge returned")?;
        let settled = until(
            "recurring charge",
            || {
                let mut ledger = ledger.clone();
                let request = GetRecurringChargeRequest {
                    recurring_charge_id: charge.recurring_charge_id.clone(),
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
        .await?;
        ensure!(settled.outcome == "allowance_short", "the charge ended {}", settled.outcome);
        Ok((
            "SAC allowance manipulation",
            json!({
                "description": "after the buyer's mandate became active, the buyer set the ledger contract's USDC approval to zero outside the gateway; the next period's charge was refused on-chain",
                "mandate_transaction": mandate.transaction_hash,
                "approval_lowered": receipt_json(&lowered),
                "charge": {
                    "outcome": settled.outcome,
                    "transaction_hash": settled.transaction_hash,
                    "public_explorer_url": tx_url(&settled.transaction_hash),
                },
            }),
        ))
    }

    async fn drill_admin_code(&self) -> Alerted {
        let (recorded, pinned) = self.deployment()?;
        let read = self.rpc.get_ledger_entries(&[pinned.instance_key()]).await?;
        let running = read
            .first()
            .and_then(|record| instance_wasm(&record.data))
            .context("the contract instance cannot be read")?;
        Ok((
            "compromised admin keys replacing the contract's code",
            json!({
                "description": "the observer is told to expect other code than the contract runs, as after an upgrade nobody planned (deploy/dev/drills.yaml); the contract itself is not changed",
                "contract": recorded.contract,
                "running_wasm": hex_lower(&running),
                "expected_wasm": "0".repeat(64),
            }),
        ))
    }

    async fn drill_dust(&self, stack: &Stack) -> Alerted {
        let (mut buyers, mut ledger) = self.clients(stack).await?;
        let token: &str = &stack.api_key;
        let buyer_id = buyers
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("drill-dust-{}", unix_now()),
                    wallet_address: SecretKey::generate()?.address().to_string(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?
            .buyer_id;
        // Deposits prepared until the deployment's quota refuses one; none is
        // signed, so none costs a fee.
        let mut prepared = 0;
        let refusal = loop {
            let reply = ledger
                .prepare_deposit(authed(
                    PrepareDepositRequest {
                        buyer_id: buyer_id.clone(),
                        amount: DEPOSIT,
                        idempotency_key: format!("drill-dust-{}-{prepared}", unix_now()),
                    },
                    token,
                )?)
                .await;
            match reply {
                Ok(_) => prepared += 1,
                Err(status) => break status,
            }
            if prepared >= 10 {
                bail!(
                    "10 deposits were prepared without reaching the deployment's quota; start the stack with deploy/dev/drills.yaml"
                );
            }
        };
        ensure!(
            refusal.message() == "deployment_deposit_quota_exceeded",
            "refused with {} rather than the deployment's quota",
            refusal.message()
        );
        Ok((
            "denial of service through dust deposits",
            json!({
                "description": "deposits were prepared for the deployment until its daily quota refused one; none was signed or sent",
                "prepared_before_refusal": prepared,
                "refusal": refusal.message(),
            }),
        ))
    }
}

/// The threat a drill stands for, and what it did.
type Alerted = anyhow::Result<(&'static str, Value)>;

/// The alert a drill must raise and labels it must carry.
const fn expected(vector: Vector) -> (&'static str, &'static [(&'static str, &'static str)]) {
    match vector {
        Vector::OperatorKey => ("PayStellarCriticalFinding", &[("kind", "unknown_charge")]),
        Vector::BuyerKey => ("PayStellarWithdrawalsToOtherAccounts", &[]),
        Vector::Allowance => {
            ("PayStellarMandateChangedOutsideGateway", &[("result", "allowance_short")])
        }
        // A standing condition, alerted on for as long as it lasts: the
        // drill finds it active whenever it runs.
        Vector::AdminCode => ("PayStellarContractCodeUnexpected", &[]),
        Vector::Dust => {
            ("PayStellarDeploymentQuotaReached", &[("reason", "deployment_deposit_quota_exceeded")])
        }
        Vector::All => ("", &[]),
    }
}

/// The first active alert named `name` with `labels`, if any.
async fn active_alert(
    stack: &Stack,
    name: &str,
    labels: &[(&str, &str)],
) -> anyhow::Result<Option<Value>> {
    let mut filters = vec![("filter".to_owned(), format!("alertname=\"{name}\""))];
    filters.extend(labels.iter().map(|(k, v)| ("filter".to_owned(), format!("{k}=\"{v}\""))));
    filters.push(("active".to_owned(), "true".to_owned()));
    let alerts: Value = http_client(Duration::from_secs(30))?
        .get(format!("{}/api/v2/alerts", stack.alertmanager))
        .query(&filters)
        .send()
        .await
        .context("reading Alertmanager")?
        .error_for_status()?
        .json()
        .await?;
    Ok(alerts.as_array().and_then(|a| a.first()).cloned())
}

/// Polls Alertmanager until an active alert named `name` with `labels` is
/// there, or the drill's timeout passes.
async fn wait_for_alert(
    stack: &Stack,
    name: &str,
    labels: &[(&str, &str)],
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + stack.timeout;
    loop {
        if let Some(alert) = active_alert(stack, name, labels).await? {
            return Ok(alert);
        }
        if Instant::now() >= deadline {
            bail!("{name} {labels:?} was not active in Alertmanager within {:?}", stack.timeout);
        }
        // Prometheus scrapes every 15 s and evaluates rules as often.
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
