//! A seller's first integration against a hosted Fermah Pay gateway, step by
//! step: register a buyer, prepare a deposit, have the buyer sign it, submit
//! it, charge the balance and read it back. Each step prints what it sent,
//! what came back and the on-chain transaction, so the run doubles as a
//! reference for wiring the same calls into a product.
//!
//! The buyer's key signs here only because this is a test buyer; in a
//! product the buyer's wallet signs the authorization entry and the seller
//! never holds the key.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use clap::Parser;
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::stellar_xdr::{Limits, ReadXdr, SorobanAuthorizationEntry, WriteXdr};
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_proto::v1::buyer_service_client::BuyerServiceClient;
use fermah_pay_stellar_proto::v1::ledger_service_client::LedgerServiceClient;
use fermah_pay_stellar_proto::v1::{
    ChargeState, CreateBuyerRequest, CreateChargeRequest, DepositState, GetBalanceRequest,
    GetChargeRequest, GetDepositRequest, PrepareDepositRequest, SubmitDepositRequest,
};
use time::OffsetDateTime;
use tonic::Request;
use tonic::transport::{Channel, ClientTlsConfig};

#[derive(Parser)]
#[command(
    name = "fermah-pay-stellar-try",
    version,
    about = "Run deposit, charge and balance against a Fermah Pay gateway, step by step"
)]
struct Cli {
    /// The gateway's gRPC endpoint, e.g. `https://pay-sandbox.fermah.xyz`.
    #[arg(long, env = "FERMAH_PAY_ENDPOINT")]
    endpoint: String,
    /// The seller deployment's API key.
    #[arg(long, env = "FERMAH_PAY_API_KEY", hide_env_values = true)]
    api_key: String,
    /// `S...` seed of the test buyer's wallet, which holds testnet USDC.
    #[arg(long, env = "FERMAH_PAY_BUYER_SECRET", hide_env_values = true)]
    buyer_secret: String,
    /// The deployment's network.
    #[arg(long, default_value = "stellar:testnet")]
    network: Network,
    /// Deposit, in USDC base units (1 USDC = 10,000,000).
    #[arg(long, default_value_t = 1_000_000)]
    deposit: i64,
    /// The daily spending limit the buyer signs with the deposit.
    #[arg(long, default_value_t = 500_000)]
    daily_limit: i64,
    /// Charge, in USDC base units.
    #[arg(long, default_value_t = 100_000)]
    charge: i64,
    /// Longest wait for a transaction to settle, in seconds.
    #[arg(long, default_value_t = 180)]
    timeout_secs: u64,
}

fn authed<T>(message: T, token: &str) -> anyhow::Result<Request<T>> {
    let mut request = Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse()?);
    Ok(request)
}

fn usdc(base_units: i64) -> String {
    format!("{}.{:07} USDC", base_units / 10_000_000, (base_units % 10_000_000).abs())
}

fn explorer(network: Network, hash: &str) -> String {
    match network {
        Network::Pubnet => format!("https://stellar.expert/explorer/public/tx/{hash}"),
        Network::Testnet => format!("https://stellar.expert/explorer/testnet/tx/{hash}"),
        Network::Local => hash.to_owned(),
    }
}

fn step(number: u32, title: &str) {
    println!("\n[{number}] {title}");
}

