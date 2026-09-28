use std::fmt;
use std::str::FromStr;

/// A Stellar network a deployment is pinned to. One daemon process serves
/// exactly one network, so a credential or record from the other network is
/// never reachable by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Network {
    Testnet,
    Pubnet,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown network identifier; expected `stellar:testnet` or `stellar:pubnet`")]
pub struct UnknownNetwork;

impl Network {
    /// CAIP-2 identifier, the form persisted and exposed on the wire.
    #[must_use]
    pub const fn caip2(self) -> &'static str {
        match self {
            Self::Testnet => "stellar:testnet",
            Self::Pubnet => "stellar:pubnet",
        }
    }

    /// Network passphrase hashed into every transaction and authorization
    /// signature payload; signing under the wrong one yields signatures the
    /// other network rejects.
    #[must_use]
    pub const fn passphrase(self) -> &'static str {
        match self {
            Self::Testnet => "Test SDF Network ; September 2015",
            Self::Pubnet => "Public Global Stellar Network ; September 2015",
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.caip2())
    }
}

impl FromStr for Network {
    type Err = UnknownNetwork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stellar:testnet" => Ok(Self::Testnet),
            "stellar:pubnet" => Ok(Self::Pubnet),
            _ => Err(UnknownNetwork),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_caip2_round_trips_for_every_network() {
        for network in [Network::Testnet, Network::Pubnet] {
            assert_eq!(network.caip2().parse::<Network>(), Ok(network));
        }
    }

    #[test]
    fn test_parse_rejects_bare_and_case_variants() {
        for raw in ["testnet", "Stellar:testnet", "stellar:TESTNET", "stellar:futurenet", ""] {
            assert_eq!(raw.parse::<Network>(), Err(UnknownNetwork), "{raw}");
        }
    }
}
