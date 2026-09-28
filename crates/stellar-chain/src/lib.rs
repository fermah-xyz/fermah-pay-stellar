//! Stellar protocol access for the gateway: key handling, transaction
//! signing, asset identities, RPC reads/submission and account onboarding.
//!
//! Everything here is chain-specific and business-agnostic; the gateway
//! decides what to sign and when.

#![forbid(unsafe_code)]

pub mod friendbot;
pub mod keys;
pub mod onboarding;
pub mod rpc;
pub mod transaction;
pub mod usdc;

pub use stellar_xdr;

use fermah_pay_stellar_domain::Network;
use sha2::{Digest, Sha256};

/// The network ID mixed into every signature payload: SHA-256 of the
/// network passphrase.
#[must_use]
pub fn network_id(network: Network) -> [u8; 32] {
    Sha256::digest(network.passphrase().as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_ids_match_published_values() {
        // Published network IDs, cross-checked against the passphrase
        // constants so a typo in either is caught.
        assert_eq!(
            hex(network_id(Network::Testnet)),
            "cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472"
        );
        assert_eq!(
            hex(network_id(Network::Pubnet)),
            "7ac33997544e3175d266bd022439b22cdb16508c01163f26e5cb2a3e1045a979"
        );
    }

    fn hex(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
