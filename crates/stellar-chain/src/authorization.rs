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

use fermah_pay_stellar_domain::AccountAddress;

use crate::keys::SecretKey;
use crate::transaction::ScAddressOf;

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
    #[error("signing the entry")]
    Signer(#[source] crate::signer::SignError),
    #[error("a signature does not verify for its public key")]
    InvalidSignature,
    #[error("a public key signs the entry twice")]
    DuplicateSigner,
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
    let (creds, account) = entry_account(entry)?;
    if signers.iter().any(|key| key.address() != account) {
        return Err(AuthorizationError::ForeignSigner);
    }
    let payload = signature_payload(network_id, &entry.credentials, &entry.root_invocation)?;
    let signatures =
        signers.iter().map(|key| (*key.address().public_key(), key.sign_raw(&payload))).collect();
    with_signatures(entry, creds, signatures)
}

/// [`sign_entry`] with one signer whose key may live in a key management
/// service; its signature is checked before it is attached.
pub async fn sign_entry_with(
    entry: &SorobanAuthorizationEntry,
    network_id: [u8; 32],
    signer: &dyn crate::signer::Signer,
) -> Result<SorobanAuthorizationEntry, AuthorizationError> {
    let (creds, account) = entry_account(entry)?;
    if signer.address() != account {
        return Err(AuthorizationError::ForeignSigner);
    }
    let payload = signature_payload(network_id, &entry.credentials, &entry.root_invocation)?;
    let signature =
        crate::signer::signature(signer, &payload).await.map_err(AuthorizationError::Signer)?;
    with_signatures(entry, creds, vec![(*account.public_key(), signature)])
}

/// Returns `entry` carrying `signatures`, each a public key and its
/// signature of the entry's payload, collected separately from the account's
/// signers. Each signature is verified here; whether the keys are the
/// account's signers, and weigh enough, is the ledger's to say (see
/// [`crate::multisig`]).
pub fn attach_signatures(
    entry: &SorobanAuthorizationEntry,
    network_id: [u8; 32],
    signatures: Vec<([u8; 32], [u8; 64])>,
) -> Result<SorobanAuthorizationEntry, AuthorizationError> {
    if signatures.is_empty() {
        return Err(AuthorizationError::NoSigner);
    }
    let (creds, _) = entry_account(entry)?;
    let payload = signature_payload(network_id, &entry.credentials, &entry.root_invocation)?;
    for (i, (public_key, signature)) in signatures.iter().enumerate() {
        if signatures[..i].iter().any(|(earlier, _)| earlier == public_key) {
            return Err(AuthorizationError::DuplicateSigner);
        }
        let valid = ed25519_dalek::VerifyingKey::from_bytes(public_key).is_ok_and(|key| {
            key.verify_strict(&payload, &ed25519_dalek::Signature::from_bytes(signature)).is_ok()
        });
        if !valid {
            return Err(AuthorizationError::InvalidSignature);
        }
    }
    with_signatures(entry, creds, signatures)
}

fn entry_account(
    entry: &SorobanAuthorizationEntry,
) -> Result<(&SorobanAddressCredentials, AccountAddress), AuthorizationError> {
    let creds = match &entry.credentials {
        SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds) => creds,
        SorobanCredentials::SourceAccount | SorobanCredentials::AddressWithDelegates(_) => {
            return Err(AuthorizationError::NotAnAddressEntry);
        }
    };
    let ScAddress::Account(account) = &creds.address else {
        return Err(AuthorizationError::NotAnAccount);
    };
    Ok((creds, crate::transaction::address_of(account)))
}

fn with_signatures(
    entry: &SorobanAuthorizationEntry,
    creds: &SorobanAddressCredentials,
    mut signatures: Vec<([u8; 32], [u8; 64])>,
) -> Result<SorobanAuthorizationEntry, AuthorizationError> {
    // The host verifies signatures in strictly increasing public-key order.
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

/// Why a buyer-returned authorization entry was refused. Every check runs
/// before any fee is spent; a distinct reason per defect lets the caller
/// report exactly what the wallet got wrong.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignedEntryRefusal {
    #[error("entry carries no address credentials")]
    NotAnAddressEntry,
    #[error("entry is signed for a different account than the expected signer")]
    WrongSigner,
    #[error("entry authorizes a different call than the one prepared")]
    InvocationMismatch,
    #[error("entry expired at ledger {expiration}; current ledger is {current}")]
    Expired { expiration: u32, current: u32 },
    #[error("entry stays valid until ledger {expiration}, beyond the allowed {latest}")]
    ValidityTooLong { expiration: u32, latest: u32 },
    #[error("entry must carry exactly one Ed25519 signature by the account's own key")]
    UnsupportedSignature,
    #[error("signature does not verify for the entry's payload")]
    BadSignature,
}

