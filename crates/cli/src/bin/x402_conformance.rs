//! Runs the x402 conformance harness against a gateway's facilitator
//! interface, as a third-party facilitator would, and writes every request
//! and response with a verdict per case. Exits non-zero if any case fails.
//!
//! The buyer must be registered with the seller deployment and hold a small
//! available balance (a few tenths of a cent are enough).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::Parser;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_chain::usdc::{asset_contract_id, circle_usdc, contract_strkey};
use fermah_pay_stellar_cli::x402_conformance::{Target, run};
use fermah_pay_stellar_domain::Network;

#[derive(Debug, Parser)]
#[command(name = "fermah-pay-stellar-x402-conformance", version, about)]
struct Cli {
    /// Base URL of the facilitator interface, e.g. `http://127.0.0.1:8402`.
    #[arg(long)]
    endpoint: String,
    /// The seller deployment's API key.
    #[arg(long, env = "PAY_STELLAR_X402_API_KEY", hide_env_values = true)]
    api_key: String,
    #[arg(long)]
    network: Network,
    /// An RPC node of the network, read independently to check the
    /// settlement on-chain.
    #[arg(long, env = "PAY_STELLAR_RPC_URL")]
    rpc_url: String,
    /// The seller's prepaid ledger contract (`C...`).
    #[arg(long)]
    pay_to: String,
    /// The USDC asset contract (`C...`); the network's USDC by default.
    #[arg(long)]
    asset: Option<String>,
    /// File holding the buyer's secret seed (`S...`).
    #[arg(long)]
    buyer_key_file: PathBuf,
    /// Seconds to wait for the settlement to land on-chain.
    #[arg(long, default_value = "300")]
    settlement_timeout_secs: u64,
    /// Where to write the record (JSON).
    #[arg(long)]
    out: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let seed = std::fs::read_to_string(&cli.buyer_key_file)
        .with_context(|| format!("reading {}", cli.buyer_key_file.display()))?;
    let buyer = SecretKey::from_strkey(seed.trim()).context("the buyer key file")?;
    let rpc = RpcClient::new(&cli.rpc_url, Duration::from_secs(30))?;
    rpc.verify_network(cli.network).await.context("checking the RPC network")?;
    let asset = cli.asset.unwrap_or_else(|| {
        contract_strkey(asset_contract_id(&circle_usdc(cli.network), cli.network))
    });
    let target = Target {
        endpoint: cli.endpoint.trim_end_matches('/').to_owned(),
        api_key: zeroize::Zeroizing::new(cli.api_key),
        network: cli.network,
        pay_to: cli.pay_to,
        asset,
        settlement_timeout: Duration::from_secs(cli.settlement_timeout_secs),
    };
    let record = run(&target, &buyer, &rpc).await?;
    std::fs::write(&cli.out, serde_json::to_string_pretty(&record)? + "\n")
        .with_context(|| format!("writing {}", cli.out.display()))?;
    let failed = record["failed_cases"].as_array().map_or(0, Vec::len);
    println!(
        "{} cases, {failed} failed; record written to {}",
        record["cases"].as_array().map_or(0, Vec::len),
        cli.out.display()
    );
    if failed > 0 {
        bail!("conformance failed: {}", record["failed_cases"]);
    }
    Ok(())
}
