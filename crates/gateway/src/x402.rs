//! x402 facilitator interface over the prepaid ledger.
//!
//! Implements the `batch-settlement` scheme of x402 v2 on Stellar networks
//! with a capital-backed commitment: a buyer's prepaid balance on the
//! seller's ledger contract backs each per-request commitment the buyer signs
//! with its Stellar key (SEP-53). `/verify` checks a commitment, `/settle`
//! turns it into a charge (debited at once, settled on-chain in the next
//! batch) and `/settlements/{commitment}` reports the charge and, once it has
//! landed, its transaction. The layout of the commitment is specified in
//! `docs/api/x402.md`.
//!
//! Callers authenticate with the seller deployment's API key, which fixes the
//! ledger contract and asset every request is checked against.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::sep53;
use fermah_pay_stellar_domain::{AccountAddress, IdempotencyKey, Network};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::auth::authenticate;
use crate::ledger::store::ChargeRecord;
use crate::ledger::{LatestLedger, LedgerApi};
use crate::refusal::Refusal;
use crate::scope::Scope;

pub const X402_VERSION: u32 = 2;
pub const SCHEME: &str = "batch-settlement";

/// Reasons this binding adds to the protocol's shared ones.
pub mod reason {
    pub const INSUFFICIENT_FUNDS: &str = "insufficient_funds";
    pub const INVALID_NETWORK: &str = "invalid_network";
    pub const INVALID_PAYLOAD: &str = "invalid_payload";
    pub const INVALID_REQUIREMENTS: &str = "invalid_payment_requirements";
    pub const UNSUPPORTED_SCHEME: &str = "unsupported_scheme";
    pub const INVALID_VERSION: &str = "invalid_x402_version";
    pub const SIGNATURE: &str = "invalid_batch_settlement_stellar_signature";
    pub const EXPIRED: &str = "invalid_batch_settlement_stellar_expired";
    pub const TOO_FAR: &str = "invalid_batch_settlement_stellar_valid_until_too_far";
    pub const UNKNOWN_PAYER: &str = "invalid_batch_settlement_stellar_unknown_payer";
    pub const USED: &str = "invalid_batch_settlement_stellar_commitment_used";
    pub const CONFLICT: &str = "invalid_batch_settlement_stellar_commitment_conflict";
    pub const NOT_CONFIGURED: &str = "invalid_batch_settlement_stellar_ledger_not_configured";
    pub const NETWORK_UNAVAILABLE: &str = "unexpected_network_unavailable";
    pub const UNEXPECTED_VERIFY: &str = "unexpected_verify_error";
    pub const UNEXPECTED_SETTLE: &str = "unexpected_settle_error";
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirements {
    pub scheme: String,
    pub network: String,
    pub amount: String,
    pub asset: String,
    pub pay_to: String,
    pub max_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    pub x402_version: u32,
    pub accepted: PaymentRequirements,
    pub payload: CommitmentPayload,
}

/// What the buyer signs over, besides the requirements it accepted.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentPayload {
    /// The buyer's Stellar account (`G...`).
    pub payer: String,
    /// 32 random bytes, lowercase hex, unique per payment.
    pub commitment: String,
    /// Unix seconds after which the commitment may not be settled.
    pub valid_until: String,
    /// Base64 of the 64-byte SEP-53 signature over [`commitment_message`].
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacilitatorRequest {
    pub x402_version: u32,
    pub payment_payload: PaymentPayload,
    pub payment_requirements: PaymentRequirements,
}

/// The exact text a buyer signs (SEP-53) for one commitment. Every field the
/// facilitator acts on is in it, one per line, so a wallet can show it.
#[must_use]
pub fn commitment_message(
    network: &str,
    asset: &str,
    pay_to: &str,
    amount: &str,
    payer: &str,
    commitment: &str,
    valid_until: &str,
) -> String {
    format!(
        "x402 batch-settlement commitment\n\
         network: {network}\n\
         asset: {asset}\n\
         payTo: {pay_to}\n\
         amount: {amount}\n\
         payer: {payer}\n\
         commitment: {commitment}\n\
         validUntil: {valid_until}"
    )
}

/// A commitment that passed every check that does not depend on the
/// buyer's balance or on earlier settlements.
struct Checked {
    payer: AccountAddress,
    buyer_id: uuid::Uuid,
    amount: i64,
    key: IdempotencyKey,
    valid_until: i64,
}

/// Unix seconds now; commitments' windows are judged against it.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct X402<L> {
    ledger: LedgerApi<L>,
    clock: Clock,
}

