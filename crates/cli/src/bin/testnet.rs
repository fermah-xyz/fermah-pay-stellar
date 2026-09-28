//! Stellar testnet tooling. Every command is hard-wired to testnet.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::onboarding::{ReservePayer, SubmissionPolicy, onboard_buyer};
use fermah_pay_stellar_chain::rpc::{RpcClient, hex_lower};
use fermah_pay_stellar_chain::{friendbot, usdc};
use fermah_pay_stellar_domain::Network;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const NETWORK: Network = Network::Testnet;

#[derive(Parser)]
#[command(name = "fermah-pay-stellar-testnet", version, about)]
struct Cli {
    /// Stellar RPC endpoint serving testnet.
    #[arg(
        long,
        env = "STELLAR_TESTNET_RPC_URL",
        default_value = "https://soroban-testnet.stellar.org"
    )]
    rpc_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new buyer account holding zero XLM, with a Circle testnet
    /// USDC trustline, whose fee and reserves a sponsor pays. Prints a JSON
    /// evidence record and exits non-zero if the ledger does not show the
    /// buyer at zero XLM with both reserves sponsored.
    OnboardBuyer {
        /// File holding the sponsor's `S...` seed. Without it, a fresh
        /// sponsor is generated and funded by Friendbot.
        #[arg(long)]
        sponsor_secret_file: Option<PathBuf>,
        /// Where to write a generated sponsor's seed (mode 0600, must not exist).
        #[arg(long, conflicts_with = "sponsor_secret_file")]
        sponsor_secret_out: Option<PathBuf>,
        /// Where to write the new buyer's seed (mode 0600, must not exist).
        #[arg(long)]
        buyer_secret_out: PathBuf,
        /// Maximum fee in stroops the sponsor offers for the transaction.
        #[arg(long, default_value_t = 10_000)]
        fee_stroops: u32,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rpc = RpcClient::new(&cli.rpc_url, Duration::from_secs(30))?;
    let info = rpc.verify_network(NETWORK).await?;
    match cli.command {
        Command::OnboardBuyer {
            sponsor_secret_file,
            sponsor_secret_out,
            buyer_secret_out,
            fee_stroops,
        } => {
            let sponsor = match &sponsor_secret_file {
                Some(path) => {
                    let seed = std::fs::read_to_string(path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    SecretKey::from_strkey(seed.trim())?
                }
                None => {
                    let sponsor = SecretKey::generate()?;
                    if let Some(path) = &sponsor_secret_out {
                        write_secret(path, &sponsor)?;
                    }
                    let url =
                        info.friendbot_url.as_deref().context("RPC reports no Friendbot URL")?;
                    friendbot::fund(url, &sponsor.address()).await?;
                    sponsor
                }
            };
            let buyer = SecretKey::generate()?;
            write_secret(&buyer_secret_out, &buyer)?;

            let asset = usdc::circle_usdc(NETWORK);
            let policy = SubmissionPolicy {
                fee_stroops,
                validity: Duration::from_secs(120),
                poll_interval: Duration::from_secs(2),
            };
            let receipt = onboard_buyer(&rpc, NETWORK, &sponsor, &buyer, &asset, policy).await?;
            let violations = receipt.provenance.violations_for_sponsor(&sponsor.address());
            let payer = |payer: &ReservePayer| match payer {
                ReservePayer::Buyer => "buyer".to_owned(),
                ReservePayer::Sponsor(address) => address.to_string(),
            };
            let hash = hex_lower(&receipt.transaction_hash);
            let evidence = serde_json::json!({
                "criterion": "buyer-onboarding-sponsored-reserves",
                "network": NETWORK.caip2(),
                "recorded_at": OffsetDateTime::now_utc().format(&Rfc3339)?,
                "transaction_hash": hash,
                "ledger": receipt.ledger,
                "fee_source": receipt.fee_source.to_string(),
                "fee_charged_stroops": receipt.fee_charged_stroops,
                "buyer": receipt.provenance.buyer.to_string(),
                "asset": {
                    "code": "USDC",
                    "issuer": usdc::circle_issuer(NETWORK).to_string(),
                    "contract_id": usdc::contract_strkey(usdc::asset_contract_id(&asset, NETWORK)),
                },
                "expected": "buyer created with 0 XLM; transaction fee, account reserve and USDC trustline reserve paid by fee_source",
                "observed": {
                    "buyer_native_balance_stroops": receipt.provenance.native_balance_stroops,
                    "account_reserve_paid_by": payer(&receipt.provenance.account_reserve),
                    "trustline_reserve_paid_by": receipt.provenance.trustline_reserve.as_ref().map(payer),
                    "violations": violations.iter().map(ToString::to_string).collect::<Vec<_>>(),
                },
                "public_explorer_url": format!("https://stellar.expert/explorer/testnet/tx/{hash}"),
            });
            println!("{}", serde_json::to_string_pretty(&evidence)?);
            if !violations.is_empty() {
                bail!("sponsorship property does not hold on the ledger");
            }
        }
    }
    Ok(())
}

fn write_secret(path: &Path, key: &SecretKey) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(key.to_strkey().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}
