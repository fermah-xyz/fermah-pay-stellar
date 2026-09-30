//! Transaction envelope construction and signing.

use fermah_pay_stellar_domain::{AccountAddress, Network};
use stellar_xdr::{
    AccountId, DecoratedSignature, MuxedAccount, PublicKey, Transaction, TransactionEnvelope,
    TransactionSignaturePayload, TransactionSignaturePayloadTaggedTransaction,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use crate::keys::SecretKey;
use crate::network_id;

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("encoding transaction XDR")]
    Encode(#[source] stellar_xdr::Error),
    #[error("a transaction carries at most 20 signatures")]
    TooManySignatures,
    #[error("signing the transaction")]
    Signer(#[source] crate::signer::SignError),
}

/// The hash each signer signs and the network identifies the transaction by.
pub fn transaction_hash(tx: &Transaction, network: Network) -> Result<[u8; 32], SigningError> {
    tx.hash(network_id(network)).map_err(SigningError::Encode)
}

/// Signs `tx` with every key in `signers` for `network`.
pub fn sign(
    tx: Transaction,
    network: Network,
    signers: &[&SecretKey],
) -> Result<TransactionEnvelope, SigningError> {
    let hash = transaction_hash(&tx, network)?;
    let signatures: Vec<DecoratedSignature> =
        signers.iter().map(|key| key.sign_payload(&hash)).collect();
    Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::try_from(signatures).map_err(|_| SigningError::TooManySignatures)?,
    }))
}

/// [`sign`] with signers whose keys may live in a key management service;
/// each signature is checked before it is attached.
pub async fn sign_with(
    tx: Transaction,
    network: Network,
    signers: &[&dyn crate::signer::Signer],
) -> Result<TransactionEnvelope, SigningError> {
    let hash = transaction_hash(&tx, network)?;
    let mut signatures = Vec::with_capacity(signers.len());
    for signer in signers {
        let signature =
            crate::signer::signature(*signer, &hash).await.map_err(SigningError::Signer)?;
        signatures.push(crate::signer::decorated(&signer.address(), signature));
    }
    Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::try_from(signatures).map_err(|_| SigningError::TooManySignatures)?,
    }))
}

/// Signature payload bytes, exposed so tests can prove the network is bound
/// into what is signed.
pub fn signature_payload(tx: &Transaction, network: Network) -> Result<Vec<u8>, SigningError> {
    TransactionSignaturePayload {
        network_id: stellar_xdr::Hash(network_id(network)),
        tagged_transaction: TransactionSignaturePayloadTaggedTransaction::Tx(tx.clone()),
    }
    .to_xdr(stellar_xdr::Limits::none())
    .map_err(SigningError::Encode)
}

#[must_use]
pub fn account_id(address: &AccountAddress) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*address.public_key())))
}

#[must_use]
pub fn muxed_account(address: &AccountAddress) -> MuxedAccount {
    MuxedAccount::Ed25519(Uint256(*address.public_key()))
}

#[must_use]
pub fn address_of(account: &AccountId) -> AccountAddress {
    let AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key))) = account;
    AccountAddress::from_public_key(*key)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use stellar_xdr::{Memo, Preconditions, SequenceNumber, TransactionExt};

    use super::*;

    fn empty_tx(source: &SecretKey) -> Transaction {
        Transaction {
            source_account: muxed_account(&source.address()),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: VecM::default(),
            ext: TransactionExt::V0,
        }
    }

    #[test]
    fn test_hash_is_sha256_of_signature_payload() {
        let key = SecretKey::generate().unwrap();
        let tx = empty_tx(&key);
        let expected: [u8; 32] =
            Sha256::digest(signature_payload(&tx, Network::Testnet).unwrap()).into();
        assert_eq!(transaction_hash(&tx, Network::Testnet).unwrap(), expected);
    }

    #[test]
    fn test_hash_differs_between_networks() {
        let key = SecretKey::generate().unwrap();
        let tx = empty_tx(&key);
        assert_ne!(
            transaction_hash(&tx, Network::Testnet).unwrap(),
            transaction_hash(&tx, Network::Pubnet).unwrap()
        );
    }

    #[test]
    fn test_sign_attaches_one_signature_per_signer_in_order() {
        let first = SecretKey::generate().unwrap();
        let second = SecretKey::generate().unwrap();
        let TransactionEnvelope::Tx(envelope) =
            sign(empty_tx(&first), Network::Testnet, &[&first, &second]).unwrap()
        else {
            panic!("expected a v1 envelope");
        };
        let hints: Vec<[u8; 4]> = envelope.signatures.iter().map(|s| s.hint.0).collect();
        let hint = |k: &SecretKey| {
            let p = k.address().public_key().to_owned();
            [p[28], p[29], p[30], p[31]]
        };
        assert_eq!(hints, vec![hint(&first), hint(&second)]);
    }
}
