//! SQLite-backed persistence for node state.
//!
//! This module stores accepted raw blocks, chain index entries, the current
//! best tip, the active UTXO set, and the mempool in a small relational
//! schema. It is the primary storage backend for the node and CLI workflows.
//!
//! Table roles:
//! - `blocks` stores the raw serialized block payload by block hash
//! - `chain_entries` stores per-block indexing facts used for fork choice, such
//!   as height, cumulative work, and parent linkage
//! - `active_utxos` stores the current active-chain unspent outputs
//! - `mempool_transactions` stores the current pending transaction set
//! - `metadata` stores singleton node-level values such as the current best tip

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    mempool::Mempool,
    node_state::{NodeState, NodeStateError},
    state::UtxoSet,
    types::{Block, BlockHash, ChainIndexEntry, OutPoint, Transaction, TxOut, Txid, Utxo},
};

#[derive(Debug)]
pub enum SqliteStoreError {
    Sqlite(rusqlite::Error),
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
    NodeState(NodeStateError),
    InvalidHashLength(usize),
    InvalidWorkLength(usize),
    InvalidHashHex(String),
    MissingBlockForEntry(BlockHash),
    RebuiltBestTipMismatch {
        rebuilt: Option<BlockHash>,
        persisted: Option<BlockHash>,
    },
    RebuiltUtxoMismatch,
    RebuiltMempoolMismatch,
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

impl From<NodeStateError> for SqliteStoreError {
    fn from(error: NodeStateError) -> Self {
        Self::NodeState(error)
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

    /// Replaces the persisted active-chain UTXO view with `utxos`.
    ///
    /// This is still a whole-view rewrite rather than incremental spend/create
    /// journaling. That keeps the first SQLite-backed UTXO step simple while we
    /// decide the longer-term schema.
    pub fn save_active_utxos(&self, utxos: &UtxoSet) -> Result<(), SqliteStoreError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM active_utxos", [])?;
        for (outpoint, utxo) in utxos.iter() {
            tx.execute(
                "INSERT INTO active_utxos (
                     txid, vout, value, locking_data, created_at_height, is_coinbase
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    outpoint.txid.to_string(),
                    outpoint.vout as i64,
                    utxo.output.value as i64,
                    utxo.output.locking_data,
                    utxo.created_at_height as i64,
                    if utxo.is_coinbase { 1_i64 } else { 0_i64 },
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads the persisted active-chain UTXO view.
    pub fn load_active_utxos(&self) -> Result<UtxoSet, SqliteStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT txid, vout, value, locking_data, created_at_height, is_coinbase
             FROM active_utxos",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;

        let mut utxos = UtxoSet::new();
        for row in rows {
            let (txid, vout, value, locking_data, created_at_height, is_coinbase) = row?;
            let outpoint = OutPoint {
                txid: parse_txid(&txid)?,
                vout: vout as u32,
            };
            utxos.insert(Utxo {
                outpoint,
                output: TxOut {
                    value: value as u64,
                    locking_data,
                },
                created_at_height: created_at_height as u64,
                is_coinbase: is_coinbase != 0,
            });
        }
        Ok(utxos)
    }

    /// Replaces the persisted mempool transaction set with `mempool`.
    pub fn save_mempool(&self, mempool: &Mempool) -> Result<(), SqliteStoreError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM mempool_transactions", [])?;
        for (txid, transaction) in mempool.iter() {
            let bytes = bincode::serde::encode_to_vec(transaction, bincode::config::standard())?;
            tx.execute(
                "INSERT INTO mempool_transactions (txid, transaction_bytes) VALUES (?1, ?2)",
                params![txid.to_string(), bytes],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads the persisted mempool transaction set keyed by transaction id.
    pub fn load_mempool(&self) -> Result<Mempool, SqliteStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT txid, transaction_bytes FROM mempool_transactions")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;

        let mut transactions = Vec::new();
        for row in rows {
            let (txid_text, transaction_bytes) = row?;
            let parsed_txid = parse_txid(&txid_text)?;
            let (transaction, _): (Transaction, usize) =
                bincode::serde::decode_from_slice(&transaction_bytes, bincode::config::standard())?;
            if transaction.txid() != parsed_txid {
                return Err(SqliteStoreError::InvalidHashHex(txid_text));
            }
            transactions.push((parsed_txid, transaction));
        }
        Ok(Mempool::from_persisted(transactions))
    }

    /// Rebuilds a full in-memory `NodeState` from persisted SQLite data.
    ///
    /// The current schema stores the authoritative block history, active UTXO
    /// view, and mempool contents. This method replays blocks in height order
    /// to recreate the cached per-block UTXO views needed for reorg handling,
    /// then checks the rebuilt state against the persisted best tip and active
    /// UTXOs.
    pub fn load_node_state(&self) -> Result<NodeState, SqliteStoreError> {
        let blocks = self.load_blocks_in_height_order()?;
        let mempool = self.load_mempool()?;
        let persisted_best_tip = self.load_best_tip()?;
        let persisted_utxos = self.load_active_utxos()?;
        let persisted_mempool = self.load_mempool()?;

        let state = NodeState::from_persisted(blocks, mempool)?;
        if state.chain().best_tip() != persisted_best_tip {
            return Err(SqliteStoreError::RebuiltBestTipMismatch {
                rebuilt: state.chain().best_tip(),
                persisted: persisted_best_tip,
            });
        }
        if !utxo_sets_match(state.active_utxos(), &persisted_utxos) {
            return Err(SqliteStoreError::RebuiltUtxoMismatch);
        }
        if !mempools_match(state.mempool(), &persisted_mempool) {
            return Err(SqliteStoreError::RebuiltMempoolMismatch);
        }

        Ok(state)
    }

    /// Persists a full `NodeState` into SQLite as the authoritative local store.
    ///
    /// This rewrites the SQLite tables from the current in-memory state. It is
    /// simple and reliable for local use, though not yet incremental.
    pub fn save_node_state(&self, state: &NodeState) -> Result<(), SqliteStoreError> {
        self.clear_all()?;

        let mut entries: Vec<ChainIndexEntry> = state
            .chain()
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect();
        entries.sort_by_key(|entry| (entry.height, entry.block_hash.to_string()));
        for entry in &entries {
            let block = state
                .get_block(&entry.block_hash)
                .ok_or(SqliteStoreError::MissingBlockForEntry(entry.block_hash))?;
            self.save_block_record(block, entry, None)?;
        }
        self.save_active_utxos(state.active_utxos())?;
        self.save_mempool(state.mempool())?;

        if let Some(best_tip) = state.chain().best_tip() {
            self.connection.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('best_tip', ?1)",
                params![best_tip.to_string()],
            )?;
        }

        Ok(())
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
             CREATE TABLE IF NOT EXISTS active_utxos (
                 txid TEXT NOT NULL,
                 vout INTEGER NOT NULL,
                 value INTEGER NOT NULL,
                 locking_data BLOB NOT NULL,
                 created_at_height INTEGER NOT NULL,
                 is_coinbase INTEGER NOT NULL,
                 PRIMARY KEY (txid, vout)
             );
             CREATE TABLE IF NOT EXISTS mempool_transactions (
                 txid TEXT PRIMARY KEY,
                 transaction_bytes BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             COMMIT;",
        )?;
        Ok(())
    }

    fn clear_all(&self) -> Result<(), SqliteStoreError> {
        self.connection.execute_batch(
            "BEGIN;
             DELETE FROM blocks;
             DELETE FROM chain_entries;
             DELETE FROM active_utxos;
             DELETE FROM mempool_transactions;
             DELETE FROM metadata;
             COMMIT;",
        )?;
        Ok(())
    }

    fn load_blocks_in_height_order(&self) -> Result<Vec<Block>, SqliteStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT chain_entries.block_hash, blocks.block_bytes
             FROM chain_entries
             LEFT JOIN blocks ON blocks.block_hash = chain_entries.block_hash
             ORDER BY chain_entries.height ASC, chain_entries.block_hash ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })?;

