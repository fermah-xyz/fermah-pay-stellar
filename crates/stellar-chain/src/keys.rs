//! Ed25519 account keys. Secret material never implements `Display`, and its
//! `Debug` is redacted, so it cannot reach a log line by accident.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey};
use fermah_pay_stellar_domain::AccountAddress;
use stellar_xdr::{BytesM, DecoratedSignature, Signature, SignatureHint};
use zeroize::Zeroizing;

pub struct SecretKey(SigningKey);

#[derive(Debug, thiserror::Error)]
pub enum SecretKeyError {
    #[error("secret key is not a valid Stellar `S...` seed")]
    Malformed,
    #[error("operating system randomness unavailable")]
    Randomness(#[source] getrandom::Error),
}

impl SecretKey {
    /// A fresh key from the operating system CSPRNG.
    pub fn generate() -> Result<Self, SecretKeyError> {
        let mut seed = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(SecretKeyError::Randomness)?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    pub fn from_strkey(seed: &str) -> Result<Self, SecretKeyError> {
        let key = stellar_strkey::ed25519::PrivateKey::from_string(seed)
            .map_err(|_| SecretKeyError::Malformed)?;
        let bytes = Zeroizing::new(key.0);
        Ok(Self(SigningKey::from_bytes(&bytes)))
    }

    /// The `S...` seed, for writing to a caller-chosen secret store. The
    /// returned buffer is wiped on drop.
    #[must_use]
    pub fn to_strkey(&self) -> Zeroizing<String> {
        let key = stellar_strkey::ed25519::PrivateKey(self.0.to_bytes());
        Zeroizing::new(stellar_strkey::Unredacted(&key).to_string().to_string())
    }

    #[must_use]
    pub fn address(&self) -> AccountAddress {
        AccountAddress::from_public_key(self.0.verifying_key().to_bytes())
    }

    /// Raw Ed25519 signature over a 32-byte payload hash.
    #[must_use]
    pub fn sign_raw(&self, payload_hash: &[u8; 32]) -> [u8; 64] {
        self.0.sign(payload_hash).to_bytes()
    }

    /// Signs a 32-byte Stellar signature payload hash and decorates it with
    /// the hint (last four public-key bytes) the network uses to match
    /// signatures to signers.
    #[must_use]
    pub fn sign_payload(&self, payload_hash: &[u8; 32]) -> DecoratedSignature {
        let public = self.0.verifying_key().to_bytes();
        let signature = self.sign_raw(payload_hash);
        DecoratedSignature {
            hint: SignatureHint([public[28], public[29], public[30], public[31]]),
            signature: Signature(
                BytesM::try_from(signature.to_vec())
                    .expect("invariant: an Ed25519 signature is 64 bytes"),
            ),
        }
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey({}, [REDACTED])", self.address())
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Verifier, VerifyingKey};

    use super::*;

    #[test]
    fn test_strkey_round_trip_preserves_address() {
        let key = SecretKey::generate().unwrap();
        let restored = SecretKey::from_strkey(&key.to_strkey()).unwrap();
        assert_eq!(restored.address(), key.address());
    }

    #[test]
    fn test_from_strkey_rejects_public_address() {
        let key = SecretKey::generate().unwrap();
        assert!(matches!(
            SecretKey::from_strkey(key.address().as_str()),
            Err(SecretKeyError::Malformed)
        ));
    }

    #[test]
    fn test_debug_never_contains_seed() {
        let key = SecretKey::generate().unwrap();
        let seed = key.to_strkey();
        assert!(!format!("{key:?}").contains(seed.as_str()));
    }

    #[test]
    fn test_signature_verifies_under_address_key_with_matching_hint() {
        let key = SecretKey::generate().unwrap();
        let payload = [5_u8; 32];
        let decorated = key.sign_payload(&payload);
        let public = *key.address().public_key();
        assert_eq!(decorated.hint.0, public[28..32]);
        let signature =
            ed25519_dalek::Signature::from_slice(decorated.signature.0.as_slice()).unwrap();
        VerifyingKey::from_bytes(&public).unwrap().verify(&payload, &signature).unwrap();
    }
}
