//! A blockchain is an ordered sequence of blocks.
//!
//! Each block contains a header, which links it to the prior block and carries
//! the proof-of-work fields, plus a list of transactions.
//! Miners vary the header nonce and related data until the header hash falls
//! below the current target; that work makes block production costly and lets
//! nodes prefer the valid chain with the most cumulative work.
//! Transactions consume prior outputs through `TxIn` entries and create new
//! outputs through `TxOut` entries.
//! The current set of unspent transaction outputs (UTXOs) forms the chain
//! state.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::crypto;

pub type Amount = u64;
/// Position of a block in the active chain, starting at zero for genesis.
pub type BlockHeight = u64;
pub type UnixTimestamp = u64;

/// Transaction identifier bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Txid([u8; 32]);

impl Txid {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_data(bytes: &[u8]) -> Self {
        Self(crypto::double_sha256(bytes))
    }
}

impl fmt::Debug for Txid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Txid({self})")
    }
}

impl fmt::Display for Txid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }

        Ok(())
    }
}

/// Block header hash bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct BlockHash([u8; 32]);

impl BlockHash {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_data(bytes: &[u8]) -> Self {
        Self(crypto::double_sha256(bytes))
    }
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlockHash({self})")
    }
}

impl fmt::Display for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }

        Ok(())
    }
}

/// Identifies a specific output of a prior transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutPoint {
    pub txid: Txid,
    pub vout: u32,
}

/// Consumes a prior output by providing spend authorization data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxIn {
    pub previous_output: OutPoint,
    pub unlocking_data: Vec<u8>,
}

/// Creates spendable value together with its future spend condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxOut {
    pub value: Amount,
    pub locking_data: Vec<u8>,
}

/// A UTXO-model transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub version: u32,
    pub inputs: Vec<TxIn>,
    pub outputs: Vec<TxOut>,
    /// Earliest block height or timestamp at which this transaction is valid.
    pub lock_time: u32,
}

/// Fields committed by proof-of-work and chain linking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    pub version: u32,
    pub prev_blockhash: BlockHash,
    pub merkle_root: [u8; 32],
    pub time: UnixTimestamp,
    pub bits: u32,
    pub nonce: u32,
}

/// A block body paired with its mined header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
}

/// Metadata used to compare competing chain tips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainIndexEntry {
    pub block_hash: BlockHash,
    pub height: BlockHeight,
    pub cumulative_work: u128,
    pub parent: Option<BlockHash>,
}

/// Materialized unspent output tracked in chain state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utxo {
    pub outpoint: OutPoint,
    pub output: TxOut,
    pub created_at_height: BlockHeight,
    pub is_coinbase: bool,
}

impl Transaction {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.version.to_le_bytes());
        encode_len(self.inputs.len(), &mut bytes);
        for input in &self.inputs {
            bytes.extend_from_slice(&input.previous_output.txid.0);
            bytes.extend_from_slice(&input.previous_output.vout.to_le_bytes());
            encode_bytes(&input.unlocking_data, &mut bytes);
        }
        encode_len(self.outputs.len(), &mut bytes);
        for output in &self.outputs {
            bytes.extend_from_slice(&output.value.to_le_bytes());
            encode_bytes(&output.locking_data, &mut bytes);
        }
        bytes.extend_from_slice(&self.lock_time.to_le_bytes());
        bytes
    }

    /// Returns true for the special block-reward transaction with no prior spends.
    pub fn is_coinbase(&self) -> bool {
        self.inputs.is_empty()
    }

    pub fn txid(&self) -> Txid {
        Txid::from_data(&self.encode())
    }
}

impl BlockHeader {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + 32 + 32 + 8 + 4 + 4);
        bytes.extend_from_slice(&self.version.to_le_bytes());
        bytes.extend_from_slice(&self.prev_blockhash.0);
        bytes.extend_from_slice(&self.merkle_root);
        bytes.extend_from_slice(&self.time.to_le_bytes());
        bytes.extend_from_slice(&self.bits.to_le_bytes());
        bytes.extend_from_slice(&self.nonce.to_le_bytes());
        bytes
    }

    pub fn block_hash(&self) -> BlockHash {
        BlockHash::from_data(&self.encode())
    }
}