/// Checks an authorization entry a signer returned against the call that was
/// prepared for them: same account, byte-identical invocation tree, a live
/// but bounded expiration, and a valid signature by the account's own key.
///
/// Accounts that authorize through other signers or thresholds are refused
/// here rather than accepted and left to fail on-chain; the network still has
/// the final word through enforcing simulation before submission.
pub fn verify_signed_entry(
    entry: &SorobanAuthorizationEntry,
    signer: &AccountAddress,
    invocation: &SorobanAuthorizedInvocation,
    network_id: [u8; 32],
    current_ledger: u32,
    max_validity_ledgers: u32,
) -> Result<(), SignedEntryRefusal> {
    let creds = verify_entry_terms(
        entry,
        &signer.sc_address(),
        invocation,
        current_ledger,
        max_validity_ledgers,
    )?;
    let (public_key, signature) =
        single_signature(&creds.signature).ok_or(SignedEntryRefusal::UnsupportedSignature)?;
    if public_key != *signer.public_key() {
        return Err(SignedEntryRefusal::UnsupportedSignature);
    }
    let payload = signature_payload(network_id, &entry.credentials, &entry.root_invocation)
        .map_err(|_| SignedEntryRefusal::NotAnAddressEntry)?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&public_key)
        .map_err(|_| SignedEntryRefusal::BadSignature)?;
    key.verify_strict(&payload, &ed25519_dalek::Signature::from_bytes(&signature))
        .map_err(|_| SignedEntryRefusal::BadSignature)
}

/// [`verify_signed_entry`] for a contract account, which authorizes through
/// its own `__check_auth` with a credential only it interprets: everything
/// but the signature is checked here. Whether the signature satisfies the
/// account is the network's to say, by simulating the call with the entry
/// in enforcing mode before it is accepted.
pub fn verify_contract_entry(
    entry: &SorobanAuthorizationEntry,
    contract: &[u8; 32],
    invocation: &SorobanAuthorizedInvocation,
    current_ledger: u32,
    max_validity_ledgers: u32,
) -> Result<(), SignedEntryRefusal> {
    let signer = ScAddress::Contract(stellar_xdr::ContractId(stellar_xdr::Hash(*contract)));
    verify_entry_terms(entry, &signer, invocation, current_ledger, max_validity_ledgers).map(|_| ())
}

/// The checks every returned entry passes whoever signs it: address
/// credentials for `signer`, the prepared invocation, and an expiration
/// that is live but not beyond `max_validity_ledgers` from now.
fn verify_entry_terms<'a>(
    entry: &'a SorobanAuthorizationEntry,
    signer: &ScAddress,
    invocation: &SorobanAuthorizedInvocation,
    current_ledger: u32,
    max_validity_ledgers: u32,
) -> Result<&'a SorobanAddressCredentials, SignedEntryRefusal> {
    let creds = match &entry.credentials {
        SorobanCredentials::Address(creds) | SorobanCredentials::AddressV2(creds) => creds,
        SorobanCredentials::SourceAccount | SorobanCredentials::AddressWithDelegates(_) => {
            return Err(SignedEntryRefusal::NotAnAddressEntry);
        }
    };
    if creds.address != *signer {
        return Err(SignedEntryRefusal::WrongSigner);
    }
    if entry.root_invocation != *invocation {
        return Err(SignedEntryRefusal::InvocationMismatch);
    }
    let expiration = creds.signature_expiration_ledger;
    if expiration < current_ledger {
        return Err(SignedEntryRefusal::Expired { expiration, current: current_ledger });
    }
    let latest = current_ledger.saturating_add(max_validity_ledgers);
    if expiration > latest {
        return Err(SignedEntryRefusal::ValidityTooLong { expiration, latest });
    }
    Ok(creds)
}

/// The `(public_key, signature)` of a signature value holding exactly one
/// `{ public_key, signature }` map.
fn single_signature(value: &ScVal) -> Option<([u8; 32], [u8; 64])> {
    let ScVal::Vec(Some(ScVec(items))) = value else { return None };
    let [ScVal::Map(Some(ScMap(fields)))] = items.as_slice() else { return None };
    let field = |name: &str| {
        fields.iter().find_map(|entry| match (&entry.key, &entry.val) {
            (ScVal::Symbol(key), ScVal::Bytes(ScBytes(bytes)))
                if key.0.as_slice() == name.as_bytes() =>
            {
                Some(bytes.as_slice().to_vec())
            }
            _ => None,
        })
    };
    if fields.len() != 2 {
        return None;
    }
    let public_key = <[u8; 32]>::try_from(field("public_key")?).ok()?;
    let signature = <[u8; 64]>::try_from(field("signature")?).ok()?;
    Some((public_key, signature))
}

