//! Network conditions a submitter reads before building an envelope: the
//! recent market for inclusion, and the latest ledger's close time.
//!
//! Both responses carry integers as JSON strings (`"p90": "100"`,
//! `"closeTime": "1790659107"`), except `ledgerCount` and the ledger
//! sequences, which are numbers; the decoding below follows that shape as
//! served by Stellar RPC and refuses anything else.

use serde::{Deserialize, Deserializer};

use super::{RpcClient, RpcError};

/// A percentile of recent inclusion fees, as `getFeeStats` reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeePercentile {
    P10,
    P20,
    P30,
    P40,
    P50,
    P60,
    P70,
    P80,
    P90,
    P95,
    P99,
    Max,
}

#[derive(Debug, thiserror::Error)]
#[error("fee percentile must be one of 10, 20, 30, 40, 50, 60, 70, 80, 90, 95, 99 or max")]
pub struct UnknownPercentile;

impl std::str::FromStr for FeePercentile {
    type Err = UnknownPercentile;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text.trim().trim_start_matches(['p', 'P']) {
            "10" => Self::P10,
            "20" => Self::P20,
            "30" => Self::P30,
            "40" => Self::P40,
            "50" => Self::P50,
            "60" => Self::P60,
            "70" => Self::P70,
            "80" => Self::P80,
            "90" => Self::P90,
            "95" => Self::P95,
            "99" => Self::P99,
            "max" | "MAX" | "Max" => Self::Max,
            _ => return Err(UnknownPercentile),
        })
    }
}

impl std::fmt::Display for FeePercentile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::P10 => "p10",
            Self::P20 => "p20",
            Self::P30 => "p30",
            Self::P40 => "p40",
            Self::P50 => "p50",
            Self::P60 => "p60",
            Self::P70 => "p70",
            Self::P80 => "p80",
            Self::P90 => "p90",
            Self::P95 => "p95",
            Self::P99 => "p99",
            Self::Max => "max",
        })
    }
}

/// The distribution of inclusion fees charged over the node's recent window
/// of ledgers, in stroops. All zero when the window holds no transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeDistribution {
    #[serde(deserialize_with = "stringly")]
    pub max: u64,
    #[serde(deserialize_with = "stringly")]
    pub min: u64,
    #[serde(deserialize_with = "stringly")]
    pub mode: u64,
    #[serde(deserialize_with = "stringly")]
    pub p10: u64,
    #[serde(deserialize_with = "stringly")]
    pub p20: u64,
    #[serde(deserialize_with = "stringly")]
    pub p30: u64,
    #[serde(deserialize_with = "stringly")]
    pub p40: u64,
    #[serde(deserialize_with = "stringly")]
    pub p50: u64,
    #[serde(deserialize_with = "stringly")]
    pub p60: u64,
    #[serde(deserialize_with = "stringly")]
    pub p70: u64,
    #[serde(deserialize_with = "stringly")]
    pub p80: u64,
    #[serde(deserialize_with = "stringly")]
    pub p90: u64,
    #[serde(deserialize_with = "stringly")]
    pub p95: u64,
    #[serde(deserialize_with = "stringly")]
    pub p99: u64,
    #[serde(deserialize_with = "stringly")]
    pub transaction_count: u64,
    pub ledger_count: u32,
}

impl FeeDistribution {
    #[must_use]
    pub const fn at(&self, percentile: FeePercentile) -> u64 {
        match percentile {
            FeePercentile::P10 => self.p10,
            FeePercentile::P20 => self.p20,
            FeePercentile::P30 => self.p30,
            FeePercentile::P40 => self.p40,
            FeePercentile::P50 => self.p50,
            FeePercentile::P60 => self.p60,
            FeePercentile::P70 => self.p70,
            FeePercentile::P80 => self.p80,
            FeePercentile::P90 => self.p90,
            FeePercentile::P95 => self.p95,
            FeePercentile::P99 => self.p99,
            FeePercentile::Max => self.max,
        }
    }
}

/// `getFeeStats`. For a Soroban transaction the node records the fee charged
/// minus the resource fee charged: the whole transaction's inclusion fee,
/// which for a fee bump spans two operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeStats {
    pub soroban_inclusion_fee: FeeDistribution,
    pub inclusion_fee: FeeDistribution,
    pub latest_ledger: u32,
}

/// The latest ledger a node has ingested, from `getLatestLedger`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestLedgerInfo {
    pub sequence: u32,
    /// Unix close time the validators agreed for that ledger.
    #[serde(deserialize_with = "stringly")]
    pub close_time: i64,
    pub protocol_version: u32,
    /// Stroops each account entry and sub-entry must keep as reserve, from
    /// the ledger header; `None` if the node sent no header.
    #[serde(rename = "headerXdr", default, deserialize_with = "base_reserve_of")]
    pub base_reserve: Option<u32>,
}

fn base_reserve_of<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    use stellar_xdr::{LedgerHeader, Limits, ReadXdr};
    let Some(header) = Option::<String>::deserialize(deserializer)? else { return Ok(None) };
    LedgerHeader::from_xdr_base64(header, Limits::none())
        .map(|header| Some(header.base_reserve))
        .map_err(serde::de::Error::custom)
}

fn stringly<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
{
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(|_| serde::de::Error::custom(format!("not an integer: {text:?}")))
}

impl RpcClient {
    pub async fn get_fee_stats(&self) -> Result<FeeStats, RpcError> {
        self.call("getFeeStats", serde_json::json!({})).await
    }

