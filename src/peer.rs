//! Minimal peer protocol handling above the raw TCP transport.
//!
//! This module validates the initial handshake and handles a small set of
//! protocol messages against local `NodeState`. The first version is
//! intentionally conservative: it supports compatibility checks, tip queries,
//! and block fetches before adding background relay loops or peer management.

use crate::{
    node_state::NodeState,
    wire::{HelloMessage, PROTOCOL_VERSION, WireMessage},
};

/// Local peer settings advertised during handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub network: String,
    pub node_name: Option<String>,
}

/// Reasons a remote peer message was rejected at the protocol layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    NetworkMismatch { local: String, remote: String },
    UnsupportedVersion(u32),
}

impl PeerConfig {
    pub fn new(network: impl Into<String>, node_name: Option<String>) -> Self {
        Self {
            network: network.into(),
            node_name,
        }
    }
}

/// Builds the local `hello` payload from configuration and current node state.
pub fn local_hello(config: &PeerConfig, state: &NodeState) -> HelloMessage {
    let tip = state.tip_summary();
    HelloMessage {
        network: config.network.clone(),
        version: PROTOCOL_VERSION,
        node_name: config.node_name.clone(),
        tip: tip.tip,
        height: tip.height,
    }
}

/// Handles one incoming wire message and returns any immediate protocol replies.
///
/// Current behavior:
/// - `hello` must match the local network and supported protocol version
/// - `get_tip` returns the current best-tip summary
/// - `get_block` returns the indexed block when known
/// - other messages are ignored for now and will be handled in later network slices
///
/// Gap: this does not yet relay accepted blocks or transactions, track peer
/// state, or request missing parents.
pub fn handle_message(
    config: &PeerConfig,
    state: &mut NodeState,
    message: WireMessage,
) -> Result<Vec<WireMessage>, PeerError> {
    match message {
        WireMessage::Hello(remote) => {
            if remote.network != config.network {
                return Err(PeerError::NetworkMismatch {
                    local: config.network.clone(),
                    remote: remote.network,
                });
            }
            if remote.version != PROTOCOL_VERSION {
                return Err(PeerError::UnsupportedVersion(remote.version));
            }

            Ok(vec![WireMessage::Hello(local_hello(config, state))])
        }
        WireMessage::GetTip => Ok(vec![WireMessage::Tip(state.tip_summary())]),
        WireMessage::GetBlock { block_hash } => Ok(vec![WireMessage::Block {
            block: state.get_block(&block_hash).cloned(),
        }]),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        node_state::NodeState,
        peer::{PeerConfig, PeerError, handle_message, local_hello},
        types::{Block, BlockHash, BlockHeader, Transaction, TxOut},
        wire::{HelloMessage, PROTOCOL_VERSION, TipSummary, WireMessage},
    };

    fn coinbase(value: u64, uniqueness: u32) -> Transaction {
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value,
                locking_data: uniqueness.to_le_bytes().to_vec(),
            }],
            lock_time: uniqueness,
        }
    }

    fn mine_block(prev_blockhash: BlockHash, bits: u32, uniqueness: u32) -> Block {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions: vec![coinbase(50, uniqueness)],
        };
        block.header.merkle_root = block.merkle_root();

        loop {
            if crate::consensus::validate_block(&block).is_ok() {
                return block;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
    }

    #[test]
    fn local_hello_reports_current_tip_state() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = PeerConfig::new("wobble-local", Some("alpha".to_string()));

        let hello = local_hello(&config, &state);

        assert_eq!(hello.network, "wobble-local");
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert_eq!(hello.node_name, Some("alpha".to_string()));
        assert_eq!(hello.tip, Some(genesis_hash));
        assert_eq!(hello.height, Some(0));
    }

    #[test]
    fn get_tip_returns_tip_summary() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = PeerConfig::new("wobble-local", None);

        let replies = handle_message(&config, &mut state, WireMessage::GetTip).unwrap();

        assert_eq!(
            replies,
            vec![WireMessage::Tip(TipSummary {
                tip: Some(genesis_hash),
                height: Some(0),
            })]
        );
    }

    #[test]
    fn get_block_returns_known_block() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis.clone()).unwrap();
        let config = PeerConfig::new("wobble-local", None);

        let replies = handle_message(
            &config,
            &mut state,
            WireMessage::GetBlock {
                block_hash: genesis_hash,
            },
        )
        .unwrap();

        assert_eq!(
            replies,
            vec![WireMessage::Block {
                block: Some(genesis)
            }]
        );
    }

    #[test]
    fn hello_rejects_network_mismatch() {
        let mut state = NodeState::new();
        let config = PeerConfig::new("wobble-local", None);

        let err = handle_message(
            &config,
            &mut state,
            WireMessage::Hello(HelloMessage {
                network: "other-net".to_string(),
                version: PROTOCOL_VERSION,
                node_name: None,
                tip: None,
                height: None,
            }),
        )
        .unwrap_err();

        assert_eq!(
            err,
            PeerError::NetworkMismatch {
                local: "wobble-local".to_string(),
                remote: "other-net".to_string(),
            }
        );
    }

    #[test]
    fn hello_rejects_unsupported_version() {
        let mut state = NodeState::new();
        let config = PeerConfig::new("wobble-local", None);

        let err = handle_message(
            &config,
            &mut state,
            WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION + 1,
                node_name: None,
                tip: None,
                height: None,
            }),
        )
        .unwrap_err();

        assert_eq!(err, PeerError::UnsupportedVersion(PROTOCOL_VERSION + 1));
    }
}
