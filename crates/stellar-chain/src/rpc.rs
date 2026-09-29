//! Minimal Stellar RPC (JSON-RPC 2.0) client.
//!
//! Only the methods the gateway uses are modelled, and each response is
//! decoded into typed XDR immediately. A transport failure during
//! `sendTransaction` means the envelope may or may not have reached the
//! network; callers must treat it as unknown-in-flight, never as a failure.

use std::sync::Arc;
use std::time::Duration;

use fermah_pay_stellar_domain::Network;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use stellar_xdr::{
    LedgerEntryData, LedgerEntryExt, LedgerKey, Limits, ReadXdr, ScVal, SorobanAuthorizationEntry,
    SorobanTransactionData, TransactionEnvelope, TransactionMeta, TransactionResult, WriteXdr,
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
    pub fn new(url: &str, request_timeout: Duration) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls_config())
            .timeout(request_timeout)
            .build()?;
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

    pub async fn get_latest_ledger(&self) -> Result<u32, RpcError> {
        #[derive(Deserialize)]
        struct Raw {
            sequence: u32,
        }
        let raw: Raw = self.call("getLatestLedger", serde_json::json!({})).await?;
        Ok(raw.sequence)
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
                })
            })
            .collect::<Result<Vec<_>, RpcError>>()?;
        Ok(LedgerEntries { entries, latest_ledger: raw.latest_ledger })
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
            .map_err(|source| RpcError::Transport { method, source })?;
        let envelope: Envelope<T> = response.json().await.map_err(|source| {
            if source.is_decode() {
                RpcError::Decode { method, detail: source.to_string() }
            } else {
                RpcError::Transport { method, source }
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
    use super::*;

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