    /// As [`Self::get_latest_ledger`], with the ledger's close time and
    /// protocol version.
    pub async fn get_latest_ledger_info(&self) -> Result<LatestLedgerInfo, RpcError> {
        self.call("getLatestLedger", serde_json::json!({})).await
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;

    /// `getFeeStats` from soroban-testnet.stellar.org at ledger 4927104.
    const FEE_STATS: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"sorobanInclusionFee":{"max":"200","min":"100","mode":"100","p10":"100","p20":"100","p30":"100","p40":"100","p50":"100","p60":"100","p70":"100","p80":"100","p90":"100","p95":"100","p99":"200","transactionCount":"510","ledgerCount":50},"inclusionFee":{"max":"100","min":"100","mode":"100","p10":"100","p20":"100","p30":"100","p40":"100","p50":"100","p60":"100","p70":"100","p80":"100","p90":"100","p95":"100","p99":"100","transactionCount":"13","ledgerCount":10},"latestLedger":4927104}}"#;

    /// `getLatestLedger` from soroban-testnet.stellar.org at ledger 4943847;
    /// the meta XDR, which this client does not read, is shortened.
    const LATEST_LEDGER: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"id":"571e0694bc2a4b1dfadc55d07e7ac7724c18bc5363d26e7def34f5933164c5b4","protocolVersion":29,"sequence":4943847,"closeTime":"1790742822","headerXdr":"AAAAHe1algTeydN2wgsKWq30/MIVLUNKTch+Tne0ABNGEFlg4x1E1dlXubnu1dS+FY8yZhWYhdxS7n2za7O4TYmfRjcAAAAAaryRJgAAAAAAAAABAAAAALVdELK7fShO1cA6R6XhtZDJD1eDVUccxFB7voIE0jyLAAAAQH2RFZMyZCO9t/p7uGzNaB4+Hmk+wW6MGml7mhus6ZLU853zhUongAQXFhpBWob+J3HRQpfGGU6pLXqesChOJQG3bVTGFINFGPSsX7ge4xdmcU1hCGOBPGDL0c03k3YJZq1/0DATClX2eJe7qg3d6HyFbQvx+SzId6ZFlHfR53wRAEtv5w3gtrOnZAAAAAAGcUP/wzMAAAAAAAAAAAAM2vIAAABkAExLQAAAAMjTkKfUJZGaXT2MFxEBePJi65sUEDUF5lNO/WI9fDriTNDn8+IypdtY7gJcEedtxtXSXWEAkNyCer4ABVrCNgqgS7zXtzJpKyr49/IfkWIuiltIV8Vvus51zD4Jzs+T3uQAfbRzTOUFrgCd0a1NWFFVuKGt8FhQJ3iV3juXCzeRGwAAAAA=","metadataXdr":"AAAAAgAA"}}"#;

    /// Answers one JSON-RPC request on a loopback port with `body`.
    fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 4096];
            // Headers, then the declared body length.
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        url
    }

    fn client(url: &str) -> RpcClient {
        RpcClient::new(url, Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn test_fee_stats_decode_the_served_shape() {
        let stats = client(&serve_once(FEE_STATS)).get_fee_stats().await.unwrap();
        let soroban = stats.soroban_inclusion_fee;
        assert_eq!(
            (
                soroban.p90,
                soroban.p99,
                soroban.max,
                soroban.transaction_count,
                soroban.ledger_count
            ),
            (100, 200, 200, 510, 50)
        );
        assert_eq!((stats.inclusion_fee.transaction_count, stats.latest_ledger), (13, 4_927_104));
        assert_eq!((soroban.at(FeePercentile::P99), soroban.at(FeePercentile::P10)), (200, 100));
    }

    #[tokio::test]
    async fn test_latest_ledger_info_decodes_the_close_time() {
        let info = client(&serve_once(LATEST_LEDGER)).get_latest_ledger_info().await.unwrap();
        assert_eq!(
            info,
            LatestLedgerInfo {
                sequence: 4943847,
                close_time: 1790742822,
                protocol_version: 29,
                base_reserve: Some(5_000_000),
            }
        );
    }

    /// A percentile served as a number rather than a string is not the shape
    /// this client knows; it is refused, not read as zero.
    #[tokio::test]
    async fn test_fee_stats_refuse_a_numeric_percentile() {
        const NUMERIC: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"sorobanInclusionFee":{"max":"200","min":"100","mode":"100","p10":"100","p20":"100","p30":"100","p40":"100","p50":"100","p60":"100","p70":"100","p80":"100","p90":100,"p95":"100","p99":"200","transactionCount":"510","ledgerCount":50},"inclusionFee":{"max":"100","min":"100","mode":"100","p10":"100","p20":"100","p30":"100","p40":"100","p50":"100","p60":"100","p70":"100","p80":"100","p90":"100","p95":"100","p99":"100","transactionCount":"13","ledgerCount":10},"latestLedger":4927104}}"#;
        let error = client(&serve_once(NUMERIC)).get_fee_stats().await.unwrap_err();
        assert!(matches!(error, RpcError::Decode { method: "getFeeStats", .. }), "{error:?}");
    }

    #[test]
    fn test_percentile_parses_every_reported_percentile() {
        for (text, percentile) in [
            ("10", FeePercentile::P10),
            ("p50", FeePercentile::P50),
            ("90", FeePercentile::P90),
            ("95", FeePercentile::P95),
            ("99", FeePercentile::P99),
            ("max", FeePercentile::Max),
        ] {
            assert_eq!(text.parse::<FeePercentile>().unwrap(), percentile, "{text}");
            assert_eq!(percentile.to_string().parse::<FeePercentile>().unwrap(), percentile);
        }
        for text in ["", "0", "85", "100", "p", "median"] {
            assert!(text.parse::<FeePercentile>().is_err(), "{text}");
        }
    }
}
