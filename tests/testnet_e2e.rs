//! End-to-end multi-node testnet flow over the real TCP wire protocol.
//!
//! This feature-gated integration test keeps the slower proposer-to-miner flow
//! out of the default unit-test path while still exercising the live network
//! stack.

#![cfg(feature = "e2e")]

use std::{
    collections::HashMap,
    fs,
    net::TcpListener,
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use wobble::{
    crypto, net,
    node_state::NodeState,
    peer::PeerConfig,
    server::{PeerEndpoint, Server},
    sqlite_store::SqliteStore,
    types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
    wire::{HelloMessage, MinePendingRequest, MinedBlock, PROTOCOL_VERSION, WireMessage},
};

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
        if wobble::consensus::validate_block(&block).is_ok() {
            return block;
        }
        block.header.nonce = block.header.nonce.wrapping_add(1);
    }
}

fn spend_with_change(
    previous_output: OutPoint,
    signer: &ed25519_dalek::SigningKey,
    recipient: &ed25519_dalek::VerifyingKey,
    amount: u64,
    change: u64,
    uniqueness: u32,
) -> Transaction {
    let mut tx = Transaction {
        version: 1,
        inputs: vec![TxIn {
            previous_output,
            unlocking_data: Vec::new(),
        }],
        outputs: vec![
            TxOut {
                value: amount,
                locking_data: crypto::verifying_key_bytes(recipient).to_vec(),
            },
            TxOut {
                value: change,
                locking_data: crypto::verifying_key_bytes(&signer.verifying_key()).to_vec(),
            },
        ],
        lock_time: uniqueness,
    };
    tx.inputs[0].unlocking_data = crypto::sign_message(signer, &tx.signing_digest()).to_vec();
    tx
}

fn temp_sqlite_path(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after unix epoch")
        .as_nanos();
    path.push(format!(
        "wobble-e2e-{}-{}-{}.sqlite",
        label,
        std::process::id(),
        nanos
    ));
    path
}

/// Runs a server against a fixed number of inbound connections for deterministic tests.
fn serve_n(mut server: Server, listener: TcpListener, count: usize) -> Server {
    for _ in 0..count {
        let (stream, _) = listener.accept().unwrap();
        server.handle_stream(stream).unwrap();
    }
    server
}

/// Establishes a protocol-compatible connection and completes the hello exchange.
fn connect_and_handshake(addr: &str, node_name: &str) -> std::net::TcpStream {
    let mut stream = net::connect(addr).unwrap();
    net::send_message(
        &mut stream,
        &WireMessage::Hello(HelloMessage {
            network: "wobble-local".to_string(),
            version: PROTOCOL_VERSION,
            node_name: Some(node_name.to_string()),
            advertised_addr: None,
            tip: None,
            height: None,
        }),
    )
    .unwrap();
    let response = net::receive_message(&mut stream).unwrap();
    assert!(matches!(response, WireMessage::Hello(_)));
    stream
}

/// Small real-TCP harness for multi-node end-to-end scenarios.
///
/// This keeps address allocation, peer wiring, listener ownership, and
/// SQLite-backed restarts out of the individual test flows.
struct TestNet {
    nodes: HashMap<String, TestNode>,
}

struct TestNode {
    name: String,
    addr: String,
    peer_names: Vec<String>,
    sqlite_path: Option<PathBuf>,
    listener: Option<TcpListener>,
}

enum NodeBootstrap {
    State(NodeState),
    Persisted,
}

struct RunningNode {
    worker: thread::JoinHandle<Server>,
}

impl TestNet {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    fn add_node(&mut self, name: &str, peer_names: &[&str]) {
        self.add_node_with_sqlite(name, peer_names, None);
    }

    fn add_node_with_sqlite(
        &mut self,
        name: &str,
        peer_names: &[&str],
        sqlite_path: Option<PathBuf>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        self.nodes.insert(
            name.to_string(),
            TestNode {
                name: name.to_string(),
                addr,
                peer_names: peer_names.iter().map(|name| name.to_string()).collect(),
                sqlite_path,
                listener: Some(listener),
            },
        );
    }

