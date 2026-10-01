use std::fmt;
use std::str::FromStr;

use stellar_strkey::Strkey;

use crate::{AccountAddress, AccountAddressError};

/// An address that can own a balance in a Soroban contract: a classic
/// Ed25519 account (`G...`) or a contract (`C...`), such as a smart wallet
/// that authorizes through its own `__check_auth`. Muxed addresses are
/// rejected, as for [`AccountAddress`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ChainAddress {
    Account(AccountAddress),
    Contract([u8; 32]),
}

impl ChainAddress {
    /// The classic account, if this is one.
    #[must_use]
    pub const fn as_account(&self) -> Option<&AccountAddress> {
        match self {
            Self::Account(account) => Some(account),
            Self::Contract(_) => None,
        }
    }
}

impl From<AccountAddress> for ChainAddress {
    fn from(account: AccountAddress) -> Self {
        Self::Account(account)
    }
}

impl FromStr for ChainAddress {
    type Err = AccountAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match Strkey::from_string(s) {
            Ok(Strkey::Contract(contract)) => Ok(Self::Contract(contract.0)),
            _ => s.parse().map(Self::Account),
        }
    }
}

impl fmt::Display for ChainAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account(account) => f.write_str(account.as_str()),
            Self::Contract(id) => f.write_str(stellar_strkey::Contract(*id).to_string().as_str()),
        }
    }
}

impl fmt::Debug for ChainAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChainAddress({self})")
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn test_parse_accepts_account_and_contract_addresses() {
        let account = AccountAddress::from_public_key([7; 32]);
        let contract = stellar_strkey::Contract([9; 32]).to_string().to_string();
        assert_eq!(account.as_str().parse(), Ok(ChainAddress::Account(account.clone())));
        assert_eq!(contract.parse(), Ok(ChainAddress::Contract([9; 32])));
        assert_eq!(contract.parse::<ChainAddress>().unwrap().to_string(), contract);
    }

    #[test]
    fn test_parse_rejects_muxed_and_secret_seeds() {
        let muxed = stellar_strkey::ed25519::MuxedAccount { ed25519: [7; 32], id: 42 }
            .to_string()
            .to_string();
        assert_eq!(muxed.parse::<ChainAddress>(), Err(AccountAddressError::UnsupportedMuxed));
        let key = stellar_strkey::ed25519::PrivateKey([3; 32]);
        let seed = stellar_strkey::Unredacted(&key).to_string().to_string();
        assert_eq!(seed.parse::<ChainAddress>(), Err(AccountAddressError::Malformed));
    }

    proptest! {
        #[test]
        fn test_contract_ids_round_trip_through_text(id in any::<[u8; 32]>()) {
            let address = ChainAddress::Contract(id);
            prop_assert_eq!(address.to_string().parse::<ChainAddress>().unwrap(), address);
        }
    }
}
