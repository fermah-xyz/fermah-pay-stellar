//! The x402 commitment verifier on testnet. Its address derives from the
//! deployer and the Wasm's hash, so deploying the same code again finds the
//! instance already there and only records it.

use anyhow::ensure;
use fermah_pay_stellar_chain::prepaid::{contract_instance_key, instance_wasm};
use fermah_pay_stellar_chain::rpc::hex_lower;
use fermah_pay_stellar_chain::stellar_xdr::{ContractId, Hash, ScAddress, ScVal};
use fermah_pay_stellar_chain::{deploy, usdc};
use serde_json::json;

use super::{Context, FEE_SOURCE, SUBMITTER, receipt_json};
use crate::testnet::evidence;
use crate::testnet::profile::X402Verifier;

impl Context {
    /// Deploys `wasm`, the `contracts/x402-verifier` build, unless an
    /// instance of it is already at its address, and records it in the
    /// profile for the gateways this tool runs.
    pub async fn deploy_x402_verifier(&self, wasm: &[u8]) -> anyhow::Result<()> {
        self.rpc.verify_network(self.network).await?;
        let (source, fee_source) = (self.profile.key(SUBMITTER)?, self.profile.key(FEE_SOURCE)?);
        let submitter = self.submitter(&source, &fee_source);
        let wasm_hash = deploy::wasm_hash(wasm);
        let id = deploy::contract_id(self.network, &source.address(), wasm_hash);
        let instance = contract_instance_key(id);
        let deployed =
            if self.rpc.get_ledger_entries(std::slice::from_ref(&instance)).await?.is_empty() {
                let upload = deploy::upload(wasm)?;
                let auth = submitter.record_source_authorization(&upload).await?;
                let uploaded = submitter.submit(upload, auth).await?;
                let create = deploy::create(&source.address(), wasm_hash, wasm_hash, vec![])?;
                let auth = submitter.record_source_authorization(&create).await?;
                let created = submitter.submit(create, auth).await?;
                ensure!(
                    created.return_value
                        == Some(ScVal::Address(ScAddress::Contract(ContractId(Hash(id))))),
                    "the network created {:?}, expected {}",
                    created.return_value,
                    usdc::contract_strkey(id)
                );
                json!({ "upload": receipt_json(&uploaded), "create": receipt_json(&created) })
            } else {
                json!("already deployed")
            };
        let running = self.rpc.get_ledger_entries(&[instance]).await?;
        let running = running.first().and_then(|entry| instance_wasm(&entry.data));
        ensure!(
            running == Some(wasm_hash),
            "the instance at {} runs {:?}, not the given Wasm",
            usdc::contract_strkey(id),
            running.map(|hash| hex_lower(&hash))
        );
        let verifier = X402Verifier {
            contract: usdc::contract_strkey(id),
            wasm_sha256: hex_lower(&wasm_hash),
        };
        self.profile.save_x402_verifier(&verifier)?;
        evidence::write(
            &self.evidence_dir,
            "x402-verifier-deployment",
            json!({
                "criterion": "x402-verifier-deployment",
                "expected": "the x402 commitment verifier runs the given Wasm at the address derived from the deployer and the Wasm's hash",
                "contract": verifier.contract,
                "wasm_sha256": verifier.wasm_sha256,
                "deployment": deployed,
            }),
        )
    }
}
