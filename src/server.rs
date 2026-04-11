//! Minimal peer server that owns node state and protocol configuration.
//!
//! This module ties together the raw TCP transport in `net`, the message
//! semantics in `peer`, and the mutable blockchain state in `NodeState`.
//! The first version is intentionally single-threaded and handles one stream at
//! a time so the protocol loop stays easy to reason about during early
//! networking work.

use std::{
    io,
    net::{TcpListener, TcpStream, ToSocketAddrs},
};

use crate::{
    net,
    node_state::NodeState,
    peer::{self, PeerConfig, PeerError},
    wire::WireMessage,
};

/// Owns local protocol configuration and mutable node state for networking.
#[derive(Debug, Clone)]
pub struct Server {
    config: PeerConfig,
    state: NodeState,
}

impl Server {
    pub fn new(config: PeerConfig, state: NodeState) -> Self {
        Self { config, state }
    }

    pub fn config(&self) -> &PeerConfig {
        &self.config
    }

    pub fn state(&self) -> &NodeState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut NodeState {
        &mut self.state
    }

    /// Handles one decoded wire message against the server's current node state.
    pub fn handle_message(&mut self, message: WireMessage) -> Result<Vec<WireMessage>, PeerError> {
        peer::handle_message(&self.config, &mut self.state, message)
    }

    /// Serves a single connected stream until the peer closes the connection or
    /// sends an invalid protocol message.
    ///
    /// Current behavior is request-response oriented: each received message is
    /// handled immediately and any resulting replies are written back in order.
    /// Gap: this does not yet track per-peer state or initiate background relay.
    pub fn handle_stream(&mut self, mut stream: TcpStream) -> io::Result<()> {
        let reader_stream = stream.try_clone()?;
        let mut reader = io::BufReader::new(reader_stream);

        loop {
            let message = match net::receive_message_from_reader(&mut reader) {
                Ok(message) => message,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            };

            let replies = self
                .handle_message(message)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}")))?;
            for reply in replies {
                net::send_message(&mut stream, &reply)?;
            }
        }
    }

    /// Binds a listener and serves inbound peers sequentially.
    ///
    /// This is enough for local manual testing. Later versions can move to
    /// concurrent connection handling once shared-state policy is clear.
    pub fn serve<A: ToSocketAddrs>(&mut self, addr: A) -> io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        self.serve_listener(listener)
    }

    /// Accepts inbound peer connections from an existing listener.
    pub fn serve_listener(&mut self, listener: TcpListener) -> io::Result<()> {
        for stream in listener.incoming() {
            self.handle_stream(stream?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use crate::{
        crypto, net,
        node_state::NodeState,
        peer::PeerConfig,
        server::Server,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
        wire::{HelloMessage, PROTOCOL_VERSION, TipSummary, WireMessage},
    };

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn read_line(reader: &mut BufReader<TcpStream>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    fn coinbase(value: u64, owner: &ed25519_dalek::VerifyingKey, uniqueness: u32) -> Transaction {
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(owner).to_vec(),
            }],
            lock_time: uniqueness,
        }
    }

    fn mine_block(
        prev_blockhash: BlockHash,
        bits: u32,
        owner: &ed25519_dalek::VerifyingKey,
        uniqueness: u32,
    ) -> Block {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions: vec![coinbase(50, owner, uniqueness)],
        };
        block.header.merkle_root = block.merkle_root();

        loop {
            if crate::consensus::validate_block(&block).is_ok() {
                return block;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
    }

    fn spend(
        previous_output: OutPoint,
        signer: &ed25519_dalek::SigningKey,
        recipient: &ed25519_dalek::VerifyingKey,
        value: u64,
        uniqueness: u32,
    ) -> Transaction {
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(recipient).to_vec(),
            }],
            lock_time: uniqueness,
        };
        tx.inputs[0].unlocking_data = crypto::sign_message(signer, &tx.signing_digest()).to_vec();
        tx
    }

    #[test]
    fn responds_to_hello_and_get_tip_over_stream() {
        let mut server = Server::new(
            PeerConfig::new("wobble-local", Some("alpha".to_string())),
            NodeState::new(),
        );
        let (mut client, server_stream) = connected_pair();

        let worker = thread::spawn(move || server.handle_stream(server_stream));

        client
            .write_all(
                b"{\"type\":\"hello\",\"data\":{\"network\":\"wobble-local\",\"version\":1,\"node_name\":\"beta\",\"tip\":null,\"height\":null}}\n",
            )
            .unwrap();
        client.write_all(b"{\"type\":\"get_tip\"}\n").unwrap();
        client.flush().unwrap();

        let mut reader = BufReader::new(client);
        let hello = WireMessage::from_json_line(&read_line(&mut reader)).unwrap();
        let tip = WireMessage::from_json_line(&read_line(&mut reader)).unwrap();

        assert_eq!(
            hello,
            WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION,
                node_name: Some("alpha".to_string()),
                tip: None,
                height: None,
            })
        );
        assert_eq!(
            tip,
            WireMessage::Tip(TipSummary {
                tip: None,
                height: None,
            })
        );

        drop(reader);
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn rejects_invalid_handshake_over_stream() {
        let mut server = Server::new(PeerConfig::new("wobble-local", None), NodeState::new());
        let (mut client, server_stream) = connected_pair();

        let worker = thread::spawn(move || server.handle_stream(server_stream));

        client
            .write_all(
                b"{\"type\":\"hello\",\"data\":{\"network\":\"other-net\",\"version\":1,\"node_name\":null,\"tip\":null,\"height\":null}}\n",
            )
            .unwrap();
        client.flush().unwrap();

        let result = worker.join().unwrap();

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn announced_transaction_reaches_server_mempool_end_to_end() {
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let transaction = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let txid = transaction.txid();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let mut server = Server::new(PeerConfig::new("wobble-local", None), state);
        let (mut client, server_stream) = connected_pair();

        let worker = thread::spawn(move || {
            server.handle_stream(server_stream).unwrap();
            server
        });

        // Handshake first so the server accepts later relay messages on this connection.
        net::send_message(
            &mut client,
            &WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION,
                node_name: Some("client".to_string()),
                tip: None,
                height: None,
            }),
        )
        .unwrap();
        let remote_hello = net::receive_message(&mut client).unwrap();
        assert!(matches!(remote_hello, WireMessage::Hello(_)));

        net::send_message(&mut client, &WireMessage::AnnounceTx { transaction }).unwrap();
        drop(client);

        let server = worker.join().unwrap();

        assert!(server.state().mempool().get(&txid).is_some());
    }
}
