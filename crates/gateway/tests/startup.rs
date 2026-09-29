//! The RPC check both processes run before serving, against a loopback
//! endpoint answering `getNetwork` in the shape Stellar RPC serves.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use fermah_pay_stellar_chain::rpc::{RpcClient, RpcError};
use fermah_pay_stellar_domain::Network;
use fermah_pay_stellar_gateway::startup::{MIN_PROTOCOL, StartupError, verify_rpc};

const TESTNET: &str = "Test SDF Network ; September 2015";

/// An endpoint that answers one request with `getNetwork` reporting
/// `passphrase` and `protocol`.
fn endpoint(passphrase: &str, protocol: u32) -> RpcClient {
    // As soroban-testnet.stellar.org answered at protocol 28.
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"friendbotUrl":"https://friendbot.stellar.org/","passphrase":"{passphrase}","protocolVersion":{protocol}}}}}"#
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap();
            request.extend_from_slice(&buf[..n]);
            if n == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let Some(end) = text.find("\r\n\r\n") else { continue };
            let length = text
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            if request.len() >= end + 4 + length {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    RpcClient::new(&url, Duration::from_secs(5)).unwrap()
}

#[tokio::test]
async fn test_network_before_protocol_27_is_refused() {
    let refused = verify_rpc(&endpoint(TESTNET, 26), Network::Testnet).await;
    assert!(matches!(refused, Err(StartupError::ProtocolTooOld { actual: 26 })), "{refused:?}");
    let message = refused.unwrap_err().to_string();
    assert!(message.contains("protocol 26") && message.contains("protocol 27"), "{message}");
}

#[tokio::test]
async fn test_network_at_protocol_27_or_later_is_accepted() {
    assert_eq!(MIN_PROTOCOL, 27);
    for protocol in [27, 28] {
        let info = verify_rpc(&endpoint(TESTNET, protocol), Network::Testnet).await.unwrap();
        assert_eq!(info.protocol_version, protocol);
    }
}

#[tokio::test]
async fn test_other_network_is_refused_before_its_protocol_is_considered() {
    let refused = verify_rpc(&endpoint(TESTNET, 28), Network::Pubnet).await;
    assert!(
        matches!(refused, Err(StartupError::Rpc(RpcError::WrongNetwork { .. }))),
        "{refused:?}"
    );
}
