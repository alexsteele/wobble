//! Outbound peer client helpers for CLI and test tooling.
//!
//! This module owns the small amount of logic needed to connect to a remote
//! peer, send the local `hello`, and validate that the first response is also a
//! `hello`. Keeping that flow here avoids repeating the same handshake code in
//! each remote CLI command. It also provides a few one-shot request helpers
//! for explicit tip, block, and transaction announcement flows during sync
//! tests and CLI operations.

use std::{io, net::TcpStream, time::Duration};

use crate::{
    net,
    peer::PeerConfig,
    types::Transaction,
    wire::{HelloMessage, PROTOCOL_VERSION, TipSummary, WireMessage},
};

/// Short timeout for one-shot outbound peer requests.
///
/// Bootstrap sync and relay-style requests are best effort, so they should
/// fail quickly when a peer is not yet accepting or replying rather than
/// stalling server startup indefinitely.
const OUTBOUND_PEER_IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Errors returned while opening and handshaking an outbound peer connection.
#[derive(Debug)]
pub enum ClientError {
    Connect(io::Error),
    SendHello(io::Error),
    ReceiveHello(io::Error),
    UnexpectedHandshake(WireMessage),
}

/// Errors returned by one-shot peer requests after handshake succeeds.
#[derive(Debug)]
pub enum RequestError {
    Handshake(ClientError),
    Send(io::Error),
    Receive(io::Error),
    UnexpectedResponse(WireMessage),
}

/// Opens a TCP connection to `peer_addr`, performs the protocol handshake, and
/// returns the connected stream plus the remote `hello` payload.
///
/// The local outbound handshake advertises network and optional node name, but
/// does not claim a listener address because these one-shot CLI clients do not
/// accept inbound relay connections.
pub fn connect_and_handshake(
    peer_addr: &str,
    config: &PeerConfig,
) -> Result<(TcpStream, HelloMessage), ClientError> {
    let mut stream = net::connect(peer_addr).map_err(ClientError::Connect)?;
    configure_outbound_stream(&stream).map_err(ClientError::Connect)?;
    let hello = WireMessage::Hello(HelloMessage {
        network: config.network.clone(),
        version: PROTOCOL_VERSION,
        node_name: config.node_name.clone(),
        advertised_addr: None,
        tip: None,
        height: None,
    });
    net::send_message(&mut stream, &hello).map_err(ClientError::SendHello)?;

    let remote_hello = net::receive_message(&mut stream).map_err(ClientError::ReceiveHello)?;
    let WireMessage::Hello(message) = remote_hello else {
        return Err(ClientError::UnexpectedHandshake(remote_hello));
    };

    Ok((stream, message))
}

/// Applies a short read/write timeout to one-shot outbound peer streams.
fn configure_outbound_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(OUTBOUND_PEER_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(OUTBOUND_PEER_IO_TIMEOUT))?;
    Ok(())
}

/// Opens a one-shot connection, asks the peer for its current best tip, and
/// returns the advertised summary.
pub fn request_tip(peer_addr: &str, config: &PeerConfig) -> Result<TipSummary, RequestError> {
    let (mut stream, _) =
        connect_and_handshake(peer_addr, config).map_err(RequestError::Handshake)?;
    net::send_message(&mut stream, &WireMessage::GetTip).map_err(RequestError::Send)?;
    let reply = net::receive_message(&mut stream).map_err(RequestError::Receive)?;
    let WireMessage::Tip(summary) = reply else {
        return Err(RequestError::UnexpectedResponse(reply));
    };
    Ok(summary)
}

/// Opens a one-shot connection, requests a specific block by hash, and returns
/// the remote response, which may be `None` when the peer does not know it.
pub fn request_block(
    peer_addr: &str,
    config: &PeerConfig,
    block_hash: crate::types::BlockHash,
) -> Result<Option<crate::types::Block>, RequestError> {
    let (mut stream, _) =
        connect_and_handshake(peer_addr, config).map_err(RequestError::Handshake)?;
    net::send_message(&mut stream, &WireMessage::GetBlock { block_hash })
        .map_err(RequestError::Send)?;
    let reply = net::receive_message(&mut stream).map_err(RequestError::Receive)?;
    let WireMessage::Block { block } = reply else {
        return Err(RequestError::UnexpectedResponse(reply));
    };
    Ok(block)
}

