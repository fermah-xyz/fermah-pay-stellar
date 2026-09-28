//! Signing Soroban address authorization entries for classic `G...` accounts.
//!
//! An authorization entry binds a signer to one invocation tree: the contract,
//! function and arguments of the root call and of every sub-call it
//! authorizes. The transaction source and fee are not part of it, which is
//! what lets a buyer authorize a call that someone else submits and pays for.

use stellar_xdr::{
    BytesM, Hash, HashIdPreimage, HashIdPreimageSorobanAuthorization,
    HashIdPreimageSorobanAuthorizationWithAddress, Limits, ScAddress, ScBytes, ScMap, ScMapEntry,
    ScSymbol, ScVal, ScVec, SorobanAddressCredentials, SorobanAuthorizationEntry,
    SorobanAuthorizedInvocation, SorobanCredentials, VecM, WriteXdr,
};

use sha2::{Digest, Sha256};

use crate::keys::SecretKey;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthorizationError {
    #[error("entry uses source-account or delegated credentials, which carry no address signature")]
    NotAnAddressEntry,
    #[error("entry is for a contract address; only classic accounts are signed here")]
    NotAnAccount,
    #[error("a signing key does not belong to the entry's account")]
    ForeignSigner,
    #[error("no signing key given")]
    NoSigner,
    #[error("encoding authorization XDR: {0}")]
    Encode(String),
}

/// The 32-byte hash an account signs to authorize `credentials` over
/// `invocation`. `AddressV2` credentials also commit to the signer's address,
/// so a signature cannot be replayed for a different address that shares a
/// key; legacy `Address` credentials do not.
pub fn signature_payload(
    network_id: [u8; 32],
    credentials: &SorobanCredentials,
    invocation: &SorobanAuthorizedInvocation,
) -> Result<[u8; 32], AuthorizationError> {
    let preimage = match credentials {
        SorobanCredentials::Address(creds) => {
            HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
                network_id: Hash(network_id),
                nonce: creds.nonce,
                signature_expiration_ledger: creds.signature_expiration_ledger,
                invocation: invocation.clone(),
            })
        }
        SorobanCredentials::AddressV2(creds) => HashIdPreimage::SorobanAuthorizationWithAddress(
            HashIdPreimageSorobanAuthorizationWithAddress {
                network_id: Hash(network_id),
                nonce: creds.nonce,
                signature_expiration_ledger: creds.signature_expiration_ledger,
                address: creds.address.clone(),
                invocation: invocation.clone(),
            },
        ),
        SorobanCredentials::SourceAccount | SorobanCredentials::AddressWithDelegates(_) => {
            return Err(AuthorizationError::NotAnAddressEntry);
        }
    };
    let bytes =
        preimage.to_xdr(Limits::none()).map_err(|e| AuthorizationError::Encode(e.to_string()))?;
    Ok(Sha256::digest(bytes).into())
}

/// Returns `entry` with its signature set by `signers`, all of which must be
/// keys of the entry's account. Nonce, expiration ledger and invocation are
/// taken from the entry unchanged: they are what the signers agree to.
pub fn sign_entry(
    entry: &SorobanAuthorizationEntry,
    network_id: [u8; 32],
    signers: &[&SecretKey],
) -> Result<SorobanAuthorizationEntry, AuthorizationError> {
    if signers.is_empty() {
        return Err(AuthorizationError::NoSigner);
    }
    let creds = match &entry.credentials {
        SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds) => creds,
        SorobanCredentials::SourceAccount | SorobanCredentials::AddressWithDelegates(_) => {
            return Err(AuthorizationError::NotAnAddressEntry);
        }
    };
    let ScAddress::Account(account) = &creds.address else {
        return Err(AuthorizationError::NotAnAccount);
    };
    let account = crate::transaction::address_of(account);
    if signers.iter().any(|key| key.address() != account) {
        return Err(AuthorizationError::ForeignSigner);
    }

    let payload = signature_payload(network_id, &entry.credentials, &entry.root_invocation)?;
    // The host verifies signatures in strictly increasing public-key order.
    let mut signatures: Vec<([u8; 32], [u8; 64])> =
        signers.iter().map(|key| (*key.address().public_key(), key.sign_raw(&payload))).collect();
    signatures.sort_by_key(|(public_key, _)| *public_key);
    let signature = ScVal::Vec(Some(ScVec(
        signatures
            .into_iter()
            .map(|(public_key, signature)| signature_map(public_key, signature))
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| AuthorizationError::Encode("too many signatures".to_owned()))?,
    )));

    let signed = SorobanAddressCredentials { signature, ..creds.clone() };
    let credentials = match entry.credentials {
        SorobanCredentials::AddressV2(_) => SorobanCredentials::AddressV2(signed),
        _ => SorobanCredentials::Address(signed),
    };
    Ok(SorobanAuthorizationEntry { credentials, root_invocation: entry.root_invocation.clone() })
}

