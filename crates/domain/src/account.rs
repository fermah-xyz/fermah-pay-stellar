use std::fmt;
use std::str::FromStr;

use stellar_strkey::Strkey;

/// A classic Ed25519 `G...` account address in canonical strkey form.
///
/// Muxed (`M...`) and contract (`C...`) addresses are rejected rather than
/// normalised: a muxed address would silently collapse distinct buyers onto
/// one underlying account, and contract accounts authorize through
/// `__check_auth`, which the buyer signing flow does not support.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AccountAddress {
    public_key: [u8; 32],
    encoded: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccountAddressError {
    /// Not a valid strkey of any account type. The input is deliberately not
    /// echoed: a pasted secret seed lands here.
    #[error("not a valid Stellar account address")]
    Malformed,
    #[error("muxed account addresses are not supported; use the underlying G-address")]
    UnsupportedMuxed,
    #[error("contract account addresses are not supported")]
    UnsupportedContract,
}

impl AccountAddress {
    #[must_use]
    pub const fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    #[must_use]
    pub fn from_public_key(public_key: [u8; 32]) -> Self {
        let encoded = stellar_strkey::ed25519::PublicKey(public_key).to_string().to_string();
        Self { public_key, encoded }
    }
}

impl FromStr for AccountAddress {
    type Err = AccountAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match Strkey::from_string(s) {
            Ok(Strkey::PublicKeyEd25519(key)) => Ok(Self::from_public_key(key.0)),
            Ok(Strkey::MuxedAccountEd25519(_)) => Err(AccountAddressError::UnsupportedMuxed),
            Ok(Strkey::Contract(_)) => Err(AccountAddressError::UnsupportedContract),
            Ok(_) | Err(_) => Err(AccountAddressError::Malformed),
        }
    }
}

impl fmt::Display for AccountAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encoded)
    }
}

impl fmt::Debug for AccountAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountAddress({})", self.encoded)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // Circle's testnet USDC issuer: a real, well-known G-address.
    const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

    #[test]
    fn test_parse_accepts_canonical_g_address() {
        let address: AccountAddress = ISSUER.parse().unwrap();
        assert_eq!(address.as_str(), ISSUER);
    }

    #[test]
    fn test_parse_rejects_flipped_checksum() {
        let mut corrupted = ISSUER.to_owned();
        corrupted.replace_range(55.., "4");
        assert_eq!(corrupted.parse::<AccountAddress>(), Err(AccountAddressError::Malformed));
    }

    #[test]
    fn test_parse_rejects_lowercase_spelling() {
        assert_eq!(
            ISSUER.to_lowercase().parse::<AccountAddress>(),
            Err(AccountAddressError::Malformed)
        );
    }

    #[test]
    fn test_parse_rejects_muxed_address() {
        let muxed = stellar_strkey::ed25519::MuxedAccount { ed25519: [7; 32], id: 42 }
            .to_string()
            .to_string();
        assert_eq!(muxed.parse::<AccountAddress>(), Err(AccountAddressError::UnsupportedMuxed));
    }

    #[test]
    fn test_parse_rejects_contract_address() {
        let contract = stellar_strkey::Contract([9; 32]).to_string().to_string();
        assert_eq!(
            contract.parse::<AccountAddress>(),
            Err(AccountAddressError::UnsupportedContract)
        );
    }

    #[test]
    fn test_parse_rejects_secret_seed_without_echoing_it() {
        let key = stellar_strkey::ed25519::PrivateKey([3; 32]);
        let seed = stellar_strkey::Unredacted(&key).to_string().to_string();
        let err = seed.parse::<AccountAddress>().unwrap_err();
        assert_eq!(err, AccountAddressError::Malformed);
        assert!(!err.to_string().contains(seed.as_str()));
    }

    proptest! {
        #[test]
        fn test_public_key_round_trips_through_text(key in any::<[u8; 32]>()) {
            let address = AccountAddress::from_public_key(key);
            let parsed: AccountAddress = address.as_str().parse().unwrap();
            prop_assert_eq!(parsed.public_key(), &key);
        }
    }
}