#[cfg(test)]
mod tests {
    use stellar_xdr::{
        ContractId, InvokeContractArgs, ScAddress, SorobanAuthorizedFunction, StringM,
    };

    use super::*;
    use crate::transaction::account_id;

    #[test]
    fn test_co_signers_signatures_are_attached_only_if_each_verifies() {
        let account = SecretKey::generate().unwrap();
        let (a, b) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        let unsigned = entry(&account, true);
        let payload =
            signature_payload([1; 32], &unsigned.credentials, &unsigned.root_invocation).unwrap();
        let sig = |key: &SecretKey| (*key.address().public_key(), key.sign_raw(&payload));
        // The positive control: two co-signers, attached in public-key order.
        let signed = attach_signatures(&unsigned, [1; 32], vec![sig(&b), sig(&a)]).unwrap();
        let mut expected = vec![sig(&a), sig(&b)];
        expected.sort_by_key(|(public_key, _)| *public_key);
        assert_eq!(
            signed,
            with_signatures(&unsigned, entry_account(&unsigned).unwrap().0, expected).unwrap()
        );
        // A signature over another network's payload, a repeated key, none.
        let foreign = (*a.address().public_key(), a.sign_raw(&[9; 32]));
        assert_eq!(
            attach_signatures(&unsigned, [1; 32], vec![foreign]),
            Err(AuthorizationError::InvalidSignature)
        );
        assert_eq!(
            attach_signatures(&unsigned, [1; 32], vec![sig(&a), sig(&a)]),
            Err(AuthorizationError::DuplicateSigner)
        );
        assert_eq!(
            attach_signatures(&unsigned, [1; 32], vec![]),
            Err(AuthorizationError::NoSigner)
        );
    }

    #[test]
    fn test_signer_backed_entry_matches_local_signing_and_refuses_another_account() {
        use crate::signer::LocalSigner;
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        for v2 in [false, true] {
            let key = SecretKey::generate().unwrap();
            let unsigned = entry(&key, v2);
            let local = sign_entry(&unsigned, [1; 32], &[&key]).unwrap();
            let signer = LocalSigner::new(SecretKey::from_strkey(&key.to_strkey()).unwrap());
            let remote = runtime.block_on(sign_entry_with(&unsigned, [1; 32], &signer)).unwrap();
            assert_eq!(local, remote);
            let stranger = LocalSigner::new(SecretKey::generate().unwrap());
            assert_eq!(
                runtime.block_on(sign_entry_with(&unsigned, [1; 32], &stranger)),
                Err(AuthorizationError::ForeignSigner)
            );
        }
    }

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

    fn prepared(key: &SecretKey) -> SorobanAuthorizationEntry {
        let mut e = entry(key, true);
        if let SorobanCredentials::AddressV2(creds) = &mut e.credentials {
            creds.signature_expiration_ledger = 1_100;
        }
        sign_entry(&e, [1; 32], &[key]).unwrap()
    }

    fn check(
        entry: &SorobanAuthorizationEntry,
        signer: &SecretKey,
    ) -> Result<(), SignedEntryRefusal> {
        verify_signed_entry(entry, &signer.address(), &invocation("deposit"), [1; 32], 1_000, 200)
    }

    #[test]
    fn test_correctly_signed_entry_is_accepted() {
        let key = SecretKey::generate().unwrap();
        assert_eq!(check(&prepared(&key), &key), Ok(()));
    }

    #[test]
    fn test_entry_for_another_account_is_refused() {
        let (buyer, other) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        assert_eq!(check(&prepared(&other), &buyer), Err(SignedEntryRefusal::WrongSigner));
    }

    #[test]
    fn test_entry_for_another_call_is_refused() {
        let key = SecretKey::generate().unwrap();
        let mut signed = prepared(&key);
        signed.root_invocation = invocation("withdraw");
        assert_eq!(check(&signed, &key), Err(SignedEntryRefusal::InvocationMismatch));
    }

    #[test]
    fn test_expired_entry_is_refused() {
        let key = SecretKey::generate().unwrap();
        let result = verify_signed_entry(
            &prepared(&key),
            &key.address(),
            &invocation("deposit"),
            [1; 32],
            1_101,
            200,
        );
        assert_eq!(result, Err(SignedEntryRefusal::Expired { expiration: 1_100, current: 1_101 }));
    }

    #[test]
    fn test_entry_valid_beyond_window_is_refused() {
        let key = SecretKey::generate().unwrap();
        let result = verify_signed_entry(
            &prepared(&key),
            &key.address(),
            &invocation("deposit"),
            [1; 32],
            1_000,
            99,
        );
        assert_eq!(
            result,
            Err(SignedEntryRefusal::ValidityTooLong { expiration: 1_100, latest: 1_099 })
        );
    }

