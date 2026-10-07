//! Live Stellar testnet check of the reads the chain observer relies on:
//! `getHealth`, `getEvents` paging, and the prepaid ledger's instance entry
//! read together with the treasury's USDC trustline. Read-only: nothing is
//! signed or submitted.
//!
//! Ignored by default because it needs network access; run with `cargo test
//! -p fermah-pay-stellar-chain --test testnet_events -- --ignored`. It reads
//! the prepaid ledger deployment recorded in `docs/evidence`, or the contract
//! in `STELLAR_TESTNET_PREPAID_CONTRACT`; set `STELLAR_TESTNET_RPC_URL` to use
//! a provider other than the public SDF endpoint.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use fermah_pay_stellar_chain::prepaid::{Custody, PrepaidDeployment, instance_state};
use fermah_pay_stellar_chain::rpc::{EventCursor, EventsFrom, RpcClient, RpcError};
use fermah_pay_stellar_chain::stellar_xdr::LedgerEntryData;
use fermah_pay_stellar_chain::usdc;
use fermah_pay_stellar_domain::Network;

const DEFAULT_RPC: &str = "https://soroban-testnet.stellar.org";
const RECORDED_CONTRACT: &str = "CDDFMUZ7RT7RYBLL4MGC3WTR7YEFSCLCZATIJN57XGKV45QCEJE5GNPV";

fn rpc() -> RpcClient {
    let url = std::env::var("STELLAR_TESTNET_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_owned());
    RpcClient::new(&url, Duration::from_secs(30)).unwrap()
}

fn contract() -> [u8; 32] {
    let text = std::env::var("STELLAR_TESTNET_PREPAID_CONTRACT")
        .unwrap_or_else(|_| RECORDED_CONTRACT.to_owned());
    stellar_strkey::Contract::from_string(&text).unwrap().0
}

#[tokio::test]
#[ignore = "requires Stellar testnet"]
async fn test_event_pages_end_at_their_last_event_or_their_scan_window() {
    let rpc = rpc();
    rpc.verify_network(Network::Testnet).await.unwrap();
    let health = rpc.get_health().await.unwrap();
    let contract = contract();
    // A start the retention window still covers even if it slides a little
    // while the test runs.
    let start = health.oldest_ledger + 100;
    let page = rpc.get_events(&contract, &EventsFrom::Ledger(start), 1).await.unwrap();
    match page.events.as_slice() {
        [event] => {
            assert_eq!(page.cursor, event.id, "a full page ends at its last event");
            assert_eq!(event.contract, contract);
            assert!(event.ledger >= start);
        }
        [] => assert_eq!(
            page.cursor,
            EventCursor::end_of_ledger((start + 9_999).min(page.latest_ledger)),
            "a short page ends at its scan window"
        ),
        more => panic!("limit 1 returned {} events", more.len()),
    }
    // Continuing from the cursor never repeats an event.
    let next = rpc.get_events(&contract, &EventsFrom::Cursor(page.cursor), 5).await.unwrap();
    assert!(next.events.iter().all(|event| event.id > page.cursor));
    assert!(next.events.windows(2).all(|pair| pair[0].id < pair[1].id));

    // A start before the retained range is refused, not silently moved.
    let refused = rpc.get_events(&contract, &EventsFrom::Ledger(page.oldest_ledger - 10), 1).await;
    assert!(
        matches!(&refused, Err(RpcError::Server { code: -32600, message, .. })
            if message.starts_with("startLedger must be within the ledger range")),
        "{refused:?}"
    );
}

#[tokio::test]
#[ignore = "requires Stellar testnet"]
async fn test_instance_entry_and_treasury_trustline_are_read_at_one_ledger() {
    let rpc = rpc();
    rpc.verify_network(Network::Testnet).await.unwrap();
    let usdc_asset = usdc::circle_usdc(Network::Testnet);
    let usdc_contract = usdc::asset_contract_id(&usdc_asset, Network::Testnet);
    let contract = contract();
    let probe = PrepaidDeployment {
        contract,
        usdc: usdc_contract,
        custody: Custody::Treasury(usdc::circle_issuer(Network::Testnet)),
    };
    let first = rpc.get_ledger_entries_at(&[probe.instance_key()]).await.unwrap();
    let state = instance_state(&first.entries[0].data).expect("the instance entry decodes");
    assert_eq!(state.config.usdc, usdc_contract);
    assert!(state.totals.liabilities >= 0 && state.totals.revenue >= 0);

    let treasury_key = usdc::trustline_key(
        state.config.treasury.as_ref().expect("a prepaid ledger has a treasury"),
        &usdc_asset,
    );
    let read = rpc.get_ledger_entries_at(&[probe.instance_key(), treasury_key]).await.unwrap();
    let held = read.entries.iter().find_map(|record| match &record.data {
        LedgerEntryData::Trustline(line) => Some(line.balance),
        _ => None,
    });
    assert!(held.is_some(), "the treasury has a USDC trustline");
    assert!(read.entries.iter().all(|record| record.last_modified_ledger <= read.latest_ledger));
}
