//! The network operations the submission engine and the settlement worker
//! need. `RpcClient` provides them; tests substitute a scripted network to
//! reach crash and ambiguity windows a live network cannot be made to produce
//! on demand.

use std::future::Future;

use fermah_pay_stellar_chain::rpc::{
    AuthMode, FeeStats, LatestLedgerInfo, LedgerEntries, RpcClient, RpcError, SendOutcome,
    SimulationOutcome, TransactionStatus,
};
use fermah_pay_stellar_chain::stellar_xdr::{
    LedgerEntryData, LedgerKey, LedgerKeyAccount, TransactionEnvelope,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::AccountAddress;

/// An account's sequence number as a node read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSequence {
    /// `None` if the account does not exist.
    pub sequence: Option<i64>,
    pub latest_ledger: u32,
}

pub trait Chain: Send + Sync {
    fn account_sequence(
        &self,
        account: &AccountAddress,
    ) -> impl Future<Output = Result<SourceSequence, RpcError>> + Send;

    /// Enforcing simulation: authorization is verified as the network will.
    fn simulate(
        &self,
        envelope: &TransactionEnvelope,
    ) -> impl Future<Output = Result<SimulationOutcome, RpcError>> + Send;

    fn send(
        &self,
        envelope: &TransactionEnvelope,
    ) -> impl Future<Output = Result<SendOutcome, RpcError>> + Send;

    fn transaction(
        &self,
        hash: &[u8; 32],
    ) -> impl Future<Output = Result<TransactionStatus, RpcError>> + Send;

    fn latest_ledger(&self) -> impl Future<Output = Result<u32, RpcError>> + Send;

    /// The latest ledger with its close time, which the engine compares with
    /// the local clock before it builds time bounds from that clock.
    fn latest_ledger_info(&self)
    -> impl Future<Output = Result<LatestLedgerInfo, RpcError>> + Send;

    /// Recent inclusion fees, to bid from.
    fn fee_stats(&self) -> impl Future<Output = Result<FeeStats, RpcError>> + Send;

    /// The entries that exist among `keys`, and the ledger they were read at.
    fn ledger_entries(
        &self,
        keys: &[LedgerKey],
    ) -> impl Future<Output = Result<LedgerEntries, RpcError>> + Send;
}

impl Chain for RpcClient {
    async fn account_sequence(&self, account: &AccountAddress) -> Result<SourceSequence, RpcError> {
        let key = LedgerKey::Account(LedgerKeyAccount { account_id: account_id(account) });
        let read = self.get_ledger_entries_at(&[key]).await?;
        Ok(SourceSequence {
            sequence: read.entries.iter().find_map(|record| match &record.data {
                LedgerEntryData::Account(entry) => Some(entry.seq_num.0),
                _ => None,
            }),
            latest_ledger: read.latest_ledger,
        })
    }

    async fn simulate(
        &self,
        envelope: &TransactionEnvelope,
    ) -> Result<SimulationOutcome, RpcError> {
        self.simulate_transaction(envelope, AuthMode::Enforce).await
    }

    async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendOutcome, RpcError> {
        self.send_transaction(envelope).await
    }

    async fn transaction(&self, hash: &[u8; 32]) -> Result<TransactionStatus, RpcError> {
        self.get_transaction(hash).await
    }

    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        self.get_latest_ledger().await
    }

    async fn latest_ledger_info(&self) -> Result<LatestLedgerInfo, RpcError> {
        self.get_latest_ledger_info().await
    }

    async fn fee_stats(&self) -> Result<FeeStats, RpcError> {
        self.get_fee_stats().await
    }

    async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, RpcError> {
        self.get_ledger_entries_at(keys).await
    }
}
