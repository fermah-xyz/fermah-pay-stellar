//! The AWS KMS signer against a stand-in service speaking the KMS JSON
//! protocol, through the real SDK client: the requests the signer sends
//! and how it reads the answers. The stand-in refuses anything but pure
//! Ed25519 signing (`ED25519_SHA_512` over the raw message), as KMS does for
//! an `ECC_NIST_EDWARDS25519` key. A last test runs against real AWS KMS
//! when a key is named in `PAY_STELLAR_TEST_AWS_KMS_KEY`.

#![cfg(feature = "aws-kms")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aws_sdk_kms::config::{BehaviorVersion, Credentials, Region};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::signer::{self, SignError, Signer as _};
use fermah_pay_stellar_gateway::signing::aws_kms::{AwsKmsSigner, KmsError};
use fermah_pay_stellar_gateway::signing::self_test;
use serde_json::{Value, json};

#[derive(Clone)]
struct Kms {
    key: Arc<SecretKey>,
    key_spec: &'static str,
    /// Signs with this key instead, as a service bound to the wrong key would.
    signs_with: Option<Arc<SecretKey>>,
    refuses_signing: bool,
}

impl Kms {
    fn new() -> Self {
        Self {
            key: Arc::new(SecretKey::generate().unwrap()),
            key_spec: "ECC_NIST_EDWARDS25519",
            signs_with: None,
            refuses_signing: false,
        }
    }
}

fn error(code: &str, message: &str) -> Response {
    let mut response = (
        StatusCode::BAD_REQUEST,
        [("content-type", "application/x-amz-json-1.1"), ("x-amzn-errortype", code)],
        json!({ "__type": code, "message": message }).to_string(),
    )
        .into_response();
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}

fn ok(body: &Value) -> Response {
    ([("content-type", "application/x-amz-json-1.1")], body.to_string()).into_response()
}

async fn serve(State(kms): State<Kms>, headers: HeaderMap, body: String) -> Response {
    let request: Value = serde_json::from_str(&body).unwrap();
    let target = headers.get("x-amz-target").and_then(|v| v.to_str().ok()).unwrap_or_default();
    match target {
        "TrentService.GetPublicKey" => {
            let mut der =
                vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
            der.extend_from_slice(kms.key.address().public_key());
            ok(&json!({
                "KeyId": request["KeyId"],
                "PublicKey": STANDARD.encode(der),
                "KeySpec": kms.key_spec,
                "KeyUsage": "SIGN_VERIFY",
                "SigningAlgorithms": ["ED25519_SHA_512", "ED25519_PH_SHA_512"],
            }))
        }
        "TrentService.Sign" => {
            if kms.refuses_signing {
                return error("AccessDeniedException", "not allowed to use this key");
            }
            if request["SigningAlgorithm"] != "ED25519_SHA_512" || request["MessageType"] != "RAW" {
                return error("ValidationException", "Ed25519 keys sign RAW with ED25519_SHA_512");
            }
            let message = STANDARD.decode(request["Message"].as_str().unwrap()).unwrap();
            let payload: [u8; 32] = message.try_into().unwrap();
            let key = kms.signs_with.as_ref().unwrap_or(&kms.key);
            ok(&json!({
                "KeyId": request["KeyId"],
                "Signature": STANDARD.encode(key.sign_raw(&payload)),
                "SigningAlgorithm": "ED25519_SHA_512",
            }))
        }
        other => error("UnknownOperationException", other),
    }
}

/// An SDK client pointed at a stand-in service with `kms`'s behaviour.
async fn client(kms: Kms) -> aws_sdk_kms::Client {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route("/", axum::routing::post(serve)).with_state(kms);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = aws_sdk_kms::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(format!("http://{addr}"))
        .credentials_provider(Credentials::new("test", "test", None, None, "test"))
        .build();
    aws_sdk_kms::Client::from_conf(config)
}

#[tokio::test]
async fn test_a_kms_key_signs_for_the_account_its_public_key_is() {
    let kms = Kms::new();
    let expected = kms.key.address();
    let signer = AwsKmsSigner::with_client(client(kms).await, "alias/operator").await.unwrap();
    assert_eq!(signer.address(), expected);
    self_test(&signer).await.unwrap();
    // A transaction's payload hash, signed and verified as every signature is.
    let payload = [7_u8; 32];
    let signature = signer::signature(&signer, &payload).await.unwrap();
    assert_eq!(signature, signer.sign(&payload).await.unwrap(), "Ed25519 is deterministic");
}

#[tokio::test]
async fn test_a_key_that_is_not_ed25519_is_refused_before_it_signs() {
    let kms = Kms { key_spec: "ECC_NIST_P256", ..Kms::new() };
    let refused = AwsKmsSigner::with_client(client(kms).await, "alias/operator").await;
    assert!(
        matches!(&refused, Err(KmsError::WrongKey { spec, .. }) if spec == "ECC_NIST_P256"),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_a_service_signing_with_another_key_fails_the_startup_check() {
    let kms = Kms { signs_with: Some(Arc::new(SecretKey::generate().unwrap())), ..Kms::new() };
    let expected = kms.key.address();
    let signer = AwsKmsSigner::with_client(client(kms).await, "alias/operator").await.unwrap();
    assert_eq!(self_test(&signer).await, Err(SignError::Invalid(expected)));
}

#[tokio::test]
async fn test_a_refused_signature_is_reported_as_a_service_failure() {
    let kms = Kms { refuses_signing: true, ..Kms::new() };
    let signer = AwsKmsSigner::with_client(client(kms).await, "alias/operator").await.unwrap();
    let Err(SignError::Service(message)) = self_test(&signer).await else {
        panic!("a refused signature must be a service failure")
    };
    assert!(message.contains("AccessDenied"), "{message}");
}

/// Needs AWS credentials and region in the environment, and the key id,
/// ARN or alias of an `ECC_NIST_EDWARDS25519` signing key.
#[tokio::test]
#[ignore = "needs an AWS KMS key named in PAY_STELLAR_TEST_AWS_KMS_KEY"]
async fn test_a_real_aws_kms_key_signs_stellar_payloads() {
    let key = std::env::var("PAY_STELLAR_TEST_AWS_KMS_KEY").unwrap();
    let signer = AwsKmsSigner::connect(&key).await.unwrap();
    self_test(&signer).await.unwrap();
    println!("{key} signs for {}", signer.address());
}