impl Block {
    pub fn merkle_root(&self) -> [u8; 32] {
        let mut level: Vec<[u8; 32]> = self
            .transactions
            .iter()
            .map(Transaction::txid_bytes)
            .collect();

        if level.is_empty() {
            return crypto::double_sha256(&[]);
        }

        while level.len() > 1 {
            if level.len() % 2 == 1 {
                let last = *level.last().expect("level is not empty");
                level.push(last);
            }

            level = level
                .chunks_exact(2)
                .map(|pair| {
                    let mut bytes = Vec::with_capacity(64);
                    bytes.extend_from_slice(&pair[0]);
                    bytes.extend_from_slice(&pair[1]);
                    crypto::double_sha256(&bytes)
                })
                .collect();
        }

        level[0]
    }
}

impl Transaction {
    fn txid_bytes(&self) -> [u8; 32] {
        *self.txid().as_bytes()
    }
}

fn encode_len(len: usize, bytes: &mut Vec<u8>) {
    let len = u32::try_from(len).expect("length exceeds u32");
    bytes.extend_from_slice(&len.to_le_bytes());
}

fn encode_bytes(data: &[u8], bytes: &mut Vec<u8>) {
    encode_len(data.len(), bytes);
    bytes.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut, Txid, Utxo};

    fn sample_transaction() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::new([0x11; 32]),
                    vout: 2,
                },
                unlocking_data: vec![0xaa, 0xbb],
            }],
            outputs: vec![
                TxOut {
                    value: 40,
                    locking_data: vec![0x51],
                },
                TxOut {
                    value: 2,
                    locking_data: vec![0x52],
                },
            ],
            lock_time: 0,
        }
    }

    #[test]
    fn coinbase_detection_matches_empty_inputs() {
        let coinbase = Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value: 50,
                locking_data: vec![0x51],
            }],
            lock_time: 0,
        };

        assert!(coinbase.is_coinbase());
        assert!(!sample_transaction().is_coinbase());
    }

    #[test]
    fn transaction_encoding_is_deterministic() {
        let tx = sample_transaction();

        assert_eq!(tx.encode(), tx.encode());
        assert_eq!(tx.txid(), tx.txid());
    }

    #[test]
    fn transaction_id_changes_when_contents_change() {
        let tx = sample_transaction();
        let mut changed = sample_transaction();
        changed.outputs[0].value += 1;

        assert_ne!(tx.txid(), changed.txid());
    }

    #[test]
    fn block_header_hash_is_deterministic() {
        let header = BlockHeader {
            version: 1,
            prev_blockhash: BlockHash::new([0x22; 32]),
            merkle_root: [0x33; 32],
            time: 1_700_000_000,
            bits: 0x1d00ffff,
            nonce: 42,
        };

        assert_eq!(header.block_hash(), header.block_hash());
    }

    #[test]
    fn merkle_root_of_empty_block_matches_empty_hash() {
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0; 32]),
                merkle_root: [0; 32],
                time: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: Vec::new(),
        };

        assert_eq!(block.merkle_root(), crate::crypto::double_sha256(&[]));
    }

    #[test]
    fn merkle_root_of_single_transaction_matches_its_txid() {
        let tx = sample_transaction();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0; 32]),
                merkle_root: [0; 32],
                time: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: vec![tx.clone()],
        };

        assert_eq!(block.merkle_root(), *tx.txid().as_bytes());
    }

    #[test]
    fn utxo_retains_outpoint_and_metadata() {
        let outpoint = OutPoint {
            txid: Txid::new([0x44; 32]),
            vout: 1,
        };
        let output = TxOut {
            value: 25,
            locking_data: vec![0x53],
        };
        let utxo = Utxo {
            outpoint,
            output: output.clone(),
            created_at_height: 10,
            is_coinbase: false,
        };

        assert_eq!(utxo.outpoint, outpoint);
        assert_eq!(utxo.output, output);
        assert_eq!(utxo.created_at_height, 10);
        assert!(!utxo.is_coinbase);
    }
}
