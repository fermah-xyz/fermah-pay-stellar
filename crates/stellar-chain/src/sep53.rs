//! SEP-53 signed messages: an Ed25519 signature over
//! `SHA-256("Stellar Signed Message:\n" || message)`, the form Stellar wallets
//! produce for "sign message" requests. The prefix keeps a signed message
//! from ever being a valid transaction or authorization signature.

use ed25519_dalek::{Signature, VerifyingKey};
use fermah_pay_stellar_domain::AccountAddress;
use sha2::{Digest, Sha256};

use crate::keys::SecretKey;

const PREFIX: &[u8] = b"Stellar Signed Message:\n";

/// The 32-byte hash a SEP-53 signature covers.
#[must_use]
pub fn message_hash(message: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PREFIX);
    hasher.update(message);
    hasher.finalize().into()
}

/// Signs `message` as a Stellar wallet would.
#[must_use]
pub fn sign(key: &SecretKey, message: &[u8]) -> [u8; 64] {
    key.sign_raw(&message_hash(message))
}

/// Whether `signature` is `signer`'s SEP-53 signature of `message`. Strict
/// verification: non-canonical and small-order encodings are refused.
#[must_use]
pub fn verify(signer: &AccountAddress, message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(signer.public_key()) else {
        return false;
    };
    key.verify_strict(&message_hash(message), &Signature::from_bytes(signature)).is_ok()
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    // The published SEP-53 test vectors (ecosystem/sep-0053.md in
    // stellar-protocol); the seed is the specification's example key.
    const SEED: &str = "SAKICEVQLYWGSOJS4WW7HZJWAHZVEEBS527LHK5V4MLJALYKICQCJXMW";
    const ADDRESS: &str = "GBXFXNDLV4LSWA4VB7YIL5GBD7BVNR22SGBTDKMO2SBZZHDXSKZYCP7L";

    fn vectors() -> [(Vec<u8>, &'static str); 3] {
        [
            (
                b"Hello, World!".to_vec(),
                "fO5dbYhXUhBMhe6kId/cuVq/AfEnHRHEvsP8vXh03M1uLpi5e46yO2Q8rEBzu3feXQewcQE5GArp88u6ePK6BA==",
            ),
            (
                "こんにちは、世界！".as_bytes().to_vec(),
                "CDU265Xs8y3OWbB/56H9jPgUss5G9A0qFuTqH2zs2YDgTm+++dIfmAEceFqB7bhfN3am59lCtDXrCtwH2k1GBA==",
            ),
            (
                STANDARD.decode("2zZDP1sa1BVBfLP7TeeMk3sUbaxAkUhBhDiNdrksaFo=").unwrap(),
                "VA1+7hefNwv2NKScH6n+Sljj15kLAge+M2wE7fzFOf+L0MMbssA1mwfJZRyyrhBORQRle10X1Dxpx+UOI4EbDQ==",
            ),
        ]
    }

    #[test]
    fn test_signs_the_published_vectors() {
        let key = SecretKey::from_strkey(SEED).unwrap();
        assert_eq!(key.address().as_str(), ADDRESS);
        for (message, expected) in vectors() {
            assert_eq!(STANDARD.encode(sign(&key, &message)), expected);
        }
    }

    #[test]
    fn test_verifies_the_published_vectors_and_nothing_else() {
        let signer: AccountAddress = ADDRESS.parse().unwrap();
        for (message, signature) in vectors() {
            let signature: [u8; 64] = STANDARD.decode(signature).unwrap().try_into().unwrap();
            assert!(verify(&signer, &message, &signature));
            // One property differs each time: the message, the signature,
            // or the signer.
            let mut other = message.clone();
            other.push(b'!');
            assert!(!verify(&signer, &other, &signature));
            let mut flipped = signature;
            flipped[0] ^= 1;
            assert!(!verify(&signer, &message, &flipped));
            let stranger = SecretKey::generate().unwrap().address();
            assert!(!verify(&stranger, &message, &signature));
        }
    }
}
