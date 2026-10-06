//! API-key authentication.
//!
//! A key is `fps_test_`, `fps_live_` or `fps_local_` followed by 43 base64url characters
//! (256 random bits). The prefix names the network the key's deployment is
//! pinned to, so a leaked key's blast radius is obvious from the text, and a
//! key for the other network is refused before any database work.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fermah_pay_stellar_domain::Network;
use http::{HeaderMap, Request, Response};
use sha2::{Digest, Sha256};
use tonic::body::Body;
use tower::{Layer, Service};
use zeroize::Zeroizing;

use crate::refusal::Refusal;
use crate::scope::Scope;
use crate::store::Store;

const TOKEN_RANDOM_BYTES: usize = 32;
const TOKEN_ENCODED_LEN: usize = 43;

/// gRPC paths reachable without a key: liveness probing only.
const UNAUTHENTICATED_PATH_PREFIX: &str = "/grpc.health.v1.Health/";

#[must_use]
pub const fn token_prefix(network: Network) -> &'static str {
    match network {
        Network::Testnet => "fps_test_",
        Network::Pubnet => "fps_live_",
        Network::Local => "fps_local_",
    }
}

pub(crate) fn generate_token(network: Network) -> Result<Zeroizing<String>, getrandom::Error> {
    let mut random = Zeroizing::new([0_u8; TOKEN_RANDOM_BYTES]);
    getrandom::fill(random.as_mut())?;
    let mut token = Zeroizing::new(String::from(token_prefix(network)));
    URL_SAFE_NO_PAD.encode_string(random.as_ref(), &mut token);
    Ok(token)
}

pub(crate) fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Returns the token if it is shaped like a key of `network`.
fn well_formed_token(token: &str, network: Network) -> Option<&str> {
    let body = token.strip_prefix(token_prefix(network))?;
    let alphabet = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_';
    (body.len() == TOKEN_ENCODED_LEN && body.bytes().all(alphabet)).then_some(token)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(http::header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// Resolves the caller's scope from the `authorization: Bearer <key>` header.
pub async fn authenticate(
    store: &Store,
    network: Network,
    headers: &HeaderMap,
) -> Result<Scope, Refusal> {
    let Some(token) = bearer_token(headers).and_then(|t| well_formed_token(t, network)) else {
        return Err(Refusal::Unauthenticated);
    };
    match store.scope_for_token(&token_digest(token), network).await {
        Ok(Some(scope)) => Ok(scope),
        Ok(None) => Err(Refusal::Unauthenticated),
        Err(error) => {
            tracing::error!(error = %error, "api key lookup failed");
            Err(Refusal::Internal)
        }
    }
}

/// Tower layer that authenticates every request except health probes and
/// attaches the resulting [`Scope`] as a request extension. Handlers read the
/// scope from there; a missing scope is refused, so a route that bypassed
/// this layer fails closed.
#[derive(Clone)]
pub struct AuthLayer {
    store: Store,
    network: Network,
}

impl AuthLayer {
    #[must_use]
    pub const fn new(store: Store, network: Network) -> Self {
        Self { store, network }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService { inner, store: self.store.clone(), network: self.network }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    store: Store,
    network: Network,
}

impl<S> Service<Request<Body>> for AuthService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        // The readied service is the one that must handle this request; the
        // clone takes its place for the next call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let store = self.store.clone();
        let network = self.network;
        Box::pin(async move {
            if !request.uri().path().starts_with(UNAUTHENTICATED_PATH_PREFIX) {
                match authenticate(&store, network, request.headers()).await {
                    Ok(scope) => {
                        request.extensions_mut().insert(scope);
                    }
                    Err(refusal) => {
                        tracing::warn!(
                            error.kind = "authn-failed",
                            path = request.uri().path(),
                            reason = refusal.reason(),
                        );
                        return Ok(tonic::Status::from(refusal).into_http());
                    }
                }
            }
            inner.call(request).await
        })
    }
}

/// The scope attached by [`AuthLayer`].
pub fn scope_of<T>(request: &tonic::Request<T>) -> Result<Scope, tonic::Status> {
    request.extensions().get::<Scope>().cloned().ok_or_else(|| {
        tracing::error!("request reached a handler without an authenticated scope");
        Refusal::Unauthenticated.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generated_token_is_well_formed_for_its_network() {
        let token = generate_token(Network::Testnet).unwrap();
        assert_eq!(well_formed_token(&token, Network::Testnet), Some(token.as_str()));
    }

    #[test]
    fn test_token_of_other_network_is_not_well_formed() {
        let token = generate_token(Network::Pubnet).unwrap();
        assert_eq!(well_formed_token(&token, Network::Testnet), None);
    }

    #[test]
    fn test_truncated_token_is_not_well_formed() {
        let token = generate_token(Network::Testnet).unwrap();
        assert_eq!(well_formed_token(&token[..token.len() - 1], Network::Testnet), None);
    }

    fn unreachable_store() -> Store {
        // Nothing listens on port 1: any query fails.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        Store::new(pool)
    }

    fn with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn test_other_network_key_is_refused_before_database_lookup() {
        let token = generate_token(Network::Pubnet).unwrap();
        let result =
            authenticate(&unreachable_store(), Network::Testnet, &with_bearer(&token)).await;
        assert_eq!(result, Err(Refusal::Unauthenticated));
    }

    #[tokio::test]
    async fn test_well_formed_key_is_looked_up_in_database() {
        // Control for the test above: a key of the right network does reach
        // the (unreachable) database and fails as an internal error.
        let token = generate_token(Network::Testnet).unwrap();
        let result =
            authenticate(&unreachable_store(), Network::Testnet, &with_bearer(&token)).await;
        assert_eq!(result, Err(Refusal::Internal));
    }

    #[test]
    fn test_generated_tokens_differ() {
        assert_ne!(
            *generate_token(Network::Testnet).unwrap(),
            *generate_token(Network::Testnet).unwrap()
        );
    }
}
