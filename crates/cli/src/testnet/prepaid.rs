//! Prepaid ledger operations on Stellar testnet.
//!
//! Role separation mirrors a production deployment: buyers, treasury,
//! operator, seller and admin hold no XLM; a submitter account signs and
//! sequences each transaction; a separate fee account pays through fee bumps;
//! a sponsor pays account reserves.

use std::path::PathBuf;

use anyhow::{Context as _, bail, ensure};
use fermah_pay_stellar_chain::authorization::{sign_entry, verify_signed_entry};
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::onboarding::{
    MAX_BUYERS_PER_TRANSACTION, SubmissionPolicy, onboard_buyers,
};
use fermah_pay_stellar_chain::payments::payments_transaction;
use fermah_pay_stellar_chain::prepaid::{
    ChargeRequest, DepositIntent, PrepaidDeployment, Roles, Totals, WithdrawIntent,
    constructor_args, contract_totals,
};
use fermah_pay_stellar_chain::rpc::{RpcClient, hex_lower};
use fermah_pay_stellar_chain::sponsored::{Credentials, Policy, Receipt, Submitter};
use fermah_pay_stellar_chain::stellar_xdr::{
    ContractId, Hash, HostFunction, Int128Parts, LedgerEntryData, LedgerKey, LedgerKeyAccount,
    ScAddress, ScVal, SorobanAuthorizationEntry, SorobanAuthorizedInvocation,
};
use fermah_pay_stellar_chain::submission::submit_and_wait;
use fermah_pay_stellar_chain::{deploy, friendbot, network_id, transaction, usdc};
use fermah_pay_stellar_domain::{AccountAddress, Network};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::evidence::{self, tx_url};
use super::ledger::balances;
use super::profile::{Deployment, Profile};

mod e2e;

const NETWORK: Network = Network::Testnet;
const SPONSOR: &str = "operator-sponsor";
const SUBMITTER: &str = "submitter";
const FEE_SOURCE: &str = "fee-source";
const USDC_RESERVE: &str = "usdc-reserve";
const ROLES: [&str; 4] = ["admin", "operator", "seller", "treasury"];
/// Ledgers (about 5 s each) a signer's authorization stays valid.
const AUTH_VALIDITY_LEDGERS: u32 = 60;
/// Ledgers (about an hour) a charge stays settleable.
const CHARGE_VALIDITY_LEDGERS: u32 = 720;

/// The charge identifier a tag gives buyer `buyer`.
fn tagged_charge_id(tag: &str, buyer: u32) -> [u8; 32] {
    Sha256::digest(format!("{tag}/{buyer}").as_bytes()).into()
}

pub struct Context {
    pub rpc: RpcClient,
    pub profile: Profile,
    pub evidence_dir: PathBuf,
    pub policy: Policy,
}

fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

fn contract_bytes(strkey: &str) -> anyhow::Result<[u8; 32]> {
    let address: ScAddress = strkey.parse().context("parsing contract address")?;
    match address {
        ScAddress::Contract(ContractId(Hash(bytes))) => Ok(bytes),
        _ => bail!("{strkey} is not a contract address"),
    }
}

fn i128_of(value: &ScVal) -> Option<i128> {
    match value {
        ScVal::I128(Int128Parts { hi, lo }) => Some((i128::from(*hi) << 64) | i128::from(*lo)),
        _ => None,
    }
}

const OUTCOMES: [&str; 6] =
    ["Charged", "InsufficientBalance", "AboveLimit", "Duplicate", "OutOfOrder", "UnknownAccount"];

fn outcomes_of(value: Option<&ScVal>) -> anyhow::Result<Vec<&'static str>> {
    let Some(ScVal::Vec(Some(items))) = value else { bail!("charge_batch returned {value:?}") };
    items
        .iter()
        .map(|item| match item {
            ScVal::U32(code) => OUTCOMES
                .get(usize::try_from(*code)?)
                .copied()
                .with_context(|| format!("unknown outcome code {code}")),
            other => bail!("unexpected outcome value {other:?}"),
        })
        .collect()
}

