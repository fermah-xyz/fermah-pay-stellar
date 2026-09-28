//! Testnet Friendbot funding for operator-side accounts (sponsors, fee
//! sources). Never used for a buyer whose zero-XLM property is under test.

use std::time::Duration;

use fermah_pay_stellar_domain::AccountAddress;

#[derive(Debug, thiserror::Error)]
pub enum FriendbotError {
    #[error("building HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("friendbot request failed")]
    Request(#[source] reqwest::Error),
}

pub async fn fund(friendbot_url: &str, account: &AccountAddress) -> Result<(), FriendbotError> {
    let http = reqwest::Client::builder()
        .tls_backend_preconfigured(crate::rpc::tls_config())
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(FriendbotError::Client)?;
    http.get(friendbot_url)
        .query(&[("addr", account.as_str())])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(FriendbotError::Request)?;
    Ok(())
}
