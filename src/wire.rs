//! JSON wire messages exchanged between peer nodes.
//!
//! This module defines the protocol envelope described in `docs/protocol.md`.
//! Messages are serialized as tagged JSON objects so peers can dispatch on the
//! `type` field and decode only the payload for that message variant.

use serde::{Deserialize, Serialize};

use crate::types::{Block, BlockHash, Transaction};

/// Current protocol version advertised during peer handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Summary of a node's current best tip shared during handshake and polling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TipSummary {
    pub tip: Option<BlockHash>,
    pub height: Option<u64>,
}

/// Initial compatibility check exchanged when a peer connection is established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMessage {
    pub network: String,
    pub version: u32,
    pub node_name: Option<String>,
    pub tip: Option<BlockHash>,
    pub height: Option<u64>,
}

/// A single newline-delimited JSON message on the peer wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WireMessage {
    Hello(HelloMessage),
    GetTip,
    Tip(TipSummary),
    AnnounceTx { transaction: Transaction },
    AnnounceBlock { block: Block },
    GetBlock { block_hash: BlockHash },
    Block { block: Option<Block> },
}

impl WireMessage {
    /// Encodes this message as one JSON line suitable for newline-delimited TCP transport.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let mut json = serde_json::to_string(self)?;
        json.push('\n');
        Ok(json)
    }

    /// Decodes one JSON message from a single input line.
    pub fn from_json_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim_end_matches('\n'))
    }
}

#[cfg(test)]
mod tests {
    use crate::types::{Block, BlockHash, BlockHeader, Transaction};

    use super::{HelloMessage, TipSummary, WireMessage};

    fn sample_transaction() -> Transaction {
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: Vec::new(),
            lock_time: 7,
        }
    }

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
            transactions: vec![sample_transaction()],
        }
    }

    #[test]
    fn encodes_tagged_hello_message() {
        let message = WireMessage::Hello(HelloMessage {
            network: "wobble-local".to_string(),
            version: 1,
            node_name: Some("alpha".to_string()),
            tip: Some(BlockHash::new([0x33; 32])),
            height: Some(12),
        });

        let json = serde_json::to_string(&message).unwrap();

        assert!(json.contains("\"type\":\"hello\""));
        assert!(json.contains("\"network\":\"wobble-local\""));
        assert!(json.contains("\"node_name\":\"alpha\""));
    }

    #[test]
    fn round_trips_block_announcement_as_json_line() {
        let message = WireMessage::AnnounceBlock {
            block: sample_block(),
        };

        let line = message.to_json_line().unwrap();
        let decoded = WireMessage::from_json_line(&line).unwrap();

        assert_eq!(decoded, message);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn decodes_get_tip_without_payload() {
        let decoded = WireMessage::from_json_line("{\"type\":\"get_tip\"}\n").unwrap();

        assert_eq!(decoded, WireMessage::GetTip);
    }

    #[test]
    fn round_trips_tip_summary() {
        let message = WireMessage::Tip(TipSummary {
            tip: Some(BlockHash::new([0x44; 32])),
            height: Some(99),
        });

        let line = message.to_json_line().unwrap();
        let decoded = WireMessage::from_json_line(&line).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn round_trips_transaction_announcement() {
        let message = WireMessage::AnnounceTx {
            transaction: sample_transaction(),
        };

        let line = message.to_json_line().unwrap();
        let decoded = WireMessage::from_json_line(&line).unwrap();

        assert_eq!(decoded, message);
    }
}
