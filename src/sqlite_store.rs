//! SQLite-backed persistence for blocks and chain metadata.
//!
//! This module is the first step away from monolithic snapshot storage. It
//! stores accepted raw blocks, chain index entries, and the current best tip in
//! a small relational schema. It does not yet persist the active UTXO set or
//! mempool, so live nodes still need snapshot persistence for full restart.
//!
//! Table roles:
//! - `blocks` stores the raw serialized block payload by block hash
//! - `chain_entries` stores per-block indexing facts used for fork choice, such
//!   as height, cumulative work, and parent linkage
//! - `metadata` stores singleton node-level values such as the current best tip

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{Block, BlockHash, ChainIndexEntry};

#[derive(Debug)]
pub enum SqliteStoreError {
    Sqlite(rusqlite::Error),
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
    InvalidHashLength(usize),
    InvalidWorkLength(usize),
    InvalidHashHex(String),
}

impl From<rusqlite::Error> for SqliteStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<bincode::error::EncodeError> for SqliteStoreError {
    fn from(error: bincode::error::EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<bincode::error::DecodeError> for SqliteStoreError {
    fn from(error: bincode::error::DecodeError) -> Self {
        Self::Decode(error)
    }
}

/// Stores accepted raw blocks and chain metadata in a SQLite database file.
#[derive(Debug)]
pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    /// Opens or creates the SQLite store at `path` and ensures the schema exists.
    pub fn open(path: &Path) -> Result<Self, SqliteStoreError> {
        let connection = Connection::open(path)?;
        let store = Self { connection };
        store.initialize_schema()?;
        Ok(store)
    }

    /// Persists one accepted block together with its chain index entry and current best tip.
    ///
    /// This writes enough metadata to recover block history and best-tip choice
    /// without replaying from genesis. It does not yet persist UTXOs or mempool.
    pub fn save_block_record(
        &self,
        block: &Block,
        entry: &ChainIndexEntry,
        best_tip: Option<BlockHash>,
    ) -> Result<(), SqliteStoreError> {
        let block_hash = entry.block_hash.to_string();
        let parent_hash = entry.parent.map(|hash| hash.to_string());
        let block_bytes = bincode::serde::encode_to_vec(block, bincode::config::standard())?;
        let work_bytes = entry.cumulative_work.to_be_bytes().to_vec();
        let tx = self.connection.unchecked_transaction()?;

        tx.execute(
            "INSERT OR REPLACE INTO blocks (block_hash, block_bytes) VALUES (?1, ?2)",
            params![block_hash, block_bytes],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO chain_entries (block_hash, height, cumulative_work, parent_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![block_hash, entry.height as i64, work_bytes, parent_hash],
        )?;
        if let Some(best_tip) = best_tip {
            tx.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('best_tip', ?1)",
                params![best_tip.to_string()],
            )?;
        } else {
            tx.execute("DELETE FROM metadata WHERE key = 'best_tip'", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads a raw block by hash when present.
    pub fn load_block(&self, block_hash: BlockHash) -> Result<Option<Block>, SqliteStoreError> {
        let block_bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT block_bytes FROM blocks WHERE block_hash = ?1",
                params![block_hash.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(block_bytes) = block_bytes else {
            return Ok(None);
        };
        let (block, _): (Block, usize) =
            bincode::serde::decode_from_slice(&block_bytes, bincode::config::standard())?;
        Ok(Some(block))
    }

    /// Loads chain index metadata by block hash when present.
    pub fn load_chain_entry(
        &self,
        block_hash: BlockHash,
    ) -> Result<Option<ChainIndexEntry>, SqliteStoreError> {
        let row: Option<(i64, Vec<u8>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT height, cumulative_work, parent_hash FROM chain_entries WHERE block_hash = ?1",
                params![block_hash.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((height, cumulative_work, parent_hash)) = row else {
            return Ok(None);
        };

        Ok(Some(ChainIndexEntry {
            block_hash,
            height: height as u64,
            cumulative_work: decode_work(&cumulative_work)?,
            parent: match parent_hash {
                Some(value) => Some(parse_block_hash(&value)?),
                None => None,
            },
        }))
    }

    /// Loads the most recently persisted best tip hash, if any.
    pub fn load_best_tip(&self) -> Result<Option<BlockHash>, SqliteStoreError> {
        let best_tip: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'best_tip'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        best_tip.map(|value| parse_block_hash(&value)).transpose()
    }

    fn initialize_schema(&self) -> Result<(), SqliteStoreError> {
        self.connection.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS blocks (
                 block_hash TEXT PRIMARY KEY,
                 block_bytes BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS chain_entries (
                 block_hash TEXT PRIMARY KEY,
                 height INTEGER NOT NULL,
                 cumulative_work BLOB NOT NULL,
                 parent_hash TEXT NULL
             );
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             COMMIT;",
        )?;
        Ok(())
    }
}

fn decode_work(bytes: &[u8]) -> Result<u128, SqliteStoreError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| SqliteStoreError::InvalidWorkLength(bytes.len()))?;
    Ok(u128::from_be_bytes(array))
}

fn parse_block_hash(value: &str) -> Result<BlockHash, SqliteStoreError> {
    let bytes =
        hex::decode(value).map_err(|_| SqliteStoreError::InvalidHashHex(value.to_string()))?;
    let len = bytes.len();
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SqliteStoreError::InvalidHashLength(len))?;
    Ok(BlockHash::new(array))
}

mod hex {
    pub fn decode(value: &str) -> Result<Vec<u8>, ()> {
        if value.len() % 2 != 0 {
            return Err(());
        }

        let mut bytes = Vec::with_capacity(value.len() / 2);
        for chunk in value.as_bytes().chunks_exact(2) {
            let hex = std::str::from_utf8(chunk).map_err(|_| ())?;
            let byte = u8::from_str_radix(hex, 16).map_err(|_| ())?;
            bytes.push(byte);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::types::{Block, BlockHash, BlockHeader, ChainIndexEntry, Transaction, TxOut};

    use super::SqliteStore;

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

    fn temp_db_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-sqlite-test-{}-{}.db",
            std::process::id(),
            nanos
        ));
        path
    }

    #[test]
    fn round_trips_block_record_and_best_tip() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let block = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let block_hash = block.header.block_hash();
        let entry = ChainIndexEntry {
            block_hash,
            height: 0,
            cumulative_work: 123,
            parent: None,
        };

        store
            .save_block_record(&block, &entry, Some(block_hash))
            .unwrap();

        let loaded_block = store.load_block(block_hash).unwrap().unwrap();
        let loaded_entry = store.load_chain_entry(block_hash).unwrap().unwrap();
        let best_tip = store.load_best_tip().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded_block, block);
        assert_eq!(loaded_entry, entry);
        assert_eq!(best_tip, Some(block_hash));
    }
}
