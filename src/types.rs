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

pub type Amount = u64;
/// Position of a block in the active chain, starting at zero for genesis.
pub type BlockHeight = u64;
pub type UnixTimestamp = u64;

/// Transaction identifier bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Txid([u8; 32]);

impl Txid {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BlockHash([u8; 32]);

impl BlockHash {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutPoint {
    pub txid: Txid,
    pub vout: u32,
}

/// Consumes a prior output by providing spend authorization data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxIn {
    pub previous_output: OutPoint,
    pub unlocking_data: Vec<u8>,
}

/// Creates spendable value together with its future spend condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOut {
    pub value: Amount,
    pub locking_data: Vec<u8>,
}

/// A UTXO-model transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub version: u32,
    pub inputs: Vec<TxIn>,
    pub outputs: Vec<TxOut>,
    /// Earliest block height or timestamp at which this transaction is valid.
    pub lock_time: u32,
}

/// Fields committed by proof-of-work and chain linking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    pub version: u32,
    pub prev_blockhash: BlockHash,
    pub merkle_root: [u8; 32],
    pub time: UnixTimestamp,
    pub bits: u32,
    pub nonce: u32,
}

/// A block body paired with its mined header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
}

/// Metadata used to compare competing chain tips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainIndexEntry {
    pub block_hash: BlockHash,
    pub height: BlockHeight,
    pub cumulative_work: u128,
    pub parent: Option<BlockHash>,
}

/// Materialized unspent output tracked in chain state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    pub outpoint: OutPoint,
    pub output: TxOut,
    pub created_at_height: BlockHeight,
    pub is_coinbase: bool,
}

impl Transaction {
    pub fn is_coinbase(&self) -> bool {
        self.inputs.is_empty()
    }
}
