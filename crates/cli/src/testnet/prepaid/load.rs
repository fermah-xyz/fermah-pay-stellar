//! A load test on testnet, through the gateway API: many zero-XLM buyers
//! each deposit Circle USDC once, then charges are admitted concurrently and
//! settled in batches by several source accounts at once. Throughput,
//! settlement latency, batch sizes and fees are read from the gateway's own
//! records and the transactions' results.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, ensure};
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::onboarding::{MAX_BUYERS_PER_TRANSACTION, onboard_buyers};
use fermah_pay_stellar_chain::payments::payments_transaction;
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::signer::{LocalSigner, Signer};
use fermah_pay_stellar_chain::stellar_xdr::{Limits, ReadXdr, SorobanAuthorizationEntry, WriteXdr};
use fermah_pay_stellar_chain::submission::submit_and_wait;
use fermah_pay_stellar_chain::{transaction, usdc};
use fermah_pay_stellar_gateway::store::Quotas;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    CreateBuyerRequest, CreateChargeRequest, PrepareDepositRequest, SubmitDepositRequest,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tonic::transport::Channel;

use super::e2e::Stack;
use super::{Context, SPONSOR, SUBMITTER, USDC_RESERVE, unix_now};
use crate::testnet::evidence;

/// Each buyer deposits 0.1 USDC.
const DEPOSIT: i64 = 1_000_000;
/// Each charge is 0.0001 USDC.
const CHARGE: i64 = 1_000;
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(900);
const POLL: Duration = Duration::from_secs(2);

pub struct LoadShape {
    pub buyers: u32,
    pub charges_per_buyer: u32,
    /// Channel accounts the worker sends from, besides the submitter.
    pub channels: u32,
    /// Charge requests in flight at once.
    pub concurrency: usize,
}

fn authed<T>(message: T, token: &str) -> anyhow::Result<tonic::Request<T>> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse()?);
    Ok(request)
}

impl Context {
    pub async fn load_test(&self, database_url: &str, shape: &LoadShape) -> anyhow::Result<()> {
        ensure!(
            shape.buyers > 0 && shape.charges_per_buyer > 0 && shape.concurrency > 0,
            "the load needs buyers, charges and concurrency"
        );
        ensure!(
            i64::from(shape.charges_per_buyer) * CHARGE <= DEPOSIT,
            "each buyer's charges must fit its deposit"
        );
        let names: Vec<String> = (1..=shape.channels).map(|i| format!("channel-{i}")).collect();
        self.ensure_sponsored(&names).await?;
        let mut sources: Vec<Arc<dyn Signer>> = Vec::new();
        for name in &names {
            sources.push(LocalSigner::arc(self.profile.key(name)?));
        }
        sources.push(LocalSigner::arc(self.profile.key(SUBMITTER)?));
        let quotas = Quotas {
            buyers_per_deployment: shape.buyers.max(Quotas::default().buyers_per_deployment),
            ..Quotas::default()
        };
        let stack = self.stack(database_url, "load", sources, quotas).await?;
        let outcome = self.load(&stack, shape).await;
        stack.stop().await;
        let record = outcome?;
        evidence::write(&self.evidence_dir, "load-test", record)
    }

    /// Creates any of `names` missing on the ledger as zero-XLM accounts the
    /// sponsor pays for.
    async fn ensure_sponsored(&self, names: &[String]) -> anyhow::Result<()> {
        let sponsor = self.profile.key(SPONSOR)?;
        let mut missing = Vec::new();
        for name in names {
            let key = self.profile.key(name)?;
            if !self.account_exists(&key.address()).await? {
                missing.push(key);
            }
        }
        for chunk in missing.chunks(MAX_BUYERS_PER_TRANSACTION) {
            let refs: Vec<&SecretKey> = chunk.iter().collect();
            onboard_buyers(
                &self.rpc,
                self.network,
                &sponsor,
                &refs,
                &usdc::circle_usdc(self.network),
                self.onboarding_policy(),
            )
            .await?;
        }
        Ok(())
    }