fn receipt_json(receipt: &Receipt) -> serde_json::Value {
    let outer = hex_lower(&receipt.outer_hash);
    json!({
        "transaction_hash": outer,
        "inner_transaction_hash": hex_lower(&receipt.inner_hash),
        "ledger": receipt.ledger,
        "fee_source": receipt.fee_source.to_string(),
        "inner_source": receipt.inner_source.to_string(),
        "fee_charged_stroops": receipt.fee_charged_stroops,
        "resources": {
            "instructions": receipt.resources.instructions,
            "disk_read_bytes": receipt.resources.disk_read_bytes,
            "write_bytes": receipt.resources.write_bytes,
            "footprint_read_only": receipt.resources.footprint.read_only.len(),
            "footprint_read_write": receipt.resources.footprint.read_write.len(),
        },
        "public_explorer_url": tx_url(&outer),
    })
}

impl Context {
    fn onboarding_policy(&self) -> SubmissionPolicy {
        SubmissionPolicy {
            fee_stroops: self.policy.inclusion_fee,
            validity: self.policy.validity,
            poll_interval: self.policy.poll_interval,
        }
    }

    fn deployment(&self) -> anyhow::Result<(Deployment, PrepaidDeployment)> {
        let recorded = self.profile.deployment()?;
        let pinned = PrepaidDeployment {
            contract: contract_bytes(&recorded.contract)?,
            usdc: contract_bytes(&recorded.usdc)?,
            treasury: recorded.treasury.parse()?,
        };
        Ok((recorded, pinned))
    }

    async fn account_exists(&self, account: &AccountAddress) -> anyhow::Result<bool> {
        let key =
            LedgerKey::Account(LedgerKeyAccount { account_id: transaction::account_id(account) });
        let records = self.rpc.get_ledger_entries(&[key]).await?;
        Ok(records.iter().any(|r| matches!(r.data, LedgerEntryData::Account(_))))
    }

    /// Creates any missing role account: funded accounts for those that pay
    /// (sponsor, submitter, fee source), sponsored zero-XLM accounts with a
    /// USDC trustline for the rest.
    pub async fn init_roles(&self) -> anyhow::Result<()> {
        let info = self.rpc.verify_network(NETWORK).await?;
        let friendbot_url = info.friendbot_url.context("RPC reports no Friendbot")?;
        for name in [SPONSOR, SUBMITTER, FEE_SOURCE] {
            let key = self.profile.key(name)?;
            if !self.account_exists(&key.address()).await? {
                friendbot::fund(&friendbot_url, &key.address()).await?;
            }
        }
        let sponsor = self.profile.key(SPONSOR)?;
        let mut missing = Vec::new();
        for name in ROLES.into_iter().chain([USDC_RESERVE]) {
            let key = self.profile.key(name)?;
            if !self.account_exists(&key.address()).await? {
                missing.push(key);
            }
        }
        if !missing.is_empty() {
            let refs: Vec<&SecretKey> = missing.iter().collect();
            let receipt = onboard_buyers(
                &self.rpc,
                NETWORK,
                &sponsor,
                &refs,
                &usdc::circle_usdc(NETWORK),
                self.onboarding_policy(),
            )
            .await?;
            for provenance in &receipt.provenance {
                ensure!(
                    provenance.violations_for_sponsor(&sponsor.address()).is_empty(),
                    "role account {} is not fully sponsored",
                    provenance.buyer
                );
            }
        }
        let mut roles = serde_json::Map::new();
        for name in [SPONSOR, SUBMITTER, FEE_SOURCE, USDC_RESERVE].into_iter().chain(ROLES) {
            roles.insert(name.to_owned(), json!(self.profile.key(name)?.address().to_string()));
        }
        println!("{}", serde_json::to_string_pretty(&roles)?);
        Ok(())
    }

