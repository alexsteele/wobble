//! Outbound peer client helpers for CLI and test tooling.
//!
//! This module owns the small amount of logic needed to connect to a remote
//! peer, send the local `hello`, and validate that the first response is also a
//! `hello`. Keeping that flow here avoids repeating the same handshake code in
//! each remote CLI command.

use std::{io, net::TcpStream};

use crate::{
    net,
    peer::PeerConfig,
    wire::{HelloMessage, PROTOCOL_VERSION, WireMessage},
};

/// Errors returned while opening and handshaking an outbound peer connection.
#[derive(Debug)]
pub enum ClientError {
    Connect(io::Error),
    SendHello(io::Error),
    ReceiveHello(io::Error),
    UnexpectedHandshake(WireMessage),
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

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread};

    use crate::{
        client::{ClientError, connect_and_handshake},
        net,
        peer::PeerConfig,
        wire::{HelloMessage, WireMessage},
    };

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
}
