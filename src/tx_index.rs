//! Persisted transaction index types for lookup-oriented queries.
//!
//! The chain and mempool already hold the authoritative transaction data.
//! These helper types describe a secondary sqlite index that makes it easier to
//! query transactions by status, chain position, and participating keys.

use serde::{Deserialize, Serialize};

use crate::types::{BlockHash, Transaction, Txid};

/// High-level persistence status for one indexed transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexedTransactionStatus {
    Mempool,
    Confirmed,
}

impl IndexedTransactionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mempool => "mempool",
            Self::Confirmed => "confirmed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mempool" => Some(Self::Mempool),
            "confirmed" => Some(Self::Confirmed),
            _ => None,
        }
    }
}

/// One indexed transaction row stored in sqlite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedTransaction {
    pub txid: Txid,
    pub transaction: Transaction,
    pub block_hash: Option<BlockHash>,
    pub block_height: Option<u64>,
    pub position_in_block: Option<u32>,
    pub status: IndexedTransactionStatus,
    pub first_seen_at: Option<String>,
    pub last_updated_at: Option<String>,
}

/// Role of one key in an indexed transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionKeyRole {
    Input,
    Output,
}

impl TransactionKeyRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "input" => Some(Self::Input),
            "output" => Some(Self::Output),
            _ => None,
        }
    }
}

/// One per-key relationship row for transaction lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionKeyEdge {
    pub txid: Txid,
    pub key_role: TransactionKeyRole,
    pub key_data: Vec<u8>,
    pub value: Option<u64>,
    pub vout: Option<u32>,
}
