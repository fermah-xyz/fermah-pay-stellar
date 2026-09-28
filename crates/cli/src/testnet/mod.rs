//! Stellar testnet operations for the prepaid ledger: role accounts,
//! deployment, buyer funding, deposits, charges and withdrawals. Every
//! operation writes an evidence record read back from the ledger.

pub mod evidence;
pub mod ledger;
pub mod prepaid;
pub mod profile;
