//! Checks both processes make against their RPC endpoint before serving.

use fermah_pay_stellar_chain::rpc::{NetworkInfo, RpcClient, RpcError};
use fermah_pay_stellar_domain::Network;

/// The oldest network protocol this release works with. Protocol 27 brought
/// `AddressV2` authorization credentials, which the worker signs for every
/// batch and which buyers sign for their deposits; protocol 23 brought
/// restoration of archived entries inside the invocation that touches them,
/// which settlement relies on.
pub const MIN_PROTOCOL: u32 = 27;

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(
        "the RPC endpoint reports protocol {actual}, but this release needs protocol \
         {MIN_PROTOCOL} or later (AddressV2 authorization credentials)"
    )]
    ProtocolTooOld { actual: u32 },
}

/// Fails unless the endpoint serves `network` at protocol [`MIN_PROTOCOL`]
/// or later.
pub async fn verify_rpc(rpc: &RpcClient, network: Network) -> Result<NetworkInfo, StartupError> {
    let info = rpc.verify_network(network).await?;
    require_protocol(&info)?;
    Ok(info)
}

pub const fn require_protocol(info: &NetworkInfo) -> Result<(), StartupError> {
    if info.protocol_version < MIN_PROTOCOL {
        return Err(StartupError::ProtocolTooOld { actual: info.protocol_version });
    }
    Ok(())
}
