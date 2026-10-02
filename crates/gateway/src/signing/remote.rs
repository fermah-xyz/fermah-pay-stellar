//! Keys behind a remote signer: an HTTP endpoint the operator runs in front
//! of whatever holds the key (a hardware security module, HashiCorp Vault,
//! another cloud's key management service). The key never reaches this
//! process; the endpoint is handed a 32-byte payload hash and answers with
//! its Ed25519 signature. `docs/self-hosting/keys.md` specifies the protocol.
//!
//! The endpoint is trusted for availability only: every signature it
//! returns is verified against the account before it is used, so one that
//! signs with another key, or returns garbage, is refused before anything
//! reaches the network.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fermah_pay_stellar_chain::signer::{SignError, SignFuture, Signer};
use fermah_pay_stellar_domain::AccountAddress;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use zeroize::Zeroizing;

/// How long one request to the signer may take.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("{0}: a remote signer is reached over https, or over plain http on this host only")]
    Insecure(String),
    #[error(
        "{0}: a remote signer on another host needs a bearer token \
         (PAY_STELLAR_REMOTE_SIGNER_TOKEN_FILE) or a client certificate \
         (PAY_STELLAR_REMOTE_SIGNER_CLIENT_CERT_FILE and _KEY_FILE)"
    )]
    Unauthenticated(String),
    #[error(
        "a client certificate needs both PAY_STELLAR_REMOTE_SIGNER_CLIENT_CERT_FILE and _KEY_FILE"
    )]
    HalfIdentity,
    #[error("{path} is accessible to other users (mode {mode:o}); restrict it to 0600")]
    Exposed { path: PathBuf, mode: u32 },
    #[error("reading {path}: {detail}")]
    Unreadable { path: PathBuf, detail: String },
    #[error("building the HTTP client: {0}")]
    Client(String),
    #[error("the remote signer at {url} {detail}")]
    Answer { url: String, detail: String },
}

/// What reaching the signer takes, beyond its URL.
#[derive(Default)]
pub struct RemoteSettings {
    /// Sent as `Authorization: Bearer <token>`.
    pub token: Option<Zeroizing<String>>,
    /// Certificates the signer's server certificate must chain to, instead
    /// of the public web roots.
    pub ca: Option<Vec<CertificateDer<'static>>>,
    /// A client certificate and its key, for mutual TLS.
    pub identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
}

impl RemoteSettings {
    /// The settings in the `PAY_STELLAR_REMOTE_SIGNER_*` variables. Files
    /// holding a secret (the token, the client key) must be readable by their
    /// owner only, as seed files are.
    pub fn from_env() -> Result<Self, RemoteError> {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let token = var("PAY_STELLAR_REMOTE_SIGNER_TOKEN_FILE")
            .map(|path| {
                let text = read_secret(&path)?;
                Ok::<_, RemoteError>(Zeroizing::new(text.trim().to_owned()))
            })
            .transpose()?;
        let ca =
            var("PAY_STELLAR_REMOTE_SIGNER_CA_FILE").map(|path| certificates(&path)).transpose()?;
        let identity = match (
            var("PAY_STELLAR_REMOTE_SIGNER_CLIENT_CERT_FILE"),
            var("PAY_STELLAR_REMOTE_SIGNER_CLIENT_KEY_FILE"),
        ) {
            (None, None) => None,
            (Some(cert), Some(key)) => {
                let pem = read_secret(&key)?;
                let key = PrivateKeyDer::from_pem_slice(pem.as_bytes())
                    .map_err(|e| unreadable(&key, &e.to_string()))?;
                Some((certificates(&cert)?, key))
            }
            _ => return Err(RemoteError::HalfIdentity),
        };
        Ok(Self { token, ca, identity })
    }
}

fn unreadable(path: &Path, detail: &str) -> RemoteError {
    RemoteError::Unreadable { path: path.to_owned(), detail: detail.to_owned() }
}

fn read_secret(path: &Path) -> Result<Zeroizing<String>, RemoteError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| unreadable(path, &e.to_string()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(RemoteError::Exposed { path: path.to_owned(), mode: mode & 0o777 });
        }
    }
    std::fs::read_to_string(path).map(Zeroizing::new).map_err(|e| unreadable(path, &e.to_string()))
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, RemoteError> {
    let certificates = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| unreadable(path, &e.to_string()))?;
    if certificates.is_empty() {
        return Err(unreadable(path, "no PEM certificate"));
    }
    Ok(certificates)
}

/// A key behind a remote signer, bound at startup to the account the signer
/// says it signs for.
pub struct RemoteSigner {
    client: reqwest::Client,
    url: reqwest::Url,
    token: Option<Zeroizing<String>>,
    address: AccountAddress,
}

impl fmt::Debug for RemoteSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSigner")
            .field("url", &self.url.as_str())
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct AccountAnswer {
    account: String,
}

#[derive(Deserialize)]
struct SignatureAnswer {
    signature: String,
}

