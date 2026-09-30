//! Minimal Stellar RPC (JSON-RPC 2.0) client.
//!
//! Only the methods the gateway uses are modelled, and each response is
//! decoded into typed XDR immediately. A transport failure during
//! `sendTransaction` means the envelope may or may not have reached the
//! network; callers must treat it as unknown-in-flight, never as a failure.

use std::sync::Arc;
use std::time::Duration;

use fermah_pay_stellar_domain::AccountAddress;
use fermah_pay_stellar_domain::Network;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use stellar_xdr::{
    HostFunction, InvokeContractArgs, LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount,
    Limits, ReadXdr, ScVal, SorobanAuthorizationEntry, SorobanTransactionData, TransactionEnvelope,
    TransactionMeta, TransactionResult, TransactionV1Envelope, VecM, WriteXdr,
};

mod conditions;
pub use conditions::{
    FeeDistribution, FeePercentile, FeeStats, LatestLedgerInfo, UnknownPercentile,
};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("RPC transport failure calling {method}")]
    Transport {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("RPC {method} returned error {code}: {message}")]
    Server { method: &'static str, code: i64, message: String },
    #[error("RPC {method} returned a response this client cannot decode: {detail}")]
    Decode { method: &'static str, detail: String },
    #[error("RPC serves network `{actual}`, but this process is pinned to {expected}")]
    WrongNetwork { expected: Network, actual: String },
    #[error("simulation refused the call: {0}")]
    SimulationRefused(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RpcClientError {
    #[error("RPC URL is not a valid URL")]
    InvalidUrl,
    /// The URL is not echoed: provider URLs often embed an API key.
    #[error("RPC URL must use https unless it points at this host")]
    Insecure,
    #[error("building the HTTP client")]
    Build(#[source] reqwest::Error),
}

#[derive(Clone, Debug)]
pub struct RpcClient {
    http: reqwest::Client,
    url: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    pub passphrase: String,
    pub protocol_version: u32,
    pub friendbot_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LedgerEntryRecord {
    pub key: LedgerKey,
    pub data: LedgerEntryData,
    /// Carries the sponsoring account of the entry, if any.
    pub ext: LedgerEntryExt,
    pub last_modified_ledger: u32,
    /// Last ledger the entry lives through before it is archived; only
    /// contract data and code have one.
    pub live_until_ledger: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum SendOutcome {
    /// Accepted for consideration; says nothing about inclusion.
    Pending { hash: [u8; 32] },
    /// Already known to the node; poll the same hash.
    Duplicate { hash: [u8; 32] },
    /// Node is shedding load; the same bytes may be resent later.
    TryAgainLater { hash: [u8; 32] },
    /// Rejected before inclusion, with the decoded result.
    Rejected { hash: [u8; 32], result: Box<TransactionResult> },
}

#[derive(Debug, Clone)]
pub enum TransactionStatus {
    /// Unknown to this node: neither success nor failure. `node` says how far
    /// the node has ingested, which is what makes "not found" evidence.
    NotFound {
        node: NodeView,
    },
    Success(Box<IncludedTransaction>),
    Failed(Box<IncludedTransaction>),
}

/// The span of ledgers a node had ingested when it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeView {
    pub latest_ledger: u32,
    /// Unix close time of `latest_ledger`.
    pub latest_close_time: i64,
    /// Unix close time of the oldest ledger the node still retains.
    pub oldest_close_time: i64,
}

/// Ledger entries that exist among the requested keys, and the ledger they
/// were read at.
#[derive(Debug, Clone)]
pub struct LedgerEntries {
    pub entries: Vec<LedgerEntryRecord>,
    pub latest_ledger: u32,
}

#[derive(Debug, Clone)]
pub struct IncludedTransaction {
    pub ledger: u32,
    pub envelope: TransactionEnvelope,
    pub result: TransactionResult,
    pub meta: Option<TransactionMeta>,
}

impl IncludedTransaction {
    /// The value the invoked contract function returned, if any.
    #[must_use]
    pub fn return_value(&self) -> Option<&ScVal> {
        match &self.meta {
            Some(TransactionMeta::V4(meta)) => meta.soroban_meta.as_ref()?.return_value.as_ref(),
            Some(TransactionMeta::V3(meta)) => Some(&meta.soroban_meta.as_ref()?.return_value),
            _ => None,
        }
    }
}

/// Where a `getEvents` read starts: at a ledger, or right after the event or
/// position a previous page's cursor names. The node refuses both at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventsFrom {
    Ledger(u32),
    Cursor(EventCursor),
}

/// A position in a node's event stream, as `getEvents` writes event ids and
/// cursors: `<TOID, 19 digits>-<event index, 10 digits>`, where the TOID is
/// `ledger << 32 | transaction << 12 | operation`. Positions order as the
/// events do, so a cursor names everything up to and including itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventCursor {
    toid: u64,
    event: u32,
}

impl EventCursor {
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (toid, event) = text.split_once('-')?;
        if toid.len() != 19 || event.len() != 10 {
            return None;
        }
        if !(toid.bytes().all(|b| b.is_ascii_digit()) && event.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        Some(Self { toid: toid.parse().ok()?, event: event.parse().ok()? })
    }

    /// The position after every event of `ledger`: what a node returns as
    /// the cursor of a page that ran to the end of its window at `ledger`.
    #[must_use]
    pub const fn end_of_ledger(ledger: u32) -> Self {
        Self { toid: ((ledger as u64) << 32) | 0xFFFF_FFFF, event: u32::MAX }
    }

    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn ledger(&self) -> u32 {
        (self.toid >> 32) as u32
    }

    /// Whether every event of [`Self::ledger`] is at or before this
    /// position.
    #[must_use]
    pub const fn ends_ledger(&self) -> bool {
        self.toid & 0xFFFF_FFFF == 0xFFFF_FFFF && self.event == u32::MAX
    }

    /// The first ledger with events that may lie after this position.
    #[must_use]
    pub const fn first_unread_ledger(&self) -> u32 {
        if self.ends_ledger() { self.ledger().saturating_add(1) } else { self.ledger() }
    }
}

impl std::fmt::Display for EventCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:019}-{:010}", self.toid, self.event)
    }
}

