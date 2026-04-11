//! End-to-end multi-node testnet flow over the real TCP wire protocol.
//!
//! This ignored integration test keeps the slower proposer-to-miner flow out
//! of the default unit-test path while still exercising the live network stack.

use std::{net::TcpListener, thread, time::Duration};

use wobble::{
    crypto, net,
    node_state::NodeState,
    peer::PeerConfig,
    server::Server,
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
            tip: None,
            height: None,
        }),
    )
    .unwrap();
    let response = net::receive_message(&mut stream).unwrap();
    assert!(matches!(response, WireMessage::Hello(_)));
    stream
}

#[test]
#[ignore = "runs a real two-node TCP scenario outside the default unit-test path"]
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

    let proposer_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proposer_addr = proposer_listener.local_addr().unwrap().to_string();
    let miner_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let miner_addr = miner_listener.local_addr().unwrap().to_string();

    let proposer = Server::new(
        PeerConfig::new("wobble-local", Some("proposer".to_string())),
        proposer_state,
    )
    .with_peers(vec![miner_addr.clone()]);
    let miner_server = Server::new(
        PeerConfig::new("wobble-local", Some("miner".to_string())),
        miner_state,
    )
    .with_peers(vec![proposer_addr.clone()]);

    // With bidirectional best-effort relay enabled, this scenario produces:
    // - proposer inbound: submitter, relayed tx duplicate, relayed mined block
    // - miner inbound: relayed tx, mining request, relayed block duplicate
    let proposer_worker = thread::spawn(move || serve_n(proposer, proposer_listener, 3));
    let miner_worker = thread::spawn(move || serve_n(miner_server, miner_listener, 3));

    let mut proposer_client = connect_and_handshake(&proposer_addr, "submitter");
    net::send_message(
        &mut proposer_client,
        &WireMessage::AnnounceTx {
            transaction: payment.clone(),
        },
    )
    .unwrap();
    drop(proposer_client);

    // Relay is best-effort and synchronous, but this small wait keeps the test
    // deterministic across slower local scheduling.
    thread::sleep(Duration::from_millis(100));

    let mut miner_client = connect_and_handshake(&miner_addr, "miner-client");
    net::send_message(
        &mut miner_client,
        &WireMessage::MinePending(MinePendingRequest {
            reward: 50,
            miner_public_key: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
            uniqueness: 2,
            bits: 0x207f_ffff,
            max_transactions: 10,
        }),
    )
    .unwrap();
    let mined = net::receive_message(&mut miner_client).unwrap();
    let WireMessage::MinedBlock(MinedBlock { block_hash }) = mined else {
        panic!("expected mined_block reply");
    };
    drop(miner_client);

    // Give the mined-block relay a brief window to complete before asserting
    // final convergence across both live servers.
    thread::sleep(Duration::from_millis(100));

    let proposer = proposer_worker.join().unwrap();
    let miner_server = miner_worker.join().unwrap();

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