    fn submitter<'a>(&'a self, source: &'a SecretKey, fee_source: &'a SecretKey) -> Submitter<'a> {
        Submitter { rpc: &self.rpc, network: NETWORK, source, fee_source, policy: self.policy }
    }

    /// Uploads the Wasm and creates an instance whose constructor pins the
    /// roles, Circle testnet USDC and the limits.
    pub async fn deploy_prepaid(
        &self,
        wasm: &[u8],
        min_deposit: i128,
        max_charge: i128,
    ) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let usdc_contract = usdc::asset_contract_id(&usdc::circle_usdc(NETWORK), NETWORK);
        let roles = Roles {
            admin: self.profile.key("admin")?.address(),
            operator: self.profile.key("operator")?.address(),
            seller: self.profile.key("seller")?.address(),
            treasury: self.profile.key("treasury")?.address(),
            usdc: usdc_contract,
        };

        let wasm_hash = deploy::wasm_hash(wasm);
        let upload = deploy::upload(wasm)?;
        let auth = submitter.record_source_authorization(&upload).await?;
        let uploaded = submitter.submit(upload, auth).await?;

        let salt: [u8; 32] = random_bytes()?;
        let expected = deploy::contract_id(NETWORK, &source.address(), salt);
        let create = deploy::create(
            &source.address(),
            salt,
            wasm_hash,
            constructor_args(&roles, min_deposit, max_charge),
        )?;
        let auth = submitter.record_source_authorization(&create).await?;
        let created = submitter.submit(create, auth).await?;
        ensure!(
            created.return_value
                == Some(ScVal::Address(ScAddress::Contract(ContractId(Hash(expected))))),
            "network created {:?}, expected {}",
            created.return_value,
            usdc::contract_strkey(expected)
        );

        let deployment = Deployment {
            contract: usdc::contract_strkey(expected),
            wasm_sha256: hex_lower(&wasm_hash),
            usdc: usdc::contract_strkey(usdc_contract),
            admin: roles.admin.to_string(),
            operator: roles.operator.to_string(),
            seller: roles.seller.to_string(),
            treasury: roles.treasury.to_string(),
            min_deposit,
            max_charge,
        };
        self.profile.save_deployment(&deployment)?;
        evidence::write(
            &self.evidence_dir,
            "prepaid-deployment",
            json!({
                "criterion": "prepaid-ledger-deployment",
                "expected": "contract created from the recorded Wasm with its constructor pinning the roles, Circle testnet USDC and limits",
                "deployment": serde_json::to_value(&deployment)?,
                "upload": receipt_json(&uploaded),
                "create": receipt_json(&created),
            }),
        )
    }

    /// Creates buyers `1..=count` (sponsored, zero XLM, USDC trustline) and
    /// tops each up to `usdc_each` base units from the USDC reserve.
    pub async fn onboard_buyers(&self, count: u32, usdc_each: i64) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let sponsor = self.profile.key(SPONSOR)?;
        let reserve = self.profile.key(USDC_RESERVE)?;
        let asset = usdc::circle_usdc(NETWORK);
        let buyers: Vec<SecretKey> =
            (1..=count).map(|i| self.profile.buyer(i)).collect::<Result<_, _>>()?;

        let mut missing = Vec::new();
        for buyer in &buyers {
            if !self.account_exists(&buyer.address()).await? {
                missing.push(buyer);
            }
        }
        let mut onboarding_txs = Vec::new();
        for chunk in missing.chunks(MAX_BUYERS_PER_TRANSACTION) {
            let receipt = onboard_buyers(
                &self.rpc,
                NETWORK,
                &sponsor,
                chunk,
                &asset,
                self.onboarding_policy(),
            )
            .await?;
            for provenance in &receipt.provenance {
                ensure!(
                    provenance.violations_for_sponsor(&sponsor.address()).is_empty(),
                    "buyer {} is not fully sponsored: {:?}",
                    provenance.buyer,
                    provenance.violations_for_sponsor(&sponsor.address())
                );
            }
            onboarding_txs.push(hex_lower(&receipt.transaction_hash));
        }

        let mut top_ups = Vec::new();
        for buyer in &buyers {
            let current = balances(&self.rpc, &buyer.address(), &asset).await?.usdc.unwrap_or(0);
            if current < usdc_each {
                top_ups.push((buyer.address(), usdc_each - current));
            }
        }
        let mut payment_txs = Vec::new();
        for chunk in top_ups.chunks(100) {
            let sequence = self.sequence_of(&sponsor.address()).await?;
            let valid_until = unix_now() + self.policy.validity.as_secs();
            let tx = payments_transaction(
                &sponsor.address(),
                sequence + 1,
                &reserve.address(),
                chunk,
                &asset,
                self.policy.inclusion_fee,
                valid_until,
            )?;
            let hash = transaction::transaction_hash(&tx, NETWORK)?;
            let envelope = transaction::sign(tx, NETWORK, &[&sponsor, &reserve])?;
            submit_and_wait(&self.rpc, &envelope, hash, valid_until, self.policy.poll_interval)
                .await?;
            payment_txs.push(hex_lower(&hash));
        }

        let mut buyer_state = Vec::new();
        for buyer in &buyers {
            let b = balances(&self.rpc, &buyer.address(), &asset).await?;
            ensure!(
                b.xlm_stroops == Some(0),
                "buyer {} holds XLM: {:?}",
                buyer.address(),
                b.xlm_stroops
            );
            buyer_state.push(json!({ "buyer": buyer.address().to_string(), "usdc": b.usdc, "xlm_stroops": b.xlm_stroops }));
        }
        evidence::write(
            &self.evidence_dir,
            &format!("buyers-funded-{count}"),
            json!({
                "criterion": "buyers-funded-without-xlm",
                "expected": "each buyer exists with 0 XLM, sponsored reserves and a Circle USDC balance of at least the requested amount",
                "sponsor": sponsor.address().to_string(),
                "usdc_reserve": reserve.address().to_string(),
                "onboarding_transactions": onboarding_txs,
                "payment_transactions": payment_txs,
                "buyers": buyer_state,
            }),
        )
    }

    async fn sequence_of(&self, account: &AccountAddress) -> anyhow::Result<i64> {
        let key =
            LedgerKey::Account(LedgerKeyAccount { account_id: transaction::account_id(account) });
        let records = self.rpc.get_ledger_entries(&[key]).await?;
        records
            .iter()
            .find_map(|r| match &r.data {
                LedgerEntryData::Account(entry) => Some(entry.seq_num.0),
                _ => None,
            })
            .with_context(|| format!("account {account} does not exist"))
    }

    /// Signs `tree` for each signer and checks every entry against the
    /// prepared call before any fee can be spent.
    async fn authorize(
        &self,
        submitter: &Submitter<'_>,
        function: &HostFunction,
        signers: &[(&SecretKey, SorobanAuthorizedInvocation)],
    ) -> anyhow::Result<Vec<SorobanAuthorizationEntry>> {
        let expected: Vec<(AccountAddress, SorobanAuthorizedInvocation)> =
            signers.iter().map(|(key, tree)| (key.address(), tree.clone())).collect();
        let prepared = submitter
            .prepare_authorizations(
                function,
                &expected,
                AUTH_VALIDITY_LEDGERS,
                Credentials::AddressV2,
            )
            .await?;
        let network = network_id(NETWORK);
        prepared
            .entries
            .iter()
            .zip(signers)
            .map(|(entry, (key, tree))| {
                let signed = sign_entry(entry, network, &[key])?;
                verify_signed_entry(
                    &signed,
                    &key.address(),
                    tree,
                    network,
                    prepared.latest_ledger,
                    AUTH_VALIDITY_LEDGERS,
                )?;
                Ok(signed)
            })
            .collect()
    }

    async fn contract_balance(
        &self,
        submitter: &Submitter<'_>,
        pinned: &PrepaidDeployment,
        owner: &AccountAddress,
    ) -> anyhow::Result<i128> {
        let value =
            submitter.read(HostFunction::InvokeContract(pinned.get_balance_call(owner))).await?;
        value.as_ref().and_then(i128_of).context("get_balance returned no amount")
    }

    /// Deposits `amount` for each buyer in `first..=last`; each buyer signs
    /// only its authorization entry and holds no XLM.
    pub async fn deposit(&self, first: u32, last: u32, amount: i128) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let asset = usdc::circle_usdc(NETWORK);
        let treasury: AccountAddress = recorded.treasury.parse()?;
        let mut records = Vec::new();
        for index in first..=last {
            let buyer = self.profile.buyer(index)?;
            let owner = buyer.address();
            let before = balances(&self.rpc, &owner, &asset).await?;
            let treasury_before = balances(&self.rpc, &treasury, &asset).await?.usdc;
            let intent =
                DepositIntent { owner: owner.clone(), amount, deposit_id: random_bytes()? };
            let function = HostFunction::InvokeContract(pinned.deposit_call(&intent));
            let auth = self
                .authorize(
                    &submitter,
                    &function,
                    &[(&buyer, pinned.deposit_authorization(&intent))],
                )
                .await?;
            let receipt = submitter.submit(function, auth).await?;
            let after = balances(&self.rpc, &owner, &asset).await?;
            let treasury_after = balances(&self.rpc, &treasury, &asset).await?.usdc;
            let credited = self.contract_balance(&submitter, &pinned, &owner).await?;
            ensure!(after.xlm_stroops == Some(0), "buyer {owner} holds XLM after deposit");
            let mut record = receipt_json(&receipt);
            record["buyer"] = json!(owner.to_string());
            record["authorizer"] = json!(owner.to_string());
            record["deposit_id"] = json!(hex_lower(&intent.deposit_id));
            record["amount"] = json!(amount.to_string());
            record["observed"] = json!({
                "buyer_xlm_stroops_before": before.xlm_stroops,
                "buyer_xlm_stroops_after": after.xlm_stroops,
                "buyer_usdc_before": before.usdc,
                "buyer_usdc_after": after.usdc,
                "treasury_usdc_before": treasury_before,
                "treasury_usdc_after": treasury_after,
                "ledger_balance_after": credited.to_string(),
            });
            records.push(record);
        }
        let name = if first == last {
            format!("deposit-buyer-{first}")
        } else {
            format!("deposits-{first}-{last}")
        };
        evidence::write(
            &self.evidence_dir,
            &name,
            json!({
                "criterion": "sponsored-deposit",
                "expected": "USDC moves from the buyer to the separate treasury and the ledger credits the buyer, in one invocation; the buyer signs only its authorization entry and pays no fee; a separate fee account pays through a fee bump",
                "contract": recorded.contract,
                "treasury": recorded.treasury,
                "deposits": records,
            }),
        )
    }

    /// Charges `amount` from each buyer in `first..=last` in one
    /// `charge_batch` transaction. Each buyer's charge identifier is derived
    /// from `tag` and the buyer's number, so repeating a tag repeats the
    /// charges.
    pub async fn charge_batch(
        &self,
        first: u32,
        last: u32,
        tag: &str,
        amount: i128,
    ) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let operator = self.profile.key("operator")?;
        let submitter = self.submitter(&source, &fee_source);
        let last_ledger = self.rpc.get_latest_ledger().await? + CHARGE_VALIDITY_LEDGERS;
        let charges: Vec<ChargeRequest> = (first..=last)
            .map(|i| {
                Ok(ChargeRequest {
                    owner: self.profile.buyer(i)?.address(),
                    charge_id: tagged_charge_id(tag, i),
                    amount,
                    last_ledger,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        let function = HostFunction::InvokeContract(pinned.charge_batch_call(&charges));
        let auth = self
            .authorize(
                &submitter,
                &function,
                &[(&operator, pinned.charge_batch_authorization(&charges))],
            )
            .await?;
        let receipt = submitter.submit(function, auth).await?;
        let outcomes = outcomes_of(receipt.return_value.as_ref())?;
        let mut record = receipt_json(&receipt);
        record["criterion"] = json!("charge-batch");
        record["expected"] = json!(
            "one transaction settles every charge; each entry's outcome is returned and emitted"
        );
        record["contract"] = json!(recorded.contract);
        record["operator"] = json!(operator.address().to_string());
        record["entries"] = json!(charges.len());
        record["charge_tag"] = json!(tag);
        record["last_ledger"] = json!(last_ledger);
        record["amount"] = json!(amount.to_string());
        record["outcomes"] = json!(outcomes);
        evidence::write(
            &self.evidence_dir,
            &format!("charge-batch-{}-{tag}", charges.len()),
            record,
        )
    }

    /// A single `charge`; a refusal is reported with the simulation error and
    /// nothing is submitted.
    pub async fn charge(&self, buyer: u32, tag: &str, amount: i128) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let operator = self.profile.key("operator")?;
        let submitter = self.submitter(&source, &fee_source);
        let owner = self.profile.buyer(buyer)?.address();
        let last_ledger = self.rpc.get_latest_ledger().await? + CHARGE_VALIDITY_LEDGERS;
        let charge = ChargeRequest {
            owner: owner.clone(),
            charge_id: tagged_charge_id(tag, buyer),
            amount,
            last_ledger,
        };
        let function = HostFunction::InvokeContract(pinned.charge_call(&charge));
        let before = self.contract_balance(&submitter, &pinned, &charge.owner).await?;
        let outcome = match self
            .authorize(&submitter, &function, &[(&operator, pinned.charge_authorization(&charge))])
            .await
        {
            Ok(auth) => match submitter.submit(function, auth).await {
                Ok(receipt) => json!({ "result": "charged", "receipt": receipt_json(&receipt) }),
                Err(error) => json!({ "result": "refused", "error": format!("{error:#}") }),
            },
            Err(error) => json!({ "result": "refused", "error": format!("{error:#}") }),
        };
        let after = self.contract_balance(&submitter, &pinned, &charge.owner).await?;
        evidence::write(
            &self.evidence_dir,
            &format!("charge-buyer-{buyer}-{tag}"),
            json!({
                "criterion": "single-charge",
                "contract": recorded.contract,
                "buyer": owner.to_string(),
                "charge_tag": tag,
                "charge_id": hex_lower(&charge.charge_id),
                "amount": amount.to_string(),
                "outcome": outcome,
                "observed": { "ledger_balance_before": before.to_string(), "ledger_balance_after": after.to_string() },
            }),
        )
    }

    /// Compares the treasury's USDC balance with what the ledger owes
    /// (buyer liabilities plus unwithdrawn revenue), both read from the
    /// network.
    pub async fn solvency(&self) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let totals = submitter
            .read(HostFunction::InvokeContract(pinned.get_totals_call()))
            .await?
            .context("get_totals returned nothing")?;
        let Totals { liabilities, revenue } =
            contract_totals(&totals).with_context(|| format!("unexpected totals {totals:?}"))?;
        let held = balances(&self.rpc, &pinned.treasury, &usdc::circle_usdc(NETWORK))
            .await?
            .usdc
            .context("treasury has no USDC trustline")?;
        let owed = liabilities + revenue;
        evidence::write(
            &self.evidence_dir,
            "treasury-solvency",
            json!({
                "criterion": "treasury-solvency",
                "expected": "treasury USDC balance equals buyer liabilities plus unwithdrawn revenue when no USDC left the treasury outside the contract",
                "contract": recorded.contract,
                "treasury": recorded.treasury,
                "observed": {
                    "treasury_usdc": held.to_string(),
                    "liabilities": liabilities.to_string(),
                    "revenue": revenue.to_string(),
                    "owed": owed.to_string(),
                    "surplus": (i128::from(held) - owed).to_string(),
                },
            }),
        )
    }

    /// Withdraws `amount` of `buyer`'s credit back to the buyer's own
    /// account, authorized by the buyer and by the treasury.
    pub async fn withdraw(&self, buyer: u32, amount: i128) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let treasury = self.profile.key("treasury")?;
        let submitter = self.submitter(&source, &fee_source);
        let buyer_key = self.profile.buyer(buyer)?;
        let owner = buyer_key.address();
        let asset = usdc::circle_usdc(NETWORK);
        let intent = WithdrawIntent {
            owner: owner.clone(),
            amount,
            destination: owner.clone(),
            withdrawal_id: random_bytes()?,
        };
        let before = balances(&self.rpc, &owner, &asset).await?;
        let treasury_before = balances(&self.rpc, &treasury.address(), &asset).await?.usdc;
        let credit_before = self.contract_balance(&submitter, &pinned, &intent.owner).await?;
        let function = HostFunction::InvokeContract(pinned.withdraw_call(&intent));
        let auth = self
            .authorize(
                &submitter,
                &function,
                &[
                    (&buyer_key, pinned.owner_withdraw_authorization(&intent)),
                    (&treasury, pinned.treasury_withdraw_authorization(&intent)),
                ],
            )
            .await?;
        let receipt = submitter.submit(function, auth).await?;
        let after = balances(&self.rpc, &owner, &asset).await?;
        let treasury_after = balances(&self.rpc, &treasury.address(), &asset).await?.usdc;
        let credit_after = self.contract_balance(&submitter, &pinned, &intent.owner).await?;
        let mut record = receipt_json(&receipt);
        record["criterion"] = json!("treasury-withdrawal");
        record["expected"] = json!(
            "the buyer's unused credit is debited and the same USDC moves from the treasury to the buyer in one invocation, authorized by both"
        );
        record["contract"] = json!(recorded.contract);
        record["buyer"] = json!(owner.to_string());
        record["treasury"] = json!(treasury.address().to_string());
        record["amount"] = json!(amount.to_string());
        record["observed"] = json!({
            "buyer_usdc_before": before.usdc,
            "buyer_usdc_after": after.usdc,
            "treasury_usdc_before": treasury_before,
            "treasury_usdc_after": treasury_after,
            "ledger_balance_before": credit_before.to_string(),
            "ledger_balance_after": credit_after.to_string(),
            "buyer_xlm_stroops_after": after.xlm_stroops,
        });
        evidence::write(&self.evidence_dir, &format!("withdraw-buyer-{buyer}"), record)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