/// One contract event as `getEvents` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractEventRecord {
    /// Unique per network, and the event's position in the stream.
    pub id: EventCursor,
    pub ledger: u32,
    /// ISO-8601 close time of `ledger`, as the node wrote it.
    pub ledger_closed_at: String,
    pub contract: [u8; 32],
    pub transaction_hash: [u8; 32],
    /// `false` for an event emitted inside a call that failed, which changed
    /// nothing. The node marks this field deprecated; absent reads as `true`.
    pub in_successful_contract_call: bool,
    pub topics: Vec<ScVal>,
    pub value: ScVal,
}

/// One page of contract events.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    pub events: Vec<ContractEventRecord>,
    /// Everything up to this position was scanned: the last event's id when
    /// the page is full, otherwise the end of the node's scan window, which
    /// covers at most 10,000 ledgers and never passes `latest_ledger`.
    pub cursor: EventCursor,
    pub latest_ledger: u32,
    pub oldest_ledger: u32,
}

/// Most events one `getEvents` call returns.
pub const MAX_EVENTS_PER_PAGE: u32 = 10_000;

/// The span of ledgers a node retains, from `getHealth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub latest_ledger: u32,
    pub oldest_ledger: u32,
}

/// How simulation treats authorization: `Record` discovers which entries a
/// call needs (nothing is verified); `Enforce` verifies the supplied signed
/// entries exactly as the network will.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Record,
    Enforce,
}

#[derive(Debug, Clone)]
pub struct Simulation {
    pub transaction_data: SorobanTransactionData,
    pub min_resource_fee: i64,
    pub auth: Vec<SorobanAuthorizationEntry>,
    pub result: Option<ScVal>,
    pub latest_ledger: u32,
}

#[derive(Debug, Clone)]
pub enum SimulationOutcome {
    Succeeded(Box<Simulation>),
    /// The invocation would fail; `error` is the node's diagnostic text.
    Failed {
        error: String,
        latest_ledger: u32,
    },
    /// Archived entries must be restored by a separate transaction first.
    RestoreRequired {
        transaction_data: Box<SorobanTransactionData>,
        min_resource_fee: i64,
    },
}

