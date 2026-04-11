//! End-to-end multi-node testnet flow over the real TCP wire protocol.
//!
//! This feature-gated integration test keeps the slower proposer-to-miner flow
//! out of the default unit-test path while still exercising the live network
//! stack. Catch-up coverage includes both startup bootstrap sync and live
//! best-effort sync when a peer hello advertises a tip this node does not yet
//! know.

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
fn serve_n(
    mut server: Server,
    listener: TcpListener,
    count: usize,
    bootstrap_sync: bool,
) -> Server {
    if bootstrap_sync {
        server.sync_configured_peers_best_effort();
    }
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

/// Sends a custom `hello` payload and returns the remote `hello` reply.
fn send_hello(addr: &str, hello: HelloMessage) -> HelloMessage {
    let mut stream = net::connect(addr).unwrap();
    net::send_message(&mut stream, &WireMessage::Hello(hello)).unwrap();
    let response = net::receive_message(&mut stream).unwrap();
    let WireMessage::Hello(remote_hello) = response else {
        panic!("expected hello reply");
    };
    remote_hello
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
        self.start_with_sync(name, bootstrap, count, false)
    }

    fn start_with_sync(
        &mut self,
        name: &str,
        bootstrap: NodeBootstrap,
        count: usize,
        bootstrap_sync: bool,
    ) -> RunningNode {
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
        .with_peers(peer_endpoints)
        .with_bootstrap_sync(bootstrap_sync);
        if let Some(sqlite_path) = sqlite_path.as_deref() {
            server = server.with_sqlite_path(sqlite_path);
        }

        RunningNode {
            worker: thread::spawn(move || serve_n(server, listener, count, bootstrap_sync)),
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
    // The miner handles the relayed payment, the local mine request, and the
    // proposer's immediate sync-back when the mined-block relay hello advertises
    // the new tip.
    let miner_server = testnet.start("miner", NodeBootstrap::State(miner_state), 4);

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
    // After restart, the proposer syncs back to the miner when the relayed
    // block hello advertises a newer tip.
    let miner_server = testnet.start("miner", NodeBootstrap::Persisted, 4);

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
    // The relay serves the proposer's payment relay, the miner's block relay,
    // and the proposer's sync-back handshake plus block fetch.
    let relay_server = testnet.start("relay", NodeBootstrap::State(relay_state), 4);
    // The miner serves the relay's payment relay, the local mine request, and
    // the relay's sync-back handshake plus block fetch.
    let miner_server = testnet.start("miner", NodeBootstrap::State(miner_state), 4);

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

#[test]
fn lagging_node_can_fetch_tip_and_missing_block_after_being_offline() {
    let sender = crypto::signing_key_from_bytes([31; 32]);
    let recipient = crypto::signing_key_from_bytes([32; 32]);
    let miner = crypto::signing_key_from_bytes([33; 32]);
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
    miner_state.accept_block(genesis.clone()).unwrap();
    let mut observer_state = NodeState::new();
    observer_state.accept_block(genesis).unwrap();

    let mut testnet = TestNet::new();
    testnet.add_node("proposer", &["miner"]);
    testnet.add_node("miner", &["proposer"]);
    testnet.add_node("observer", &["miner"]);

    // The observer stays offline while the payment is proposed and mined, so it
    // must catch up from its configured peer during bootstrap before it begins
    // serving new inbound connections.
    let proposer = testnet.start("proposer", NodeBootstrap::State(proposer_state), 2);
    // The miner must stay online for the observer bootstrap handshake plus the
    // one-shot tip block fetch that follows, plus the proposer's sync-back when
    // the mined-block relay hello advertises the new tip.
    let miner_server = testnet.start("miner", NodeBootstrap::State(miner_state), 6);

    testnet.submit_payment("proposer", payment.clone());
    testnet.sync_delay();
    let block_hash = testnet.mine_pending("miner", &miner);
    testnet.sync_delay();

    let observer =
        testnet.start_with_sync("observer", NodeBootstrap::State(observer_state), 0, true);

    let proposer = proposer.join();
    let miner_server = miner_server.join();
    let observer = observer.join();

    assert_eq!(observer.state().chain().best_tip(), Some(block_hash));
    assert_eq!(
        observer.state().chain().best_tip(),
        proposer.state().chain().best_tip()
    );
    assert_eq!(
        observer.state().chain().best_tip(),
        miner_server.state().chain().best_tip()
    );
    assert!(observer.state().mempool().is_empty());
    let observer_tip = observer.state().get_block(&block_hash).unwrap();
    assert_eq!(observer_tip.transactions.len(), 2);
    assert_eq!(observer_tip.transactions[1].txid(), payment_txid);
    assert_eq!(
        observer.state().balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        observer.state().balance_for_key(&sender.verifying_key()),
        20
    );
    assert_eq!(observer.state().balance_for_key(&miner.verifying_key()), 50);
}

#[test]
fn lagging_live_node_syncs_when_peer_hello_advertises_new_tip() {
    let sender = crypto::signing_key_from_bytes([41; 32]);
    let recipient = crypto::signing_key_from_bytes([42; 32]);
    let miner = crypto::signing_key_from_bytes([43; 32]);
    let genesis = mine_block(
        BlockHash::default(),
        0x207f_ffff,
        &sender.verifying_key(),
        0,
    );
    let genesis_hash = genesis.header.block_hash();
    let spendable = OutPoint {
        txid: genesis.transactions[0].txid(),
        vout: 0,
    };
    let payment = spend_with_change(spendable, &sender, &recipient.verifying_key(), 30, 20, 1);
    let payment_txid = payment.txid();

    let mut proposer_state = NodeState::new();
    proposer_state.accept_block(genesis.clone()).unwrap();
    let mut miner_state = NodeState::new();
    miner_state.accept_block(genesis.clone()).unwrap();
    let mut observer_state = NodeState::new();
    observer_state.accept_block(genesis).unwrap();

    let mut testnet = TestNet::new();
    testnet.add_node("proposer", &["miner"]);
    testnet.add_node("miner", &["proposer"]);
    testnet.add_node("observer", &[]);

    let proposer = testnet.start("proposer", NodeBootstrap::State(proposer_state), 2);
    // The miner must stay online for the later hello-triggered sync handshake
    // and one-shot block fetch from the observer, plus the proposer's sync-back
    // when the mined-block relay hello advertises the new tip.
    let miner_server = testnet.start("miner", NodeBootstrap::State(miner_state), 6);
    let observer = testnet.start("observer", NodeBootstrap::State(observer_state), 1);

    testnet.submit_payment("proposer", payment.clone());
    testnet.sync_delay();
    let block_hash = testnet.mine_pending("miner", &miner);
    testnet.sync_delay();

    let reply = send_hello(
        testnet.addr("observer"),
        HelloMessage {
            network: "wobble-local".to_string(),
            version: PROTOCOL_VERSION,
            node_name: Some("miner-notify".to_string()),
            advertised_addr: Some(testnet.addr("miner").to_string()),
            tip: Some(block_hash),
            height: Some(1),
        },
    );

    let proposer = proposer.join();
    let miner_server = miner_server.join();
    let observer = observer.join();

    // The handshake reply is sent before the best-effort sync runs, so it
    // still reflects the observer's pre-sync local tip.
    assert_eq!(reply.tip, Some(genesis_hash));
    assert_eq!(reply.height, Some(0));
    assert_eq!(observer.state().chain().best_tip(), Some(block_hash));
    assert_eq!(
        observer.state().chain().best_tip(),
        proposer.state().chain().best_tip()
    );
    assert_eq!(
        observer.state().chain().best_tip(),
        miner_server.state().chain().best_tip()
    );
    assert!(observer.state().mempool().is_empty());
    let observer_tip = observer.state().get_block(&block_hash).unwrap();
    assert_eq!(observer_tip.transactions.len(), 2);
    assert_eq!(observer_tip.transactions[1].txid(), payment_txid);
    assert_eq!(
        observer.state().balance_for_key(&recipient.verifying_key()),
        30
    );
    assert_eq!(
        observer.state().balance_for_key(&sender.verifying_key()),
        20
    );
    assert_eq!(observer.state().balance_for_key(&miner.verifying_key()), 50);
}