pub fn router<L: LatestLedger + 'static>(ledger: LedgerApi<L>) -> axum::Router {
    router_with_clock(ledger, Arc::new(|| OffsetDateTime::now_utc().unix_timestamp()))
}

pub fn router_with_clock<L: LatestLedger + 'static>(
    ledger: LedgerApi<L>,
    clock: Clock,
) -> axum::Router {
    axum::Router::new()
        .route("/supported", get(supported::<L>))
        .route("/verify", post(verify::<L>))
        .route("/settle", post(settle::<L>))
        .route("/settlements/{commitment}", get(settlement::<L>))
        .with_state(Arc::new(X402 { ledger, clock }))
}

type Reply = (StatusCode, Json<Value>);

fn refused(status: StatusCode, error: &str) -> Reply {
    (status, Json(json!({ "error": error })))
}

async fn scope<L: LatestLedger>(x402: &X402<L>, headers: &HeaderMap) -> Result<Scope, Reply> {
    authenticate(x402.ledger.store(), x402.ledger.network(), headers).await.map_err(|refusal| {
        match refusal {
            Refusal::Unauthenticated => refused(StatusCode::UNAUTHORIZED, "unauthenticated"),
            _ => refused(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    })
}

/// Parses the body leniently, so a malformed request is answered with the
/// protocol's reason rather than a framework error.
fn parse(body: &Value) -> Result<FacilitatorRequest, &'static str> {
    serde_json::from_value(body.clone()).map_err(|_| reason::INVALID_PAYLOAD)
}

async fn supported<L: LatestLedger>(State(x402): State<Arc<X402<L>>>) -> Reply {
    (
        StatusCode::OK,
        Json(json!({
            "kinds": [{
                "x402Version": X402_VERSION,
                "scheme": SCHEME,
                "network": x402.ledger.network().caip2(),
            }],
            "extensions": [],
            "signers": {},
        })),
    )
}

impl<L: LatestLedger> X402<L> {
    /// Every check that needs neither the balance nor earlier settlements:
    /// protocol version, scheme, network, the requirements against the
    /// seller's ledger contract and asset, the payload's shape, the payer
    /// being one of the seller's buyers, and the signature.
    async fn check(
        &self,
        scope: &Scope,
        request: &FacilitatorRequest,
    ) -> Result<Checked, &'static str> {
        let requirements = &request.payment_requirements;
        let payload = &request.payment_payload;
        if request.x402_version != X402_VERSION || payload.x402_version != X402_VERSION {
            return Err(reason::INVALID_VERSION);
        }
        if requirements.scheme != SCHEME || payload.accepted.scheme != SCHEME {
            return Err(reason::UNSUPPORTED_SCHEME);
        }
        let network = scope.network().caip2();
        if requirements.network != network || payload.accepted.network != network {
            return Err(reason::INVALID_NETWORK);
        }
        if payload.accepted != *requirements {
            return Err(reason::INVALID_REQUIREMENTS);
        }
        let deployment = self
            .ledger
            .store()
            .ledger_binding(scope)
            .await
            .map_err(|error| {
                tracing::error!(error = %error, "reading the ledger binding");
                reason::UNEXPECTED_VERIFY
            })?
            .ok_or(reason::NOT_CONFIGURED)?;
        let contract = stellar_strkey::Contract(deployment.contract).to_string();
        let asset = stellar_strkey::Contract(deployment.usdc).to_string();
        if requirements.pay_to != contract.as_str() || requirements.asset != asset.as_str() {
            return Err(reason::INVALID_REQUIREMENTS);
        }
        let amount = requirements
            .amount
            .parse::<i64>()
            .ok()
            .filter(|amount| *amount > 0 && requirements.amount == amount.to_string())
            .ok_or(reason::INVALID_REQUIREMENTS)?;

        let commitment = &payload.payload;
        let payer: AccountAddress =
            commitment.payer.parse().map_err(|_| reason::INVALID_PAYLOAD)?;
        let id = decode_commitment(&commitment.commitment).ok_or(reason::INVALID_PAYLOAD)?;
        let valid_until = commitment
            .valid_until
            .parse::<i64>()
            .ok()
            .filter(|at| commitment.valid_until == at.to_string())
            .ok_or(reason::INVALID_PAYLOAD)?;
        let signature: [u8; 64] = STANDARD
            .decode(&commitment.signature)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(reason::INVALID_PAYLOAD)?;
        let message = commitment_message(
            network,
            &requirements.asset,
            &requirements.pay_to,
            &requirements.amount,
            &commitment.payer,
            &commitment.commitment,
            &commitment.valid_until,
        );
        if !sep53::verify(&payer, message.as_bytes(), &signature) {
            return Err(reason::SIGNATURE);
        }
        let buyer_id = self
            .ledger
            .store()
            .buyer_by_wallet(scope, &payer.clone().into())
            .await
            .map_err(|error| {
                tracing::error!(error = %error, "finding the payer");
                reason::UNEXPECTED_VERIFY
            })?
            .ok_or(reason::UNKNOWN_PAYER)?;
        let key =
            format!("x402:{}", hex_lower(&id)).parse().map_err(|_| reason::INVALID_PAYLOAD)?;
        Ok(Checked { payer, buyer_id, amount, key, valid_until })
    }
}

