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
use fermah_pay_stellar_chain::multisig::{
    AccountSigners, Policy as MultisigPolicy, account_signers, set_signers_transaction,
};
use fermah_pay_stellar_chain::onboarding::{
    MAX_BUYERS_PER_TRANSACTION, SubmissionPolicy, onboard_buyers,
};
use fermah_pay_stellar_chain::payments::payments_transaction;
use fermah_pay_stellar_chain::prepaid::{
    ChargeRequest, DepositIntent, PrepaidDeployment, Roles, Totals, WithdrawIntent,
    constructor_args, contract_totals, instance_wasm,
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
mod load;

pub use load::LoadShape;

const NETWORK: Network = Network::Testnet;
const SPONSOR: &str = "operator-sponsor";
const SUBMITTER: &str = "submitter";
const FEE_SOURCE: &str = "fee-source";
const USDC_RESERVE: &str = "usdc-reserve";
/// A further source account for the settlement worker, sponsored with zero
/// XLM like the roles: fee bumps pay for everything it sends.
const CHANNEL: &str = "channel-1";
/// The treasury's cold reserve: two of its three keys move anything.
const COLD_RESERVE: &str = "cold-reserve";
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

const OUTCOMES: [&str; 7] = [
    "Charged",
    "InsufficientBalance",
    "AboveLimit",
    "Duplicate",
    "Expired",
    "UnknownAccount",
    "AboveDailyLimit",
];

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
        for name in ROLES.into_iter().chain([USDC_RESERVE, CHANNEL]) {
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
        for name in [SPONSOR, SUBMITTER, FEE_SOURCE, USDC_RESERVE, CHANNEL].into_iter().chain(ROLES)
        {
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

    /// Gives the admin account a two-of-three policy: its master key and two
    /// further signers weigh one each, and a contract call, a payment or a
    /// change of signers needs two. The sponsor pays the fee and the
    /// signers' reserves, so the admin still holds no XLM. Running it again
    /// on an account that already has the policy changes nothing.
    pub async fn admin_multisig(&self) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let admin = self.profile.key("admin")?;
        let cosigners = [self.profile.key("admin-signer-1")?, self.profile.key("admin-signer-2")?];
        let (transaction_hash, after, xlm) = self.two_of_three(&admin, &cosigners).await?;
        evidence::write(
            &self.evidence_dir,
            "admin-multisig",
            json!({
                "criterion": "admin-multisig",
                "expected": "the admin account needs two of its three keys for any contract call, payment or change of signers, and still holds no XLM",
                "admin": admin.address().to_string(),
                "transaction_hash": transaction_hash,
                "observed": signers_json(&after, xlm, "admin_xlm_stroops"),
            }),
        )
    }

    /// Creates the treasury's cold reserve if it is missing: an account with
    /// a Circle USDC trustline, sponsored like the roles so it holds no XLM,
    /// that needs two of its three keys to move anything. The worker never
    /// holds any of them.
    pub async fn cold_reserve(&self) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let sponsor = self.profile.key(SPONSOR)?;
        let reserve = self.profile.key(COLD_RESERVE)?;
        let mut onboarding = None;
        if !self.account_exists(&reserve.address()).await? {
            let receipt = onboard_buyers(
                &self.rpc,
                NETWORK,
                &sponsor,
                &[&reserve],
                &usdc::circle_usdc(NETWORK),
                self.onboarding_policy(),
            )
            .await?;
            for provenance in &receipt.provenance {
                ensure!(
                    provenance.violations_for_sponsor(&sponsor.address()).is_empty(),
                    "the cold reserve is not fully sponsored"
                );
            }
            onboarding = Some(hex_lower(&receipt.transaction_hash));
        }
        let cosigners = [self.profile.key("cold-signer-1")?, self.profile.key("cold-signer-2")?];
        let (transaction_hash, after, xlm) = self.two_of_three(&reserve, &cosigners).await?;
        let usdc = balances(&self.rpc, &reserve.address(), &usdc::circle_usdc(NETWORK))
            .await?
            .usdc
            .context("the cold reserve has no USDC trustline")?;
        evidence::write(
            &self.evidence_dir,
            "cold-reserve",
            json!({
                "criterion": "cold-reserve",
                "expected": "the treasury's cold reserve holds a Circle USDC trustline, needs two of its three keys to move anything, and holds no XLM",
                "reserve": reserve.address().to_string(),
                "onboarding_transaction": onboarding,
                "signers_transaction": transaction_hash,
                "observed": {
                    "usdc": usdc,
                    "policy": signers_json(&after, xlm, "reserve_xlm_stroops"),
                },
            }),
        )
    }

    /// Gives `account` its own key and `cosigners` one weight each, and
    /// requires two of the three for anything medium or high; the sponsor
    /// pays for the signers' reserves. Returns the transaction's hash (none
    /// if the policy was in place already), the policy as the ledger holds
    /// it, and the account's XLM, which must be zero.
    async fn two_of_three(
        &self,
        account: &SecretKey,
        cosigners: &[SecretKey; 2],
    ) -> anyhow::Result<(Option<String>, AccountSigners, Option<i64>)> {
        let sponsor = self.profile.key(SPONSOR)?;
        let policy = MultisigPolicy {
            signers: cosigners.iter().map(|key| (key.address(), 1)).collect(),
            master_weight: 1,
            low: 1,
            medium: 2,
            high: 2,
        };
        let matches = |current: &AccountSigners| {
            (current.master_weight, current.low, current.medium, current.high)
                == (policy.master_weight, policy.low, policy.medium, policy.high)
                && policy.signers.iter().all(|signer| current.signers.contains(signer))
        };
        let before = account_signers(&self.rpc, &account.address())
            .await?
            .with_context(|| format!("{} does not exist", account.address()))?;
        let transaction_hash = if matches(&before) {
            None
        } else {
            let sequence = self.sequence_of(&sponsor.address()).await?;
            let valid_until = unix_now() + self.policy.validity.as_secs();
            let tx = set_signers_transaction(
                &sponsor.address(),
                sequence + 1,
                &account.address(),
                &policy,
                self.policy.inclusion_fee,
                valid_until,
            );
            let hash = transaction::transaction_hash(&tx, NETWORK)?;
            let envelope = transaction::sign(tx, NETWORK, &[&sponsor, account])?;
            submit_and_wait(&self.rpc, &envelope, hash, valid_until, self.policy.poll_interval)
                .await?;
            Some(hex_lower(&hash))
        };
        let after = account_signers(&self.rpc, &account.address())
            .await?
            .with_context(|| format!("{} does not exist", account.address()))?;
        ensure!(matches(&after), "the account's policy is not the requested one: {after:?}");
        let xlm =
            balances(&self.rpc, &account.address(), &usdc::circle_usdc(NETWORK)).await?.xlm_stroops;
        ensure!(xlm == Some(0), "{} holds XLM: {xlm:?}", account.address());
        Ok((transaction_hash, after, xlm))
    }

    /// Moves `amount` of the treasury's USDC to the cold reserve with the
    /// same call and treasury authorization the settlement worker's sweep
    /// builds.
    pub async fn sweep(&self, amount: i128) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let treasury = self.profile.key("treasury")?;
        let reserve = self.profile.key(COLD_RESERVE)?.address();
        let submitter = self.submitter(&source, &fee_source);
        let asset = usdc::circle_usdc(NETWORK);
        let hot_before = balances(&self.rpc, &treasury.address(), &asset).await?.usdc;
        let cold_before = balances(&self.rpc, &reserve, &asset).await?.usdc;
        let function =
            HostFunction::InvokeContract(pinned.treasury_transfer_call(&reserve, amount));
        let auth = self
            .authorize(
                &submitter,
                &function,
                &[(&treasury, pinned.treasury_transfer_authorization(&reserve, amount))],
            )
            .await?;
        let receipt = submitter.submit(function, auth).await?;
        let hot_after = balances(&self.rpc, &treasury.address(), &asset).await?.usdc;
        let cold_after = balances(&self.rpc, &reserve, &asset).await?.usdc;
        let moved = |before: Option<i64>, after: Option<i64>| after.zip(before).map(|(a, b)| a - b);
        ensure!(
            moved(cold_before, cold_after) == Some(i64::try_from(amount)?),
            "the reserve's USDC went from {cold_before:?} to {cold_after:?}"
        );
        let mut record = receipt_json(&receipt);
        record["criterion"] = json!("treasury-sweep");
        record["expected"] =
            json!("the treasury's key alone moves USDC from the treasury to its cold reserve");
        record["contract"] = json!(recorded.contract);
        record["treasury"] = json!(treasury.address().to_string());
        record["reserve"] = json!(reserve.to_string());
        record["amount"] = json!(amount.to_string());
        record["observed"] = json!({
            "treasury_usdc_before": hot_before,
            "treasury_usdc_after": hot_after,
            "reserve_usdc_before": cold_before,
            "reserve_usdc_after": cold_after,
        });
        evidence::write(&self.evidence_dir, "treasury-sweep", record)
    }

    /// Replaces the recorded contract's code with `wasm`, authorized by the
    /// admin, and checks on the ledger that the instance now runs it and that
    /// its totals are unchanged.
    pub async fn upgrade_prepaid(&self, wasm: &[u8]) -> anyhow::Result<()> {
        self.rpc.verify_network(NETWORK).await?;
        let (mut recorded, pinned) = self.deployment()?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let admin = self.profile.key("admin")?;
        let submitter = self.submitter(&source, &fee_source);
        let wasm_hash = deploy::wasm_hash(wasm);
        let before = self.running_wasm(&pinned).await?;
        ensure!(before != wasm_hash, "the contract already runs {}", hex_lower(&wasm_hash));
        let totals_before = self.totals(&submitter, &pinned).await?;

        let upload = deploy::upload(wasm)?;
        let auth = submitter.record_source_authorization(&upload).await?;
        let uploaded = submitter.submit(upload, auth).await?;
        let function = HostFunction::InvokeContract(pinned.upgrade_call(wasm_hash));
        let auth = self
            .admin_authorization(&submitter, &function, pinned.upgrade_authorization(wasm_hash))
            .await?;
        let upgraded = submitter.submit(function, vec![auth]).await?;

        let after = self.running_wasm(&pinned).await?;
        ensure!(after == wasm_hash, "the contract runs {} after the upgrade", hex_lower(&after));
        let totals_after = self.totals(&submitter, &pinned).await?;
        ensure!(totals_after == totals_before, "the upgrade changed the contract's totals");
        recorded.wasm_sha256 = hex_lower(&wasm_hash);
        self.profile.save_deployment(&recorded)?;
        evidence::write(
            &self.evidence_dir,
            "prepaid-upgrade",
            json!({
                "criterion": "prepaid-ledger-upgrade",
                "expected": "the admin replaces the contract's code in place; the contract address, balances and totals are unchanged",
                "contract": recorded.contract,
                "admin": admin.address().to_string(),
                "upload": receipt_json(&uploaded),
                "upgrade": receipt_json(&upgraded),
                "observed": {
                    "wasm_before": hex_lower(&before),
                    "wasm_after": hex_lower(&after),
                    "liabilities": totals_after.liabilities.to_string(),
                    "revenue": totals_after.revenue.to_string(),
                },
            }),
        )
    }

    /// The admin's authorization of `function`, signed by the admin key and,
    /// once the admin account needs two signatures (`admin-multisig`), by
    /// its first co-signer as well.
    async fn admin_authorization(
        &self,
        submitter: &Submitter<'_>,
        function: &HostFunction,
        tree: SorobanAuthorizedInvocation,
    ) -> anyhow::Result<SorobanAuthorizationEntry> {
        let admin = self.profile.key("admin")?;
        let mut keys = vec![admin];
        if self.profile.has_key("admin-signer-1") {
            keys.push(self.profile.key("admin-signer-1")?);
        }
        let prepared = submitter
            .prepare_authorizations(
                function,
                &[(keys[0].address(), tree)],
                AUTH_VALIDITY_LEDGERS,
                Credentials::AddressV2,
            )
            .await?;
        let entry = prepared.entries.first().context("no authorization prepared")?;
        let payload = fermah_pay_stellar_chain::authorization::signature_payload(
            network_id(NETWORK),
            &entry.credentials,
            &entry.root_invocation,
        )?;
        let signatures =
            keys.iter().map(|key| (*key.address().public_key(), key.sign_raw(&payload))).collect();
        Ok(fermah_pay_stellar_chain::authorization::attach_signatures(
            entry,
            network_id(NETWORK),
            signatures,
        )?)
    }

    async fn running_wasm(&self, pinned: &PrepaidDeployment) -> anyhow::Result<[u8; 32]> {
        let entries = self.rpc.get_ledger_entries(&[pinned.instance_key()]).await?;
        entries
            .first()
            .and_then(|entry| instance_wasm(&entry.data))
            .context("the contract instance is missing or runs no Wasm")
    }

    async fn totals(
        &self,
        submitter: &Submitter<'_>,
        pinned: &PrepaidDeployment,
    ) -> anyhow::Result<Totals> {
        let totals = submitter
            .read(HostFunction::InvokeContract(pinned.get_totals_call()))
            .await?
            .context("get_totals returned nothing")?;
        contract_totals(&totals).with_context(|| format!("unexpected totals {totals:?}"))
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
        let hot = balances(&self.rpc, &pinned.treasury, &usdc::circle_usdc(NETWORK))
            .await?
            .usdc
            .context("treasury has no USDC trustline")?;
        // Swept to the cold reserve, it is still the treasury's.
        let cold = if self.profile.has_key(COLD_RESERVE) {
            let reserve = self.profile.key(COLD_RESERVE)?.address();
            balances(&self.rpc, &reserve, &usdc::circle_usdc(NETWORK)).await?.usdc.unwrap_or(0)
        } else {
            0
        };
        let held = hot + cold;
        let owed = liabilities + revenue;
        evidence::write(
            &self.evidence_dir,
            "treasury-solvency",
            json!({
                "criterion": "treasury-solvency",
                "expected": "the USDC of the treasury and its cold reserve equals buyer liabilities plus unwithdrawn revenue when no USDC left them outside the contract",
                "contract": recorded.contract,
                "treasury": recorded.treasury,
                "observed": {
                    "treasury_usdc": hot.to_string(),
                    "reserve_usdc": cold.to_string(),
                    "held": held.to_string(),
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

fn signers_json(policy: &AccountSigners, xlm: Option<i64>, xlm_field: &str) -> serde_json::Value {
    let mut observed = json!({
        "master_weight": policy.master_weight,
        "signers": policy.signers.iter().map(|(k, w)| json!({ "key": k.to_string(), "weight": w })).collect::<Vec<_>>(),
        "thresholds": { "low": policy.low, "medium": policy.medium, "high": policy.high },
    });
    observed[xlm_field] = json!(xlm);
    observed
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Context {
    /// Sends a transaction on testnet signed by the key `reference` names,
    /// such as a key in AWS KMS: the account is funded by Friendbot if it is
    /// missing, and the transaction (a sequence bump, which moves nothing)
    /// is signed through the same path the settlement worker signs source
    /// accounts with. Writes a `key-signature` evidence record.
    pub async fn key_check(&self, reference: &str) -> anyhow::Result<()> {
        use fermah_pay_stellar_chain::stellar_xdr::{
            BumpSequenceOp, Memo, Operation, OperationBody, Preconditions, SequenceNumber,
            TimeBounds, TimePoint, Transaction as Tx, TransactionExt, VecM,
        };
        use fermah_pay_stellar_chain::transaction::muxed_account;

        let info = self.rpc.verify_network(NETWORK).await?;
        let signer = fermah_pay_stellar_gateway::signing::open(reference).await?;
        let account = signer.address();
        let mut funded = false;
        if !self.account_exists(&account).await? {
            let friendbot = info.friendbot_url.context("RPC reports no Friendbot")?;
            friendbot::fund(&friendbot, &account).await?;
            funded = true;
        }
        let sequence = self.sequence_of(&account).await?;
        let valid_until = unix_now() + self.policy.validity.as_secs();
        let tx = Tx {
            source_account: muxed_account(&account),
            fee: self.policy.inclusion_fee,
            seq_num: SequenceNumber(sequence + 1),
            cond: Preconditions::Time(TimeBounds {
                min_time: TimePoint(0),
                max_time: TimePoint(valid_until),
            }),
            memo: Memo::None,
            operations: VecM::try_from(vec![Operation {
                source_account: None,
                body: OperationBody::BumpSequence(BumpSequenceOp { bump_to: SequenceNumber(0) }),
            }])?,
            ext: TransactionExt::V0,
        };
        let hash = transaction::transaction_hash(&tx, NETWORK)?;
        let envelope = transaction::sign_with(tx, NETWORK, &[signer.as_ref()]).await?;
        submit_and_wait(&self.rpc, &envelope, hash, valid_until, self.policy.poll_interval).await?;
        let after = self.sequence_of(&account).await?;
        ensure!(after == sequence + 1, "the account's sequence is {after}, not {}", sequence + 1);
        let transaction_hash = hex_lower(&hash);
        evidence::write(
            &self.evidence_dir,
            "key-signature",
            json!({
                "criterion": "key-signature",
                "expected": "a transaction signed through the key reference is accepted by testnet for the account the key's public key encodes",
                "key_service": reference.split_once("://").map(|(scheme, _)| scheme),
                "account": account.to_string(),
                "funded_by_friendbot": funded,
                "transaction_hash": transaction_hash,
                "public_explorer_url": evidence::tx_url(&transaction_hash),
                "observed": { "sequence_before": sequence, "sequence_after": after },
            }),
        )
    }
}
