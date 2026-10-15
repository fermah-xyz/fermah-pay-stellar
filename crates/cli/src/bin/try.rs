//! A seller's first integration against a hosted Fermah Pay gateway, step by
//! step: register a buyer, prepare a deposit, have the buyer sign it, submit
//! it, charge the balance and read it back. Each step prints what it sent,
//! what came back and the on-chain transaction, so the run doubles as a
//! reference for wiring the same calls into a product.
//!
//! The buyer's key signs here only because this is a test buyer; in a
//! product the buyer's wallet signs the authorization entry and the seller
//! never holds the key. Like a wallet, it signs only after checking that the
//! entry is the deposit it asked for: the amount, the limit, and Circle's
//! USDC moving from the buyer into the vault the entry names.
//!
//! It runs on testnet only: each run leaves its deposit in the vault, which
//! is harmless with testnet USDC and would not be with real funds. The API
//! key and the buyer's secret are read from the environment only, never from
//! arguments, which other users of the machine can list.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use clap::Parser;
use fermah_pay_stellar_chain::authorization::sign_entry;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{Custody, DepositIntent, PrepaidDeployment};
use fermah_pay_stellar_chain::stellar_xdr::{
    ContractId, Hash, Limits, ReadXdr, ScAddress, ScVal, SorobanAuthorizationEntry,
    SorobanAuthorizedFunction, WriteXdr,
};
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc};
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
    /// Plain `http://` is accepted only for a gateway on this machine.
    #[arg(long, env = "FERMAH_PAY_ENDPOINT")]
    endpoint: String,
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

const NETWORK: Network = Network::Testnet;

