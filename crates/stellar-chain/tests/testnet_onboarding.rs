//! Live Stellar testnet check of sponsored buyer onboarding.
//!
//! Ignored by default because it needs network access and Friendbot; run
//! with `cargo test -p fermah-pay-stellar-chain --test testnet_onboarding --
//! --ignored`. Set `STELLAR_TESTNET_RPC_URL` to use a provider other than the
//! public SDF endpoint.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use fermah_pay_stellar_chain::keys::SecretKey;
use fermah_pay_stellar_chain::onboarding::{
    SubmissionPolicy, onboard_buyer, provenance_keys, reserve_provenance,
};
use fermah_pay_stellar_chain::rpc::RpcClient;
use fermah_pay_stellar_chain::stellar_xdr::{
    AccountEntryExt, AccountEntryExtensionV1Ext, LedgerEntryData,
};
use fermah_pay_stellar_chain::{friendbot, usdc};
use fermah_pay_stellar_domain::Network;

const DEFAULT_RPC: &str = "https://soroban-testnet.stellar.org";

fn rpc() -> RpcClient {
    let url = std::env::var("STELLAR_TESTNET_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_owned());
    RpcClient::new(&url, Duration::from_secs(30)).unwrap()
}

#[tokio::test]
#[ignore = "requires Stellar testnet and Friendbot"]
async fn test_sponsored_onboarding_leaves_buyer_without_xlm_on_testnet() {
    let rpc = rpc();
    let info = rpc.verify_network(Network::Testnet).await.unwrap();
    let sponsor = SecretKey::generate().unwrap();
    let buyer = SecretKey::generate().unwrap();
    friendbot::fund(info.friendbot_url.as_deref().unwrap(), &sponsor.address()).await.unwrap();

    let usdc = usdc::circle_usdc(Network::Testnet);
    // Pre-state: the buyer must not exist, or the zero-XLM result could be
    // an artefact of an earlier run rather than of this transaction.
    let before = rpc.get_ledger_entries(&provenance_keys(&buyer.address(), &usdc)).await.unwrap();
    assert!(before.is_empty(), "fresh buyer key unexpectedly has ledger entries");

    let policy = SubmissionPolicy {
        fee_stroops: 10_000,
        validity: Duration::from_secs(120),
        poll_interval: Duration::from_secs(2),
    };
    let receipt =
        onboard_buyer(&rpc, Network::Testnet, &sponsor, &buyer, &usdc, policy).await.unwrap();

    assert_eq!(receipt.provenance.violations_for_sponsor(&sponsor.address()), vec![]);
    assert_eq!(receipt.fee_source, sponsor.address());
    assert!(receipt.fee_charged_stroops > 0, "sponsor paid no fee");

    // Independent read of the sponsor side. The counter is in base reserves,
    // not entries: an account costs two base reserves and a trustline one.
    let sponsor_entry =
        rpc.get_ledger_entries(&provenance_keys(&sponsor.address(), &usdc)[..1]).await.unwrap();
    let LedgerEntryData::Account(account) = &sponsor_entry[0].data else {
        panic!("sponsor account entry missing");
    };
    let AccountEntryExt::V1(v1) = &account.ext else { panic!("sponsor has no v1 extension") };
    let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else { panic!("sponsor has no v2 extension") };
    assert_eq!(v2.num_sponsoring, 3);

    // The same classifier over a fresh read agrees with the receipt.
    let records = rpc.get_ledger_entries(&provenance_keys(&buyer.address(), &usdc)).await.unwrap();
    let reread =
        reserve_provenance(&buyer.address(), &usdc, records.iter().map(|r| (&r.data, &r.ext)))
            .unwrap();
    assert_eq!(reread, receipt.provenance);
}

#[tokio::test]
#[ignore = "requires Stellar testnet"]
async fn test_testnet_endpoint_is_refused_for_pubnet_process() {
    let error = rpc().verify_network(Network::Pubnet).await.unwrap_err();
    assert!(
        matches!(&error, fermah_pay_stellar_chain::rpc::RpcError::WrongNetwork { expected: Network::Pubnet, actual } if actual == Network::Testnet.passphrase()),
        "{error:?}"
    );
}
