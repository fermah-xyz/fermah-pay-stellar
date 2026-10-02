//! Administers a prepaid ledger contract whose admin account needs several
//! signatures. A change goes through three steps, each of which may run on a
//! different machine:
//!
//! 1. `propose` simulates the change and writes a proposal file holding the
//!    call and the unsigned authorization of every account that must agree;
//! 2. `sign`, run once per signer, shows what is being signed and adds that
//!    signer's signature;
//! 3. `submit` checks each authorization against its account's signers and
//!    medium threshold on the ledger, then sends the call fee-bumped.
//!
//! No signer ever needs another signer's key, and a proposal expires with its
//! authorizations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Parser, Subcommand, ValueEnum};
use fermah_pay_stellar_chain::authorization::{attach_signatures, signature_payload};
use fermah_pay_stellar_chain::multisig::account_signers;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{AdminAction, Role, usdc_transfer_call};
use fermah_pay_stellar_chain::rpc::{RpcClient, hex_lower};
use fermah_pay_stellar_chain::sponsored::{Credentials, Policy, Submitter};
use fermah_pay_stellar_chain::stellar_xdr::{
    HostFunction, Int128Parts, Limits, ReadXdr, ScAddress, ScVal, SorobanAuthorizationEntry,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation, SorobanCredentials, VecM, WriteXdr,
};
use fermah_pay_stellar_chain::transaction::address_of;
use fermah_pay_stellar_chain::{signer, usdc};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use fermah_pay_stellar_gateway::signing;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Parser)]
#[command(name = "fermah-pay-stellar-contract", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Simulate an admin change and write a proposal file to be signed.
    Propose {
        #[command(flatten)]
        network: NetworkArgs,
        /// `C...` address of the ledger contract.
        #[arg(long)]
        contract: String,
        /// The contract's admin account.
        #[arg(long)]
        admin: AccountAddress,
        /// Seed file of the account that sequences the transaction.
        #[arg(long)]
        source_key: PathBuf,
        /// Seed file of the account that pays the fee.
        #[arg(long)]
        fee_key: PathBuf,
        /// Ledgers the signers have to sign and submit (about five seconds
        /// each; 17280 is about a day).
        #[arg(long, default_value = "17280")]
        valid_for_ledgers: u32,
        #[arg(long)]
        out: PathBuf,
        #[command(subcommand)]
        action: ActionArgs,
    },
    /// Propose a USDC transfer out of an account that needs several
    /// signatures, such as topping up the treasury from its cold reserve,
    /// and write a proposal file to be signed.
    ProposeTransfer {
        #[command(flatten)]
        network: NetworkArgs,
        /// The account the USDC leaves, which authorizes the transfer.
        #[arg(long)]
        from: AccountAddress,
        /// The account that receives it; it needs a USDC trustline.
        #[arg(long)]
        to: AccountAddress,
        /// USDC base units (1 USDC = 10,000,000).
        #[arg(long)]
        amount: i128,
        #[arg(long)]
        source_key: PathBuf,
        #[arg(long)]
        fee_key: PathBuf,
        #[arg(long, default_value = "17280")]
        valid_for_ledgers: u32,
        #[arg(long)]
        out: PathBuf,
    },
    /// Upload contract Wasm to the network, so an `upgrade` proposal can name
    /// its hash. Uploading changes no contract and needs no admin signature.
    Upload {
        #[command(flatten)]
        network: NetworkArgs,
        /// Path of the built `.wasm` file.
        #[arg(long)]
        wasm: PathBuf,
        #[arg(long)]
        source_key: PathBuf,
        #[arg(long)]
        fee_key: PathBuf,
    },
    /// Print the account a key reference signs for, after checking that it
    /// signs: for instance the `G...` address of a key held in AWS KMS.
    Address {
        /// Key reference: a seed file path, a remote signer (`https://...`),
        /// or `aws-kms://alias/pay-stellar-operator` (built with the
        /// `aws-kms` feature).
        #[arg(long)]
        key: String,
    },
    /// Show a proposal and add one signer's signature to it.
    Sign {
        #[arg(long)]
        proposal: PathBuf,
        /// Key reference of the signer: a seed file path, or a key in a key
        /// management service.
        #[arg(long)]
        key: String,
        /// The account this signer signs for; needed when the proposal
        /// holds more than one authorization.
        #[arg(long)]
        account: Option<AccountAddress>,
    },
    /// Check a proposal's signatures against the ledger and send it.
    Submit {
        #[command(flatten)]
        network: NetworkArgs,
        #[arg(long)]
        proposal: PathBuf,
        #[arg(long)]
        source_key: PathBuf,
        #[arg(long)]
        fee_key: PathBuf,
    },
}

