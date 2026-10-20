//! A conformance harness for the x402 facilitator interface. It treats the
//! interface as a black box over HTTP, as a third-party facilitator would:
//! it signs commitments as the buyer, a classic account or a contract
//! account, calls `/supported`, `/verify`,
//! `/settle` and `/settlements`, checks every answer against the protocol
//! and the documented binding, and then checks the settlement on-chain
//! through an RPC node of its own choosing. Every request and response is
//! logged, without the API key.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fermah_pay_stellar_chain::authorization::signature_payload;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::network_id;
use fermah_pay_stellar_chain::prepaid::{Outcome, settled_in, x402_commitment_entry};
use fermah_pay_stellar_chain::rpc::{RpcClient, TransactionStatus, hex_lower, http_client};
use fermah_pay_stellar_chain::sep53;
use fermah_pay_stellar_chain::stellar_xdr::{
    BytesM, Limits, ScBytes, ScVal, SorobanCredentials, WriteXdr,
};
use fermah_pay_stellar_domain::{ChainAddress, Network};
use fermah_pay_stellar_gateway::x402::commitment_message;
use serde_json::{Value, json};

/// The paid amount each case presents, in USDC base units.
const AMOUNT: i64 = 50_000;
const MAX_TIMEOUT_SECONDS: i64 = 300;
const SIGNATURE: &str = "invalid_batch_settlement_stellar_signature";

pub struct Target {
    /// Where the interface is served, e.g. `http://127.0.0.1:8402`.
    pub endpoint: String,
    pub api_key: zeroize::Zeroizing<String>,
    pub network: Network,
    /// The seller's prepaid ledger contract (`payTo`) and its USDC asset
    /// contract, as `C...` addresses.
    pub pay_to: String,
    pub asset: String,
    /// How long to wait for the settlement to land on-chain.
    pub settlement_timeout: Duration,
}

/// The buyer the harness pays as.
pub enum Buyer {
    /// A classic account, which signs commitments with its key (SEP-53).
    Account(SecretKey),
    /// A contract account authorized by one Ed25519 key, as
    /// `contracts/example-account` is: it signs an authorization entry for
    /// the commitment verifier, and only the account's `__check_auth`
    /// interprets the signature.
    ContractAccount { wallet: [u8; 32], key: SecretKey },
}

impl Buyer {
    #[must_use]
    pub fn address(&self) -> ChainAddress {
        match self {
            Self::Account(key) => key.address().into(),
            Self::ContractAccount { wallet, .. } => ChainAddress::Contract(*wallet),
        }
    }
}

/// How the harness proves a commitment for its buyer: what a contract
/// account's entry needs besides the terms.
struct Signing {
    network_id: [u8; 32],
    /// The verifier `/supported` names; only a contract account needs it.
    verifier: Option<[u8; 32]>,
    /// The last ledger a contract account's signature is valid in.
    expiration_ledger: u32,
}

/// The payload field that proves a commitment, and its value.
type Proof = (&'static str, String);

/// What a case expects from `/verify` or `/settle`.
enum Expect {
    Valid,
    Invalid(&'static str),
}

/// Terms a commitment is signed for and presented with.
#[derive(Clone)]
struct Terms {
    network: String,
    asset: String,
    pay_to: String,
    amount: String,
    payer: String,
    commitment: String,
    valid_until: String,
}

struct Harness<'a> {
    target: &'a Target,
    http: reqwest::Client,
    log: Vec<Value>,
    cases: Vec<Value>,
}

fn fresh_commitment() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(hex_lower(&bytes))
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn message(terms: &Terms) -> String {
    commitment_message(
        &terms.network,
        &terms.asset,
        &terms.pay_to,
        &terms.amount,
        &terms.payer,
        &terms.commitment,
        &terms.valid_until,
    )
}

/// A classic account's proof of `terms`: its SEP-53 signature by `key`.
fn sign(key: &SecretKey, terms: &Terms) -> Proof {
    ("signature", STANDARD.encode(sep53::sign(key, message(terms).as_bytes())))
}

impl Signing {
    /// `buyer`'s proof of `terms`, signed by `key`: the buyer's own key, or
    /// another to show it is refused.
    fn prove(&self, buyer: &Buyer, key: &SecretKey, terms: &Terms) -> anyhow::Result<Proof> {
        let Buyer::ContractAccount { wallet, .. } = buyer else { return Ok(sign(key, terms)) };
        let verifier = self.verifier.context("/supported names no commitment verifier")?;
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce)?;
        let mut entry = x402_commitment_entry(
            verifier,
            &ChainAddress::Contract(*wallet),
            &message(terms),
            i64::from_le_bytes(nonce),
            self.expiration_ledger,
        );
        let payload =
            signature_payload(self.network_id, &entry.credentials, &entry.root_invocation)?;
        let SorobanCredentials::Address(creds) = &mut entry.credentials else {
            bail!("the entry carries no address credentials");
        };
        creds.signature = ScVal::Bytes(ScBytes(BytesM::try_from(key.sign_raw(&payload).to_vec())?));
        Ok(("authorizationEntry", entry.to_xdr_base64(Limits::none())?))
    }
}

