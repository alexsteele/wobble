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
    path::PathBuf,
    thread,
    time::Duration,
};

use crate::{
    net,
    node_state::NodeState,
    peer::{self, PeerConfig, PeerError},
    sqlite_store::{self, SqliteStoreError},
    types::BlockHash,
    wire::WireMessage,
};

/// Owns local protocol configuration and mutable node state for networking.
#[derive(Debug, Clone)]
pub struct Server {
    config: PeerConfig,
    state: NodeState,
    peers: Vec<String>,
    sqlite_path: Option<PathBuf>,
}

/// Errors produced by the live server while handling protocol messages.
#[derive(Debug)]
pub enum ServerError {
    Peer(PeerError),
    SqlitePersist(SqliteStoreError),
}

/// Retry budget for best-effort outbound relay dialing.
const RELAY_CONNECT_ATTEMPTS: usize = 3;
/// Small pause between relay dial attempts so listeners finishing a prior
/// request can accept the next inbound connection.
const RELAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);

impl Server {
    pub fn new(config: PeerConfig, state: NodeState) -> Self {
        Self {
            config,
            state,
            peers: Vec::new(),
            sqlite_path: None,
        }
    }

    /// Configures the peer addresses that should receive relayed transactions and blocks.
    pub fn with_peers(mut self, peers: Vec<String>) -> Self {
        self.peers = peers;
        self
    }

    /// Configures the server to persist accepted blocks and chain metadata to SQLite.
    ///
    /// This stores the live server state in SQLite for restart and sync.
    pub fn with_sqlite_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.sqlite_path = Some(path.into());
        self
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
    ///
    /// If the message mutates the live chain or mempool and SQLite persistence
    /// is enabled, the updated `NodeState` is saved after the protocol action
    /// succeeds.
    pub fn handle_message(
        &mut self,
        message: WireMessage,
    ) -> Result<Vec<WireMessage>, ServerError> {
        let should_persist = message_mutates_state(&message);
        let relay = self.relay_message_before_handle(&message);
        let sqlite_block_hash = persisted_block_hash(&message);
        let replies = peer::handle_message(&self.config, &mut self.state, message)
            .map_err(ServerError::Peer)?;
        if should_persist {
            self.persist_sqlite(sqlite_block_hash, &replies)
                .map_err(ServerError::SqlitePersist)?;
        }
        if let Some(relay) = relay.or_else(|| self.relay_message_from_replies(&replies)) {
            self.relay_best_effort(&relay);
        }
        Ok(replies)
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

    fn persist_sqlite(
        &self,
        request_block_hash: Option<BlockHash>,
        replies: &[WireMessage],
    ) -> Result<(), SqliteStoreError> {
        let Some(path) = self.sqlite_path.as_deref() else {
            return Ok(());
        };
        let Some(block_hash) = request_block_hash.or_else(|| mined_block_hash(replies)) else {
            let store = sqlite_store::SqliteStore::open(path)?;
            store.save_active_utxos(self.state.active_utxos())?;
            store.save_mempool(self.state.mempool())?;
            return Ok(());
        };
        let store = sqlite_store::SqliteStore::open(path)?;
        store.save_active_utxos(self.state.active_utxos())?;
        store.save_mempool(self.state.mempool())?;
        let Some(block) = self.state.get_block(&block_hash) else {
            return Ok(());
        };
        let Some(entry) = self.state.chain().get(&block_hash) else {
            return Ok(());
        };
        store.save_block_record(block, entry, self.state.chain().best_tip())
    }

    fn relay_best_effort(&self, message: &WireMessage) {
        for peer_addr in &self.peers {
            let _ = relay_to_peer(peer_addr, &self.config, message);
        }
    }

    /// Decides whether an inbound message should be relayed if local handling succeeds.
    ///
    /// Relay happens only when this server does not already know the announced
    /// object. That keeps duplicate gossip idempotent and avoids forwarding an
    /// object we already had before this peer sent it.
    fn relay_message_before_handle(&self, message: &WireMessage) -> Option<WireMessage> {
        match message {
            WireMessage::AnnounceTx { transaction } => {
                if self.state.mempool().get(&transaction.txid()).is_some() {
                    None
                } else {
                    Some(message.clone())
                }
            }
            WireMessage::AnnounceBlock { block } => {
                if self.state.get_block(&block.header.block_hash()).is_some() {
                    None
                } else {
                    Some(message.clone())
                }
            }
            _ => None,
        }
    }

    /// Builds any relay message implied by successful local side effects.
    ///
    /// `mine_pending` returns only the mined block hash on the wire, so the
    /// server resolves that hash back into the concrete block from local state
    /// before announcing it to peers.
    fn relay_message_from_replies(&self, replies: &[WireMessage]) -> Option<WireMessage> {
        replies.iter().find_map(|reply| match reply {
            WireMessage::MinedBlock(result) => self
                .state
                .get_block(&result.block_hash)
                .cloned()
                .map(|block| WireMessage::AnnounceBlock { block }),
            _ => None,
        })
    }
}

fn message_mutates_state(message: &WireMessage) -> bool {
    matches!(
        message,
        WireMessage::AnnounceTx { .. }
            | WireMessage::AnnounceBlock { .. }
            | WireMessage::MinePending(..)
    )
}

fn persisted_block_hash(message: &WireMessage) -> Option<BlockHash> {
    match message {
        WireMessage::AnnounceBlock { block } => Some(block.header.block_hash()),
        _ => None,
    }
}

fn mined_block_hash(replies: &[WireMessage]) -> Option<BlockHash> {
    replies.iter().find_map(|reply| match reply {
        WireMessage::MinedBlock(result) => Some(result.block_hash),
        _ => None,
    })
}

/// Opens a short-lived outbound relay connection, completes the handshake, and
/// sends one announcement message.
///
/// Relay is intentionally best effort. A small bounded retry helps with local
/// testnet timing where the destination listener may still be unwinding a
/// previous connection before it accepts the next one.
fn relay_to_peer(peer_addr: &str, config: &PeerConfig, message: &WireMessage) -> io::Result<()> {
    let mut last_error = None;
    for attempt in 0..RELAY_CONNECT_ATTEMPTS {
        match relay_to_peer_once(peer_addr, config, message) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if attempt + 1 < RELAY_CONNECT_ATTEMPTS {
                    thread::sleep(RELAY_CONNECT_RETRY_DELAY);
                }
            }
        }
    }

    Err(last_error.expect("relay attempts should record an error"))
}

