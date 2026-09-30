//! Keys held in AWS KMS. The key must be an asymmetric `ECC_NIST_EDWARDS25519`
//! key for signing: KMS signs with pure Ed25519 (`ED25519_SHA_512` over the
//! raw message), exactly what a Stellar signature is, and the private key
//! never leaves the service. Credentials and region come from the standard
//! AWS sources (environment, shared profile, instance or task role).

use std::fmt;

use aws_sdk_kms::Client;
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::types::{KeySpec, KeyUsageType, MessageType, SigningAlgorithmSpec};
use fermah_pay_stellar_chain::signer::{SignError, SignFuture, Signer};
use fermah_pay_stellar_domain::AccountAddress;

/// DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410): the whole
/// encoding is this followed by the 32-byte public key.
const ED25519_SPKI_PREFIX: [u8; 12] =
    [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

#[derive(Debug, thiserror::Error)]
pub enum KmsError {
    #[error("reading the public key of {key}: {detail}")]
    PublicKey { key: String, detail: String },
    #[error("{key} is a {spec} key for {usage}; an ECC_NIST_EDWARDS25519 signing key is needed")]
    WrongKey { key: String, spec: String, usage: String },
    #[error("{key} does not offer ED25519_SHA_512 signing")]
    NoEd25519 { key: String },
    #[error("{key}'s public key is not an Ed25519 SubjectPublicKeyInfo")]
    Encoding { key: String },
}

/// A key in AWS KMS, bound at startup to the account its public key is.
pub struct AwsKmsSigner {
    client: Client,
    key_id: String,
    address: AccountAddress,
}

impl fmt::Debug for AwsKmsSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsKmsSigner")
            .field("key_id", &self.key_id)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl AwsKmsSigner {
    /// Connects with the standard AWS configuration and binds `key_id` (a
    /// key id, key ARN, alias name or alias ARN).
    pub async fn connect(key_id: &str) -> Result<Self, KmsError> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Self::with_client(Client::new(&config), key_id).await
    }

    /// Binds `key_id` through `client`: reads its public key, checks it is an
    /// Ed25519 signing key, and derives the account from it.
    pub async fn with_client(client: Client, key_id: &str) -> Result<Self, KmsError> {
        let key = key_id.to_owned();
        let public =
            client.get_public_key().key_id(key_id).send().await.map_err(|error| {
                KmsError::PublicKey { key: key.clone(), detail: detail(&error) }
            })?;
        if public.key_spec() != Some(&KeySpec::EccNistEdwards25519)
            || public.key_usage() != Some(&KeyUsageType::SignVerify)
        {
            return Err(KmsError::WrongKey {
                key,
                spec: public.key_spec().map_or("unknown", KeySpec::as_str).to_owned(),
                usage: public.key_usage().map_or("unknown", KeyUsageType::as_str).to_owned(),
            });
        }
        if !public.signing_algorithms().contains(&SigningAlgorithmSpec::Ed25519Sha512) {
            return Err(KmsError::NoEd25519 { key });
        }
        let der = public.public_key().map(Blob::as_ref).unwrap_or_default();
        let raw: [u8; 32] = der
            .strip_prefix(ED25519_SPKI_PREFIX.as_slice())
            .and_then(|rest| rest.try_into().ok())
            .ok_or_else(|| KmsError::Encoding { key: key.clone() })?;
        Ok(Self { client, key_id: key, address: AccountAddress::from_public_key(raw) })
    }
}

impl Signer for AwsKmsSigner {
    fn address(&self) -> AccountAddress {
        self.address.clone()
    }

    fn sign<'a>(&'a self, payload: &'a [u8; 32]) -> SignFuture<'a> {
        Box::pin(async move {
            let signed = self
                .client
                .sign()
                .key_id(&self.key_id)
                .message(Blob::new(payload.to_vec()))
                .message_type(MessageType::Raw)
                .signing_algorithm(SigningAlgorithmSpec::Ed25519Sha512)
                .send()
                .await
                .map_err(|error| SignError::Service(format!("AWS KMS: {}", detail(&error))))?;
            // An Ed25519 signature is the raw 64 bytes, not DER.
            signed
                .signature()
                .and_then(|blob| <[u8; 64]>::try_from(blob.as_ref()).ok())
                .ok_or_else(|| SignError::Service("AWS KMS returned no 64-byte signature".into()))
        })
    }
}

/// An SDK error with its causes, which carry the service's error code.
fn detail(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