fn requirements(terms: &Terms) -> Value {
    json!({
        "scheme": "batch-settlement",
        "network": terms.network,
        "amount": terms.amount,
        "asset": terms.asset,
        "payTo": terms.pay_to,
        "maxTimeoutSeconds": MAX_TIMEOUT_SECONDS,
    })
}

/// The protocol's request body for `terms`, with `proof`.
fn body(terms: &Terms, proof: &Proof) -> Value {
    let mut payload = json!({
        "payer": terms.payer,
        "commitment": terms.commitment,
        "validUntil": terms.valid_until,
    });
    payload[proof.0] = json!(proof.1);
    json!({
        "x402Version": 2,
        "paymentPayload": {
            "x402Version": 2,
            "resource": { "url": "https://seller.example/api/answer" },
            "accepted": requirements(terms),
            "payload": payload,
        },
        "paymentRequirements": requirements(terms),
    })
}

impl<'a> Harness<'a> {
    async fn call(
        &mut self,
        case: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
        api_key: Option<&str>,
    ) -> anyhow::Result<(u16, Value)> {
        let url = format!("{}{path}", self.target.endpoint);
        let mut request = match body {
            Some(body) => self.http.post(&url).json(body),
            None => self.http.get(&url),
        };
        if let Some(key) = api_key {
            request = request.bearer_auth(key);
        }
        let started = Instant::now();
        let response = request.send().await.with_context(|| format!("{method} {path}"))?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let reply: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        self.log.push(json!({
            "case": case,
            "request": {
                "method": method,
                "path": path,
                "authorization": api_key.map(|_| "Bearer <redacted>"),
                "body": body,
            },
            "response": { "status": status, "body": reply },
            "elapsed_ms": started.elapsed().as_millis(),
        }));
        Ok((status, reply))
    }

    fn verdict(&mut self, case: &str, expected: String, observed: &Value, passed: bool) {
        self.cases.push(json!({
            "case": case,
            "expected": expected,
            "observed": observed,
            "passed": passed,
        }));
    }

    /// Presents `request` to `path` with the target's key and checks the
    /// answer against `expect`.
    async fn present(
        &mut self,
        case: &str,
        path: &str,
        request: &Value,
        expect: Expect,
        payer: &str,
    ) -> anyhow::Result<Value> {
        let key = self.target.api_key.clone();
        let (status, reply) = self.call(case, "POST", path, Some(request), Some(&key)).await?;
        let (flag, reason) = if path == "/verify" {
            ("isValid", "invalidReason")
        } else {
            ("success", "errorReason")
        };
        let (expected, passed) = match expect {
            Expect::Valid => (
                format!("HTTP 200, {flag} true, payer {payer}"),
                status == 200 && reply[flag] == true && reply["payer"] == payer,
            ),
            Expect::Invalid(want) => (
                format!("HTTP 200, {flag} false, {reason} {want}"),
                status == 200 && reply[flag] == false && reply[reason] == want,
            ),
        };
        self.verdict(case, expected, &json!({ "status": status, "body": reply }), passed);
        Ok(reply)
    }
}