impl RpcClient {
    /// Plain `http` is accepted only for a node on this host: across a
    /// network, whoever sits on the path could forge ledger reads and answers
    /// about what was included.
    pub fn new(url: &str, request_timeout: Duration) -> Result<Self, RpcClientError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| RpcClientError::InvalidUrl)?;
        let loopback = parsed.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        match parsed.scheme() {
            "https" => {}
            "http" if loopback => {}
            _ => return Err(RpcClientError::Insecure),
        }
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls_config())
            .timeout(request_timeout)
            .build()
            .map_err(RpcClientError::Build)?;
        Ok(Self { http, url: Arc::from(url) })
    }

    /// Fails unless the endpoint serves `expected`, so a misconfigured URL
    /// cannot route signed transactions to the other network.
    pub async fn verify_network(&self, expected: Network) -> Result<NetworkInfo, RpcError> {
        let info = self.get_network().await?;
        if info.passphrase != expected.passphrase() {
            return Err(RpcError::WrongNetwork { expected, actual: info.passphrase });
        }
        Ok(info)
    }

    pub async fn get_network(&self) -> Result<NetworkInfo, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            passphrase: String,
            protocol_version: u32,
            friendbot_url: Option<String>,
        }
        let raw: Raw = self.call("getNetwork", serde_json::json!({})).await?;
        Ok(NetworkInfo {
            passphrase: raw.passphrase,
            protocol_version: raw.protocol_version,
            friendbot_url: raw.friendbot_url,
        })
    }

    /// The latest ledger the node has ingested. Read from `getHealth`:
    /// `getLatestLedger` also carries the whole ledger's metadata, megabytes
    /// on pubnet, and is kept for when the header is needed.
    pub async fn get_latest_ledger(&self) -> Result<u32, RpcError> {
        Ok(self.get_health().await?.latest_ledger)
    }

    /// Entries that exist, in no guaranteed order; absent keys are omitted.
    pub async fn get_ledger_entries(
        &self,
        keys: &[LedgerKey],
    ) -> Result<Vec<LedgerEntryRecord>, RpcError> {
        Ok(self.get_ledger_entries_at(keys).await?.entries)
    }

    /// As [`Self::get_ledger_entries`], with the ledger the node read them
    /// at: an absent entry proves absence only as of that ledger.
    pub async fn get_ledger_entries_at(
        &self,
        keys: &[LedgerKey],
    ) -> Result<LedgerEntries, RpcError> {
        const METHOD: &str = "getLedgerEntries";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            entries: Option<Vec<RawEntry>>,
            latest_ledger: u32,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawEntry {
            key: String,
            xdr: String,
            ext_xdr: Option<String>,
            last_modified_ledger_seq: u32,
            live_until_ledger_seq: Option<u32>,
        }
        let encoded = keys
            .iter()
            .map(|key| key.to_xdr_base64(Limits::none()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RpcError::Decode { method: METHOD, detail: e.to_string() })?;
        let raw: Raw = self.call(METHOD, serde_json::json!({ "keys": encoded })).await?;
        let entries = raw
            .entries
            .unwrap_or_default()
            .into_iter()
            .map(|entry| {
                let ext = match entry.ext_xdr {
                    Some(ext) => decode(METHOD, &ext)?,
                    // A missing extension is read as "no sponsor": the error
                    // it can cause is attributing a reserve to the buyer,
                    // never a false claim that a sponsor paid it.
                    None => LedgerEntryExt::V0,
                };
                Ok(LedgerEntryRecord {
                    key: decode(METHOD, &entry.key)?,
                    data: decode(METHOD, &entry.xdr)?,
                    ext,
                    last_modified_ledger: entry.last_modified_ledger_seq,
                    live_until_ledger: entry.live_until_ledger_seq,
                })
            })
            .collect::<Result<Vec<_>, RpcError>>()?;
        Ok(LedgerEntries { entries, latest_ledger: raw.latest_ledger })
    }

    /// The ledgers the node retains. A read that starts before
    /// `oldest_ledger`, or after `latest_ledger`, is refused.
    pub async fn get_health(&self) -> Result<Health, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            status: String,
            latest_ledger: u32,
            oldest_ledger: u32,
        }
        let raw: Raw = self.call("getHealth", serde_json::json!({})).await?;
        if raw.status != "healthy" {
            return Err(RpcError::Decode {
                method: "getHealth",
                detail: format!("node status {}", raw.status),
            });
        }
        Ok(Health { latest_ledger: raw.latest_ledger, oldest_ledger: raw.oldest_ledger })
    }

    /// Up to `limit` events `contract` emitted from `from` on, oldest first.
    pub async fn get_events(
        &self,
        contract: &[u8; 32],
        from: &EventsFrom,
        limit: u32,
    ) -> Result<EventPage, RpcError> {
        let raw: RawEventPage = self.call(EVENTS, events_request(contract, from, limit)).await?;
        decode_event_page(raw)
    }

    pub async fn send_transaction(
        &self,
        envelope: &TransactionEnvelope,
    ) -> Result<SendOutcome, RpcError> {
        const METHOD: &str = "sendTransaction";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            status: String,
            hash: String,
            error_result_xdr: Option<String>,
        }
        let bytes = envelope
            .to_xdr_base64(Limits::none())
            .map_err(|e| RpcError::Decode { method: METHOD, detail: e.to_string() })?;
        let raw: Raw = self.call(METHOD, serde_json::json!({ "transaction": bytes })).await?;
        let hash = decode_hash(METHOD, &raw.hash)?;
        match raw.status.as_str() {
            "PENDING" => Ok(SendOutcome::Pending { hash }),
            "DUPLICATE" => Ok(SendOutcome::Duplicate { hash }),
            "TRY_AGAIN_LATER" => Ok(SendOutcome::TryAgainLater { hash }),
            "ERROR" => {
                let xdr = raw.error_result_xdr.ok_or_else(|| RpcError::Decode {
                    method: METHOD,
                    detail: "ERROR status without errorResultXdr".to_owned(),
                })?;
                Ok(SendOutcome::Rejected { hash, result: Box::new(decode(METHOD, &xdr)?) })
            }
            other => {
                Err(RpcError::Decode { method: METHOD, detail: format!("unknown status {other}") })
            }
        }
    }

    pub async fn get_transaction(&self, hash: &[u8; 32]) -> Result<TransactionStatus, RpcError> {
        const METHOD: &str = "getTransaction";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            status: String,
            ledger: Option<u32>,
            envelope_xdr: Option<String>,
            result_xdr: Option<String>,
            result_meta_xdr: Option<String>,
            latest_ledger: u32,
            latest_ledger_close_time: String,
            oldest_ledger_close_time: String,
        }
        let raw: Raw = self.call(METHOD, serde_json::json!({ "hash": hex_lower(hash) })).await?;
        let included = |raw: Raw| -> Result<Box<IncludedTransaction>, RpcError> {
            let missing = |field: &str| RpcError::Decode {
                method: METHOD,
                detail: format!("{} without {field}", raw.status),
            };
            Ok(Box::new(IncludedTransaction {
                ledger: raw.ledger.ok_or_else(|| missing("ledger"))?,
                envelope: decode(
                    METHOD,
                    raw.envelope_xdr.as_deref().ok_or_else(|| missing("envelopeXdr"))?,
                )?,
                result: decode(
                    METHOD,
                    raw.result_xdr.as_deref().ok_or_else(|| missing("resultXdr"))?,
                )?,
                meta: raw.result_meta_xdr.as_deref().map(|m| decode(METHOD, m)).transpose()?,
            }))
        };
        match raw.status.as_str() {
            "NOT_FOUND" => {
                let time = |text: &str| {
                    text.parse::<i64>().map_err(|_| RpcError::Decode {
                        method: METHOD,
                        detail: format!("close time {text}"),
                    })
                };
                Ok(TransactionStatus::NotFound {
                    node: NodeView {
                        latest_ledger: raw.latest_ledger,
                        latest_close_time: time(&raw.latest_ledger_close_time)?,
                        oldest_close_time: time(&raw.oldest_ledger_close_time)?,
                    },
                })
            }
            "SUCCESS" => Ok(TransactionStatus::Success(included(raw)?)),
            "FAILED" => Ok(TransactionStatus::Failed(included(raw)?)),
            other => {
                Err(RpcError::Decode { method: METHOD, detail: format!("unknown status {other}") })
            }
        }
    }

    /// The value a read-only contract call returns, by simulation; no key
    /// signs anything. `source` must be an existing account; its sequence
    /// number only makes the simulated transaction well formed.
    pub async fn read_contract(
        &self,
        source: &AccountAddress,
        call: InvokeContractArgs,
    ) -> Result<Option<ScVal>, RpcError> {
        const METHOD: &str = "simulateTransaction";
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: crate::transaction::account_id(source),
        });
        let sequence = self
            .get_ledger_entries(&[key])
            .await?
            .iter()
            .find_map(|record| match &record.data {
                LedgerEntryData::Account(entry) => Some(entry.seq_num.0),
                _ => None,
            })
            .ok_or_else(|| RpcError::Decode {
                method: METHOD,
                detail: format!("source account {source} does not exist"),
            })?;
        let tx = crate::soroban::invocation_transaction(
            source,
            sequence + 1,
            HostFunction::InvokeContract(call),
            vec![],
            100,
            u64::MAX,
        )
        .map_err(|e| RpcError::Decode { method: METHOD, detail: e.to_string() })?;
        let envelope =
            TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: VecM::default() });
        match self.simulate_transaction(&envelope, AuthMode::Record).await? {
            SimulationOutcome::Succeeded(simulation) => Ok(simulation.result),
            SimulationOutcome::Failed { error, .. } => Err(RpcError::SimulationRefused(error)),
            SimulationOutcome::RestoreRequired { .. } => {
                Err(RpcError::SimulationRefused("the contract's state is archived".to_owned()))
            }
        }
    }

    pub async fn simulate_transaction(
        &self,
        envelope: &TransactionEnvelope,
        auth_mode: AuthMode,
    ) -> Result<SimulationOutcome, RpcError> {
        const METHOD: &str = "simulateTransaction";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            latest_ledger: u32,
            error: Option<String>,
            min_resource_fee: Option<String>,
            transaction_data: Option<String>,
            results: Option<Vec<RawResult>>,
            restore_preamble: Option<RawRestore>,
        }
        #[derive(Deserialize)]
        struct RawResult {
            auth: Option<Vec<String>>,
            xdr: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawRestore {
            transaction_data: String,
            min_resource_fee: String,
        }
        let fee = |text: &str| {
            text.parse::<i64>()
                .map_err(|_| RpcError::Decode { method: METHOD, detail: format!("fee {text}") })
        };
        let bytes = envelope
            .to_xdr_base64(Limits::none())
            .map_err(|e| RpcError::Decode { method: METHOD, detail: e.to_string() })?;
        let mode = match auth_mode {
            AuthMode::Record => "record",
            AuthMode::Enforce => "enforce",
        };
        let raw: Raw = self
            .call(METHOD, serde_json::json!({ "transaction": bytes, "authMode": mode }))
            .await?;
        if let Some(error) = raw.error {
            return Ok(SimulationOutcome::Failed { error, latest_ledger: raw.latest_ledger });
        }
        if let Some(restore) = raw.restore_preamble {
            return Ok(SimulationOutcome::RestoreRequired {
                transaction_data: Box::new(decode(METHOD, &restore.transaction_data)?),
                min_resource_fee: fee(&restore.min_resource_fee)?,
            });
        }
        let missing = |field: &str| RpcError::Decode {
            method: METHOD,
            detail: format!("success without {field}"),
        };
        let first = raw.results.and_then(|r| r.into_iter().next());
        let auth = first
            .as_ref()
            .and_then(|r| r.auth.as_ref())
            .map(|entries| entries.iter().map(|e| decode(METHOD, e)).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        let result = first.and_then(|r| r.xdr).map(|x| decode(METHOD, &x)).transpose()?;
        Ok(SimulationOutcome::Succeeded(Box::new(Simulation {
            transaction_data: decode(
                METHOD,
                raw.transaction_data.as_deref().ok_or_else(|| missing("transactionData"))?,
            )?,
            min_resource_fee: fee(raw
                .min_resource_fee
                .as_deref()
                .ok_or_else(|| missing("minResourceFee"))?)?,
            auth,
            result,
            latest_ledger: raw.latest_ledger,
        })))
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        #[derive(Deserialize)]
        struct Envelope<T> {
            result: Option<T>,
            error: Option<ServerError>,
        }
        #[derive(Deserialize)]
        struct ServerError {
            code: i64,
            message: String,
        }
        let body =
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .http
            .post(&*self.url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| RpcError::Transport { method, source: source.without_url() })?;
        let envelope: Envelope<T> = response.json().await.map_err(|source| {
            if source.is_decode() {
                RpcError::Decode { method, detail: source.to_string() }
            } else {
                RpcError::Transport { method, source: source.without_url() }
            }
        })?;
        match (envelope.result, envelope.error) {
            (_, Some(error)) => {
                Err(RpcError::Server { method, code: error.code, message: error.message })
            }
            (Some(result), None) => Ok(result),
            (None, None) => {
                Err(RpcError::Decode { method, detail: "neither result nor error".to_owned() })
            }
        }
    }
}

