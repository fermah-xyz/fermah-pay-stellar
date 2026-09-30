//! Stellar testnet tooling. Every command runs on testnet, or on a local
//! standalone network (`--network stellar:local`) for development and CI.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::onboarding::{ReservePayer, SubmissionPolicy, onboard_buyer};
use fermah_pay_stellar_chain::rpc::{RpcClient, hex_lower};
use fermah_pay_stellar_chain::sponsored::Policy;
use fermah_pay_stellar_chain::{friendbot, usdc};
use fermah_pay_stellar_cli::testnet::prepaid::{Context as Testnet, LoadShape};
use fermah_pay_stellar_cli::testnet::profile::Profile;
use fermah_pay_stellar_domain::Network;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Parser)]
#[command(name = "fermah-pay-stellar-testnet", version, about)]
struct Cli {
    /// `stellar:testnet`, or `stellar:local` for a standalone network on this
    /// machine such as `stellar/quickstart --local`.
    #[arg(long, env = "PAY_STELLAR_TESTNET_NETWORK", default_value = "stellar:testnet")]
    network: Network,
    /// Stellar RPC endpoint serving that network.
    #[arg(
        long,
        env = "STELLAR_TESTNET_RPC_URL",
        default_value = "https://soroban-testnet.stellar.org"
    )]
    rpc_url: String,
    /// Directory holding testnet keys and deployment state. Never inside a
    /// repository.
    #[arg(long, env = "PAY_STELLAR_TESTNET_PROFILE")]
    profile_dir: Option<PathBuf>,
    /// Where evidence records are written: `docs/evidence/testnet` on
    /// testnet, `target/local-evidence` on a local network.
    #[arg(long)]
    evidence_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create any missing role account: sponsor, submitter and fee source
    /// funded by Friendbot; admin, operator, seller, treasury and USDC reserve
    /// sponsored with zero XLM.
    InitRoles,
    /// On a local network only: deploy the stand-in USDC's asset contract if
    /// missing and issue AMOUNT base units to the USDC reserve.
    MintLocalUsdc {
        #[arg(long)]
        amount: i64,
    },
    /// Require two of the admin account's three keys (its own and two
    /// co-signers kept in the profile) for anything it authorizes
    AdminMultisig,
    /// Create the treasury's cold reserve if missing: a Circle USDC
    /// trustline, zero XLM, and two of its three keys (its own and two
    /// co-signers kept in the profile) to move anything.
    ColdReserve,
    /// Move AMOUNT of the treasury's USDC to its cold reserve, authorized by
    /// the treasury alone, as the settlement worker's sweep does.
    Sweep {
        #[arg(long)]
        amount: i128,
    },
    /// Replace the recorded contract's code with WASM in place, authorized by
    /// the admin; balances and totals are kept
    UpgradePrepaid {
        #[arg(long)]
        wasm: PathBuf,
    },
    /// Upload the prepaid ledger Wasm and create an instance pinned to the
    /// profile's roles and Circle testnet USDC.
    DeployPrepaid {
        #[arg(long)]
        wasm: PathBuf,
        /// Minimum deposit, USDC base units (1 USDC = 10,000,000).
        #[arg(long)]
        min_deposit: i128,
        /// Maximum single charge, USDC base units.
        #[arg(long)]
        max_charge: i128,
    },
    /// Create buyers 1..=COUNT with zero XLM and top each up to USDC_EACH
    /// base units from the USDC reserve.
    OnboardBuyers {
        #[arg(long)]
        count: u32,
        #[arg(long)]
        usdc_each: i64,
    },
    /// Deposit AMOUNT base units for buyers FIRST..=LAST.
    Deposit {
        #[arg(long)]
        first: u32,
        #[arg(long)]
        last: u32,
        #[arg(long)]
        amount: i128,
    },
    /// Charge buyers FIRST..=LAST in one charge_batch transaction.
    ChargeBatch {
        #[arg(long)]
        first: u32,
        #[arg(long)]
        last: u32,
        /// Names the charges: each buyer's identifier is derived from the tag
        /// and its number, so the same tag repeats the same charges.
        #[arg(long)]
        tag: String,
        #[arg(long)]
        amount: i128,
    },
    /// Submit a single charge for BUYER.
    Charge {
        #[arg(long)]
        buyer: u32,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        amount: i128,
    },
    /// Compare the treasury's USDC with the ledger's liabilities and revenue.
    Solvency,
    /// Run one seller's flow through the gateway API against the recorded
    /// deployment: a new zero-XLM buyer deposits Circle USDC, is charged three
    /// times, and a retried charge is not charged again. The gateway and the
    /// settlement worker run in-process on the given database.
    EndToEnd {
        /// PostgreSQL URL of the database owner; the run applies migrations.
        #[arg(long, env = "PAY_STELLAR_E2E_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Load the recorded deployment through the gateway API: BUYERS new
    /// zero-XLM buyers deposit once each, then CHARGES_PER_BUYER charges each
    /// are admitted CONCURRENCY at a time and settled by the worker from
    /// CHANNELS channel accounts and the submitter. Writes a `load-test`
    /// evidence record with throughput, latency and fees.
    LoadTest {
        /// PostgreSQL URL of the database owner; the run applies migrations.
        #[arg(long, env = "PAY_STELLAR_E2E_DATABASE_URL", hide_env_values = true)]
        database_url: String,
        #[arg(long, default_value = "10")]
        buyers: u32,
        #[arg(long, default_value = "100")]
        charges_per_buyer: u32,
        #[arg(long, default_value = "4")]
        channels: u32,
        #[arg(long, default_value = "32")]
        concurrency: usize,
    },
    /// Send a testnet transaction signed through a key reference, such as
    /// `aws-kms://alias/...`, from the account that key signs for; Friendbot
    /// funds the account first if it does not exist.
    KeyCheck {
        #[arg(long)]
        key: String,
    },
    /// Withdraw AMOUNT of BUYER's credit back to the buyer.
    Withdraw {
        #[arg(long)]
        buyer: u32,
        #[arg(long)]
        amount: i128,
    },
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
    let network = cli.network;
    if network == Network::Pubnet {
        bail!("this tooling runs on testnet or a local network, never on pubnet");
    }
    fermah_pay_stellar_cli::testnet::evidence::set_network(network);
    let info = rpc.verify_network(network).await?;
    let local = network == Network::Local;
    let profile_dir = match cli.profile_dir {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var("HOME").context("HOME is not set")?).join(if local {
            ".config/fermah-pay-stellar/local"
        } else {
            ".config/fermah-pay-stellar/testnet"
        }),
    };
    let evidence_dir = cli.evidence_dir.clone().unwrap_or_else(|| {
        PathBuf::from(if local { "target/local-evidence" } else { "docs/evidence/testnet" })
    });
    let context = || -> anyhow::Result<Testnet> {
        Ok(Testnet {
            network,
            rpc: rpc.clone(),
            profile: Profile::open(&profile_dir)?,
            evidence_dir: evidence_dir.clone(),
            policy: Policy {
                inclusion_fee: 1_000,
                resource_fee_margin_percent: 15,
                validity: Duration::from_secs(120),
                poll_interval: Duration::from_secs(2),
            },
        })
    };
    match cli.command {
        Command::InitRoles => context()?.init_roles().await?,
        Command::MintLocalUsdc { amount } => context()?.mint_local_usdc(amount).await?,
        Command::AdminMultisig => context()?.admin_multisig().await?,
        Command::ColdReserve => context()?.cold_reserve().await?,
        Command::Sweep { amount } => context()?.sweep(amount).await?,
        Command::UpgradePrepaid { wasm } => {
            let code =
                std::fs::read(&wasm).with_context(|| format!("reading {}", wasm.display()))?;
            context()?.upgrade_prepaid(&code).await?;
        }
        Command::DeployPrepaid { wasm, min_deposit, max_charge } => {
            let code =
                std::fs::read(&wasm).with_context(|| format!("reading {}", wasm.display()))?;
            context()?.deploy_prepaid(&code, min_deposit, max_charge).await?;
        }
        Command::OnboardBuyers { count, usdc_each } => {
            context()?.onboard_buyers(count, usdc_each).await?
        }
        Command::Deposit { first, last, amount } => context()?.deposit(first, last, amount).await?,
        Command::ChargeBatch { first, last, tag, amount } => {
            context()?.charge_batch(first, last, &tag, amount).await?;
        }
        Command::Charge { buyer, tag, amount } => context()?.charge(buyer, &tag, amount).await?,
        Command::Withdraw { buyer, amount } => context()?.withdraw(buyer, amount).await?,
        Command::Solvency => context()?.solvency().await?,
        Command::EndToEnd { database_url } => context()?.end_to_end(&database_url).await?,
        Command::KeyCheck { key } => context()?.key_check(&key).await?,
        Command::LoadTest { database_url, buyers, charges_per_buyer, channels, concurrency } => {
            let shape = LoadShape { buyers, charges_per_buyer, channels, concurrency };
            context()?.load_test(&database_url, &shape).await?;
        }
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

            let asset = usdc::circle_usdc(network);
            let policy = SubmissionPolicy {
                fee_stroops,
                validity: Duration::from_secs(120),
                poll_interval: Duration::from_secs(2),
            };
            let receipt = onboard_buyer(&rpc, network, &sponsor, &buyer, &asset, policy).await?;
            let violations = receipt.provenance.violations_for_sponsor(&sponsor.address());
            let payer = |payer: &ReservePayer| match payer {
                ReservePayer::Buyer => "buyer".to_owned(),
                ReservePayer::Sponsor(address) => address.to_string(),
            };
            let hash = hex_lower(&receipt.transaction_hash);
            let evidence = serde_json::json!({
                "criterion": "buyer-onboarding-sponsored-reserves",
                "network": network.caip2(),
                "recorded_at": OffsetDateTime::now_utc().format(&Rfc3339)?,
                "transaction_hash": hash,
                "ledger": receipt.ledger,
                "fee_source": receipt.fee_source.to_string(),
                "fee_charged_stroops": receipt.fee_charged_stroops,
                "buyer": receipt.provenance.buyer.to_string(),
                "asset": {
                    "code": "USDC",
                    "issuer": usdc::circle_issuer(network).to_string(),
                    "contract_id": usdc::contract_strkey(usdc::asset_contract_id(&asset, network)),
                },
                "expected": "buyer created with 0 XLM; transaction fee, account reserve and USDC trustline reserve paid by fee_source",
                "observed": {
                    "buyer_native_balance_stroops": receipt.provenance.native_balance_stroops,
                    "account_reserve_paid_by": payer(&receipt.provenance.account_reserve),
                    "trustline_reserve_paid_by": receipt.provenance.trustline_reserve.as_ref().map(payer),
                    "violations": violations.iter().map(ToString::to_string).collect::<Vec<_>>(),
                },
                "public_explorer_url": fermah_pay_stellar_cli::testnet::evidence::tx_url(&hash),
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
