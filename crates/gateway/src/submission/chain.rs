//! The four network operations the submission engine needs. `RpcClient`
//! provides them; tests substitute a scripted network to reach crash and
//! ambiguity windows a live network cannot be made to produce on demand.

use std::future::Future;

use fermah_pay_stellar_chain::rpc::{
    AuthMode, RpcClient, RpcError, SendOutcome, SimulationOutcome, TransactionStatus,
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
}
