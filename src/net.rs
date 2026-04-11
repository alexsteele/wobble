//! Minimal TCP transport helpers for the JSON peer wire protocol.
//!
//! This module is intentionally small and synchronous. It provides line-based
//! send and receive operations for `WireMessage` values over a connected TCP
//! stream. Higher-level peer management, retries, relay policy, and concurrent
//! node orchestration belong in a later networking layer.

use std::{
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
};

use crate::wire::WireMessage;

/// Opens an outbound TCP connection to a peer address.
pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
    TcpStream::connect(addr)
}

/// Binds a listening socket for inbound peer connections.
pub fn listen<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
}

/// Sends one newline-delimited protocol message to a connected peer.
pub fn send_message(stream: &mut TcpStream, message: &WireMessage) -> io::Result<()> {
    let line = message
        .to_json_line()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Receives one newline-delimited protocol message from a connected peer.
pub fn receive_message(stream: &mut TcpStream) -> io::Result<WireMessage> {
    let cloned = stream.try_clone()?;
    receive_message_from_reader(BufReader::new(cloned))
}

/// Receives one protocol message from any buffered line reader.
///
/// This is split out so tests and future connection loops can keep a reader
/// alive across multiple calls without reparsing prior bytes.
pub fn receive_message_from_reader<R: BufRead>(mut reader: R) -> io::Result<WireMessage> {
    let mut line = String::new();
    let bytes_read = reader.read_line(&mut line)?;
    if bytes_read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed connection",
        ));
    }

    WireMessage::from_json_line(&line)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufReader, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use crate::{
        net::{receive_message, receive_message_from_reader, send_message},
        types::{BlockHash, Transaction},
        wire::{HelloMessage, WireMessage},
    };

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn sends_and_receives_message_over_tcp() {
        let (mut client, mut server) = connected_pair();
        let expected = WireMessage::Hello(HelloMessage {
            network: "wobble-local".to_string(),
            version: 1,
            node_name: Some("alpha".to_string()),
            tip: Some(BlockHash::new([0x11; 32])),
            height: Some(3),
        });

        let sender = thread::spawn(move || send_message(&mut client, &expected));
        let received = receive_message(&mut server).unwrap();

        sender.join().unwrap().unwrap();
        assert_eq!(
            received,
            WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: 1,
                node_name: Some("alpha".to_string()),
                tip: Some(BlockHash::new([0x11; 32])),
                height: Some(3),
            })
        );
    }

    #[test]
    fn reads_multiple_messages_from_one_buffered_stream() {
        let (mut client, server) = connected_pair();
        let first = WireMessage::GetTip;
        let second = WireMessage::AnnounceTx {
            transaction: Transaction {
                version: 1,
                inputs: Vec::new(),
                outputs: Vec::new(),
                lock_time: 9,
            },
        };

        send_message(&mut client, &first).unwrap();
        send_message(&mut client, &second).unwrap();

        let mut reader = BufReader::new(server);
        assert_eq!(receive_message_from_reader(&mut reader).unwrap(), first);
        assert_eq!(receive_message_from_reader(&mut reader).unwrap(), second);
    }

    #[test]
    fn returns_invalid_data_for_malformed_json() {
        let (mut client, server) = connected_pair();

        thread::spawn(move || {
            client
                .write_all(b"{\"type\":\"hello\",\"data\":\n")
                .unwrap();
            client.flush().unwrap();
        });

        let err = receive_message_from_reader(BufReader::new(server)).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