    fn addr(&self, name: &str) -> &str {
        &self.nodes.get(name).unwrap().addr
    }

    fn peer_endpoints(&self, name: &str) -> Vec<PeerEndpoint> {
        let node = self.nodes.get(name).unwrap();
        node.peer_names
            .iter()
            .map(|peer_name| {
                let peer = self.nodes.get(peer_name).unwrap();
                PeerEndpoint::new(peer.addr.clone(), Some(peer.name.clone()))
            })
            .collect()
    }

    fn start(&mut self, name: &str, bootstrap: NodeBootstrap, count: usize) -> RunningNode {
        let addr = self.addr(name).to_string();
        let peer_endpoints = self.peer_endpoints(name);
        let node = self.nodes.get_mut(name).unwrap();
        let listener = node.listener.take().unwrap();
        let (state, sqlite_path) = match bootstrap {
            NodeBootstrap::State(state) => (state, None),
            NodeBootstrap::Persisted => {
                let sqlite_path = node
                    .sqlite_path
                    .clone()
                    .expect("persisted test node requires sqlite path");
                let state = SqliteStore::open(&sqlite_path)
                    .unwrap()
                    .load_node_state()
                    .unwrap();
                (state, Some(sqlite_path))
            }
        };
        let mut server = Server::new(
            PeerConfig::new("wobble-local", Some(node.name.clone())).with_advertised_addr(addr),
            state,
        )
        .with_peers(peer_endpoints);
        if let Some(sqlite_path) = sqlite_path.as_deref() {
            server = server.with_sqlite_path(sqlite_path);
        }

        RunningNode {
            worker: thread::spawn(move || serve_n(server, listener, count)),
        }
    }

    fn restart(&mut self, name: &str, count: usize) -> RunningNode {
        let addr = self.addr(name).to_string();
        assert!(
            self.nodes.get(name).unwrap().sqlite_path.is_some(),
            "restart requires persisted node backing"
        );
        let listener = TcpListener::bind(&addr).unwrap();
        self.nodes.get_mut(name).unwrap().listener = Some(listener);
        self.start(name, NodeBootstrap::Persisted, count)
    }

    fn submit_payment(&self, node_name: &str, transaction: Transaction) {
        let mut client = connect_and_handshake(self.addr(node_name), "submitter");
        net::send_message(&mut client, &WireMessage::AnnounceTx { transaction }).unwrap();
        drop(client);
    }

    fn mine_pending(&self, node_name: &str, miner: &ed25519_dalek::SigningKey) -> BlockHash {
        let mut client = connect_and_handshake(self.addr(node_name), "miner-client");
        net::send_message(
            &mut client,
            &WireMessage::MinePending(MinePendingRequest {
                reward: 50,
                miner_public_key: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
                uniqueness: 2,
                bits: 0x207f_ffff,
                max_transactions: 10,
            }),
        )
        .unwrap();
        let mined = net::receive_message(&mut client).unwrap();
        let WireMessage::MinedBlock(MinedBlock { block_hash }) = mined else {
            panic!("expected mined_block reply");
        };
        drop(client);
        block_hash
    }

    fn sync_delay(&self) {
        thread::sleep(Duration::from_millis(100));
    }
}

impl RunningNode {
    fn join(self) -> Server {
        self.worker.join().unwrap()
    }
}