    async fn load(&self, stack: &Stack, shape: &LoadShape) -> anyhow::Result<Value> {
        let asset = usdc::circle_usdc(self.network);
        let sponsor = self.profile.key(SPONSOR)?;
        let reserve = self.profile.key(USDC_RESERVE)?;
        let token: &str = &stack.token;

        // Buyers: zero XLM, sponsored, each sent its deposit from the reserve.
        let buyers: Vec<SecretKey> =
            (0..shape.buyers).map(|_| SecretKey::generate()).collect::<Result<_, _>>()?;
        for chunk in buyers.chunks(MAX_BUYERS_PER_TRANSACTION) {
            let refs: Vec<&SecretKey> = chunk.iter().collect();
            onboard_buyers(
                &self.rpc,
                self.network,
                &sponsor,
                &refs,
                &asset,
                self.onboarding_policy(),
            )
            .await?;
        }
        for chunk in buyers.chunks(100) {
            let sequence = self.sequence_of(&sponsor.address()).await?;
            let valid_until = unix_now() + self.policy.validity.as_secs();
            let recipients: Vec<_> = chunk.iter().map(|b| (b.address(), DEPOSIT)).collect();
            let tx = payments_transaction(
                &sponsor.address(),
                sequence + 1,
                &reserve.address(),
                &recipients,
                &asset,
                self.policy.inclusion_fee,
                valid_until,
            )?;
            let hash = transaction::transaction_hash(&tx, self.network)?;
            let envelope = transaction::sign(tx, self.network, &[&sponsor, &reserve])?;
            submit_and_wait(&self.rpc, &envelope, hash, valid_until, self.policy.poll_interval)
                .await?;
        }

        // Registration and one deposit each, through the API.
        let channel = Channel::from_shared(stack.endpoint.clone())?.connect().await?;
        let mut ids = Vec::new();
        for (i, buyer) in buyers.iter().enumerate() {
            let created = BuyerServiceClient::new(channel.clone())
                .create_buyer(authed(
                    CreateBuyerRequest {
                        external_ref: format!("load-{i}"),
                        wallet_address: buyer.address().to_string(),
                    },
                    token,
                )?)
                .await?
                .into_inner()
                .buyer
                .context("no buyer returned")?;
            let mut ledger = LedgerServiceClient::new(channel.clone());
            let prepared = ledger
                .prepare_deposit(authed(
                    PrepareDepositRequest {
                        buyer_id: created.buyer_id.clone(),
                        amount: DEPOSIT,
                        idempotency_key: format!("deposit-{i}"),
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
            let signed = sign_entry(&entry, network_id(self.network), &[buyer])?;
            ledger
                .submit_deposit(authed(
                    SubmitDepositRequest {
                        deposit_id: prepared.deposit_id,
                        signed_authorization_entry_xdr: signed.to_xdr_base64(Limits::none())?,
                    },
                    token,
                )?)
                .await?;
            ids.push(created.buyer_id);
        }
        let deposits_started = tokio::time::Instant::now();
        until_none(&stack.records, "deposits", DEPOSITS_OPEN).await?;
        let deposits_took = deposits_started.elapsed();
        let confirmed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pay_stellar.deposits WHERE state = 'confirmed'",
        )
        .fetch_one(&stack.records)
        .await?;
        ensure!(confirmed == i64::from(shape.buyers), "{confirmed} deposits confirmed");

        // Charges, admitted concurrently.
        let requests: Vec<(String, String)> = ids
            .iter()
            .flat_map(|id| {
                (0..shape.charges_per_buyer).map(move |n| (id.clone(), format!("{id}-{n}")))
            })
            .collect();
        let limit = Arc::new(tokio::sync::Semaphore::new(shape.concurrency));
        let admission_started = tokio::time::Instant::now();
        let mut admissions = Vec::new();
        for (buyer_id, key) in requests {
            let permit = Arc::clone(&limit).acquire_owned().await?;
            let mut ledger = LedgerServiceClient::new(channel.clone());
            let request = authed(
                CreateChargeRequest { buyer_id, amount: CHARGE, idempotency_key: key },
                token,
            )?;
            admissions.push(tokio::spawn(async move {
                let admitted = ledger.create_charge(request).await;
                drop(permit);
                admitted.map(|_| ())
            }));
        }
        let mut refused = 0_u32;
        for admission in admissions {
            if admission.await?.is_err() {
                refused += 1;
            }
        }
        let admission_took = admission_started.elapsed();
        until_none(&stack.records, "charges", CHARGES_OPEN).await?;
        let settled_took = admission_started.elapsed();

        let stats = charge_stats(&stack.records).await?;
        let total = i64::from(shape.buyers) * i64::from(shape.charges_per_buyer);
        ensure!(refused == 0, "{refused} charges were refused at admission");
        ensure!(stats.charged == total, "{} of {total} charges settled as charged", stats.charged);

        #[allow(clippy::cast_precision_loss)]
        let per_second = |count: i64, took: Duration| count as f64 / took.as_secs_f64();
        Ok(json!({
            "criterion": "load-test",
            "expected": "through the gateway API on testnet with Circle USDC: every admitted charge settles on-chain as charged, in batches sent from several source accounts at once",
            "contract": stack.contract,
            "shape": {
                "buyers": shape.buyers,
                "charges_per_buyer": shape.charges_per_buyer,
                "charge_amount": CHARGE,
                "channels": shape.channels,
                "concurrency": shape.concurrency,
            },
            "observed": {
                "deposits_confirmed": confirmed,
                "deposits_seconds": deposits_took.as_secs_f64(),
                "charges": total,
                "charged": stats.charged,
                "admission_seconds": admission_took.as_secs_f64(),
                "admitted_per_second": per_second(total, admission_took),
                "settled_seconds": settled_took.as_secs_f64(),
                "settled_per_second": per_second(total, settled_took),
                "settlement_latency_seconds": {
                    "p50": stats.p50, "p95": stats.p95, "max": stats.max,
                },
                "batches": stats.batches,
                "charges_per_batch": stats.per_batch,
                "sources": stats.sources,
                "fee_stroops_total": stats.fees,
                "fee_stroops_per_charge": stats.fees / total.max(1),
                "batch_transactions": stats.hashes.iter().map(|h| json!({
                    "hash": h,
                    "public_explorer_url": evidence::tx_url(h),
                })).collect::<Vec<_>>(),
            },
        }))
    }
}

const DEPOSITS_OPEN: &str = "SELECT count(*) FROM pay_stellar.deposits
     WHERE state IN ('awaiting_signature', 'signed', 'submitted')";
const CHARGES_OPEN: &str =
    "SELECT count(*) FROM pay_stellar.charges WHERE state IN ('admitted', 'submitted')";

/// Waits until `open` counts no rows, or the settlement timeout passes.
async fn until_none(records: &PgPool, what: &str, open: &'static str) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + SETTLEMENT_TIMEOUT;
    loop {
        let left: i64 = sqlx::query_scalar(open).fetch_one(records).await?;
        if left == 0 {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "{left} {what} still open after {SETTLEMENT_TIMEOUT:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

struct ChargeStats {
    charged: i64,
    p50: f64,
    p95: f64,
    max: f64,
    batches: i64,
    per_batch: f64,
    sources: i64,
    fees: i64,
    hashes: Vec<String>,
}

async fn charge_stats(records: &PgPool) -> anyhow::Result<ChargeStats> {
    let (charged, p50, p95, max): (i64, f64, f64, f64) = sqlx::query_as(
        "SELECT count(*),
                percentile_cont(0.5) WITHIN GROUP (ORDER BY latency),
                percentile_cont(0.95) WITHIN GROUP (ORDER BY latency),
                max(latency)
         FROM (SELECT EXTRACT(EPOCH FROM settled_at - created_at)::float8 AS latency
               FROM pay_stellar.charges WHERE state = 'charged') c",
    )
    .fetch_one(records)
    .await?;
    let (batches, per_batch, sources, fees): (i64, f64, i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(avg(n), 0)::float8, count(DISTINCT source_address),
                COALESCE(sum(fee_charged), 0)::bigint
         FROM (SELECT s.id, s.source_address, s.fee_charged, count(c.id) AS n
               FROM pay_stellar.submissions s
               JOIN pay_stellar.charges c ON c.submission_id = s.id
               WHERE s.kind = 'charge_batch' AND s.state = 'succeeded'
               GROUP BY s.id) b",
    )
    .fetch_one(records)
    .await?;
    let hashes: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT outer_hash FROM pay_stellar.submissions
         WHERE kind = 'charge_batch' AND state = 'succeeded' ORDER BY created_at",
    )
    .fetch_all(records)
    .await?;
    Ok(ChargeStats {
        charged,
        p50,
        p95,
        max,
        batches,
        per_batch,
        sources,
        fees,
        hashes: hashes.iter().map(|h| hex_lower(h)).collect(),
    })
}
