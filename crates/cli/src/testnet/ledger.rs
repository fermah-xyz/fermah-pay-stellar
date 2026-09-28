//! Balances read straight from ledger entries.

use fermah_pay_stellar_chain::onboarding::provenance_keys;
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_chain::stellar_xdr::{Asset, LedgerEntryData};
use fermah_pay_stellar_domain::AccountAddress;

pub struct Balances {
    /// `None` when the account does not exist.
    pub xlm_stroops: Option<i64>,
    /// `None` when the account has no trustline for the asset.
    pub usdc: Option<i64>,
}

pub async fn balances(
    rpc: &RpcClient,
    account: &AccountAddress,
    usdc: &Asset,
) -> anyhow::Result<Balances> {
    let records = rpc.get_ledger_entries(&provenance_keys(account, usdc)).await?;
    let mut balances = Balances { xlm_stroops: None, usdc: None };
    for record in records {
        match record.data {
            LedgerEntryData::Account(entry) => balances.xlm_stroops = Some(entry.balance),
            LedgerEntryData::Trustline(entry) => balances.usdc = Some(entry.balance),
            _ => {}
        }
    }
    Ok(balances)
}