#[test]
fn proposer_transaction_reaches_miner_and_returns_as_a_block() {
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
    let payment = spend_with_change(spendable, &sender, &recipient.verifying_key(), 30, 20, 1);
    let payment_txid = payment.txid();

    let mut proposer_state = NodeState::new();
    proposer_state.accept_block(genesis.clone()).unwrap();
    let mut miner_state = NodeState::new();
    miner_state.accept_block(genesis).unwrap();

    let mut testnet = TestNet::new();
    testnet.add_node("proposer", &["miner"]);
    testnet.add_node("miner", &["proposer"]);

    let proposer = testnet.start("proposer", NodeBootstrap::State(proposer_state), 2);
    let miner_server = testnet.start("miner", NodeBootstrap::State(miner_state), 2);

    testnet.submit_payment("proposer", payment.clone());
    testnet.sync_delay();
    let block_hash = testnet.mine_pending("miner", &miner);
    testnet.sync_delay();

    let proposer = proposer.join();
    let miner_server = miner_server.join();

    assert_eq!(
        proposer.state().chain().best_tip(),
        miner_server.state().chain().best_tip()
    );
    assert_eq!(proposer.state().chain().best_tip(), Some(block_hash));
    assert!(proposer.state().mempool().is_empty());
    assert!(miner_server.state().mempool().is_empty());

    let proposer_tip = proposer.state().get_block(&block_hash).unwrap();
    let miner_tip = miner_server.state().get_block(&block_hash).unwrap();
    assert_eq!(proposer_tip, miner_tip);
    assert_eq!(proposer_tip.transactions.len(), 2);
    assert_eq!(proposer_tip.transactions[1].txid(), payment_txid);

    assert_eq!(
        proposer.state().balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        miner_server
            .state()
            .balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        proposer.state().balance_for_key(&sender.verifying_key()),
        20
    );
    assert_eq!(
        miner_server
            .state()
            .balance_for_key(&sender.verifying_key()),
        20
    );
    assert_eq!(proposer.state().balance_for_key(&miner.verifying_key()), 50);
    assert_eq!(
        miner_server.state().balance_for_key(&miner.verifying_key()),
        50
    );
}

#[test]
fn restarted_proposer_loads_persisted_payment_and_accepts_relayed_block() {
    let sender = crypto::signing_key_from_bytes([11; 32]);
    let recipient = crypto::signing_key_from_bytes([12; 32]);
    let miner = crypto::signing_key_from_bytes([13; 32]);
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
    let payment = spend_with_change(spendable, &sender, &recipient.verifying_key(), 30, 20, 1);
    let payment_txid = payment.txid();

    let mut proposer_state = NodeState::new();
    proposer_state.accept_block(genesis.clone()).unwrap();
    let mut miner_state = NodeState::new();
    miner_state.accept_block(genesis).unwrap();

    let proposer_sqlite = temp_sqlite_path("proposer");
    let miner_sqlite = temp_sqlite_path("miner");
    SqliteStore::open(&proposer_sqlite)
        .unwrap()
        .save_node_state(&proposer_state)
        .unwrap();
    SqliteStore::open(&miner_sqlite)
        .unwrap()
        .save_node_state(&miner_state)
        .unwrap();

    let mut testnet = TestNet::new();
    testnet.add_node_with_sqlite("proposer", &["miner"], Some(proposer_sqlite.clone()));
    testnet.add_node_with_sqlite("miner", &["proposer"], Some(miner_sqlite.clone()));

    let proposer = testnet.start("proposer", NodeBootstrap::Persisted, 1);
    let miner_server = testnet.start("miner", NodeBootstrap::Persisted, 2);

    testnet.submit_payment("proposer", payment.clone());

    let first_proposer = proposer.join();
    assert!(
        first_proposer
            .state()
            .mempool()
            .get(&payment_txid)
            .is_some()
    );

    let reloaded = SqliteStore::open(&proposer_sqlite)
        .unwrap()
        .load_node_state()
        .unwrap();
    assert!(reloaded.mempool().get(&payment_txid).is_some());

    let restarted_proposer = testnet.restart("proposer", 1);

    testnet.sync_delay();
    let block_hash = testnet.mine_pending("miner", &miner);
    testnet.sync_delay();

    let restarted_proposer = restarted_proposer.join();
    let miner_server = miner_server.join();

    assert_eq!(
        restarted_proposer.state().chain().best_tip(),
        miner_server.state().chain().best_tip()
    );
    assert_eq!(
        restarted_proposer.state().chain().best_tip(),
        Some(block_hash)
    );
    assert!(restarted_proposer.state().mempool().is_empty());
    assert!(miner_server.state().mempool().is_empty());

    let proposer_tip = restarted_proposer.state().get_block(&block_hash).unwrap();
    let miner_tip = miner_server.state().get_block(&block_hash).unwrap();
    assert_eq!(proposer_tip, miner_tip);
    assert_eq!(proposer_tip.transactions[1].txid(), payment_txid);
    assert_eq!(
        restarted_proposer
            .state()
            .balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        miner_server
            .state()
            .balance_for_key(&recipient.verifying_key()),
        30
    );

    fs::remove_file(&proposer_sqlite).unwrap();
    fs::remove_file(&miner_sqlite).unwrap();
}