/// A commitment's validity window: not past, and not further ahead than the
/// requirements allow, so a signed commitment is spent promptly or not at
/// all.
fn window(
    now: i64,
    valid_until: i64,
    requirements: &PaymentRequirements,
) -> Result<(), &'static str> {
    if valid_until < now {
        return Err(reason::EXPIRED);
    }
    let horizon =
        now.saturating_add(i64::try_from(requirements.max_timeout_seconds).unwrap_or(i64::MAX));
    if valid_until > horizon {
        return Err(reason::TOO_FAR);
    }
    Ok(())
}

fn decode_commitment(raw: &str) -> Option<[u8; 32]> {
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut id = [0_u8; 32];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&raw[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}

fn invalid(reason: &'static str, payer: Option<&str>) -> Reply {
    metrics::counter!("pay_stellar_x402_total", "call" => "verify", "result" => reason)
        .increment(1);
    let mut body = json!({ "isValid": false, "invalidReason": reason });
    if let Some(payer) = payer {
        body["payer"] = json!(payer);
    }
    (StatusCode::OK, Json(body))
}

async fn verify<L: LatestLedger>(
    State(x402): State<Arc<X402<L>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    let scope = match scope(&x402, &headers).await {
        Ok(scope) => scope,
        Err(reply) => return reply,
    };
    let request = match parse(&body) {
        Ok(request) => request,
        Err(reason) => return (StatusCode::BAD_REQUEST, invalid(reason, None).1),
    };
    let payer = request.payment_payload.payload.payer.clone();
    let checked = match x402.check(&scope, &request).await {
        Ok(checked) => checked,
        Err(reason) => return invalid(reason, Some(&payer)),
    };
    let store = x402.ledger.store();
    match store.charge_with_key(&scope, &checked.key).await {
        Ok(None) => {}
        Ok(Some(_)) => return invalid(reason::USED, Some(&payer)),
        Err(error) => {
            tracing::error!(error = %error, "reading an earlier settlement");
            return invalid(reason::UNEXPECTED_VERIFY, Some(&payer));
        }
    }
    if let Err(reason) = window((x402.clock)(), checked.valid_until, &request.payment_requirements)
    {
        return invalid(reason, Some(&payer));
    }
    match store.balance(&scope, checked.buyer_id).await {
        Ok(Some(balance)) if balance.available >= checked.amount => {
            metrics::counter!("pay_stellar_x402_total", "call" => "verify", "result" => "valid")
                .increment(1);
            (StatusCode::OK, Json(json!({ "isValid": true, "payer": checked.payer.as_str() })))
        }
        Ok(_) => invalid(reason::INSUFFICIENT_FUNDS, Some(&payer)),
        Err(error) => {
            tracing::error!(error = %error, "reading the payer's balance");
            invalid(reason::UNEXPECTED_VERIFY, Some(&payer))
        }
    }
}

fn settle_failed(reason: &'static str, payer: &str, network: Network) -> Reply {
    metrics::counter!("pay_stellar_x402_total", "call" => "settle", "result" => reason)
        .increment(1);
    (
        StatusCode::OK,
        Json(json!({
            "success": false,
            "errorReason": reason,
            "payer": payer,
            "transaction": "",
            "network": network.caip2(),
        })),
    )
}

/// The charge a commitment became, as the settlement's details.
fn settlement_detail(record: &ChargeRecord, replayed: bool) -> Value {
    json!({
        "chargeId": record.id.to_string(),
        "contractChargeId": hex_lower(&record.charge_id),
        "state": record.state.as_str(),
        "outcome": record.outcome,
        "transactionHash": record.transaction_hash.as_deref().map(hex_lower),
        "ledger": record.ledger,
        "replayed": replayed,
    })
}

/// Settles a commitment: debits the buyer's available balance at once and
/// queues the charge for the next batch. Settling the same commitment again
/// answers with the same charge; the same commitment with another amount or
/// payer is refused. The response's `transaction` is the commitment itself,
/// the identifier this binding settles under; the on-chain transaction is
/// reported by `/settlements/{commitment}` once the batch lands.
async fn settle<L: LatestLedger>(
    State(x402): State<Arc<X402<L>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    let scope = match scope(&x402, &headers).await {
        Ok(scope) => scope,
        Err(reply) => return reply,
    };
    let network = scope.network();
    let request = match parse(&body) {
        Ok(request) => request,
        Err(reason) => return (StatusCode::BAD_REQUEST, settle_failed(reason, "", network).1),
    };
    let payer = request.payment_payload.payload.payer.clone();
    let checked = match x402.check(&scope, &request).await {
        Ok(checked) => checked,
        Err(reason) => return settle_failed(reason, &payer, network),
    };
    // A retried settlement is answered from the charge it created, even once
    // the commitment's window has closed; only a new one must be in time.
    let earlier = match x402.ledger.store().charge_with_key(&scope, &checked.key).await {
        Ok(earlier) => earlier,
        Err(error) => {
            tracing::error!(error = %error, "reading an earlier settlement");
            return settle_failed(reason::UNEXPECTED_SETTLE, &payer, network);
        }
    };
    if earlier.is_none()
        && let Err(reason) =
            window((x402.clock)(), checked.valid_until, &request.payment_requirements)
    {
        return settle_failed(reason, &payer, network);
    }
    let (record, created) =
        match x402.ledger.admit(&scope, checked.buyer_id, checked.amount, &checked.key).await {
            Ok(admitted) => admitted,
            Err(Refusal::InsufficientBalance) => {
                return settle_failed(reason::INSUFFICIENT_FUNDS, &payer, network);
            }
            Err(Refusal::IdempotencyConflict) => {
                return settle_failed(reason::CONFLICT, &payer, network);
            }
            Err(Refusal::NetworkUnavailable) => {
                return settle_failed(reason::NETWORK_UNAVAILABLE, &payer, network);
            }
            Err(_) => return settle_failed(reason::UNEXPECTED_SETTLE, &payer, network),
        };
    let result = if created { "settled" } else { "replayed" };
    metrics::counter!("pay_stellar_x402_total", "call" => "settle", "result" => result)
        .increment(1);
    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "payer": checked.payer.as_str(),
            "transaction": request.payment_payload.payload.commitment,
            "network": network.caip2(),
            "amount": record.amount.to_string(),
            "extensions": { "prepaidLedger": settlement_detail(&record, !created) },
        })),
    )
}

async fn settlement<L: LatestLedger>(
    State(x402): State<Arc<X402<L>>>,
    headers: HeaderMap,
    Path(commitment): Path<String>,
) -> Reply {
    let scope = match scope(&x402, &headers).await {
        Ok(scope) => scope,
        Err(reply) => return reply,
    };
    let Some(id) = decode_commitment(&commitment) else {
        return refused(StatusCode::BAD_REQUEST, reason::INVALID_PAYLOAD);
    };
    let Ok(key) = format!("x402:{}", hex_lower(&id)).parse::<IdempotencyKey>() else {
        return refused(StatusCode::BAD_REQUEST, reason::INVALID_PAYLOAD);
    };
    match x402.ledger.store().charge_with_key(&scope, &key).await {
        Ok(Some(record)) => (
            StatusCode::OK,
            Json(json!({
                "commitment": commitment,
                "network": scope.network().caip2(),
                "amount": record.amount.to_string(),
                "settlement": settlement_detail(&record, false),
            })),
        ),
        Ok(None) => refused(StatusCode::NOT_FOUND, "settlement_not_found"),
        Err(error) => {
            tracing::error!(error = %error, "reading a settlement");
            refused(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}