/// Opens a one-shot connection and announces a signed transaction to the peer.
///
/// The peer may accept the transaction into its mempool, treat it as a known
/// duplicate, or reject it during validation. This helper only guarantees that
/// the transaction was delivered over a successful protocol session.
pub fn announce_transaction(
    peer_addr: &str,
    config: &PeerConfig,
    transaction: Transaction,
) -> Result<(), RequestError> {
    let (mut stream, _) =
        connect_and_handshake(peer_addr, config).map_err(RequestError::Handshake)?;
    net::send_message(&mut stream, &WireMessage::AnnounceTx { transaction })
        .map_err(RequestError::Send)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io, net::TcpListener, thread, time::Duration};

    use crate::{
        client::{
            ClientError, OUTBOUND_PEER_IO_TIMEOUT, RequestError, announce_transaction,
            connect_and_handshake, request_block, request_tip,
        },
        net,
        peer::PeerConfig,
        types::{Block, BlockHash, BlockHeader, Transaction},
        wire::{HelloMessage, TipSummary, WireMessage},
    };

    fn sample_block() -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0x11; 32]),
                merkle_root: [0x22; 32],
                time: 123,
                bits: 0x207f_ffff,
                nonce: 42,
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: Vec::new(),
                outputs: Vec::new(),
                lock_time: 7,
            }],
        }
    }

    fn connected_listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        (listener, addr)
    }

    #[test]
    fn connect_and_handshake_returns_remote_hello() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let first = net::receive_message(&mut stream).unwrap();
            assert!(matches!(first, WireMessage::Hello(_)));
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: 1,
                    node_name: Some("server".to_string()),
                    advertised_addr: Some("127.0.0.1:9000".to_string()),
                    tip: None,
                    height: None,
                }),
            )
            .unwrap();
        });

        let (stream, remote) = connect_and_handshake(
            &addr,
            &PeerConfig::new("wobble-local", Some("client".to_string())),
        )
        .unwrap();
        drop(stream);
        worker.join().unwrap();

        assert_eq!(remote.node_name, Some("server".to_string()));
        assert_eq!(remote.advertised_addr, Some("127.0.0.1:9000".to_string()));
    }

    #[test]
    fn connect_and_handshake_rejects_non_hello_response() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = net::receive_message(&mut stream).unwrap();
            net::send_message(&mut stream, &WireMessage::GetTip).unwrap();
        });

        let err = connect_and_handshake(&addr, &PeerConfig::new("wobble-local", None)).unwrap_err();
        worker.join().unwrap();

        assert!(matches!(
            err,
            ClientError::UnexpectedHandshake(WireMessage::GetTip)
        ));
    }

    #[test]
    fn connect_and_handshake_times_out_when_peer_never_replies() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = net::receive_message(&mut stream).unwrap();
            thread::sleep(Duration::from_millis(
                OUTBOUND_PEER_IO_TIMEOUT.as_millis() as u64 + 100,
            ));
        });

        let err = connect_and_handshake(&addr, &PeerConfig::new("wobble-local", None)).unwrap_err();
        worker.join().unwrap();

        assert!(matches!(
            err,
            ClientError::ReceiveHello(source)
                if matches!(source.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
        ));
    }

    #[test]
    fn request_tip_returns_remote_tip_summary() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = net::receive_message(&mut stream).unwrap();
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: 1,
                    node_name: Some("server".to_string()),
                    advertised_addr: None,
                    tip: None,
                    height: None,
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            assert_eq!(request, WireMessage::GetTip);
            net::send_message(
                &mut stream,
                &WireMessage::Tip(TipSummary {
                    tip: Some(BlockHash::new([0x55; 32])),
                    height: Some(8),
                }),
            )
            .unwrap();
        });

        let summary = request_tip(&addr, &PeerConfig::new("wobble-local", None)).unwrap();
        worker.join().unwrap();

        assert_eq!(summary.tip, Some(BlockHash::new([0x55; 32])));
        assert_eq!(summary.height, Some(8));
    }

    #[test]
    fn request_block_returns_optional_block_payload() {
        let (listener, addr) = connected_listener();
        let block = sample_block();
        let block_hash = block.header.block_hash();
        let expected = block.clone();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = net::receive_message(&mut stream).unwrap();
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: 1,
                    node_name: Some("server".to_string()),
                    advertised_addr: None,
                    tip: None,
                    height: None,
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            assert_eq!(request, WireMessage::GetBlock { block_hash });
            net::send_message(&mut stream, &WireMessage::Block { block: Some(block) }).unwrap();
        });

        let fetched =
            request_block(&addr, &PeerConfig::new("wobble-local", None), block_hash).unwrap();
        worker.join().unwrap();

        assert_eq!(fetched, Some(expected));
    }

    #[test]
    fn request_tip_rejects_wrong_response_type() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = net::receive_message(&mut stream).unwrap();
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: 1,
                    node_name: Some("server".to_string()),
                    advertised_addr: None,
                    tip: None,
                    height: None,
                }),
            )
            .unwrap();
            let _ = net::receive_message(&mut stream).unwrap();
            net::send_message(&mut stream, &WireMessage::GetTip).unwrap();
        });

        let err = request_tip(&addr, &PeerConfig::new("wobble-local", None)).unwrap_err();
        worker.join().unwrap();

        assert!(matches!(
            err,
            RequestError::UnexpectedResponse(WireMessage::GetTip)
        ));
    }

    #[test]
    fn announce_transaction_sends_transaction_after_handshake() {
        let (listener, addr) = connected_listener();
        let expected = sample_block().transactions.into_iter().next().unwrap();

        let worker = thread::spawn({
            let expected = expected.clone();
            move || {
                let (mut stream, _) = listener.accept().unwrap();
                let first = net::receive_message(&mut stream).unwrap();
                assert!(matches!(first, WireMessage::Hello(_)));
                net::send_message(
                    &mut stream,
                    &WireMessage::Hello(HelloMessage {
                        network: "wobble-local".to_string(),
                        version: 1,
                        node_name: Some("server".to_string()),
                        advertised_addr: None,
                        tip: None,
                        height: None,
                    }),
                )
                .unwrap();
                let message = net::receive_message(&mut stream).unwrap();
                assert_eq!(
                    message,
                    WireMessage::AnnounceTx {
                        transaction: expected
                    }
                );
            }
        });

        announce_transaction(&addr, &PeerConfig::new("wobble-local", None), expected).unwrap();
        worker.join().unwrap();
    }
}