#[test]
fn multi_hop_relay_carries_payment_to_miner_and_block_back_to_proposer() {
    let sender = crypto::signing_key_from_bytes([21; 32]);
    let recipient = crypto::signing_key_from_bytes([22; 32]);
    let miner = crypto::signing_key_from_bytes([23; 32]);
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
    let payment = spend_with_change(spendable, &sender, &recipient.verifying_key(), 30, 20, 1);
    let payment_txid = payment.txid();

    let mut proposer_state = NodeState::new();
    proposer_state.accept_block(genesis.clone()).unwrap();
    let mut relay_state = NodeState::new();
    relay_state.accept_block(genesis.clone()).unwrap();
    let mut miner_state = NodeState::new();
    miner_state.accept_block(genesis).unwrap();

    let mut testnet = TestNet::new();
    testnet.add_node("proposer", &["relay"]);
    testnet.add_node("relay", &["proposer", "miner"]);
    testnet.add_node("miner", &["relay"]);

    let proposer = testnet.start("proposer", NodeBootstrap::State(proposer_state), 2);
    let relay_server = testnet.start("relay", NodeBootstrap::State(relay_state), 2);
    let miner_server = testnet.start("miner", NodeBootstrap::State(miner_state), 2);

    testnet.submit_payment("proposer", payment.clone());
    testnet.sync_delay();
    let block_hash = testnet.mine_pending("miner", &miner);
    testnet.sync_delay();

    let proposer = proposer.join();
    let relay_server = relay_server.join();
    let miner_server = miner_server.join();

    assert_eq!(
        proposer.state().chain().best_tip(),
        relay_server.state().chain().best_tip()
    );
    assert_eq!(
        relay_server.state().chain().best_tip(),
        miner_server.state().chain().best_tip()
    );
    assert_eq!(proposer.state().chain().best_tip(), Some(block_hash));

    assert!(proposer.state().mempool().is_empty());
    assert!(relay_server.state().mempool().is_empty());
    assert!(miner_server.state().mempool().is_empty());

    let proposer_tip = proposer.state().get_block(&block_hash).unwrap();
    let relay_tip = relay_server.state().get_block(&block_hash).unwrap();
    let miner_tip = miner_server.state().get_block(&block_hash).unwrap();
    assert_eq!(proposer_tip, relay_tip);
    assert_eq!(relay_tip, miner_tip);
    assert_eq!(proposer_tip.transactions.len(), 2);
    assert_eq!(proposer_tip.transactions[1].txid(), payment_txid);

    assert_eq!(
        proposer.state().balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        relay_server
            .state()
            .balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        miner_server
            .state()
            .balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(proposer.state().balance_for_key(&miner.verifying_key()), 50);
    assert_eq!(
        relay_server.state().balance_for_key(&miner.verifying_key()),
        50
    );
    assert_eq!(
        miner_server.state().balance_for_key(&miner.verifying_key()),
        50
    );
}
