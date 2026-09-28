//! Submitting a signed envelope and resolving its outcome by hash.
//!
//! Only the given bytes are ever resent. A lost response, a node that reports
//! the transaction as unknown, or a timeout never produce a second, different
//! transaction: the caller decides what to do with an unresolved hash.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stellar_xdr::{TransactionEnvelope, TransactionResult};

use crate::rpc::hex_lower;
use crate::rpc::{IncludedTransaction, RpcClient, RpcError, SendOutcome, TransactionStatus};

#[derive(Debug, thiserror::Error)]
pub enum SubmissionError {
    #[error("network rejected transaction {} before inclusion: {result:?}", hex_lower(.hash))]
    Rejected { hash: [u8; 32], result: Box<TransactionResult> },
    #[error("transaction {} was included and failed", hex_lower(&.included.envelope_hash))]
    Failed { included: Box<Included> },
    #[error("transaction {} was not found after its validity window", hex_lower(.hash))]
    NotIncluded { hash: [u8; 32] },
    #[error("RPC failure while transaction {} may be in flight; resolve by hash", hex_lower(.hash))]
    InFlight {
        hash: [u8; 32],
        #[source]
        source: RpcError,
    },
}

/// An included transaction and the hash it was submitted under.
#[derive(Debug, Clone)]
pub struct Included {
    pub envelope_hash: [u8; 32],
    pub transaction: IncludedTransaction,
}

/// Sends `envelope` (whose network hash is `hash`) and waits for its outcome.
/// `valid_until_unix` is the envelope's upper time bound: once it has passed,
/// with a margin for ledger close and RPC ingestion, an unseen envelope can no
/// longer be included.
pub async fn submit_and_wait(
    rpc: &RpcClient,
    envelope: &TransactionEnvelope,
    hash: [u8; 32],
    valid_until_unix: u64,
    poll_interval: Duration,
) -> Result<Included, SubmissionError> {
    const INGESTION_MARGIN_SECS: u64 = 30;
    let mut accepted = false;
    loop {
        if !accepted {
            match rpc.send_transaction(envelope).await {
                Ok(SendOutcome::Pending { .. } | SendOutcome::Duplicate { .. }) => accepted = true,
                Ok(SendOutcome::TryAgainLater { .. }) => {}
                Ok(SendOutcome::Rejected { result, .. }) => {
                    return Err(SubmissionError::Rejected { hash, result });
                }
                Err(source) => return Err(SubmissionError::InFlight { hash, source }),
            }
        }
        if accepted {
            match rpc.get_transaction(&hash).await {
                Ok(TransactionStatus::Success(tx)) => {
                    return Ok(Included { envelope_hash: hash, transaction: *tx });
                }
                Ok(TransactionStatus::Failed(tx)) => {
                    return Err(SubmissionError::Failed {
                        included: Box::new(Included { envelope_hash: hash, transaction: *tx }),
                    });
                }
                Ok(TransactionStatus::NotFound)
                    if unix_now() > valid_until_unix + INGESTION_MARGIN_SECS =>
                {
                    return Err(SubmissionError::NotIncluded { hash });
                }
                Ok(TransactionStatus::NotFound) => {}
                Err(source) => return Err(SubmissionError::InFlight { hash, source }),
            }
        } else if unix_now() > valid_until_unix {
            return Err(SubmissionError::NotIncluded { hash });
        }
        tokio::time::sleep(poll_interval).await;
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
