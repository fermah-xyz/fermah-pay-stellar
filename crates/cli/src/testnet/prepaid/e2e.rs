//! One seller's view end to end, on testnet, through the gateway API: a new
//! buyer with no XLM deposits Circle USDC with one wallet signature, is
//! charged three times, a retried charge is not charged again, and the
//! gateway's balance matches the contract's.
//!
//! The gateway and the settlement worker run inside this process, each on a
//! database pool under its production role, against the database at
//! `database_url`, which the run migrates. Nothing is mocked: every
//! transaction goes to testnet.

use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use base64::Engine as _;
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::onboarding::onboard_buyers;
use fermah_pay_stellar_chain::payments::payments_transaction;
use fermah_pay_stellar_chain::prepaid::{ChargeRequest, MAX_BATCH, account_balance, charge_record};
use fermah_pay_stellar_chain::rpc::{FeePercentile, hex_lower, http_client};
use fermah_pay_stellar_chain::sep53;
use fermah_pay_stellar_chain::signer::LocalSigner;
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Limits, ReadXdr, SorobanAuthorizationEntry, WriteXdr,
};
use fermah_pay_stellar_chain::submission::submit_and_wait;
use fermah_pay_stellar_chain::{transaction, usdc};
use fermah_pay_stellar_gateway::issuance::{self, LedgerBinding};
use fermah_pay_stellar_gateway::ledger::{LedgerApi, LedgerPolicy};
use fermah_pay_stellar_gateway::server::{ServerLimits, serve, serve_x402};
use fermah_pay_stellar_gateway::store::Store;
use fermah_pay_stellar_gateway::submission::{
    Engine, FeePolicy, Keys, Policy as EnginePolicy, SystemClock,
};
use fermah_pay_stellar_gateway::worker::{Settings, Worker};
use fermah_pay_stellar_gateway::x402::commitment_message;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    ChargeState, CreateBuyerRequest, CreateChargeRequest, DepositState, GetBalanceRequest,
    GetChargeRequest, GetDepositRequest, PrepareDepositRequest, SubmitDepositRequest,
};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use tokio::net::TcpListener;
use tonic::transport::Channel;
use tonic::{Code, Request};

use super::{CHANNEL, Context, FEE_SOURCE, NETWORK, SPONSOR, SUBMITTER, USDC_RESERVE, unix_now};
use crate::testnet::evidence::{self, tx_url};
use crate::testnet::ledger::balances;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../db/migrations");

/// 0.1 USDC in, 0.01 + 0.02 + 0.03 USDC charged.
const DEPOSIT: i64 = 1_000_000;
const CHARGES: [i64; 3] = [100_000, 200_000, 300_000];
/// Settled through the x402 interface, from a commitment the buyer signs.
const X402_AMOUNT: i64 = 50_000;
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(2);

async fn pool_as(connect: &PgConnectOptions, role: &'static str) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                conn.execute(role).await?;
                Ok(())
            })
        })
        .connect_with(connect.clone())
        .await
        .with_context(|| format!("connecting with `{role}`"))
}

fn authed<T>(message: T, token: &str) -> anyhow::Result<Request<T>> {
    let mut request = Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse()?);
    Ok(request)
}