    #[test]
    fn test_signature_over_another_network_is_refused() {
        let key = SecretKey::generate().unwrap();
        let mut e = entry(&key, true);
        if let SorobanCredentials::AddressV2(creds) = &mut e.credentials {
            creds.signature_expiration_ledger = 1_100;
        }
        let other_network = sign_entry(&e, [2; 32], &[&key]).unwrap();
        assert_eq!(check(&other_network, &key), Err(SignedEntryRefusal::BadSignature));
    }

    #[test]
    fn test_raised_expiration_after_signing_is_refused() {
        let key = SecretKey::generate().unwrap();
        let mut signed = prepared(&key);
        if let SorobanCredentials::AddressV2(creds) = &mut signed.credentials {
            creds.signature_expiration_ledger = 1_150;
        }
        assert_eq!(check(&signed, &key), Err(SignedEntryRefusal::BadSignature));
    }

    #[test]
    fn test_unsigned_entry_is_refused() {
        let key = SecretKey::generate().unwrap();
        let mut unsigned = entry(&key, true);
        if let SorobanCredentials::AddressV2(creds) = &mut unsigned.credentials {
            creds.signature_expiration_ledger = 1_100;
        }
        assert_eq!(check(&unsigned, &key), Err(SignedEntryRefusal::UnsupportedSignature));
    }

    #[test]
    fn test_valid_signature_by_another_key_is_refused() {
        let (buyer, stranger) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        let mut forged = entry(&buyer, true);
        if let SorobanCredentials::AddressV2(creds) = &mut forged.credentials {
            creds.signature_expiration_ledger = 1_100;
        }
        // The stranger signs the buyer's exact payload and presents its own
        // public key: a signature that verifies, by the wrong key.
        let payload =
            signature_payload([1; 32], &forged.credentials, &forged.root_invocation).unwrap();
        let signature = ScVal::Vec(Some(ScVec(
            vec![
                signature_map(*stranger.address().public_key(), stranger.sign_raw(&payload))
                    .unwrap(),
            ]
            .try_into()
            .unwrap(),
        )));
        if let SorobanCredentials::AddressV2(creds) = &mut forged.credentials {
            creds.signature = signature;
        }
        assert_eq!(check(&forged, &buyer), Err(SignedEntryRefusal::UnsupportedSignature));
    }

    /// An entry for the contract account `account`, its credential an
    /// opaque value only that account interprets.
    fn contract_entry(account: [u8; 32], expiration: u32) -> SorobanAuthorizationEntry {
        SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: ScAddress::Contract(ContractId(stellar_xdr::Hash(account))),
                nonce: 7,
                signature_expiration_ledger: expiration,
                signature: ScVal::Bytes(ScBytes(vec![5_u8; 64].try_into().unwrap())),
            }),
            root_invocation: invocation("deposit"),
        }
    }

    #[test]
    fn test_contract_entry_terms_are_checked_without_its_signature() {
        let check = |entry: &SorobanAuthorizationEntry, current: u32| {
            verify_contract_entry(entry, &[3; 32], &invocation("deposit"), current, 200)
        };
        assert_eq!(check(&contract_entry([3; 32], 1_100), 1_000), Ok(()));
        assert_eq!(
            check(&contract_entry([4; 32], 1_100), 1_000),
            Err(SignedEntryRefusal::WrongSigner)
        );
        let mut other_call = contract_entry([3; 32], 1_100);
        other_call.root_invocation = invocation("withdraw");
        assert_eq!(check(&other_call, 1_000), Err(SignedEntryRefusal::InvocationMismatch));
        assert_eq!(
            check(&contract_entry([3; 32], 1_100), 1_101),
            Err(SignedEntryRefusal::Expired { expiration: 1_100, current: 1_101 })
        );
        assert_eq!(
            check(&contract_entry([3; 32], 1_300), 1_000),
            Err(SignedEntryRefusal::ValidityTooLong { expiration: 1_300, latest: 1_200 })
        );
    }

    #[test]
    fn test_a_classic_account_entry_is_not_a_contract_accounts() {
        let key = SecretKey::generate().unwrap();
        assert_eq!(
            verify_contract_entry(&prepared(&key), &[3; 32], &invocation("deposit"), 1_000, 200),
            Err(SignedEntryRefusal::WrongSigner)
        );
        // Nor does a contract account's entry pass as a classic account's.
        assert_eq!(
            check(&contract_entry([3; 32], 1_100), &key),
            Err(SignedEntryRefusal::WrongSigner)
        );
    }
}
