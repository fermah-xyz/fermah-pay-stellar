//! Circle USDC identity per network. A local standalone network has no
//! Circle issuer; its USDC is a stand-in issued by [`local_usdc_issuer`].
//!
//! Admission code must bind the asset by issuer *and* network and check the
//! Stellar Asset Contract address by derivation: any contract can advertise
//! the symbol `USDC`.

use fermah_pay_stellar_domain::{AccountAddress, Network};
use stellar_xdr::{
    AlphaNum4, Asset, AssetCode4, ContractIdPreimage, Hash, HashIdPreimage,
    HashIdPreimageContractId, LedgerKey, LedgerKeyTrustLine, WriteXdr,
};

use sha2::{Digest, Sha256};

use crate::network_id;
use crate::transaction::account_id;

/// USDC has seven decimal places on Stellar: one USDC is 10^7 base units.
pub const USDC_DECIMALS: u32 = 7;

/// Circle's USDC issuer account on `network`; on a local network, the
/// stand-in issuer.
#[must_use]
pub fn circle_issuer(network: Network) -> AccountAddress {
    let issuer = match network {
        Network::Testnet => "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
        Network::Pubnet => "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN",
        Network::Local => return local_usdc_issuer().address(),
    };
    issuer.parse().expect("invariant: Circle issuer constants are canonical G-addresses")
}

/// The issuer of the stand-in USDC on a local standalone network. Its seed
/// is derived from a public string, so anyone can issue that asset: it has
/// no value and exists only so a local network can run the same flows as
/// testnet.
#[must_use]
pub fn local_usdc_issuer() -> crate::keys::SecretKey {
    crate::keys::SecretKey::from_seed(
        Sha256::digest(b"fermah-pay-stellar local network USDC issuer").into(),
    )
}

#[must_use]
pub fn circle_usdc(network: Network) -> Asset {
    Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USDC"),
        issuer: account_id(&circle_issuer(network)),
    })
}

/// The Stellar Asset Contract address of `asset` on `network`, derived the
/// way the network derives it, rather than trusted from configuration.
#[must_use]
pub fn asset_contract_id(asset: &Asset, network: Network) -> [u8; 32] {
    let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(network_id(network)),
        contract_id_preimage: ContractIdPreimage::Asset(asset.clone()),
    });
    let bytes = preimage
        .to_xdr(stellar_xdr::Limits::none())
        .expect("invariant: a contract-id preimage always encodes");
    Sha256::digest(bytes).into()
}

/// Ledger key of `holder`'s trustline for `asset`, whose entry holds the
/// balance of a classic account.
#[must_use]
pub fn trustline_key(holder: &AccountAddress, asset: &Asset) -> LedgerKey {
    LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: account_id(holder),
        asset: crate::onboarding::trustline_asset(asset),
    })
}

/// `C...` strkey form of a contract ID.
#[must_use]
pub fn contract_strkey(contract_id: [u8; 32]) -> String {
    stellar_strkey::Contract(contract_id).to_string().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contract IDs Horizon's asset endpoint reports for Circle USDC on each
    // network; the derivation must reproduce them.
    const TESTNET_SAC: &str = "CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA";
    const PUBNET_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    #[test]
    fn test_derived_testnet_sac_matches_published_address() {
        let id = asset_contract_id(&circle_usdc(Network::Testnet), Network::Testnet);
        assert_eq!(contract_strkey(id), TESTNET_SAC);
    }

    #[test]
    fn test_derived_pubnet_sac_matches_published_address() {
        let id = asset_contract_id(&circle_usdc(Network::Pubnet), Network::Pubnet);
        assert_eq!(contract_strkey(id), PUBNET_SAC);
    }

    #[test]
    fn test_same_asset_on_other_network_derives_different_contract() {
        let asset = circle_usdc(Network::Testnet);
        assert_ne!(
            asset_contract_id(&asset, Network::Testnet),
            asset_contract_id(&asset, Network::Pubnet)
        );
    }
}
