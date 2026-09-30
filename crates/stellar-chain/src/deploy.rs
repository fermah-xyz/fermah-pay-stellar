//! Contract deployment: uploading Wasm and creating an instance whose
//! constructor runs in the same operation, so no one can initialize the
//! instance with other arguments in between.

use fermah_pay_stellar_domain::{AccountAddress, Network};
use stellar_xdr::{
    BytesM, ContractExecutable, ContractIdPreimage, ContractIdPreimageFromAddress,
    CreateContractArgsV2, Hash, HashIdPreimage, HashIdPreimageContractId, HostFunction, ScAddress,
    ScVal, Uint256, VecM, WriteXdr,
};

use sha2::{Digest, Sha256};

use crate::network_id;
use crate::transaction::account_id;

#[must_use]
pub fn wasm_hash(wasm: &[u8]) -> [u8; 32] {
    Sha256::digest(wasm).into()
}

pub fn upload(wasm: &[u8]) -> Result<HostFunction, stellar_xdr::Error> {
    Ok(HostFunction::UploadContractWasm(BytesM::try_from(wasm.to_vec())?))
}

pub fn create(
    deployer: &AccountAddress,
    salt: [u8; 32],
    wasm_hash: [u8; 32],
    constructor_args: Vec<ScVal>,
) -> Result<HostFunction, stellar_xdr::Error> {
    Ok(HostFunction::CreateContractV2(CreateContractArgsV2 {
        contract_id_preimage: preimage(deployer, salt),
        executable: ContractExecutable::Wasm(Hash(wasm_hash)),
        constructor_args: VecM::try_from(constructor_args)?,
    }))
}

/// The address the network will assign to the instance `deployer` creates
/// with `salt`, computed before deployment so the result can be checked.
#[must_use]
pub fn contract_id(network: Network, deployer: &AccountAddress, salt: [u8; 32]) -> [u8; 32] {
    let bytes = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(network_id(network)),
        contract_id_preimage: preimage(deployer, salt),
    })
    .to_xdr(stellar_xdr::Limits::none())
    .expect("invariant: a contract-id preimage always encodes");
    Sha256::digest(bytes).into()
}

fn preimage(deployer: &AccountAddress, salt: [u8; 32]) -> ContractIdPreimage {
    ContractIdPreimage::Address(ContractIdPreimageFromAddress {
        address: ScAddress::Account(account_id(deployer)),
        salt: Uint256(salt),
    })
}

/// The call that deploys the Stellar Asset Contract of a classic `asset`.
/// Anyone may deploy it, once; its address is derived from the asset.
#[must_use]
pub fn asset_contract(asset: stellar_xdr::Asset) -> HostFunction {
    HostFunction::CreateContract(stellar_xdr::CreateContractArgs {
        contract_id_preimage: stellar_xdr::ContractIdPreimage::Asset(asset),
        executable: stellar_xdr::ContractExecutable::StellarAsset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::SecretKey;

    #[test]
    fn test_contract_id_depends_on_salt() {
        let deployer = SecretKey::generate().unwrap().address();
        assert_ne!(
            contract_id(Network::Testnet, &deployer, [1; 32]),
            contract_id(Network::Testnet, &deployer, [2; 32])
        );
    }

    #[test]
    fn test_contract_id_depends_on_network() {
        let deployer = SecretKey::generate().unwrap().address();
        assert_ne!(
            contract_id(Network::Testnet, &deployer, [1; 32]),
            contract_id(Network::Pubnet, &deployer, [1; 32])
        );
    }
}
