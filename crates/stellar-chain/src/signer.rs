//! Signing through a [`Signer`], so a key can live in a key management
//! service rather than in the process: the caller hands over the 32-byte
//! payload hash and gets an Ed25519 signature back, possibly over the
//! network.
//!
//! Every signature obtained through [`signature`] is verified against the
//! signer's address before it is used. A service configured with the wrong
//! key, or one that returns garbage, is caught before anything reaches the
//! network.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey};
use fermah_pay_stellar_domain::AccountAddress;
use stellar_xdr::{BytesM, DecoratedSignature, Signature, SignatureHint};

use crate::keys::SecretKey;

pub type SignFuture<'a> = Pin<Box<dyn Future<Output = Result<[u8; 64], SignError>> + Send + 'a>>;

/// An Ed25519 key that signs 32-byte payload hashes for one account.
pub trait Signer: Send + Sync + fmt::Debug {
    /// The account whose key this is.
    fn address(&self) -> AccountAddress;

    /// Signs `payload`. Callers use [`signature`], which checks the result.
    fn sign<'a>(&'a self, payload: &'a [u8; 32]) -> SignFuture<'a>;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignError {
    #[error("the signing service failed: {0}")]
    Service(String),
    #[error("the signer returned a signature that does not verify for {0}")]
    Invalid(AccountAddress),
}

/// Signs `payload` with `signer` and checks the signature against the
/// signer's address.
pub async fn signature(signer: &dyn Signer, payload: &[u8; 32]) -> Result<[u8; 64], SignError> {
    let signed = signer.sign(payload).await?;
    let address = signer.address();
    let valid = VerifyingKey::from_bytes(address.public_key()).is_ok_and(|key| {
        key.verify_strict(payload, &Ed25519Signature::from_bytes(&signed)).is_ok()
    });
    if valid { Ok(signed) } else { Err(SignError::Invalid(address)) }
}

/// `signature` in the form a transaction envelope carries: with the hint (the
/// last four public-key bytes) the network matches signers by.
#[must_use]
pub fn decorated(address: &AccountAddress, signature: [u8; 64]) -> DecoratedSignature {
    let public = address.public_key();
    DecoratedSignature {
        hint: SignatureHint([public[28], public[29], public[30], public[31]]),
        signature: Signature(
            BytesM::try_from(signature.to_vec())
                .expect("invariant: an Ed25519 signature is 64 bytes"),
        ),
    }
}

/// A key held in this process.
pub struct LocalSigner(SecretKey);

impl LocalSigner {
    #[must_use]
    pub const fn new(key: SecretKey) -> Self {
        Self(key)
    }

    #[must_use]
    pub fn arc(key: SecretKey) -> Arc<dyn Signer> {
        Arc::new(Self(key))
    }
}

impl fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LocalSigner").field(&self.0.address()).finish()
    }
}

impl Signer for LocalSigner {
    fn address(&self) -> AccountAddress {
        self.0.address()
    }

    fn sign<'a>(&'a self, payload: &'a [u8; 32]) -> SignFuture<'a> {
        let signed = self.0.sign_raw(payload);
        Box::pin(async move { Ok(signed) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claims one account but signs with another key.
    #[derive(Debug)]
    struct Impostor {
        claims: AccountAddress,
        key: SecretKey,
    }

    impl Signer for Impostor {
        fn address(&self) -> AccountAddress {
            self.claims.clone()
        }

        fn sign<'a>(&'a self, payload: &'a [u8; 32]) -> SignFuture<'a> {
            let signed = self.key.sign_raw(payload);
            Box::pin(async move { Ok(signed) })
        }
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(future)
    }

    #[test]
    fn test_a_signature_is_used_only_if_it_verifies_for_the_signers_address() {
        let key = SecretKey::generate().unwrap();
        let payload = [7_u8; 32];
        // The positive control: the signer's own key.
        let local = LocalSigner::new(SecretKey::from_strkey(&key.to_strkey()).unwrap());
        assert_eq!(block_on(signature(&local, &payload)).unwrap(), key.sign_raw(&payload));
        // The same claimed address, signed with another key.
        let impostor = Impostor { claims: key.address(), key: SecretKey::generate().unwrap() };
        assert!(matches!(
            block_on(signature(&impostor, &payload)),
            Err(SignError::Invalid(address)) if address == key.address()
        ));
    }

    #[test]
    fn test_decorated_signature_carries_the_key_hint() {
        let key = SecretKey::generate().unwrap();
        let payload = [9_u8; 32];
        assert_eq!(decorated(&key.address(), key.sign_raw(&payload)), key.sign_payload(&payload));
    }
}