/// Polls `read` until `done` holds or the settlement timeout passes.
async fn until<T, F, Fut>(what: &str, mut read: F, done: impl Fn(&T) -> bool) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + SETTLEMENT_TIMEOUT;
    loop {
        let value = read().await?;
        if done(&value) {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("{what} did not settle within {SETTLEMENT_TIMEOUT:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

impl Context {
    pub async fn end_to_end(&self, database_url: &str) -> anyhow::Result<()> {
        fermah_pay_stellar_gateway::startup::verify_rpc(&self.rpc, NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let operator = self.profile.key("operator")?;
        ensure!(
            operator.address().as_str() == recorded.operator,
            "the profile's operator key is not the deployment's operator"
        );

        // Database: migrate as the owner, then one pool per production role.
        let connect: PgConnectOptions = database_url.parse().context("parsing database URL")?;
        let owner = PgPoolOptions::new().max_connections(1).connect_with(connect.clone()).await?;
        MIGRATOR.run(&owner).await.context("applying migrations")?;
        let issuer = pool_as(&connect, "SET ROLE pay_stellar_issuer").await?;
        let api = pool_as(&connect, "SET ROLE pay_stellar_api").await?;
        let worker_pool = pool_as(&connect, "SET ROLE pay_stellar_worker").await?;

        let run = format!("e2e-{}", unix_now());
        let product = issuance::create_product(&issuer, &run).await?;
        let deployment =
            issuance::create_seller_deployment(&issuer, product, "testnet", NETWORK).await?;
        let key = issuance::issue_api_key(&issuer, deployment, "end-to-end").await?;
        let token = key.token;
        issuance::bind_ledger_contract(
            &issuer,
            deployment,
            &LedgerBinding {
                contract: recorded.contract.clone(),
                treasury: recorded.treasury.parse()?,
                operator: operator.address(),
            },
        )
        .await?;

        // Gateway and worker, in this process.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let store = Store::new(api);
        let ledger = LedgerApi::new(
            store.clone(),
            self.rpc.clone(),
            NETWORK,
            LedgerPolicy { authorization_validity_ledgers: 720, charge_validity_ledgers: 720 },
        );
        let mut gateway_stop = stopped.clone();
        let limits =
            ServerLimits { max_concurrent_requests: 16, request_timeout: Duration::from_secs(30) };
        let x402_listener = TcpListener::bind("127.0.0.1:0").await?;
        let x402_addr = x402_listener.local_addr()?;
        let mut x402_stop = stopped.clone();
        let x402 = tokio::spawn(serve_x402(x402_listener, ledger.clone(), limits, async move {
            let _ = x402_stop.wait_for(|stop| *stop).await;
        }));
        let gateway = tokio::spawn(serve(listener, store, NETWORK, ledger, limits, async move {
            let _ = gateway_stop.wait_for(|stop| *stop).await;
        }));
        let engine = Engine::new(
            worker_pool.clone(),
            self.rpc.clone(),
            SystemClock,
            NETWORK,
            // The zero-XLM channel account comes first, so it sends whenever
            // it is free.
            Keys::new(
                vec![
                    LocalSigner::arc(self.profile.key(CHANNEL)?),
                    LocalSigner::arc(self.profile.key(SUBMITTER)?),
                ],
                LocalSigner::arc(self.profile.key(FEE_SOURCE)?),
            )?,
            EnginePolicy {
                // The worker's own defaults: bid from the market, never
                // below the profile's fee nor above a hundred times it.
                fees: FeePolicy::new(
                    self.policy.inclusion_fee,
                    self.policy.inclusion_fee.saturating_mul(100),
                    FeePercentile::P90,
                )?,
                resource_fee_margin_percent: 20,
                validity: Duration::from_secs(60),
                max_clock_skew: Duration::from_secs(20),
            },
        );
        let worker = Worker::new(
            engine,
            worker_pool,
            LocalSigner::arc(operator),
            Settings {
                operator_authorization_ledgers: 24,
                retry_after: Duration::from_secs(10),
                max_batch: MAX_BATCH,
                // 10 XLM, the worker's default.
                fee_floor_stroops: 100_000_000,
                // The worker's defaults.
                ttl_threshold_ledgers: 120_960,
                ttl_extend_to_ledgers: 518_400,
                ttl_check_every: Duration::from_secs(600),
            },
        );
        let mut worker_stop = stopped.clone();
        let settlement = tokio::spawn(async move {
            worker
                .run(Duration::from_secs(1), Duration::from_secs(2), async move {
                    let _ = worker_stop.wait_for(|stop| *stop).await;
                })
                .await;
        });

        let outcome = self
            .seller_flow(
                &format!("http://{addr}"),
                &format!("http://{x402_addr}"),
                &token,
                &pinned,
                &recorded.contract,
                &owner,
            )
            .await;
        let _ = stop.send(true);
        let _ = settlement.await;
        let _ = gateway.await;
        let _ = x402.await;
        let record = outcome?;
        evidence::write(&self.evidence_dir, "api-end-to-end", record)
    }

    async fn seller_flow(
        &self,
        endpoint: &str,
        x402_endpoint: &str,
        token: &str,
        pinned: &fermah_pay_stellar_chain::prepaid::PrepaidDeployment,
        contract: &str,
        records: &PgPool,
    ) -> anyhow::Result<serde_json::Value> {
        let asset = usdc::circle_usdc(NETWORK);
        let channel = Channel::from_shared(endpoint.to_owned())?.connect().await?;
        let mut buyers = BuyerServiceClient::new(channel.clone());
        let mut ledger = LedgerServiceClient::new(channel);

        // A new buyer: created with 0 XLM by the sponsor, then sent the
        // deposit amount in USDC from the reserve.
        let buyer = SecretKey::generate()?;
        let sponsor = self.profile.key(SPONSOR)?;
        let reserve = self.profile.key(USDC_RESERVE)?;
        let onboarding = onboard_buyers(
            &self.rpc,
            NETWORK,
            &sponsor,
            &[&buyer],
            &asset,
            self.onboarding_policy(),
        )
        .await?;
        let sequence = self.sequence_of(&sponsor.address()).await?;
        let valid_until = unix_now() + self.policy.validity.as_secs();
        let tx = payments_transaction(
            &sponsor.address(),
            sequence + 1,
            &reserve.address(),
            &[(buyer.address(), DEPOSIT)],
            &asset,
            self.policy.inclusion_fee,
            valid_until,
        )?;
        let funding_hash = transaction::transaction_hash(&tx, NETWORK)?;
        let envelope = transaction::sign(tx, NETWORK, &[&sponsor, &reserve])?;
        submit_and_wait(&self.rpc, &envelope, funding_hash, valid_until, self.policy.poll_interval)
            .await?;
        let before = balances(&self.rpc, &buyer.address(), &asset).await?;
        ensure!(
            before.xlm_stroops == Some(0),
            "buyer holds XLM before depositing: {:?}",
            before.xlm_stroops
        );

        let created = buyers
            .create_buyer(authed(
                CreateBuyerRequest {
                    external_ref: format!("buyer-{}", unix_now()),
                    wallet_address: buyer.address().to_string(),
                },
                token,
            )?)
            .await?
            .into_inner()
            .buyer
            .context("no buyer returned")?;
        let buyer_id = created.buyer_id;

        // Deposit: the wallet signs the prepared entry; nothing else.
        let prepared = ledger
            .prepare_deposit(authed(
                PrepareDepositRequest {
                    buyer_id: buyer_id.clone(),
                    amount: DEPOSIT,
                    idempotency_key: "deposit-1".to_owned(),
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
        let signed = sign_entry(&entry, network_id(NETWORK), &[&buyer])?;
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

        // Three charges, then a retry of the first.
        let mut charge_ids = Vec::new();
        for (i, amount) in CHARGES.into_iter().enumerate() {
            let charge = ledger
                .create_charge(authed(
                    CreateChargeRequest {
                        buyer_id: buyer_id.clone(),
                        amount,
                        idempotency_key: format!("charge-{}", i + 1),
                    },
                    token,
                )?)
                .await?
                .into_inner();
            ensure!(charge.created, "charge {} was not created", i + 1);
            charge_ids.push(charge.charge.context("no charge")?.charge_id);
        }
        let retry = ledger
            .create_charge(authed(
                CreateChargeRequest {
                    buyer_id: buyer_id.clone(),
                    amount: CHARGES[0],
                    idempotency_key: "charge-1".to_owned(),
                },
                token,
            )?)
            .await?
            .into_inner();
        let retried = retry.charge.context("no charge")?;
        ensure!(
            !retry.created && retried.charge_id == charge_ids[0],
            "the retry created a new charge"
        );
        let conflict = ledger
            .create_charge(authed(
                CreateChargeRequest {
                    buyer_id: buyer_id.clone(),
                    amount: CHARGES[0] + 1,
                    idempotency_key: "charge-1".to_owned(),
                },
                token,
            )?)
            .await
            .err()
            .context("reusing a key with another amount was accepted")?;
        ensure!(
            (conflict.code(), conflict.message()) == (Code::AlreadyExists, "idempotency_conflict"),
            "unexpected refusal {conflict:?}"
        );

        // A facilitator's calls through the x402 interface, for a commitment
        // the buyer signs; settled on-chain with the next batch.
        let mut facilitator = Facilitator::new(x402_endpoint, token)?;
        let commitment =
            facilitator.pay(&buyer, contract, &usdc::contract_strkey(pinned.usdc)).await?;

        let mut charges = Vec::new();
        for id in &charge_ids {
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
            charges.push(charge);
        }

        let x402_settlement = until(
            "x402 settlement",
            || facilitator.settlement(&commitment),
            |s| {
                s["settlement"]["state"] == "charged"
                    && s["settlement"]["transactionHash"].is_string()
            },
        )
        .await?;
        facilitator.log_settlement(&commitment, &x402_settlement);
        let x402_charge_id = x402_settlement["settlement"]["contractChargeId"]
            .as_str()
            .context("settlement without a contract charge id")?
            .to_owned();

        let balance = ledger
            .get_balance(authed(GetBalanceRequest { buyer_id: buyer_id.clone() }, token)?)
            .await?
            .into_inner();
        let expected = DEPOSIT - CHARGES.iter().sum::<i64>() - X402_AMOUNT;
        ensure!(
            (balance.available, balance.pending_charges) == (expected, 0),
            "gateway balance {} pending {}, expected {expected}",
            balance.available,
            balance.pending_charges
        );
        let account = self
            .rpc
            .get_ledger_entries(&[pinned.account_key(&buyer.address())])
            .await?
            .first()
            .and_then(|record| account_balance(&record.data))
            .context("no contract account for the buyer")?;
        ensure!(account == i128::from(expected), "contract balance {account}, expected {expected}");
        // Each charge left its record on the contract, holding its outcome.
        let recorded_ids = charges
            .iter()
            .map(|charge| charge.contract_charge_id.clone())
            .chain(std::iter::once(x402_charge_id));
        for contract_charge_id in recorded_ids {
            let id = parse_charge_id(&contract_charge_id)?;
            let recorded = self
                .rpc
                .get_ledger_entries(&[pinned.charge_record_key(&buyer.address(), &id)])
                .await?
                .first()
                .and_then(|record| charge_record(&record.data));
            ensure!(
                recorded == Some(fermah_pay_stellar_chain::prepaid::Outcome::Charged),
                "charge {contract_charge_id} is recorded as {recorded:?}"
            );
        }
        // The contract's own replay protection, independent of the gateway:
        // the first charge's identifier is recorded, so the same charge sent
        // straight to the contract is refused. Only simulated; nothing is
        // submitted.
        let replay = ChargeRequest {
            owner: buyer.address(),
            charge_id: parse_charge_id(&charges[0].contract_charge_id)?,
            amount: i128::from(CHARGES[0]),
            last_ledger: charges[0].last_ledger,
        };
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let operator = self.profile.key("operator")?;
        let submitter = self.submitter(&source, &fee_source);
        let function = HostFunction::InvokeContract(pinned.charge_call(&replay));
        let tree = pinned.charge_authorization(&replay);
        let replay_refusal = match self.authorize(&submitter, &function, &[(&operator, tree)]).await
        {
            Ok(_) => bail!("the contract accepted a replay of the first charge"),
            Err(error) => format!("{error:#}"),
        };
        ensure!(
            replay_refusal.contains("Error(Contract, #110)"),
            "the replay was refused for another reason: {replay_refusal}"
        );
        let after_replay = self
            .rpc
            .get_ledger_entries(&[pinned.account_key(&buyer.address())])
            .await?
            .first()
            .and_then(|record| account_balance(&record.data));
        ensure!(after_replay == Some(account), "the refused replay changed the account");

        let after = balances(&self.rpc, &buyer.address(), &asset).await?;
        ensure!(
            after.xlm_stroops == Some(0),
            "buyer holds XLM after the run: {:?}",
            after.xlm_stroops
        );

        let mut batches: Vec<String> = charges.iter().map(|c| c.transaction_hash.clone()).collect();
        batches.sort();
        batches.dedup();

        // Which accounts sequenced the deposit and the batches: the channel
        // account, holding no XLM, sends while it is free.
        let hashes: Vec<String> =
            std::iter::once(deposit.transaction_hash.clone()).chain(batches.clone()).collect();
        let sources: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT source_address FROM pay_stellar.submissions
             WHERE encode(outer_hash, 'hex') = ANY($1)",
        )
        .bind(&hashes)
        .fetch_all(records)
        .await?;
        let channel_account = self.profile.key(CHANNEL)?.address();
        ensure!(
            sources.iter().any(|source| source == channel_account.as_str()),
            "the channel account {channel_account} sent none of {hashes:?}"
        );
        let channel_after = balances(&self.rpc, &channel_account, &asset).await?;
        ensure!(
            channel_after.xlm_stroops == Some(0),
            "the channel account holds XLM: {:?}",
            channel_after.xlm_stroops
        );
        Ok(json!({
            "criterion": "api-end-to-end",
            "expected": "through the gateway API: a new buyer with 0 XLM deposits Circle USDC with one signature, three charges settle on-chain, a retried charge returns the original, the contract refuses a replayed charge, and the gateway balance equals the contract balance",
            "contract": contract,
            "buyer": buyer.address().to_string(),
            "onboarding_transaction": hex_lower(&onboarding.transaction_hash),
            "funding_transaction": hex_lower(&funding_hash),
            "deposit": {
                "amount": DEPOSIT,
                "transaction_hash": deposit.transaction_hash,
                "ledger": deposit.ledger,
                "public_explorer_url": tx_url(&deposit.transaction_hash),
            },
            "charges": charges.iter().map(|c| json!({
                "amount": c.amount,
                "contract_charge_id": c.contract_charge_id,
                "last_ledger": c.last_ledger,
                "state": "charged",
                "transaction_hash": c.transaction_hash,
                "ledger": c.ledger,
            })).collect::<Vec<_>>(),
            "charge_transactions": batches.iter().map(|h| json!({ "hash": h, "public_explorer_url": tx_url(h) })).collect::<Vec<_>>(),
            "retried_charge": {
                "created": retry.created,
                "same_charge": retried.charge_id == charge_ids[0],
            },
            "conflicting_reuse": conflict.message(),
            "contract_replay": {
                "contract_charge_id": charges[0].contract_charge_id,
                "result": "refused before submission as DuplicateCharge (contract error 110)",
                "account_unchanged": true,
            },
            "x402": {
                "commitment": commitment,
                "amount": X402_AMOUNT,
                "transaction_hash": x402_settlement["settlement"]["transactionHash"],
                "exchange": facilitator.log,
            },
            "observed": {
                "gateway_available": balance.available,
                "gateway_pending": balance.pending_charges,
                "contract_balance": account.to_string(),
                "charge_records": "charged",
                "buyer_xlm_stroops_before": before.xlm_stroops,
                "buyer_xlm_stroops_after": after.xlm_stroops,
                "buyer_usdc_after": after.usdc,
                "sources": sources,
                "channel_account": channel_account.to_string(),
                "channel_xlm_stroops_after": channel_after.xlm_stroops,
            },
        }))
    }
}

/// Plays a facilitator against the x402 interface, logging every request
/// and response (the API key is never logged).
struct Facilitator {
    http: reqwest::Client,
    endpoint: String,
    token: String,
    log: Vec<Value>,
}

impl Facilitator {
    fn new(endpoint: &str, token: &str) -> anyhow::Result<Self> {
        Ok(Self {
            http: http_client(Duration::from_secs(30))?,
            endpoint: endpoint.to_owned(),
            token: token.to_owned(),
            log: Vec::new(),
        })
    }

    async fn call(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let url = format!("{}{path}", self.endpoint);
        let request = match body {
            Some(body) => self.http.post(&url).json(body),
            None => self.http.get(&url),
        };
        let response = request.bearer_auth(&self.token).send().await?;
        let status = response.status().as_u16();
        let reply: Value = response.json().await?;
        self.log.push(json!({
            "request": { "method": method, "path": path, "body": body },
            "response": { "status": status, "body": reply },
        }));
        Ok(reply)
    }

    /// Signs a commitment as the buyer, then verifies, settles, settles again
    /// and presents it with an altered amount. Returns the commitment.
    async fn pay(
        &mut self,
        buyer: &SecretKey,
        pay_to: &str,
        asset: &str,
    ) -> anyhow::Result<String> {
        let supported = self.call("GET", "/supported", None).await?;
        ensure!(
            supported["kinds"][0]["scheme"] == "batch-settlement"
                && supported["kinds"][0]["network"] == NETWORK.caip2(),
            "unexpected /supported {supported}"
        );
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        let commitment = hex_lower(&bytes);
        let valid_until = (unix_now() + 120).to_string();
        let amount = X402_AMOUNT.to_string();
        let payer = buyer.address();
        let message = commitment_message(
            NETWORK.caip2(),
            asset,
            pay_to,
            &amount,
            payer.as_str(),
            &commitment,
            &valid_until,
        );
        let signature = sep53::sign(buyer, message.as_bytes());
        let requirements = |amount: &str| {
            json!({
                "scheme": "batch-settlement",
                "network": NETWORK.caip2(),
                "amount": amount,
                "asset": asset,
                "payTo": pay_to,
                "maxTimeoutSeconds": 300,
            })
        };
        let request = |amount: &str| {
            json!({
                "x402Version": 2,
                "paymentPayload": {
                    "x402Version": 2,
                    "resource": { "url": "https://seller.example/api/answer" },
                    "accepted": requirements(amount),
                    "payload": {
                        "payer": payer.as_str(),
                        "commitment": commitment,
                        "validUntil": valid_until,
                        "signature": base64::engine::general_purpose::STANDARD.encode(signature),
                    },
                },
                "paymentRequirements": requirements(amount),
            })
        };
        let verified = self.call("POST", "/verify", Some(&request(&amount))).await?;
        ensure!(verified["isValid"] == true, "verify refused: {verified}");
        let settled = self.call("POST", "/settle", Some(&request(&amount))).await?;
        ensure!(
            settled["success"] == true && settled["transaction"] == commitment.as_str(),
            "settle refused: {settled}"
        );
        let again = self.call("POST", "/settle", Some(&request(&amount))).await?;
        ensure!(
            again["success"] == true && again["extensions"]["prepaidLedger"]["replayed"] == true,
            "a retried settlement was not answered with the same charge: {again}"
        );
        let altered = (X402_AMOUNT + 1).to_string();
        let forged = self.call("POST", "/verify", Some(&request(&altered))).await?;
        ensure!(
            forged["invalidReason"] == "invalid_batch_settlement_stellar_signature",
            "an altered amount was not refused: {forged}"
        );
        Ok(commitment)
    }

    async fn settlement(&self, commitment: &str) -> anyhow::Result<Value> {
        let url = format!("{}/settlements/{commitment}", self.endpoint);
        Ok(self.http.get(url).bearer_auth(&self.token).send().await?.json().await?)
    }

    fn log_settlement(&mut self, commitment: &str, reply: &Value) {
        self.log.push(json!({
            "request": { "method": "GET", "path": format!("/settlements/{commitment}") },
            "response": { "status": 200, "body": reply },
        }));
    }
}

fn parse_charge_id(hex: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| hex.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()))
        .collect::<Option<Vec<u8>>>()
        .context("charge identifier is not hex")?;
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow::anyhow!("charge identifier is not 32 bytes"))
}