/// Runs every case against `target` with `buyer`, a buyer of the seller
/// deployment whose available balance covers a few small payments, and
/// checks the settlement on-chain through `rpc`. Returns the record:
/// every request and response, and a verdict per case.
pub async fn run(target: &Target, buyer: &Buyer, rpc: &RpcClient) -> anyhow::Result<Value> {
    let mut h = Harness {
        target,
        http: http_client(Duration::from_secs(30))?,
        log: Vec::new(),
        cases: Vec::new(),
    };
    let network = target.network.caip2().to_owned();
    let payer = buyer.address().to_string();
    let own_key = match buyer {
        Buyer::Account(key) | Buyer::ContractAccount { key, .. } => key,
    };
    let terms = |amount: i64, valid_until: i64| -> anyhow::Result<Terms> {
        Ok(Terms {
            network: network.clone(),
            asset: target.asset.clone(),
            pay_to: target.pay_to.clone(),
            amount: amount.to_string(),
            payer: payer.clone(),
            commitment: fresh_commitment()?,
            valid_until: valid_until.to_string(),
        })
    };
    let soon = || now() + 120;

    // ---- discovery
    let (status, supported) = h.call("supported", "GET", "/supported", None, None).await?;
    let served = supported["kinds"].as_array().is_some_and(|kinds| {
        kinds.iter().any(|kind| {
            kind["x402Version"] == 2
                && kind["scheme"] == "batch-settlement"
                && kind["network"] == network.as_str()
        })
    });
    let shaped = supported["extensions"].is_array() && supported["signers"].is_object();
    h.verdict(
        "supported",
        format!(
            "HTTP 200 listing x402 v2 batch-settlement on {network}, with extensions and signers"
        ),
        &json!({ "status": status, "body": supported }),
        status == 200 && served && shaped,
    );
    let verifier = supported["kinds"].as_array().and_then(|kinds| {
        kinds.iter().find_map(|kind| {
            let address = kind["extra"]["commitmentVerifier"].as_str()?;
            stellar_strkey::Contract::from_string(address).ok().map(|contract| contract.0)
        })
    });
    // A contract account's signature stays valid for the whole run, about
    // an hour of ledgers.
    let signing = Signing {
        network_id: network_id(target.network),
        verifier,
        expiration_ledger: rpc.get_latest_ledger().await?.saturating_add(720),
    };
    let contract_payer = matches!(buyer, Buyer::ContractAccount { .. });
    if contract_payer {
        h.verdict(
            "supported_commitment_verifier",
            "a kind names extra.commitmentVerifier, the contract a contract account authorizes"
                .to_owned(),
            &supported,
            verifier.is_some(),
        );
        if verifier.is_none() {
            bail!("the interface names no commitment verifier, so contract accounts cannot pay");
        }
    }
    let prove = |terms: &Terms| signing.prove(buyer, own_key, terms);

    // ---- authentication
    let valid = terms(AMOUNT, soon())?;
    let request = body(&valid, &prove(&valid)?);
    for (case, key) in [("no_api_key", None), ("unknown_api_key", Some("fps_test_unknown"))] {
        let (status, reply) = h.call(case, "POST", "/verify", Some(&request), key).await?;
        h.verdict(
            case,
            "HTTP 401, unauthenticated".to_owned(),
            &json!({ "status": status, "body": reply }),
            status == 401,
        );
    }

    // ---- verification refusals, each differing from a valid request in one
    // property
    let mut refusals: Vec<(&str, Value, Expect)> = Vec::new();
    let mut altered = valid.clone();
    altered.amount = (AMOUNT + 1).to_string();
    let signed_valid = prove(&valid)?;
    refusals.push(("altered_amount", body(&altered, &signed_valid), Expect::Invalid(SIGNATURE)));
    let mut altered = valid.clone();
    altered.commitment = fresh_commitment()?;
    refusals.push((
        "altered_commitment",
        body(&altered, &signed_valid),
        Expect::Invalid(SIGNATURE),
    ));
    let mut other_pay_to = valid.clone();
    other_pay_to.pay_to = target.asset.clone();
    refusals.push((
        "other_pay_to",
        body(&other_pay_to, &prove(&other_pay_to)?),
        Expect::Invalid("invalid_payment_requirements"),
    ));
    let mut other_asset = valid.clone();
    other_asset.asset = target.pay_to.clone();
    refusals.push((
        "other_asset",
        body(&other_asset, &prove(&other_asset)?),
        Expect::Invalid("invalid_payment_requirements"),
    ));
    let mut accepted_differs = request.clone();
    accepted_differs["paymentPayload"]["accepted"]["amount"] = json!((AMOUNT * 2).to_string());
    refusals.push((
        "accepted_differs",
        accepted_differs,
        Expect::Invalid("invalid_payment_requirements"),
    ));
    let mut other_network = valid.clone();
    other_network.network =
        if target.network == Network::Pubnet { "stellar:testnet" } else { "stellar:pubnet" }
            .to_owned();
    refusals.push((
        "other_network",
        body(&other_network, &prove(&other_network)?),
        Expect::Invalid("invalid_network"),
    ));
    let mut version_one = request.clone();
    version_one["x402Version"] = json!(1);
    version_one["paymentPayload"]["x402Version"] = json!(1);
    refusals.push(("x402_version_1", version_one, Expect::Invalid("invalid_x402_version")));
    let mut exact = request.clone();
    exact["paymentRequirements"]["scheme"] = json!("exact");
    exact["paymentPayload"]["accepted"]["scheme"] = json!("exact");
    refusals.push(("scheme_exact", exact, Expect::Invalid("unsupported_scheme")));
    let mut bad_signature = request.clone();
    bad_signature["paymentPayload"]["payload"][signed_valid.0] = json!("AAAA");
    refusals.push(("malformed_signature", bad_signature, Expect::Invalid("invalid_payload")));
    let stranger = SecretKey::generate()?;
    if contract_payer {
        // Signed by a key the account does not hold: only the account's
        // `__check_auth`, run by the network, can tell.
        refusals.push((
            "entry_signed_by_another_key",
            body(&valid, &signing.prove(buyer, &stranger, &valid)?),
            Expect::Invalid(SIGNATURE),
        ));
        // A classic account's proof, for a payer that is a contract.
        refusals.push((
            "signature_for_contract_payer",
            body(&valid, &sign(own_key, &valid)),
            Expect::Invalid("invalid_payload"),
        ));
    }
    let mut unknown = valid.clone();
    unknown.payer = stranger.address().to_string();
    refusals.push((
        "unknown_payer",
        body(&unknown, &sign(&stranger, &unknown)),
        Expect::Invalid("invalid_batch_settlement_stellar_unknown_payer"),
    ));
    let expired = terms(AMOUNT, now() - 60)?;
    refusals.push((
        "expired",
        body(&expired, &prove(&expired)?),
        Expect::Invalid("invalid_batch_settlement_stellar_expired"),
    ));
    let far = terms(AMOUNT, now() + MAX_TIMEOUT_SECONDS + 600)?;
    refusals.push((
        "valid_until_too_far",
        body(&far, &prove(&far)?),
        Expect::Invalid("invalid_batch_settlement_stellar_valid_until_too_far"),
    ));
    let rich = terms(1_000_000_000_000_000, soon())?;
    refusals.push((
        "insufficient_funds",
        body(&rich, &prove(&rich)?),
        Expect::Invalid("insufficient_funds"),
    ));
    for (case, request, expect) in refusals {
        h.present(case, "/verify", &request, expect, &payer).await?;
    }
    let key = target.api_key.clone();
    let (status, reply) = h
        .call("unparseable_body", "POST", "/verify", Some(&json!({ "x402Version": 2 })), Some(&key))
        .await?;
    h.verdict(
        "unparseable_body",
        "HTTP 400, invalid_payload".to_owned(),
        &json!({ "status": status, "body": reply }),
        status == 400 && reply["invalidReason"] == "invalid_payload",
    );

    // ---- the payment: verify, settle, settle again, reuse
    h.present("verify", "/verify", &request, Expect::Valid, &payer).await?;
    let settled = h.present("settle", "/settle", &request, Expect::Valid, &payer).await?;
    let ledger = &settled["extensions"]["prepaidLedger"];
    let identified = settled["transaction"] == valid.commitment.as_str()
        && settled["network"] == network.as_str()
        && settled["amount"] == valid.amount.as_str()
        && ledger["replayed"] == false
        && ledger["chargeId"].is_string();
    h.verdict(
        "settle_identifier",
        "transaction is the commitment; network, amount and the charge in extensions.prepaidLedger"
            .to_owned(),
        &settled,
        identified,
    );
    let again = h.present("settle_again", "/settle", &request, Expect::Valid, &payer).await?;
    let same = again["extensions"]["prepaidLedger"]["replayed"] == true
        && again["extensions"]["prepaidLedger"]["chargeId"] == ledger["chargeId"];
    h.verdict(
        "settle_again_same_charge",
        "replayed true, the same charge".to_owned(),
        &again,
        same,
    );
    h.present(
        "verify_settled",
        "/verify",
        &request,
        Expect::Invalid("invalid_batch_settlement_stellar_commitment_used"),
        &payer,
    )
    .await?;
    let mut conflicting = valid.clone();
    conflicting.amount = (AMOUNT * 2).to_string();
    h.present(
        "settle_conflict",
        "/settle",
        &body(&conflicting, &prove(&conflicting)?),
        Expect::Invalid("invalid_batch_settlement_stellar_commitment_conflict"),
        &payer,
    )
    .await?;

    // ---- redemption: the charge lands on-chain, checked independently
    let path = format!("/settlements/{}", valid.commitment);
    let deadline = Instant::now() + target.settlement_timeout;
    let landed = loop {
        let (status, reply) = h.call("settlement", "GET", &path, None, Some(&key)).await?;
        let hash = reply["settlement"]["transactionHash"].as_str().map(str::to_owned);
        if status == 200 && hash.is_some() {
            break reply;
        }
        if Instant::now() >= deadline {
            bail!(
                "the settlement did not land within {:?}; last answer HTTP {status}: {reply}",
                target.settlement_timeout
            );
        }
        // Keep only the first and last poll in the log.
        if h.log.len() > 1 && h.log[h.log.len() - 2]["case"] == "settlement" {
            h.log.remove(h.log.len() - 2);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    let ledger = &landed["settlement"];
    let hash_hex = ledger["transactionHash"].as_str().context("no transaction hash")?;
    let contract_charge_id =
        ledger["contractChargeId"].as_str().context("no contract charge id")?;
    let hash: [u8; 32] = hex_bytes(hash_hex).context("transaction hash is not 32 bytes of hex")?;
    let charge_id: [u8; 32] =
        hex_bytes(contract_charge_id).context("contract charge id is not 32 bytes of hex")?;
    let pay_to = stellar_strkey::Contract::from_string(&target.pay_to)
        .context("payTo is not a contract address")?
        .0;
    // A node behind the one the worker read may not know the transaction
    // yet: "not found" is retried until the node reaches it, or the wait
    // ends.
    let status = loop {
        let status = rpc.get_transaction(&hash).await?;
        if !matches!(status, TransactionStatus::NotFound { .. }) || Instant::now() >= deadline {
            break status;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    let on_chain = match status {
        TransactionStatus::Success(tx) => tx
            .meta
            .as_ref()
            .map(|meta| settled_in(meta, &pay_to))
            .unwrap_or_default()
            .into_iter()
            .find(|entry| entry.charge_id == charge_id && entry.owner == buyer.address()),
        _ => None,
    };
    let settled_on_chain = on_chain.as_ref().is_some_and(|entry| {
        entry.amount == i128::from(AMOUNT) && entry.outcome == Outcome::Charged
    });
    h.verdict(
        "settled_on_chain",
        format!("transaction {hash_hex} succeeded and its charges event charges the payer {AMOUNT} under {contract_charge_id}"),
        &json!({
            "transaction_hash": hash_hex,
            "entry": on_chain.map(|e| json!({
                "owner": e.owner.to_string(),
                "contract_charge_id": hex_lower(&e.charge_id),
                "amount": e.amount.to_string(),
                "outcome": e.outcome.token(),
            })),
        }),
        settled_on_chain,
    );

    let failed: Vec<Value> =
        h.cases.iter().filter(|c| c["passed"] != true).map(|c| c["case"].clone()).collect();
    Ok(json!({
        "criterion": "x402-conformance",
        "expected": "the x402 v2 facilitator interface, called as a black box: discovery, authentication, every refusal the binding documents, verify, settle, an idempotent retry, reuse refused, and the settlement checked on-chain through an independent RPC read",
        "endpoint": target.endpoint,
        "pay_to": target.pay_to,
        "asset": target.asset,
        "payer": payer,
        "commitment": valid.commitment,
        "transaction_hash": hash_hex,
        "passed": failed.is_empty(),
        "failed_cases": failed,
        "cases": h.cases,
        "exchange": h.log,
    }))
}

fn hex_bytes(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let bytes: Option<Vec<u8>> =
        (0..64).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect();
    bytes?.try_into().ok()
}