/// Reads `read` until `done` holds, backing off from one to five seconds,
/// for at most `limit`.
async fn wait_for<T, F, Fut>(
    what: &str,
    limit: Duration,
    mut read: F,
    done: impl Fn(&T) -> bool,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    let mut pause = Duration::from_secs(1);
    loop {
        let value = read().await?;
        if done(&value) {
            return Ok(value);
        }
        if tokio::time::Instant::now() + pause > deadline {
            bail!("{what} did not settle within {limit:?}");
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(5));
    }
}

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let limit = Duration::from_secs(cli.timeout_secs);
    let buyer = SecretKey::from_strkey(cli.buyer_secret.trim())
        .map_err(|e| anyhow::anyhow!("the buyer secret is not an S... seed: {e}"))?;
    let token = cli.api_key.as_str();

    let mut endpoint = Channel::from_shared(cli.endpoint.clone())?;
    if cli.endpoint.starts_with("https://") {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_webpki_roots())?;
    }
    let channel =
        endpoint.connect().await.with_context(|| format!("connecting to {}", cli.endpoint))?;
    let mut buyers = BuyerServiceClient::new(channel.clone());
    let mut ledger = LedgerServiceClient::new(channel);

    println!("Fermah Pay against {} as buyer {}", cli.endpoint, buyer.address());

    step(1, "Register the buyer (CreateBuyer)");
    // The same reference and wallet return the same buyer, so the run can be
    // repeated.
    let external_ref = format!("try-{}", buyer.address());
    let created = buyers
        .create_buyer(authed(
            CreateBuyerRequest {
                external_ref: external_ref.clone(),
                wallet_address: buyer.address().to_string(),
            },
            token,
        )?)
        .await?
        .into_inner();
    let buyer_id = created.buyer.context("no buyer returned")?.buyer_id;
    println!(
        "    buyer_id {buyer_id} ({})",
        if created.created { "new" } else { "already registered" }
    );

    let run = OffsetDateTime::now_utc().unix_timestamp();
    step(2, "Prepare a deposit (PrepareDeposit)");
    let prepared = ledger
        .prepare_deposit(authed(
            PrepareDepositRequest {
                buyer_id: buyer_id.clone(),
                amount: cli.deposit,
                idempotency_key: format!("try-deposit-{run}"),
                daily_limit: Some(cli.daily_limit),
            },
            token,
        )?)
        .await?
        .into_inner()
        .deposit
        .context("no deposit returned")?;
    println!(
        "    {} with a daily spending limit of {}; the gateway returns an authorization entry \
         for the buyer's wallet to sign, valid until ledger {}",
        usdc(cli.deposit),
        usdc(cli.daily_limit),
        prepared.expiration_ledger
    );

    step(3, "The buyer signs the authorization entry (in the wallet)");
    let entry = SorobanAuthorizationEntry::from_xdr_base64(
        &prepared.authorization_entry_xdr,
        Limits::none(),
    )
    .context("the authorization entry does not decode")?;
    let signed =
        sign_entry(&entry, network_id(cli.network), &[&buyer])?.to_xdr_base64(Limits::none())?;
    println!("    signed by {}; the buyer needs no XLM and pays no fee", buyer.address());

    step(4, "Submit the signed entry (SubmitDeposit) and wait for it on-chain");
    ledger
        .submit_deposit(authed(
            SubmitDepositRequest {
                deposit_id: prepared.deposit_id.clone(),
                signed_authorization_entry_xdr: signed,
            },
            token,
        )?)
        .await?;
    let deposit = wait_for(
        "the deposit",
        limit,
        || {
            let mut ledger = ledger.clone();
            let request = GetDepositRequest { deposit_id: prepared.deposit_id.clone() };
            async move {
                ledger
                    .get_deposit(authed(request, token)?)
                    .await?
                    .into_inner()
                    .deposit
                    .context("no deposit returned")
            }
        },
        |d| !matches!(d.state(), DepositState::Signed | DepositState::Submitted),
    )
    .await?;
    ensure!(deposit.state() == DepositState::Confirmed, "the deposit ended {:?}", deposit.state());
    println!("    confirmed: {}", explorer(cli.network, &deposit.transaction_hash));

    // The gateway admits charges against the limit once it has read it from
    // the contract.
    wait_for(
        "the daily limit",
        limit,
        || {
            let mut ledger = ledger.clone();
            let request = GetBalanceRequest { buyer_id: buyer_id.clone() };
            async move { Ok(ledger.get_balance(authed(request, token)?).await?.into_inner()) }
        },
        |b| b.daily_limit > 0,
    )
    .await?;

    step(5, "Charge the balance (CreateCharge) and wait for it to settle");
    let charge = ledger
        .create_charge(authed(
            CreateChargeRequest {
                buyer_id: buyer_id.clone(),
                amount: cli.charge,
                idempotency_key: format!("try-charge-{run}"),
            },
            token,
        )?)
        .await?
        .into_inner()
        .charge
        .context("no charge returned")?;
    println!(
        "    {} admitted at once (state {:?}); the worker settles it on-chain in a batch",
        usdc(cli.charge),
        charge.state()
    );
    let charge = wait_for(
        "the charge",
        limit,
        || {
            let mut ledger = ledger.clone();
            let request = GetChargeRequest { charge_id: charge.charge_id.clone() };
            async move {
                ledger
                    .get_charge(authed(request, token)?)
                    .await?
                    .into_inner()
                    .charge
                    .context("no charge returned")
            }
        },
        |c| !matches!(c.state(), ChargeState::Admitted | ChargeState::Submitted),
    )
    .await?;
    ensure!(charge.state() == ChargeState::Charged, "the charge ended {:?}", charge.state());
    println!("    charged: {}", explorer(cli.network, &charge.transaction_hash));

    step(6, "Read the balance (GetBalance)");
    let balance =
        ledger.get_balance(authed(GetBalanceRequest { buyer_id }, token)?).await?.into_inner();
    println!(
        "    available {}, daily limit {}, pending charges {}",
        usdc(balance.available),
        usdc(balance.daily_limit),
        usdc(balance.pending_charges)
    );
    println!(
        "\nDone. Every call above is in the API reference; nothing else is needed to integrate."
    );
    Ok(())
}