/// The contract a deposit entry calls, checked by `check_deposit_entry`.
fn vault_of(entry: &SorobanAuthorizationEntry) -> String {
    match &entry.root_invocation.function {
        SorobanAuthorizedFunction::ContractFn(call) => match &call.contract_address {
            ScAddress::Contract(ContractId(Hash(id))) => {
                stellar_strkey::Contract(*id).to_string().to_string()
            }
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn explorer(hash: &str) -> String {
    format!("https://stellar.expert/explorer/testnet/tx/{hash}")
}

/// A secret from the environment; arguments are visible to other users.
fn secret(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("set {name} in the environment"))
}

/// The bearer key travels in every request: in the clear only to a gateway
/// on this machine.
fn check_transport(endpoint: &str) -> anyhow::Result<()> {
    if endpoint.starts_with("https://") {
        return Ok(());
    }
    let Some(rest) = endpoint.strip_prefix("http://") else {
        bail!("the endpoint must start with https:// (or http:// for a gateway on this machine)");
    };
    let host = rest.split(['/', '?']).next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(host, _)| host);
    ensure!(
        matches!(host, "localhost" | "127.0.0.1" | "[::1]"),
        "{endpoint} would send the API key unencrypted; use https://"
    );
    Ok(())
}

/// The deposit a wallet should agree to sign: `amount` with `daily_limit`,
/// Circle's USDC moving from the buyer into the vault the entry calls. The
/// deposit id is the gateway's to choose; everything else must match.
fn check_deposit_entry(
    entry: &SorobanAuthorizationEntry,
    buyer: &fermah_pay_stellar_domain::AccountAddress,
    amount: i64,
    daily_limit: i64,
) -> anyhow::Result<()> {
    let SorobanAuthorizedFunction::ContractFn(call) = &entry.root_invocation.function else {
        bail!("the entry does not call a contract");
    };
    let ScAddress::Contract(ContractId(Hash(vault))) = call.contract_address else {
        bail!("the entry does not call a contract");
    };
    let Some(ScVal::Bytes(id)) = call.args.get(2) else {
        bail!("the entry is not a deposit");
    };
    let deposit_id: [u8; 32] =
        id.as_slice().try_into().map_err(|_| anyhow::anyhow!("the entry is not a deposit"))?;
    let deployment = PrepaidDeployment {
        contract: vault,
        usdc: asset_contract_id(&circle_usdc(NETWORK), NETWORK),
        custody: Custody::Vault,
    };
    let intent = DepositIntent {
        owner: buyer.clone().into(),
        amount: i128::from(amount),
        deposit_id,
        cap: Some(i128::from(daily_limit)),
    };
    ensure!(
        entry.root_invocation == deployment.deposit_authorization(&intent),
        "the gateway returned an entry other than the deposit asked for; not signing it"
    );
    Ok(())
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
    check_transport(&cli.endpoint)?;
    let limit = Duration::from_secs(cli.timeout_secs);
    let api_key = secret("FERMAH_PAY_API_KEY")?;
    let buyer = SecretKey::from_strkey(secret("FERMAH_PAY_BUYER_SECRET")?.trim())
        .map_err(|e| anyhow::anyhow!("FERMAH_PAY_BUYER_SECRET is not an S... seed: {e}"))?;
    let token = api_key.as_str();

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
        "    deposit_id {}: {} with a daily spending limit of {}; the gateway returns an \
         authorization entry for the buyer's wallet to sign, valid until ledger {}",
        prepared.deposit_id,
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
    check_deposit_entry(&entry, &buyer.address(), cli.deposit, cli.daily_limit)?;
    let signed =
        sign_entry(&entry, network_id(NETWORK), &[&buyer])?.to_xdr_base64(Limits::none())?;
    println!(
        "    checked: a deposit of {} into vault {}; signed by {}, who needs no XLM and pays \
         no fee",
        usdc(cli.deposit),
        vault_of(&entry),
        buyer.address()
    );

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
    println!("    confirmed: {}", explorer(&deposit.transaction_hash));

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
    let charge = match ledger
        .create_charge(authed(
            CreateChargeRequest {
                buyer_id: buyer_id.clone(),
                amount: cli.charge,
                idempotency_key: format!("try-charge-{run}"),
            },
            token,
        )?)
        .await
    {
        Ok(reply) => reply.into_inner().charge.context("no charge returned")?,
        Err(status) if status.message() == "above_spending_limit" => bail!(
            "CreateCharge refused: above_spending_limit. The buyer's charges today would exceed \
             the daily limit it signed ({}); the limit applies per UTC day, so run again \
             tomorrow or with a smaller --charge",
            usdc(cli.daily_limit)
        ),
        Err(status) => return Err(status.into()),
    };
    println!(
        "    charge_id {}: {} admitted at once (state {:?}); the worker settles it on-chain in \
         a batch",
        charge.charge_id,
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
    println!("    charged: {}", explorer(&charge.transaction_hash));

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use fermah_pay_stellar_chain::stellar_xdr::SorobanCredentials;

    use super::*;

    fn entry(deployment: &PrepaidDeployment, intent: &DepositIntent) -> SorobanAuthorizationEntry {
        SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: deployment.deposit_authorization(intent),
        }
    }

    #[test]
    fn test_only_the_deposit_asked_for_is_signed() {
        let buyer = SecretKey::generate().unwrap().address();
        let usdc = asset_contract_id(&circle_usdc(NETWORK), NETWORK);
        let vault = PrepaidDeployment { contract: [7; 32], usdc, custody: Custody::Vault };
        let intent = DepositIntent {
            owner: buyer.clone().into(),
            amount: 1_000_000,
            deposit_id: [1; 32],
            cap: Some(500_000),
        };
        check_deposit_entry(&entry(&vault, &intent), &buyer, 1_000_000, 500_000).unwrap();

        let other = SecretKey::generate().unwrap().address();
        let refused = [
            (
                "a larger amount",
                entry(&vault, &DepositIntent { amount: 2_000_000, ..intent.clone() }),
            ),
            (
                "a higher limit",
                entry(&vault, &DepositIntent { cap: Some(900_000), ..intent.clone() }),
            ),
            ("no limit", entry(&vault, &DepositIntent { cap: None, ..intent.clone() })),
            (
                "another payer",
                entry(&vault, &DepositIntent { owner: other.clone().into(), ..intent.clone() }),
            ),
            (
                "USDC to an account rather than the vault",
                entry(
                    &PrepaidDeployment { custody: Custody::Treasury(other), ..vault.clone() },
                    &intent,
                ),
            ),
            (
                "another asset",
                entry(&PrepaidDeployment { usdc: [9; 32], ..vault.clone() }, &intent),
            ),
        ];
        for (what, entry) in refused {
            let error = check_deposit_entry(&entry, &buyer, 1_000_000, 500_000).unwrap_err();
            assert!(error.to_string().contains("not signing it"), "{what}: {error}");
        }
    }

    #[test]
    fn test_the_api_key_travels_unencrypted_only_to_this_machine() {
        for endpoint in [
            "https://pay-sandbox.fermah.xyz",
            "http://localhost:50051",
            "http://127.0.0.1:50051",
            "http://[::1]:50051",
        ] {
            check_transport(endpoint).unwrap();
        }
        for endpoint in [
            "http://pay-sandbox.fermah.xyz",
            "http://localhost.example.com:50051",
            "http://user@localhost:50051",
            "pay-sandbox.fermah.xyz:443",
        ] {
            assert!(check_transport(endpoint).is_err(), "{endpoint}");
        }
    }
}