const EVENTS: &str = "getEvents";

fn events_request(contract: &[u8; 32], from: &EventsFrom, limit: u32) -> serde_json::Value {
    let contract = stellar_strkey::Contract(*contract).to_string().to_string();
    let filters = serde_json::json!([{ "type": "contract", "contractIds": [contract] }]);
    match from {
        EventsFrom::Ledger(ledger) => serde_json::json!({
            "startLedger": ledger,
            "filters": filters,
            "pagination": { "limit": limit },
        }),
        EventsFrom::Cursor(cursor) => serde_json::json!({
            "filters": filters,
            "pagination": { "cursor": cursor.to_string(), "limit": limit },
        }),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEventPage {
    events: Vec<RawEvent>,
    cursor: String,
    latest_ledger: u32,
    oldest_ledger: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEvent {
    #[serde(rename = "type")]
    kind: String,
    ledger: u32,
    ledger_closed_at: String,
    contract_id: Option<String>,
    id: String,
    tx_hash: String,
    in_successful_contract_call: Option<bool>,
    topic: Vec<String>,
    value: String,
}

fn decode_event_page(raw: RawEventPage) -> Result<EventPage, RpcError> {
    let bad = |detail: String| RpcError::Decode { method: EVENTS, detail };
    let cursor =
        EventCursor::parse(&raw.cursor).ok_or_else(|| bad(format!("cursor {}", raw.cursor)))?;
    let events = raw
        .events
        .into_iter()
        .map(|event| {
            if event.kind != "contract" {
                return Err(bad(format!("event {} of type {}", event.id, event.kind)));
            }
            let id = EventCursor::parse(&event.id)
                .ok_or_else(|| bad(format!("event id {}", event.id)))?;
            if id.ledger() != event.ledger {
                return Err(bad(format!("event {} outside ledger {}", event.id, event.ledger)));
            }
            let contract = event
                .contract_id
                .as_deref()
                .and_then(|c| stellar_strkey::Contract::from_string(c).ok())
                .ok_or_else(|| bad(format!("event {} without a contract", event.id)))?;
            Ok(ContractEventRecord {
                id,
                ledger: event.ledger,
                ledger_closed_at: event.ledger_closed_at,
                contract: contract.0,
                transaction_hash: decode_hash(EVENTS, &event.tx_hash)?,
                in_successful_contract_call: event.in_successful_contract_call.unwrap_or(true),
                topics: event
                    .topic
                    .iter()
                    .map(|topic| decode(EVENTS, topic))
                    .collect::<Result<_, _>>()?,
                value: decode(EVENTS, &event.value)?,
            })
        })
        .collect::<Result<Vec<_>, RpcError>>()?;
    Ok(EventPage {
        events,
        cursor,
        latest_ledger: raw.latest_ledger,
        oldest_ledger: raw.oldest_ledger,
    })
}

/// An HTTP client with the RPC client's TLS setup, for calling other
/// services (plain `http` included).
pub fn http_client(timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().tls_backend_preconfigured(tls_config()).timeout(timeout).build()
}

pub(crate) fn tls_config() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("invariant: the ring provider supports the safe default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

fn decode<T: ReadXdr>(method: &'static str, base64: &str) -> Result<T, RpcError> {
    T::from_xdr_base64(base64, Limits::none())
        .map_err(|e| RpcError::Decode { method, detail: e.to_string() })
}

fn decode_hash(method: &'static str, text: &str) -> Result<[u8; 32], RpcError> {
    let bad = || RpcError::Decode { method, detail: format!("malformed hash {text}") };
    if text.len() != 64 {
        return Err(bad());
    }
    let mut out = [0_u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2).ok_or_else(bad)?, 16)
            .map_err(|_| bad())?;
    }
    Ok(out)
}

#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_plain_http_is_accepted_only_for_this_host() {
        let timeout = Duration::from_secs(1);
        for url in [
            "http://127.0.0.1:8000",
            "http://localhost:8000/rpc",
            "http://[::1]:8000",
            "https://rpc.example",
        ] {
            assert!(RpcClient::new(url, timeout).is_ok(), "{url}");
        }
        for url in ["http://rpc.example", "http://10.0.0.5:8000", "ftp://127.0.0.1"] {
            assert!(matches!(RpcClient::new(url, timeout), Err(RpcClientError::Insecure)), "{url}");
        }
    }
    use super::*;

    // A page `getEvents` returned on testnet for the prepaid ledger contract
    // CDDFMUZ7...: one `deposit` event. The expected values below are what the
    // same node returned for the same event with `xdrFormat: "json"`.
    const LIVE_DEPOSIT_PAGE: &str = r#"{
        "events": [{
            "type": "contract", "ledger": 4926799, "ledgerClosedAt": "2026-09-29T04:53:02Z",
            "contractId": "CDDFMUZ7RT7RYBLL4MGC3WTR7YEFSCLCZATIJN57XGKV45QCEJE5GNPV",
            "id": "0021160440578969600-0000000001", "operationIndex": 0, "transactionIndex": 1,
            "txHash": "a39847ebff4f7a21f8092834d0e64ac0db4cc750320001f7457aed647898ba08",
            "inSuccessfulContractCall": true,
            "topic": ["AAAADwAAAAdkZXBvc2l0AA==", "AAAAEgAAAAAAAAAApDDeli/J1RNw3at1WfsXX/9WgFg2KxTvYMoyskV5ZJc="],
            "value": "AAAAEAAAAAEAAAACAAAACgAAAAAAAAAAAAAAAAAEk+AAAAANAAAAILtpCxgjkLJuwgyIqP1zjkR1j5RpJIuZXkRPVPAfcmTf"
        }],
        "cursor": "0021160440578969600-0000000001",
        "latestLedger": 4926818, "oldestLedger": 4805859,
        "latestLedgerCloseTime": "1790657677", "oldestLedgerCloseTime": "1790052882"
    }"#;

    // A page with no matching event: the cursor is the end of the node's
    // scan window, here 10,000 ledgers after the requested start.
    const LIVE_EMPTY_PAGE: &str = r#"{
        "events": [], "cursor": "0020684133000806399-4294967295",
        "latestLedger": 4926807, "oldestLedger": 4805848,
        "latestLedgerCloseTime": "1790657622", "oldestLedgerCloseTime": "1790052827"
    }"#;

    fn page(json: &str) -> EventPage {
        decode_event_page(serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn test_live_events_page_decodes_to_the_values_the_node_reports() {
        let page = page(LIVE_DEPOSIT_PAGE);
        let [event] = page.events.as_slice() else { panic!("one event") };
        let owner: AccountAddress =
            "GCSDBXUWF7E5KE3Q3WVXKWP3C5P76VUALA3CWFHPMDFDFMSFPFSJPW4H".parse().unwrap();
        let deposit_id =
            decode_hash("t", "bb690b182390b26ec20c88a8fd738e44758f9469248b995e444f54f01f7264df")
                .unwrap();
        assert_eq!(event.id.to_string(), "0021160440578969600-0000000001");
        assert_eq!(
            (event.ledger, event.ledger_closed_at.as_str()),
            (4926799, "2026-09-29T04:53:02Z")
        );
        assert_eq!(
            stellar_strkey::Contract(event.contract).to_string().to_string(),
            "CDDFMUZ7RT7RYBLL4MGC3WTR7YEFSCLCZATIJN57XGKV45QCEJE5GNPV"
        );
        assert_eq!(
            hex_lower(&event.transaction_hash),
            "a39847ebff4f7a21f8092834d0e64ac0db4cc750320001f7457aed647898ba08"
        );
        assert!(event.in_successful_contract_call);
        assert_eq!(
            event.topics,
            [
                ScVal::Symbol(stellar_xdr::ScSymbol("deposit".try_into().unwrap())),
                ScVal::Address(stellar_xdr::ScAddress::Account(crate::transaction::account_id(
                    &owner
                ))),
            ]
        );
        assert_eq!(
            event.value,
            ScVal::Vec(Some(stellar_xdr::ScVec(
                vec![
                    ScVal::I128(stellar_xdr::Int128Parts { hi: 0, lo: 300_000 }),
                    ScVal::Bytes(stellar_xdr::ScBytes(deposit_id.to_vec().try_into().unwrap())),
                ]
                .try_into()
                .unwrap()
            )))
        );
        assert_eq!(
            (page.cursor, page.latest_ledger, page.oldest_ledger),
            (event.id, 4926818, 4805859),
            "a full page's cursor is its last event"
        );
    }

    #[test]
    fn test_empty_page_cursor_ends_the_scanned_window() {
        let page = page(LIVE_EMPTY_PAGE);
        assert!(page.events.is_empty());
        assert_eq!(page.cursor, EventCursor::end_of_ledger(4815899));
        assert!(page.cursor.ends_ledger());
        assert_eq!(page.cursor.first_unread_ledger(), 4815900);
    }

    #[test]
    fn test_event_id_is_a_position_inside_its_ledger() {
        let id = EventCursor::parse("0021160440578969600-0000000001").unwrap();
        assert_eq!(
            (id.ledger(), id.ends_ledger(), id.first_unread_ledger()),
            (4926799, false, 4926799)
        );
        // Later events of the same ledger, and the end of that ledger, sort
        // after it; the end of the previous ledger sorts before it.
        let next = EventCursor::parse("0021160440578969600-0000000002").unwrap();
        assert!(id < next && next < EventCursor::end_of_ledger(4926799));
        assert!(EventCursor::end_of_ledger(4926798) < id);
    }

    #[test]
    fn test_malformed_event_positions_are_refused() {
        for text in [
            "",
            "0021160440578969600",
            "21160440578969600-0000000001",
            "0021160440578969600-1",
            "002116044057896960x-0000000001",
            "0021160440578969600-+000000001",
            "9999999999999999999-9999999999",
        ] {
            assert_eq!(EventCursor::parse(text), None, "{text}");
        }
    }

    #[test]
    fn test_event_with_id_outside_its_ledger_is_refused() {
        let tampered = LIVE_DEPOSIT_PAGE.replace("\"ledger\": 4926799", "\"ledger\": 4926800");
        let raw: RawEventPage = serde_json::from_str(&tampered).unwrap();
        assert!(matches!(decode_event_page(raw), Err(RpcError::Decode { .. })));
    }

    #[test]
    fn test_events_request_sets_start_ledger_or_cursor_never_both() {
        // The node answers "ledger ranges and cursor cannot both be set".
        let contract = [7_u8; 32];
        let from_ledger = events_request(&contract, &EventsFrom::Ledger(4926799), 200);
        assert_eq!(from_ledger["startLedger"], 4926799);
        assert!(from_ledger["pagination"].get("cursor").is_none());
        let cursor = EventCursor::end_of_ledger(4815899);
        let from_cursor = events_request(&contract, &EventsFrom::Cursor(cursor), 200);
        assert!(from_cursor.get("startLedger").is_none());
        assert_eq!(from_cursor["pagination"]["cursor"], "0020684133000806399-4294967295");
        assert_eq!(from_cursor["pagination"]["limit"], 200);
        assert_eq!(
            from_cursor["filters"][0]["contractIds"][0],
            stellar_strkey::Contract(contract).to_string().to_string()
        );
    }

    #[test]
    fn test_decode_hash_round_trips_hex() {
        let hash = [0xab_u8; 32];
        assert_eq!(decode_hash("t", &hex_lower(&hash)).unwrap(), hash);
    }

    #[test]
    fn test_decode_hash_rejects_wrong_length() {
        assert!(decode_hash("t", &"a".repeat(63)).is_err());
    }

    #[test]
    fn test_decode_hash_rejects_non_hex_digits() {
        assert!(decode_hash("t", &"g".repeat(64)).is_err());
    }
}