/// `{ public_key: Bytes, signature: Bytes }`, the shape the host's account
/// authentication decodes; map keys are in the sorted order `ScMap` requires.
fn signature_map(public_key: [u8; 32], signature: [u8; 64]) -> Result<ScVal, AuthorizationError> {
    let bytes = |b: &[u8]| -> Result<ScVal, AuthorizationError> {
        Ok(ScVal::Bytes(ScBytes(
            BytesM::try_from(b.to_vec()).map_err(|e| AuthorizationError::Encode(e.to_string()))?,
        )))
    };
    let symbol = |s: &str| -> Result<ScVal, AuthorizationError> {
        Ok(ScVal::Symbol(ScSymbol(
            s.try_into().map_err(|_| AuthorizationError::Encode(format!("symbol {s}")))?,
        )))
    };
    let entries: VecM<ScMapEntry> = vec![
        ScMapEntry { key: symbol("public_key")?, val: bytes(&public_key)? },
        ScMapEntry { key: symbol("signature")?, val: bytes(&signature)? },
    ]
    .try_into()
    .map_err(|_| AuthorizationError::Encode("signature map".to_owned()))?;
    Ok(ScVal::Map(Some(ScMap(entries))))
}

#[cfg(test)]
mod tests {
    use stellar_xdr::{
        ContractId, InvokeContractArgs, ScAddress, SorobanAuthorizedFunction, StringM,
    };

    use super::*;
    use crate::transaction::account_id;

    fn invocation(function: &str) -> SorobanAuthorizedInvocation {
        SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([9; 32]))),
                function_name: ScSymbol(StringM::try_from(function).unwrap()),
                args: VecM::default(),
            }),
            sub_invocations: VecM::default(),
        }
    }

    fn entry(key: &SecretKey, v2: bool) -> SorobanAuthorizationEntry {
        let creds = SorobanAddressCredentials {
            address: ScAddress::Account(account_id(&key.address())),
            nonce: 7,
            signature_expiration_ledger: 100,
            signature: ScVal::Void,
        };
        SorobanAuthorizationEntry {
            credentials: if v2 {
                SorobanCredentials::AddressV2(creds)
            } else {
                SorobanCredentials::Address(creds)
            },
            root_invocation: invocation("deposit"),
        }
    }

    #[test]
    fn test_v2_payload_commits_to_the_address() {
        let (a, b) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        let payload = |key: &SecretKey| {
            signature_payload([1; 32], &entry(key, true).credentials, &invocation("deposit"))
        };
        assert_ne!(payload(&a).unwrap(), payload(&b).unwrap());
    }

    #[test]
    fn test_v1_payload_does_not_commit_to_the_address() {
        let (a, b) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        let payload = |key: &SecretKey| {
            signature_payload([1; 32], &entry(key, false).credentials, &invocation("deposit"))
        };
        assert_eq!(payload(&a).unwrap(), payload(&b).unwrap());
    }

    #[test]
    fn test_payload_depends_on_network() {
        let key = SecretKey::generate().unwrap();
        let e = entry(&key, true);
        assert_ne!(
            signature_payload([1; 32], &e.credentials, &e.root_invocation).unwrap(),
            signature_payload([2; 32], &e.credentials, &e.root_invocation).unwrap()
        );
    }

    #[test]
    fn test_foreign_signer_is_refused() {
        let (owner, stranger) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        assert_eq!(
            sign_entry(&entry(&owner, true), [1; 32], &[&stranger]),
            Err(AuthorizationError::ForeignSigner)
        );
    }

    #[test]
    fn test_source_account_entry_is_refused() {
        let key = SecretKey::generate().unwrap();
        let source = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: invocation("deposit"),
        };
        assert_eq!(
            sign_entry(&source, [1; 32], &[&key]),
            Err(AuthorizationError::NotAnAddressEntry)
        );
    }

    #[test]
    fn test_signing_keeps_nonce_expiration_and_invocation() {
        let key = SecretKey::generate().unwrap();
        let unsigned = entry(&key, true);
        let signed = sign_entry(&unsigned, [1; 32], &[&key]).unwrap();
        let (SorobanCredentials::AddressV2(before), SorobanCredentials::AddressV2(after)) =
            (&unsigned.credentials, &signed.credentials)
        else {
            panic!("credential version changed");
        };
        assert_eq!(
            (
                after.nonce,
                after.signature_expiration_ledger,
                &after.address,
                &signed.root_invocation
            ),
            (
                before.nonce,
                before.signature_expiration_ledger,
                &before.address,
                &unsigned.root_invocation
            )
        );
    }
}