        let mut blocks = Vec::new();
        for row in rows {
            let (block_hash_text, block_bytes) = row?;
            let block_hash = parse_block_hash(&block_hash_text)?;
            let Some(block_bytes) = block_bytes else {
                return Err(SqliteStoreError::MissingBlockForEntry(block_hash));
            };
            let (block, _): (Block, usize) =
                bincode::serde::decode_from_slice(&block_bytes, bincode::config::standard())?;
            blocks.push(block);
        }
        Ok(blocks)
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

fn parse_txid(value: &str) -> Result<Txid, SqliteStoreError> {
    let bytes =
        hex::decode(value).map_err(|_| SqliteStoreError::InvalidHashHex(value.to_string()))?;
    let len = bytes.len();
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SqliteStoreError::InvalidHashLength(len))?;
    Ok(Txid::new(array))
}

fn utxo_sets_match(left: &UtxoSet, right: &UtxoSet) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter()
        .all(|(outpoint, utxo)| right.get(outpoint) == Some(utxo))
}

fn mempools_match(left: &Mempool, right: &Mempool) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter().all(|(txid, tx)| right.get(txid) == Some(tx))
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

    use crate::{
        mempool::Mempool,
        state::UtxoSet,
        types::{
            Block, BlockHash, BlockHeader, ChainIndexEntry, OutPoint, Transaction, TxIn, TxOut,
            Txid, Utxo,
        },
    };

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

    #[test]
    fn round_trips_active_utxos() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut utxos = UtxoSet::new();
        utxos.insert(Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 1,
            },
            output: TxOut {
                value: 42,
                locking_data: vec![0xaa, 0xbb],
            },
            created_at_height: 7,
            is_coinbase: true,
        });

        store.save_active_utxos(&utxos).unwrap();

        let loaded = store.load_active_utxos().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.get(&OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 1,
            }),
            utxos.get(&OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 1,
            })
        );
    }

    #[test]
    fn round_trips_mempool_transactions() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let transaction = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::new([0x22; 32]),
                    vout: 0,
                },
                unlocking_data: vec![0x01, 0x02],
            }],
            outputs: vec![TxOut {
                value: 9,
                locking_data: vec![0xaa],
            }],
            lock_time: 3,
        };
        let txid = transaction.txid();
        let mempool = Mempool::from_persisted(vec![(txid, transaction.clone())]);

        store.save_mempool(&mempool).unwrap();

        let loaded = store.load_mempool().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get(&txid), Some(&transaction));
    }

    #[test]
    fn rebuilds_node_state_from_sqlite() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let owner = crate::crypto::signing_key_from_bytes([7; 32]);
        let recipient = crate::crypto::signing_key_from_bytes([9; 32]);
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::default(),
                merkle_root: [0; 32],
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: Vec::new(),
                outputs: vec![TxOut {
                    value: 50,
                    locking_data: crate::crypto::verifying_key_bytes(&owner.verifying_key())
                        .to_vec(),
                }],
                lock_time: 0,
            }],
        };
        block.header.merkle_root = block.merkle_root();
        loop {
            if crate::consensus::validate_block(&block).is_ok() {
                break;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
        let block_hash = block.header.block_hash();
        let entry = ChainIndexEntry {
            block_hash,
            height: 0,
            cumulative_work: 123,
            parent: None,
        };
        let utxo = Utxo {
            outpoint: OutPoint {
                txid: block.transactions[0].txid(),
                vout: 0,
            },
            output: block.transactions[0].outputs[0].clone(),
            created_at_height: 0,
            is_coinbase: true,
        };
        let mut mempool_tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: utxo.outpoint,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value: 25,
                locking_data: crate::crypto::verifying_key_bytes(&recipient.verifying_key())
                    .to_vec(),
            }],
            lock_time: 9,
        };
        mempool_tx.inputs[0].unlocking_data =
            crate::crypto::sign_message(&owner, &mempool_tx.signing_digest()).to_vec();
        let mempool = Mempool::from_persisted(vec![(mempool_tx.txid(), mempool_tx.clone())]);
        let mut utxos = UtxoSet::new();
        utxos.insert(utxo);

        store
            .save_block_record(&block, &entry, Some(block_hash))
            .unwrap();
        store.save_active_utxos(&utxos).unwrap();
        store.save_mempool(&mempool).unwrap();

        let rebuilt = store.load_node_state().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(rebuilt.chain().best_tip(), Some(block_hash));
        assert_eq!(rebuilt.active_utxos().len(), 1);
        assert_eq!(rebuilt.mempool().get(&mempool_tx.txid()), Some(&mempool_tx));
    }

    #[test]
    fn saves_and_reloads_full_node_state() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let owner = crate::crypto::signing_key_from_bytes([7; 32]);
        let recipient = crate::crypto::signing_key_from_bytes([9; 32]);
        let mut state = crate::node_state::NodeState::new();
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::default(),
                merkle_root: [0; 32],
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: Vec::new(),
                outputs: vec![TxOut {
                    value: 50,
                    locking_data: crate::crypto::verifying_key_bytes(&owner.verifying_key())
                        .to_vec(),
                }],
                lock_time: 0,
            }],
        };
        block.header.merkle_root = block.merkle_root();
        loop {
            if crate::consensus::validate_block(&block).is_ok() {
                break;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
        let spendable = OutPoint {
            txid: block.transactions[0].txid(),
            vout: 0,
        };
        state.accept_block(block).unwrap();
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: spendable,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value: 25,
                locking_data: crate::crypto::verifying_key_bytes(&recipient.verifying_key())
                    .to_vec(),
            }],
            lock_time: 9,
        };
        tx.inputs[0].unlocking_data =
            crate::crypto::sign_message(&owner, &tx.signing_digest()).to_vec();
        state.submit_transaction(tx.clone()).unwrap();

        store.save_node_state(&state).unwrap();
        let reloaded = store.load_node_state().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(reloaded.chain().best_tip(), state.chain().best_tip());
        assert_eq!(reloaded.active_utxos().len(), state.active_utxos().len());
        assert_eq!(reloaded.mempool().get(&tx.txid()), Some(&tx));
    }
}