#[derive(clap::Args)]
struct NetworkArgs {
    #[arg(long, env = "PAY_STELLAR_NETWORK")]
    network: Network,
    #[arg(long, env = "PAY_STELLAR_RPC_URL")]
    rpc_url: String,
    /// Inclusion bid per operation, in stroops.
    #[arg(long, default_value = "10000")]
    inclusion_fee: u32,
}

#[derive(Subcommand)]
enum ActionArgs {
    Pause,
    Unpause,
    SetLimits {
        #[arg(long)]
        min_deposit: i128,
        #[arg(long)]
        max_charge: i128,
    },
    Upgrade {
        /// SHA-256 of the uploaded Wasm, hex.
        #[arg(long)]
        wasm_hash: String,
    },
    SetRole {
        #[arg(long, value_enum)]
        role: RoleArg,
        #[arg(long)]
        holder: AccountAddress,
    },
    /// Limit what one account, and all accounts together, may be charged in
    /// a UTC day, in USDC base units.
    SetDailyLimits {
        #[arg(long)]
        per_buyer: i128,
        #[arg(long)]
        per_seller: i128,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    Admin,
    Operator,
    Seller,
    Treasury,
}

/// A change awaiting signatures. Everything a signer approves is in
/// `function` and each authorization's `entry`; `summary` is for reading
/// only, and `sign` recomputes it from them.
#[derive(Serialize, Deserialize)]
struct Proposal {
    network: String,
    contract: String,
    summary: String,
    function: String,
    expiration_ledger: u32,
    authorizations: Vec<Authorization>,
}

#[derive(Serialize, Deserialize)]
struct Authorization {
    account: String,
    entry: String,
    signatures: Vec<Signed>,
}

#[derive(Serialize, Deserialize)]
struct Signed {
    signer: String,
    signature: String,
}

fn action(args: ActionArgs) -> anyhow::Result<AdminAction> {
    Ok(match args {
        ActionArgs::Pause => AdminAction::Pause,
        ActionArgs::Unpause => AdminAction::Unpause,
        ActionArgs::SetLimits { min_deposit, max_charge } => {
            AdminAction::SetLimits { min_deposit, max_charge }
        }
        ActionArgs::Upgrade { wasm_hash } => {
            let bytes = hex_bytes(&wasm_hash).context("the Wasm hash is not 32 bytes of hex")?;
            AdminAction::Upgrade { wasm_hash: bytes }
        }
        ActionArgs::SetRole { role, holder } => AdminAction::SetRole {
            role: match role {
                RoleArg::Admin => Role::Admin,
                RoleArg::Operator => Role::Operator,
                RoleArg::Seller => Role::Seller,
                RoleArg::Treasury => Role::Treasury,
            },
            holder,
        },
        ActionArgs::SetDailyLimits { per_buyer, per_seller } => {
            AdminAction::SetDailyLimits { per_buyer, per_seller }
        }
    })
}

fn hex_bytes(raw: &str) -> Option<[u8; 32]> {
    if raw.len() != 64 {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(raw.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(bytes)
}

fn contract_bytes(raw: &str) -> anyhow::Result<[u8; 32]> {
    Ok(stellar_strkey::Contract::from_string(raw).context("not a C... contract address")?.0)
}

/// What a function call does, in words, derived from the call itself.
fn describe(function: &HostFunction) -> anyhow::Result<String> {
    let HostFunction::InvokeContract(call) = function else { bail!("not a contract call") };
    let ScAddress::Contract(contract) = &call.contract_address else { bail!("not a contract") };
    Ok(format!(
        "call {} on {} with {}",
        String::from_utf8_lossy(call.function_name.0.as_slice()),
        stellar_strkey::Contract(contract.0.0),
        call.args.iter().map(show).collect::<Vec<_>>().join(", "),
    ))
}

/// An argument as a signer reads it: accounts and contracts as addresses,
/// numbers as numbers, bytes as hex.
fn show(value: &ScVal) -> String {
    match value {
        ScVal::Address(ScAddress::Account(account)) => address_of(account).to_string(),
        ScVal::Address(ScAddress::Contract(id)) => {
            stellar_strkey::Contract(id.0.0).to_string().as_str().to_owned()
        }
        ScVal::I128(Int128Parts { hi, lo }) => {
            ((i128::from(*hi) << 64) | i128::from(*lo)).to_string()
        }
        ScVal::U32(n) => n.to_string(),
        ScVal::Bool(b) => b.to_string(),
        ScVal::Bytes(bytes) => hex_lower(bytes.as_slice()),
        ScVal::Symbol(symbol) => String::from_utf8_lossy(symbol.0.as_slice()).into_owned(),
        ScVal::Map(Some(map)) => format!(
            "{{{}}}",
            map.iter()
                .map(|e| format!("{}: {}", show(&e.key), show(&e.val)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScVal::Vec(Some(items)) => {
            format!("[{}]", items.iter().map(show).collect::<Vec<_>>().join(", "))
        }
        other => format!("{other:?}"),
    }
}

fn entry_account(entry: &SorobanAuthorizationEntry) -> anyhow::Result<AccountAddress> {
    match &entry.credentials {
        SorobanCredentials::Address(c) | SorobanCredentials::AddressV2(c) => match &c.address {
            ScAddress::Account(account) => Ok(address_of(account)),
            _ => bail!("an authorization is not for a classic account"),
        },
        _ => bail!("an authorization carries no address credentials"),
    }
}

fn read_proposal(path: &Path) -> anyhow::Result<Proposal> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn write_proposal(path: &Path, proposal: &Proposal) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(proposal)? + "\n")
        .with_context(|| format!("writing {}", path.display()))
}

fn submitter<'a>(
    rpc: &'a RpcClient,
    network: Network,
    source: &'a fermah_pay_stellar_chain::keys::SecretKey,
    fee_source: &'a fermah_pay_stellar_chain::keys::SecretKey,
    inclusion_fee: u32,
) -> Submitter<'a> {
    Submitter {
        rpc,
        network,
        source,
        fee_source,
        policy: Policy {
            inclusion_fee,
            resource_fee_margin_percent: 20,
            validity: Duration::from_secs(60),
            poll_interval: Duration::from_secs(2),
        },
    }
}

struct ProposalRequest<'a> {
    network: &'a NetworkArgs,
    source_key: &'a Path,
    fee_key: &'a Path,
    valid_for_ledgers: u32,
    out: &'a Path,
}

/// Simulates `function` with unsigned authorizations of `trees` and writes
/// the proposal for the signers.
async fn propose(
    request: &ProposalRequest<'_>,
    contract: String,
    function: &HostFunction,
    trees: &[(AccountAddress, SorobanAuthorizedInvocation)],
) -> anyhow::Result<()> {
    let network = request.network;
    let rpc = RpcClient::new(&network.rpc_url, Duration::from_secs(30))?;
    rpc.verify_network(network.network).await?;
    let (source, fee) =
        (signing::read_seed(request.source_key)?, signing::read_seed(request.fee_key)?);
    let submitter = submitter(&rpc, network.network, &source, &fee, network.inclusion_fee);
    let prepared = submitter
        .prepare_authorizations(function, trees, request.valid_for_ledgers, Credentials::AddressV2)
        .await?;
    let proposal = Proposal {
        network: network.network.caip2().to_owned(),
        contract,
        summary: describe(function)?,
        function: function.to_xdr_base64(Limits::none())?,
        expiration_ledger: prepared.latest_ledger.saturating_add(request.valid_for_ledgers),
        authorizations: prepared
            .entries
            .iter()
            .map(|entry| {
                Ok(Authorization {
                    account: entry_account(entry)?.to_string(),
                    entry: entry.to_xdr_base64(Limits::none())?,
                    signatures: Vec::new(),
                })
            })
            .collect::<anyhow::Result<_>>()?,
    };
    write_proposal(request.out, &proposal)?;
    println!(
        "{}",
        json!({ "proposal": request.out, "summary": proposal.summary,
        "expiration_ledger": proposal.expiration_ledger,
        "accounts": proposal.authorizations.iter().map(|a| &a.account).collect::<Vec<_>>() })
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Propose {
            network,
            contract,
            admin,
            source_key,
            fee_key,
            valid_for_ledgers,
            out,
            action: args,
        } => {
            let action = action(args)?;
            let contract_id = contract_bytes(&contract)?;
            let function = HostFunction::InvokeContract(action.call(contract_id));
            let trees = action.authorizations(contract_id, &admin);
            let request = ProposalRequest {
                network: &network,
                source_key: &source_key,
                fee_key: &fee_key,
                valid_for_ledgers,
                out: &out,
            };
            propose(&request, contract, &function, &trees).await?;
        }
        Command::ProposeTransfer {
            network,
            from,
            to,
            amount,
            source_key,
            fee_key,
            valid_for_ledgers,
            out,
        } => {
            ensure!(amount > 0, "the amount must be positive");
            ensure!(from != to, "the transfer must go to another account");
            let usdc =
                usdc::asset_contract_id(&usdc::circle_usdc(network.network), network.network);
            let call = usdc_transfer_call(usdc, &from, &to, amount);
            let function = HostFunction::InvokeContract(call.clone());
            let trees = vec![(
                from,
                SorobanAuthorizedInvocation {
                    function: SorobanAuthorizedFunction::ContractFn(call),
                    sub_invocations: VecM::default(),
                },
            )];
            let request = ProposalRequest {
                network: &network,
                source_key: &source_key,
                fee_key: &fee_key,
                valid_for_ledgers,
                out: &out,
            };
            propose(&request, usdc::contract_strkey(usdc), &function, &trees).await?;
        }
        Command::Upload { network, wasm, source_key, fee_key } => {
            let code =
                std::fs::read(&wasm).with_context(|| format!("reading {}", wasm.display()))?;
            let rpc = RpcClient::new(&network.rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(network.network).await?;
            let (source, fee) = (signing::read_seed(&source_key)?, signing::read_seed(&fee_key)?);
            let submitter = submitter(&rpc, network.network, &source, &fee, network.inclusion_fee);
            let upload = fermah_pay_stellar_chain::deploy::upload(&code)?;
            let auth = submitter.record_source_authorization(&upload).await?;
            let receipt = submitter.submit(upload, auth).await?;
            println!(
                "{}",
                json!({ "wasm_hash": hex_lower(&fermah_pay_stellar_chain::deploy::wasm_hash(&code)),
                        "transaction_hash": hex_lower(&receipt.outer_hash) })
            );
        }
        Command::Address { key } => {
            println!("{}", signing::open(&key).await?.address());
        }
        Command::Sign { proposal: path, key, account } => {
            let mut proposal = read_proposal(&path)?;
            let network: Network = proposal.network.parse()?;
            let function = HostFunction::from_xdr_base64(&proposal.function, Limits::none())?;
            let summary = describe(&function)?;
            ensure!(summary == proposal.summary, "the proposal's summary does not match its call");
            let index = match (account, proposal.authorizations.len()) {
                (Some(account), _) => proposal
                    .authorizations
                    .iter()
                    .position(|a| a.account == account.as_str())
                    .with_context(|| format!("the proposal needs no authorization of {account}"))?,
                (None, 1) => 0,
                (None, _) => bail!("the proposal holds several authorizations; name --account"),
            };
            let authorization = &mut proposal.authorizations[index];
            let entry =
                SorobanAuthorizationEntry::from_xdr_base64(&authorization.entry, Limits::none())?;
            let HostFunction::InvokeContract(call) = &function else { unreachable!() };
            ensure!(
                entry.root_invocation.function
                    == fermah_pay_stellar_chain::stellar_xdr::SorobanAuthorizedFunction::ContractFn(
                        call.clone()
                    ),
                "the authorization is not for the proposed call"
            );
            let signer = signing::open(&key).await?;
            let payload =
                signature_payload(network_id(network), &entry.credentials, &entry.root_invocation)?;
            let signature = signer::signature(signer.as_ref(), &payload).await?;
            let who = signer.address().to_string();
            authorization.signatures.retain(|s| s.signer != who);
            authorization
                .signatures
                .push(Signed { signer: who.clone(), signature: STANDARD.encode(signature) });
            let account = authorization.account.clone();
            write_proposal(&path, &proposal)?;
            eprintln!(
                "signed as {who} for {account}: {summary} (expires at ledger {})",
                proposal.expiration_ledger
            );
        }
        Command::Submit { network, proposal: path, source_key, fee_key } => {
            let proposal = read_proposal(&path)?;
            ensure!(
                proposal.network == network.network.caip2(),
                "the proposal is for {}, not {}",
                proposal.network,
                network.network
            );
            let rpc = RpcClient::new(&network.rpc_url, Duration::from_secs(30))?;
            rpc.verify_network(network.network).await?;
            let function = HostFunction::from_xdr_base64(&proposal.function, Limits::none())?;
            let mut entries = Vec::new();
            for authorization in &proposal.authorizations {
                let account: AccountAddress = authorization.account.parse()?;
                let entry = SorobanAuthorizationEntry::from_xdr_base64(
                    &authorization.entry,
                    Limits::none(),
                )?;
                let mut signers = Vec::new();
                let mut signatures = Vec::new();
                for signed in &authorization.signatures {
                    let signer: AccountAddress = signed.signer.parse()?;
                    let signature: [u8; 64] = STANDARD
                        .decode(&signed.signature)?
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("a signature is not 64 bytes"))?;
                    signatures.push((*signer.public_key(), signature));
                    signers.push(signer);
                }
                let policy = account_signers(&rpc, &account)
                    .await?
                    .with_context(|| format!("account {account} does not exist"))?;
                ensure!(
                    policy.meets_medium(&signers),
                    "{account} needs signatures weighing {} (its medium threshold); these weigh {}",
                    policy.medium.max(1),
                    policy.weight_of(&signers)
                );
                entries.push(attach_signatures(&entry, network_id(network.network), signatures)?);
            }
            let (source, fee) = (signing::read_seed(&source_key)?, signing::read_seed(&fee_key)?);
            let receipt = submitter(&rpc, network.network, &source, &fee, network.inclusion_fee)
                .submit(function, entries)
                .await?;
            println!(
                "{}",
                json!({
                    "summary": proposal.summary,
                    "transaction_hash": hex_lower(&receipt.outer_hash),
                    "ledger": receipt.ledger,
                    "fee_charged_stroops": receipt.fee_charged_stroops,
                })
            );
        }
    }
    Ok(())
}