fn relay_to_peer_once(
    peer_addr: &str,
    config: &PeerConfig,
    message: &WireMessage,
) -> io::Result<()> {
    let mut stream = net::connect(peer_addr)?;
    net::send_message(
        &mut stream,
        &WireMessage::Hello(crate::wire::HelloMessage {
            network: config.network.clone(),
            version: crate::wire::PROTOCOL_VERSION,
            node_name: config.node_name.clone(),
            tip: None,
            height: None,
        }),
    )?;
    let _ = net::receive_message(&mut stream)?;
    net::send_message(&mut stream, message)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        crypto, net,
        node_state::NodeState,
        peer::PeerConfig,
        server::Server,
        sqlite_store::SqliteStore,
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

    fn temp_sqlite_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-server-test-{}-{}.sqlite",
            std::process::id(),
            nanos
        ));
        path
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

    #[test]
    fn persists_mempool_to_sqlite_after_transaction_message() {
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
        let sqlite_path = temp_sqlite_path();
        let mut server = Server::new(PeerConfig::new("wobble-local", None), state)
            .with_sqlite_path(&sqlite_path);

        server
            .handle_message(WireMessage::AnnounceTx { transaction })
            .unwrap();

        let store = SqliteStore::open(&sqlite_path).unwrap();
        let loaded_mempool = store.load_mempool().unwrap();
        drop(store);
        fs::remove_file(&sqlite_path).unwrap();

        assert_eq!(
            loaded_mempool.get(&txid),
            server.state().mempool().get(&txid)
        );
    }

    #[test]
    fn persists_block_metadata_and_active_utxos_to_sqlite() {
        let owner = crypto::signing_key_from_bytes([1; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let genesis_hash = genesis.header.block_hash();
        let sqlite_path = temp_sqlite_path();
        let mut server = Server::new(PeerConfig::new("wobble-local", None), NodeState::new())
            .with_sqlite_path(&sqlite_path);

        server
            .handle_message(WireMessage::AnnounceBlock {
                block: genesis.clone(),
            })
            .unwrap();

        let store = SqliteStore::open(&sqlite_path).unwrap();
        let loaded_block = store.load_block(genesis_hash).unwrap();
        let loaded_entry = store.load_chain_entry(genesis_hash).unwrap();
        let loaded_best_tip = store.load_best_tip().unwrap();
        let loaded_utxos = store.load_active_utxos().unwrap();
        drop(store);
        fs::remove_file(&sqlite_path).unwrap();

        assert_eq!(loaded_block, Some(genesis));
        assert_eq!(
            loaded_entry,
            server.state().chain().get(&genesis_hash).cloned()
        );
        assert_eq!(loaded_best_tip, server.state().chain().best_tip());
        assert_eq!(loaded_utxos.len(), server.state().active_utxos().len());
    }

    #[test]
    fn relay_policy_skips_known_transaction_announcements() {
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
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state.submit_transaction(transaction.clone()).unwrap();
        let server = Server::new(PeerConfig::new("wobble-local", None), state);

        let relay = server.relay_message_before_handle(&WireMessage::AnnounceTx { transaction });

        assert_eq!(relay, None);
    }

    #[test]
    fn mined_block_reply_relays_the_concrete_block_from_state() {
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let miner = crypto::signing_key_from_bytes([3; 32]);
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
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state.submit_transaction(transaction.clone()).unwrap();
        let mut server = Server::new(PeerConfig::new("wobble-local", None), state);

        let replies = server
            .handle_message(WireMessage::MinePending(crate::wire::MinePendingRequest {
                reward: 50,
                miner_public_key: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
                uniqueness: 2,
                bits: 0x207f_ffff,
                max_transactions: 10,
            }))
            .unwrap();
        let relay = server.relay_message_from_replies(&replies);

        let Some(WireMessage::AnnounceBlock { block }) = relay else {
            panic!("expected mined block relay");
        };
        let [WireMessage::MinedBlock(crate::wire::MinedBlock { block_hash })] = replies.as_slice()
        else {
            panic!("expected mined block reply");
        };
        assert_eq!(block.header.block_hash(), *block_hash);
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.transactions[1], transaction);
    }
}