impl RemoteSigner {
    /// Reaches the signer at `url` and asks which account it signs for.
    pub async fn connect(url: &str, settings: RemoteSettings) -> Result<Self, RemoteError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| RemoteError::Insecure(url.to_owned()))?;
        let local = parsed.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        match parsed.scheme() {
            "https" => {}
            "http" if local => {}
            _ => return Err(RemoteError::Insecure(url.to_owned())),
        }
        if !local && settings.token.is_none() && settings.identity.is_none() {
            return Err(RemoteError::Unauthenticated(url.to_owned()));
        }
        let client = client(settings.ca, settings.identity)?;
        let mut signer = Self {
            client,
            url: parsed,
            token: settings.token,
            address: AccountAddress::from_public_key([0; 32]),
        };
        let answer: AccountAnswer = signer.call(signer.client.get(signer.url.clone())).await?;
        signer.address = answer.account.parse().map_err(|_| RemoteError::Answer {
            url: signer.url.to_string(),
            detail: "named no classic account (G...)".to_owned(),
        })?;
        Ok(signer)
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, RemoteError> {
        let answer = |detail: String| RemoteError::Answer { url: self.url.to_string(), detail };
        let request = match &self.token {
            Some(token) => request.bearer_auth(token.as_str()),
            None => request,
        };
        let response = request.send().await.map_err(|e| answer(format!("is unreachable: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(answer(format!("answered HTTP {status}")));
        }
        response.json().await.map_err(|e| answer(format!("answered unreadably: {e}")))
    }
}

/// An HTTP client verifying the server against `ca` (the public web roots
/// without it) and presenting `identity` when given.
fn client(
    ca: Option<Vec<CertificateDer<'static>>>,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
) -> Result<reqwest::Client, RemoteError> {
    let mut roots = rustls::RootCertStore::empty();
    match ca {
        Some(certificates) => {
            for certificate in certificates {
                roots.add(certificate).map_err(|e| RemoteError::Client(e.to_string()))?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| RemoteError::Client(e.to_string()))?
    .with_root_certificates(roots);
    let tls = match identity {
        Some((chain, key)) => builder
            .with_client_auth_cert(chain, key)
            .map_err(|e| RemoteError::Client(e.to_string()))?,
        None => builder.with_no_client_auth(),
    };
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| RemoteError::Client(e.to_string()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N {
        return None;
    }
    let mut out = [0_u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

impl Signer for RemoteSigner {
    fn address(&self) -> AccountAddress {
        self.address.clone()
    }

    fn sign<'a>(&'a self, payload: &'a [u8; 32]) -> SignFuture<'a> {
        Box::pin(async move {
            let request = self
                .client
                .post(self.url.clone())
                .json(&serde_json::json!({ "payload": hex(payload) }));
            let answer: SignatureAnswer =
                self.call(request).await.map_err(|e| SignError::Service(e.to_string()))?;
            unhex::<64>(&answer.signature).ok_or_else(|| {
                SignError::Service(format!(
                    "the remote signer at {} answered a signature that is not 64 bytes of hex",
                    self.url
                ))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use fermah_pay_stellar_chain::keys::SecretKey;
    use fermah_pay_stellar_chain::signer;

    use super::*;

    /// A signer endpoint answering for `account`, signing with `key` (the
    /// account's own, or another, as a misconfigured one would), and
    /// requiring `token` when given.
    #[derive(Clone)]
    struct Endpoint {
        account: AccountAddress,
        key: Arc<SecretKey>,
        token: Option<&'static str>,
    }

    fn authorized(endpoint: &Endpoint, headers: &HeaderMap) -> bool {
        endpoint.token.is_none_or(|token| {
            headers.get("authorization").and_then(|v| v.to_str().ok())
                == Some(format!("Bearer {token}").as_str())
        })
    }

    fn router(endpoint: Endpoint) -> Router {
        async fn account(
            State(e): State<Endpoint>,
            headers: HeaderMap,
        ) -> Result<axum::Json<serde_json::Value>, StatusCode> {
            if !authorized(&e, &headers) {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok(axum::Json(serde_json::json!({ "account": e.account.to_string() })))
        }
        async fn sign(
            State(e): State<Endpoint>,
            headers: HeaderMap,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> Result<axum::Json<serde_json::Value>, StatusCode> {
            if !authorized(&e, &headers) {
                return Err(StatusCode::UNAUTHORIZED);
            }
            let payload = unhex::<32>(body["payload"].as_str().unwrap_or_default())
                .ok_or(StatusCode::BAD_REQUEST)?;
            Ok(axum::Json(serde_json::json!({ "signature": hex(&e.key.sign_raw(&payload)) })))
        }
        Router::new().route("/sign", get(account).post(sign)).with_state(endpoint)
    }

    /// Serves `endpoint` over plain HTTP on this host; returns its URL.
    async fn serve_http(endpoint: Endpoint) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(endpoint)).await });
        format!("http://{addr}/sign")
    }

    fn endpoint(token: Option<&'static str>) -> (Endpoint, SecretKey) {
        let key = SecretKey::generate().unwrap();
        let seed = SecretKey::from_strkey(&key.to_strkey()).unwrap();
        (Endpoint { account: key.address(), key: Arc::new(seed), token }, key)
    }

    fn with_token(token: &str) -> RemoteSettings {
        RemoteSettings {
            token: Some(Zeroizing::new(token.to_owned())),
            ..RemoteSettings::default()
        }
    }

    #[tokio::test]
    async fn test_a_remote_signer_signs_for_the_account_it_names() {
        let (endpoint, key) = endpoint(Some("secret-token"));
        let url = serve_http(endpoint).await;
        let remote = RemoteSigner::connect(&url, with_token("secret-token")).await.unwrap();
        assert_eq!(remote.address(), key.address());
        // The signature comes back and verifies for that account.
        let payload = [7_u8; 32];
        assert_eq!(signer::signature(&remote, &payload).await, Ok(key.sign_raw(&payload)));
        assert!(!format!("{remote:?}").contains("secret-token"));
    }

    #[tokio::test]
    async fn test_a_key_reference_naming_a_remote_signer_opens_it() {
        // Through the worker's own entry point, which reads no remote-signer
        // setting here: a signer on this host needs none.
        let (endpoint, key) = endpoint(None);
        let url = serve_http(endpoint).await;
        let opened = super::super::open(&url).await.unwrap();
        assert_eq!(opened.address(), key.address());
    }

    #[tokio::test]
    async fn test_a_remote_signer_refuses_a_wrong_token() {
        let (endpoint, _) = endpoint(Some("secret-token"));
        let url = serve_http(endpoint).await;
        let refused = RemoteSigner::connect(&url, with_token("another")).await.unwrap_err();
        assert!(
            matches!(&refused, RemoteError::Answer { detail, .. } if detail.contains("401")),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn test_a_signer_signing_with_another_key_fails_its_startup_test() {
        let (mut endpoint, _) = endpoint(None);
        endpoint.key = Arc::new(SecretKey::generate().unwrap());
        let url = serve_http(endpoint).await;
        let remote = RemoteSigner::connect(&url, RemoteSettings::default()).await.unwrap();
        assert!(matches!(super::super::self_test(&remote).await, Err(SignError::Invalid(_))));
    }

    #[tokio::test]
    async fn test_a_remote_signer_elsewhere_needs_tls_and_credentials() {
        let insecure = RemoteSigner::connect("http://signer.example/sign", with_token("t")).await;
        assert!(matches!(insecure, Err(RemoteError::Insecure(_))), "{insecure:?}");
        let anonymous =
            RemoteSigner::connect("https://signer.example/sign", RemoteSettings::default()).await;
        assert!(matches!(anonymous, Err(RemoteError::Unauthenticated(_))), "{anonymous:?}");
    }

    /// A certificate authority, and a certificate it issued for `name`.
    fn issue(
        ca: &(rcgen::Certificate, rcgen::Issuer<'static, rcgen::KeyPair>),
        name: &str,
    ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
        let certificate = params.signed_by(&key, &ca.1).unwrap();
        (certificate.der().clone(), PrivateKeyDer::try_from(key.serialize_der()).unwrap())
    }

    fn authority() -> (rcgen::Certificate, rcgen::Issuer<'static, rcgen::KeyPair>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let certificate = params.self_signed(&key).unwrap();
        (certificate, rcgen::Issuer::new(params, key))
    }

    /// Serves `endpoint` over TLS on this host, requiring a client
    /// certificate issued by `clients`; returns its URL.
    async fn serve_mtls(
        endpoint: Endpoint,
        server: (CertificateDer<'static>, PrivateKeyDer<'static>),
        clients: CertificateDer<'static>,
    ) -> String {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(clients).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![server.0], server.1)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = router(endpoint);
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let (acceptor, app) = (acceptor.clone(), app.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let service = hyper_util::service::TowerToHyperService::new(app);
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
                });
            }
        });
        format!("https://localhost:{port}/sign")
    }

    #[tokio::test]
    async fn test_a_remote_signer_over_mutual_tls() {
        let ca = authority();
        let (endpoint, key) = endpoint(None);
        let url = serve_mtls(endpoint, issue(&ca, "localhost"), ca.0.der().clone()).await;
        let (client_cert, client_key) = issue(&ca, "worker");
        let settings = RemoteSettings {
            token: None,
            ca: Some(vec![ca.0.der().clone()]),
            identity: Some((vec![client_cert], client_key)),
        };
        let remote = RemoteSigner::connect(&url, settings).await.unwrap();
        assert_eq!(remote.address(), key.address());
        let payload = [9_u8; 32];
        assert_eq!(signer::signature(&remote, &payload).await, Ok(key.sign_raw(&payload)));

        // Without a client certificate the server refuses the connection.
        let anonymous = RemoteSettings {
            token: Some(Zeroizing::new("t".to_owned())),
            ca: Some(vec![ca.0.der().clone()]),
            identity: None,
        };
        let refused = RemoteSigner::connect(&url, anonymous).await.unwrap_err();
        assert!(
            matches!(&refused, RemoteError::Answer { detail, .. } if detail.contains("unreachable")),
            "{refused}"
        );
    }
}
