//! Where the worker's keys come from. A key reference is either the path of
//! a seed file on this host or `<service>://<key>`, naming a key in a key
//! management service. Only seed files are served by this build; a service
//! reference is refused at startup rather than ignored.
//!
//! Every signer is tested once at startup: it signs a random payload, and the
//! signature must verify for the address it claims, before any transaction
//! is built with it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::signer::{self, LocalSigner, SignError, Signer};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("key reference `{0}` uses a scheme this build does not serve")]
    Unsupported(String),
    #[error("reading key file {path}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("key file {path} is accessible to other users (mode {mode:o}); restrict it to 0600")]
    Exposed { path: PathBuf, mode: u32 },
    #[error("key file {path} does not hold a Stellar seed")]
    Invalid { path: PathBuf },
    #[error("the signer for {reference} failed its startup test")]
    SelfTest {
        reference: String,
        #[source]
        source: SignError,
    },
}

/// Opens the signer `reference` names and tests it.
pub async fn open(reference: &str) -> Result<Arc<dyn Signer>, KeyError> {
    let signer: Arc<dyn Signer> = match reference.split_once("://") {
        Some((scheme, _)) => return Err(KeyError::Unsupported(scheme.to_owned())),
        None => Arc::new(LocalSigner::new(read_seed(Path::new(reference))?)),
    };
    self_test(signer.as_ref())
        .await
        .map_err(|source| KeyError::SelfTest { reference: redact(reference), source })?;
    Ok(signer)
}

/// Signs a random payload and checks the signature against the signer's
/// address.
pub async fn self_test(signer: &dyn Signer) -> Result<(), SignError> {
    let mut payload = [0_u8; 32];
    getrandom::fill(&mut payload).map_err(|e| SignError::Service(e.to_string()))?;
    signer::signature(signer, &payload).await.map(|_| ())
}

/// A reference fit for logs: a path or a service key identifier, never
/// credentials a URL might carry.
fn redact(reference: &str) -> String {
    match reference.split_once("://") {
        Some((scheme, rest)) => {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
            format!("{scheme}://{rest}")
        }
        None => reference.to_owned(),
    }
}

fn read_seed(path: &Path) -> Result<SecretKey, KeyError> {
    let unreadable = |source| KeyError::Unreadable { path: path.to_owned(), source };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).map_err(unreadable)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(KeyError::Exposed { path: path.to_owned(), mode: mode & 0o777 });
        }
    }
    let seed = Zeroizing::new(std::fs::read_to_string(path).map_err(unreadable)?);
    SecretKey::from_strkey(seed.trim()).map_err(|_| KeyError::Invalid { path: path.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_file(dir: &Path, name: &str, mode: u32) -> (PathBuf, SecretKey) {
        let key = SecretKey::generate().unwrap();
        let path = dir.join(name);
        std::fs::write(&path, key.to_strkey().as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        (path, key)
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pay-stellar-keys-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_a_private_seed_file_opens_by_path() {
        let dir = scratch();
        let (path, key) = key_file(&dir, "operator.secret", 0o600);
        let signer = open(path.to_str().unwrap()).await.unwrap();
        assert_eq!(signer.address(), key.address());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_seed_file_readable_by_others_is_refused() {
        let dir = scratch();
        let (path, _) = key_file(&dir, "operator.secret", 0o644);
        let error = open(path.to_str().unwrap()).await.err();
        assert!(matches!(error, Some(KeyError::Exposed { mode: 0o644, .. })), "{error:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn test_a_scheme_this_build_does_not_serve_is_refused() {
        let error = open("aws-kms://alias/operator").await.err();
        assert!(matches!(&error, Some(KeyError::Unsupported(scheme)) if scheme == "aws-kms"));
    }

    #[test]
    fn test_references_are_logged_without_credentials() {
        assert_eq!(
            redact("vault://token:secret@vault.internal/transit/op"),
            "vault://vault.internal/transit/op"
        );
        assert_eq!(redact("/etc/keys/op.secret"), "/etc/keys/op.secret");
    }
}
