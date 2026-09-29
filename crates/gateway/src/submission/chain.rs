//! The network operations the submission engine and the settlement worker
//! need. `RpcClient` provides them; tests substitute a scripted network to
//! reach crash and ambiguity windows a live network cannot be made to produce
//! on demand.

use std::future::Future;

use fermah_pay_stellar_chain::rpc::{
    AuthMode, LedgerEntryRecord, RpcClient, RpcError, SendOutcome, SimulationOutcome,
    TransactionStatus,
};
use fermah_pay_stellar_chain::stellar_xdr::{
    LedgerEntryData, LedgerKey, LedgerKeyAccount, TransactionEnvelope,
};
use fermah_pay_stellar_chain::transaction::account_id;
use fermah_pay_stellar_domain::AccountAddress;

pub trait Chain: Send + Sync {
    /// The account's current sequence number, or `None` if it does not exist.
    fn account_sequence(
        &self,
        account: &AccountAddress,
    ) -> impl Future<Output = Result<Option<i64>, RpcError>> + Send;

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

    /// The entries that exist among `keys`; absent keys are omitted.
    fn ledger_entries(
        &self,
        keys: &[LedgerKey],
    ) -> impl Future<Output = Result<Vec<LedgerEntryRecord>, RpcError>> + Send;
}

impl Chain for RpcClient {
    async fn account_sequence(&self, account: &AccountAddress) -> Result<Option<i64>, RpcError> {
        let key = LedgerKey::Account(LedgerKeyAccount { account_id: account_id(account) });
        let records = self.get_ledger_entries(&[key]).await?;
        Ok(records.iter().find_map(|record| match &record.data {
            LedgerEntryData::Account(entry) => Some(entry.seq_num.0),
            _ => None,
        }))
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

    async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<Vec<LedgerEntryRecord>, RpcError> {
        self.get_ledger_entries(keys).await
    }
}
