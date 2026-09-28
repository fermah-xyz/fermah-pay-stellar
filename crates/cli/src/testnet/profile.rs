//! A directory of testnet keys and deployment state, outside the repository.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fermah_pay_stellar_chain::keys::SecretKey;
use serde::{Deserialize, Serialize};

pub struct Profile {
    dir: PathBuf,
}

/// What a deployment is pinned to, recorded when it is created.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deployment {
    pub contract: String,
    pub wasm_sha256: String,
    pub usdc: String,
    pub admin: String,
    pub operator: String,
    pub seller: String,
    pub treasury: String,
    pub min_deposit: i128,
    pub max_charge: i128,
}

impl Profile {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("buyers"))
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir: dir.to_owned() })
    }

    /// The key stored under `name`, created on first use.
    pub fn key(&self, name: &str) -> anyhow::Result<SecretKey> {
        self.key_at(&self.dir.join(format!("{name}.secret")))
    }

    pub fn buyer(&self, index: u32) -> anyhow::Result<SecretKey> {
        self.key_at(&self.dir.join("buyers").join(format!("{index:03}.secret")))
    }

    pub fn has_key(&self, name: &str) -> bool {
        self.dir.join(format!("{name}.secret")).exists()
    }

    fn key_at(&self, path: &Path) -> anyhow::Result<SecretKey> {
        if path.exists() {
            let seed = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            return Ok(SecretKey::from_strkey(seed.trim())?);
        }
        let key = SecretKey::generate()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file =
            options.open(path).with_context(|| format!("creating {}", path.display()))?;
        file.write_all(key.to_strkey().as_bytes())?;
        file.write_all(b"\n")?;
        Ok(key)
    }

    pub fn save_deployment(&self, deployment: &Deployment) -> anyhow::Result<()> {
        let path = self.dir.join("deployment.json");
        std::fs::write(&path, serde_json::to_vec_pretty(deployment)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn deployment(&self) -> anyhow::Result<Deployment> {
        let path = self.dir.join("deployment.json");
        if !path.exists() {
            bail!("no deployment recorded in {}; run deploy-prepaid first", self.dir.display());
        }
        Ok(serde_json::from_slice(&std::fs::read(&path)?)?)
    }
}
