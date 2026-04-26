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
//! - `peers` stores seed and learned peer metadata for future discovery work
//! - `transactions` stores one raw confirmed or mempool record per transaction
//! - `transaction_keys` stores the one-to-many per-key input/output edges for
//!   each transaction so wallet-style queries can match any participating key
//! - `metadata` stores singleton node-level values such as the current best tip
//!
//! Save strategy:
//! - admitted mempool transactions use `save_mempool_transaction` so the hot
//!   path can upsert one pending transaction without rewriting the full mempool
//! - best-tip block extensions use `save_accepted_block`, which updates the new
//!   block record, active UTXO view, stale mempool rows, and confirmed
//!   transaction rows for that block
//! - `save_node_state` remains available as the authoritative full-snapshot
//!   path for initialization, rebuilds, and test seeding when incremental
//!   updates are not enough

use std::{io, path::Path};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::{
    mempool::Mempool,
    node_state::{NodeState, NodeStateError},
    peers::{PeerSource, StoredPeer},
    state::UtxoSet,
    tx_index::{
        IndexedTransaction, IndexedTransactionStatus, TransactionKeyEdge, TransactionKeyRole,
    },
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
    InvalidTransactionStatus(String),
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

    /// Opens an existing SQLite store in read-only mode for local inspection.
    ///
    /// This avoids taking a write-capable handle when CLI commands only need
    /// persisted state and a running server may already own the writable
    /// connection.
    pub fn open_read_only(path: &Path) -> Result<Self, SqliteStoreError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { connection })
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

    /// Returns how many chain index entries are currently persisted.
    pub fn count_chain_entries(&self) -> Result<usize, SqliteStoreError> {
        let count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM chain_entries", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Saves one admitted mempool transaction without rewriting the whole mempool view.
    ///
    /// This upserts the raw mempool row, the wallet-style transaction index
    /// row, and the per-key participation edges for exactly one pending
    /// transaction. Input edges are resolved from already-persisted confirmed
    /// or mempool transactions so chained mempool spends remain queryable.
    ///
    /// Caller requirements:
    /// - `transaction` must already have passed local mempool admission and be
    ///   present in the node's in-memory mempool.
    /// - any referenced inputs that should appear as indexed input edges must
    ///   already be present in the persisted confirmed or mempool transaction
    ///   tables; otherwise those input edges are omitted until a broader save
    ///   path rebuilds the index.
    /// - this should not be used to mark a confirmed transaction; confirmed
    ///   block inclusion belongs in `save_accepted_block`.
    pub fn save_mempool_transaction(
        &self,
        transaction: &Transaction,
    ) -> Result<(), SqliteStoreError> {
        let txid = transaction.txid();
        let transaction_bytes =
            bincode::serde::encode_to_vec(transaction, bincode::config::standard())?;
        let indexed = IndexedTransaction {
            txid,
            transaction: transaction.clone(),
            block_hash: None,
            block_height: None,
            position_in_block: None,
            status: IndexedTransactionStatus::Mempool,
            first_seen_at: None,
            last_updated_at: None,
        };
        let edges = self.build_transaction_key_edges_for_persisted_context(transaction)?;
        let tx = self.connection.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO mempool_transactions (txid, transaction_bytes) VALUES (?1, ?2)",
            params![txid.to_string(), transaction_bytes],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO transactions (
                 txid, transaction_bytes, block_hash, block_height, position_in_block,
                 status, first_seen_at, last_updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                indexed.txid.to_string(),
                bincode::serde::encode_to_vec(&indexed.transaction, bincode::config::standard())?,
                Option::<String>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                indexed.status.as_str(),
                indexed.first_seen_at,
                indexed.last_updated_at,
            ],
        )?;
        tx.execute(
            "DELETE FROM transaction_keys WHERE txid = ?1",
            params![txid.to_string()],
        )?;
        for edge in &edges {
            tx.execute(
                "INSERT INTO transaction_keys (
                     txid, key_role, key_data, value, vout
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    edge.txid.to_string(),
                    edge.key_role.as_str(),
                    edge.key_data,
                    edge.value.map(|value| value as i64),
                    edge.vout.map(|value| value as i64),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads one persisted mempool transaction by transaction id when present.
    pub fn load_mempool_transaction(
        &self,
        txid: Txid,
    ) -> Result<Option<Transaction>, SqliteStoreError> {
        let transaction_bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT transaction_bytes FROM mempool_transactions WHERE txid = ?1",
                params![txid.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(transaction_bytes) = transaction_bytes else {
            return Ok(None);
        };
        let (transaction, _): (Transaction, usize) =
            bincode::serde::decode_from_slice(&transaction_bytes, bincode::config::standard())?;
        Ok(Some(transaction))
    }

    /// Saves the current mempool-facing node state after pending transactions changed.
    ///
    /// This keeps the persisted mempool tables and wallet-style transaction
    /// index aligned with the current in-memory state without rewriting block
    /// history or chain metadata.
    ///
    /// EXPENSIVE: rewrites the full persisted mempool and transaction index.
    ///
    /// Caller requirements:
    /// - `state` should already reflect the full desired in-memory mempool
    ///   after admission, replacement, or eviction decisions have been made.
    /// - the persisted block history should already match the chain view in
    ///   `state`; this method only rewrites mempool-derived tables.
    pub fn save_mempool_state(&self, state: &NodeState) -> Result<(), SqliteStoreError> {
        // EXPENSIVE: rewrites the full persisted mempool and transaction index.
        let (transactions, edges) = build_transaction_index(state);
        self.save_mempool(state.mempool())?;
        self.save_transaction_index(&transactions, &edges)
    }

    /// Saves the current node state after one block was accepted into the local chain view.
    ///
    /// The caller identifies which block triggered the save, and the store
    /// derives the associated block record, chain entry, best tip, active UTXO
    /// view, mempool removals, and confirmed transaction index updates from
    /// the current `NodeState`.
    ///
    /// Caller requirements:
    /// - `state` must already include `block_hash` in its block store and chain
    ///   index.
    /// - `state` must already reflect any mempool removals, best-tip changes,
    ///   and UTXO updates caused by accepting that block.
    /// - `block_hash` should name the block that just caused the state change,
    ///   not an unrelated historical block.
    pub fn save_accepted_block(
        &self,
        state: &NodeState,
        block_hash: BlockHash,
    ) -> Result<(), SqliteStoreError> {
        let block = state
            .get_block(&block_hash)
            .ok_or(SqliteStoreError::MissingBlockForEntry(block_hash))?;
        let entry = state
            .chain()
            .get(&block_hash)
            .ok_or(SqliteStoreError::MissingBlockForEntry(block_hash))?;
        self.save_block_record(block, entry, state.chain().best_tip())?;
        if !best_chain_contains(state, block_hash) {
            return Ok(());
        }
        self.save_active_utxos(state.active_utxos())?;
        self.delete_stale_mempool_transactions(state.mempool())?;
        self.save_confirmed_block_transactions(block, entry)
    }

    /// Replaces the persisted active-chain UTXO view with `utxos`.
    ///
    /// This is still a whole-view rewrite rather than incremental spend/create
    /// journaling. That keeps the first SQLite-backed UTXO step simple while we
    /// decide the longer-term schema.
    ///
    /// EXPENSIVE: deletes and rewrites the full active UTXO view.
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
    ///
    /// EXPENSIVE: deletes and rewrites the full persisted mempool.
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

    /// Replaces the persisted peer store with `peers`.
    ///
    /// EXPENSIVE: deletes and rewrites the full persisted peer set.
    pub fn save_peers(&self, peers: &[StoredPeer]) -> Result<(), SqliteStoreError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM peers", [])?;
        for peer in peers {
            tx.execute(
                "INSERT INTO peers (
                     addr, node_name, source, advertised_tip_hash, advertised_height,
                     last_hello_at, last_seen_at, last_connect_at, last_success_at,
                     last_error, connections, failed_connections, behavior_score, banned_until
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    peer.addr,
                    peer.node_name,
                    peer.source.as_str(),
                    peer.advertised_tip_hash.map(|hash| hash.to_string()),
                    peer.advertised_height.map(|height| height as i64),
                    peer.last_hello_at,
                    peer.last_seen_at,
                    peer.last_connect_at,
                    peer.last_success_at,
                    peer.last_error,
                    peer.connections as i64,
                    peer.failed_connections as i64,
                    peer.behavior_score,
                    peer.banned_until,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Saves one peer record by address without rewriting the full peer set.
    pub fn save_peer(&self, peer: &StoredPeer) -> Result<(), SqliteStoreError> {
        self.connection.execute(
            "INSERT OR REPLACE INTO peers (
                 addr, node_name, source, advertised_tip_hash, advertised_height,
                 last_hello_at, last_seen_at, last_connect_at, last_success_at,
                 last_error, connections, failed_connections, behavior_score, banned_until
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                peer.addr,
                peer.node_name,
                peer.source.as_str(),
                peer.advertised_tip_hash.map(|hash| hash.to_string()),
                peer.advertised_height.map(|height| height as i64),
                peer.last_hello_at,
                peer.last_seen_at,
                peer.last_connect_at,
                peer.last_success_at,
                peer.last_error,
                peer.connections as i64,
                peer.failed_connections as i64,
                peer.behavior_score,
                peer.banned_until,
            ],
        )?;
        Ok(())
    }

    /// Loads one persisted peer by address when present.
    pub fn load_peer(&self, addr: &str) -> Result<Option<StoredPeer>, SqliteStoreError> {
        self.connection
            .query_row(
                "SELECT
                     addr, node_name, source, advertised_tip_hash, advertised_height,
                     last_hello_at, last_seen_at, last_connect_at, last_success_at,
                     last_error, connections, failed_connections, behavior_score, banned_until
                 FROM peers
                 WHERE addr = ?1",
                params![addr],
                |row| stored_peer_from_row(row),
            )
            .optional()
            .map_err(SqliteStoreError::from)
    }

    /// Loads the persisted peer store ordered by address.
    pub fn load_peers(&self) -> Result<Vec<StoredPeer>, SqliteStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT
                 addr, node_name, source, advertised_tip_hash, advertised_height,
                 last_hello_at, last_seen_at, last_connect_at, last_success_at,
                 last_error, connections, failed_connections, behavior_score, banned_until
             FROM peers
             ORDER BY addr ASC",
        )?;
        let rows = statement.query_map([], stored_peer_from_row)?;

        let mut peers = Vec::new();
        for row in rows {
            peers.push(row?);
        }
        Ok(peers)
    }

    /// Replaces the persisted transaction index with `transactions` and `edges`.
    ///
    /// EXPENSIVE: deletes and rewrites the full wallet-style transaction index.
    pub fn save_transaction_index(
        &self,
        transactions: &[IndexedTransaction],
        edges: &[TransactionKeyEdge],
    ) -> Result<(), SqliteStoreError> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM transactions", [])?;
        tx.execute("DELETE FROM transaction_keys", [])?;
        for indexed in transactions {
            let transaction_bytes =
                bincode::serde::encode_to_vec(&indexed.transaction, bincode::config::standard())?;
            tx.execute(
                "INSERT INTO transactions (
                     txid, transaction_bytes, block_hash, block_height, position_in_block,
                     status, first_seen_at, last_updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    indexed.txid.to_string(),
                    transaction_bytes,
                    indexed.block_hash.map(|value| value.to_string()),
                    indexed.block_height.map(|value| value as i64),
                    indexed.position_in_block.map(|value| value as i64),
                    indexed.status.as_str(),
                    indexed.first_seen_at,
                    indexed.last_updated_at,
                ],
            )?;
        }
        for edge in edges {
            tx.execute(
                "INSERT INTO transaction_keys (
                     txid, key_role, key_data, value, vout
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    edge.txid.to_string(),
                    edge.key_role.as_str(),
                    edge.key_data,
                    edge.value.map(|value| value as i64),
                    edge.vout.map(|value| value as i64),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads one indexed transaction by transaction id when present.
    pub fn load_indexed_transaction(
        &self,
        txid: Txid,
    ) -> Result<Option<IndexedTransaction>, SqliteStoreError> {
        let row: Option<IndexedTransaction> = self
            .connection
            .query_row(
                "SELECT
                     txid, transaction_bytes, block_hash, block_height, position_in_block,
                     status, first_seen_at, last_updated_at
                 FROM transactions
                 WHERE txid = ?1",
                params![txid.to_string()],
                |row| {
                    let txid_text: String = row.get(0)?;
                    let transaction_bytes: Vec<u8> = row.get(1)?;
                    let block_hash_text: Option<String> = row.get(2)?;
                    let status_text: String = row.get(5)?;
                    let status = IndexedTransactionStatus::parse(&status_text).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("invalid transaction status: {status_text}"),
                            )),
                        )
                    })?;
                    let parsed_txid = parse_txid(&txid_text).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("{err:?}"),
                            )),
                        )
                    })?;
                    let (transaction, _): (Transaction, usize) = bincode::serde::decode_from_slice(
                        &transaction_bytes,
                        bincode::config::standard(),
                    )
                    .map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Blob,
                            Box::new(err),
                        )
                    })?;
                    Ok(IndexedTransaction {
                        txid: parsed_txid,
                        transaction,
                        block_hash: block_hash_text
                            .map(|value| parse_block_hash(&value))
                            .transpose()
                            .map_err(|err| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    rusqlite::types::Type::Text,
                                    Box::new(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!("{err:?}"),
                                    )),
                                )
                            })?,
                        block_height: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                        position_in_block: row
                            .get::<_, Option<i64>>(4)?
                            .map(|value| value as u32),
                        status,
                        first_seen_at: row.get(6)?,
                        last_updated_at: row.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Loads all indexed transactions ordered by confirmation height then txid.
    pub fn load_indexed_transactions(&self) -> Result<Vec<IndexedTransaction>, SqliteStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT
                 txid, transaction_bytes, block_hash, block_height, position_in_block,
                 status, first_seen_at, last_updated_at
             FROM transactions
             ORDER BY block_height ASC NULLS LAST, position_in_block ASC NULLS LAST, txid ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let txid_text: String = row.get(0)?;
            let transaction_bytes: Vec<u8> = row.get(1)?;
            let block_hash_text: Option<String> = row.get(2)?;
            let status_text: String = row.get(5)?;
            let status = IndexedTransactionStatus::parse(&status_text).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid transaction status: {status_text}"),
                    )),
                )
            })?;
            let txid = parse_txid(&txid_text).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{err:?}"),
                    )),
                )
            })?;
            let (transaction, _): (Transaction, usize) =
                bincode::serde::decode_from_slice(&transaction_bytes, bincode::config::standard())
                    .map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Blob,
                            Box::new(err),
                        )
                    })?;
            Ok(IndexedTransaction {
                txid,
                transaction,
                block_hash: block_hash_text
                    .map(|value| parse_block_hash(&value))
                    .transpose()
                    .map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("{err:?}"),
                            )),
                        )
                    })?,
                block_height: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                position_in_block: row.get::<_, Option<i64>>(4)?.map(|value| value as u32),
                status,
                first_seen_at: row.get(6)?,
                last_updated_at: row.get(7)?,
            })
        })?;

        let mut transactions = Vec::new();
        for row in rows {
            transactions.push(row?);
        }
        Ok(transactions)
    }

    /// Loads indexed transactions that mention any of `keys` as inputs or outputs.
    ///
    /// The current query fan-outs once per key, then de-duplicates by txid in
    /// memory so multi-key wallets can ask for "my transactions" across all of
    /// their owned locking keys.
    pub fn load_transactions_for_keys(
        &self,
        keys: &[Vec<u8>],
    ) -> Result<Vec<IndexedTransaction>, SqliteStoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = self.connection.prepare(
            "SELECT DISTINCT
                 transactions.txid,
                 transactions.transaction_bytes,
                 transactions.block_hash,
                 transactions.block_height,
                 transactions.position_in_block,
                 transactions.status,
                 transactions.first_seen_at,
                 transactions.last_updated_at
             FROM transactions
             INNER JOIN transaction_keys ON transaction_keys.txid = transactions.txid
             WHERE transaction_keys.key_data = ?1
             ORDER BY transactions.block_height ASC NULLS LAST,
                      transactions.position_in_block ASC NULLS LAST,
                      transactions.txid ASC",
        )?;

        let mut by_txid = std::collections::HashMap::new();
        for key in keys {
            let rows = statement.query_map(params![key], |row| {
                let txid_text: String = row.get(0)?;
                let transaction_bytes: Vec<u8> = row.get(1)?;
                let block_hash_text: Option<String> = row.get(2)?;
                let status_text: String = row.get(5)?;
                let status = IndexedTransactionStatus::parse(&status_text).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid transaction status: {status_text}"),
                        )),
                    )
                })?;
                let txid = parse_txid(&txid_text).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("{err:?}"),
                        )),
                    )
                })?;
                let (transaction, _): (Transaction, usize) = bincode::serde::decode_from_slice(
                    &transaction_bytes,
                    bincode::config::standard(),
                )
                .map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Blob,
                        Box::new(err),
                    )
                })?;
                Ok(IndexedTransaction {
                    txid,
                    transaction,
                    block_hash: block_hash_text
                        .map(|value| parse_block_hash(&value))
                        .transpose()
                        .map_err(|err| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!("{err:?}"),
                                )),
                            )
                        })?,
                    block_height: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                    position_in_block: row.get::<_, Option<i64>>(4)?.map(|value| value as u32),
                    status,
                    first_seen_at: row.get(6)?,
                    last_updated_at: row.get(7)?,
                })
            })?;
            for row in rows {
                let indexed = row?;
                by_txid.entry(indexed.txid).or_insert(indexed);
            }
        }

        let mut transactions: Vec<IndexedTransaction> = by_txid.into_values().collect();
        transactions.sort_by_key(|indexed| {
            (
                indexed.block_height.unwrap_or(u64::MAX),
                indexed.position_in_block.unwrap_or(u32::MAX),
                indexed.txid.to_string(),
            )
        });
        Ok(transactions)
    }

    /// Loads all persisted key-participation edges for one indexed transaction.
    ///
    /// Wallet-facing tools use these rows to compute how much value the local
    /// wallet sent, received, or moved internally without rescanning the chain.
    pub fn load_transaction_key_edges(
        &self,
        txid: Txid,
    ) -> Result<Vec<TransactionKeyEdge>, SqliteStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT key_role, key_data, value, vout
             FROM transaction_keys
             WHERE txid = ?1
             ORDER BY key_role ASC, vout ASC NULLS LAST",
        )?;
        let rows = statement.query_map(params![txid.to_string()], |row| {
            let role_text: String = row.get(0)?;
            let key_role = TransactionKeyRole::parse(&role_text).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid transaction key role: {role_text}"),
                    )),
                )
            })?;
            Ok(TransactionKeyEdge {
                txid,
                key_role,
                key_data: row.get(1)?,
                value: row.get::<_, Option<i64>>(2)?.map(|value| value as u64),
                vout: row.get::<_, Option<i64>>(3)?.map(|value| value as u32),
            })
        })?;

        let mut edges = Vec::new();
        for row in rows {
            edges.push(row?);
        }
        Ok(edges)
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
    ///
    /// EXPENSIVE: rewrites the full persisted node snapshot.
    pub fn save_node_state(&self, state: &NodeState) -> Result<(), SqliteStoreError> {
        // TODO: Keep shrinking the remaining callers of this full snapshot
        // path as more initialization and repair flows get narrower saves.
        let tx = self.connection.unchecked_transaction()?;
        tx.execute_batch(
            "DELETE FROM blocks;
             DELETE FROM chain_entries;
             DELETE FROM active_utxos;
             DELETE FROM mempool_transactions;
             DELETE FROM peers;
             DELETE FROM transactions;
             DELETE FROM transaction_keys;
             DELETE FROM metadata;",
        )?;

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
            let block_hash = entry.block_hash.to_string();
            let parent_hash = entry.parent.map(|hash| hash.to_string());
            let block_bytes =
                bincode::serde::encode_to_vec(block, bincode::config::standard())?;
            let work_bytes = entry.cumulative_work.to_be_bytes().to_vec();
            tx.execute(
                "INSERT OR REPLACE INTO blocks (block_hash, block_bytes) VALUES (?1, ?2)",
                params![block_hash, block_bytes],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO chain_entries (block_hash, height, cumulative_work, parent_hash)
                 VALUES (?1, ?2, ?3, ?4)",
                params![block_hash, entry.height as i64, work_bytes, parent_hash],
            )?;
        }

        for (outpoint, utxo) in state.active_utxos().iter() {
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

        for (txid, transaction) in state.mempool().iter() {
            let bytes =
                bincode::serde::encode_to_vec(transaction, bincode::config::standard())?;
            tx.execute(
                "INSERT INTO mempool_transactions (txid, transaction_bytes) VALUES (?1, ?2)",
                params![txid.to_string(), bytes],
            )?;
        }

        let (transactions, edges) = build_transaction_index(state);
        for indexed in &transactions {
            let transaction_bytes = bincode::serde::encode_to_vec(
                &indexed.transaction,
                bincode::config::standard(),
            )?;
            tx.execute(
                "INSERT INTO transactions (
                     txid, transaction_bytes, block_hash, block_height, position_in_block,
                     status, first_seen_at, last_updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    indexed.txid.to_string(),
                    transaction_bytes,
                    indexed.block_hash.map(|value| value.to_string()),
                    indexed.block_height.map(|value| value as i64),
                    indexed.position_in_block.map(|value| value as i64),
                    indexed.status.as_str(),
                    indexed.first_seen_at,
                    indexed.last_updated_at,
                ],
            )?;
        }
        for edge in &edges {
            tx.execute(
                "INSERT INTO transaction_keys (
                     txid, key_role, key_data, value, vout
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    edge.txid.to_string(),
                    edge.key_role.as_str(),
                    edge.key_data,
                    edge.value.map(|value| value as i64),
                    edge.vout.map(|value| value as i64),
                ],
            )?;
        }

        if let Some(best_tip) = state.chain().best_tip() {
            tx.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('best_tip', ?1)",
                params![best_tip.to_string()],
            )?;
        }
        tx.commit()?;

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
             CREATE TABLE IF NOT EXISTS peers (
                 addr TEXT PRIMARY KEY,
                 node_name TEXT NULL,
                 source TEXT NOT NULL,
                 advertised_tip_hash TEXT NULL,
                 advertised_height INTEGER NULL,
                 last_hello_at TEXT NULL,
                 last_seen_at TEXT NULL,
                 last_connect_at TEXT NULL,
                 last_success_at TEXT NULL,
                 last_error TEXT NULL,
                 connections INTEGER NOT NULL DEFAULT 0,
                 failed_connections INTEGER NOT NULL DEFAULT 0,
                 behavior_score INTEGER NOT NULL DEFAULT 100,
                 banned_until TEXT NULL
             );
             CREATE TABLE IF NOT EXISTS transactions (
                 txid TEXT PRIMARY KEY,
                 transaction_bytes BLOB NOT NULL,
                 block_hash TEXT NULL,
                 block_height INTEGER NULL,
                 position_in_block INTEGER NULL,
                 status TEXT NOT NULL,
                 first_seen_at TEXT NULL,
                 last_updated_at TEXT NULL
             );
             CREATE TABLE IF NOT EXISTS transaction_keys (
                 txid TEXT NOT NULL,
                 key_role TEXT NOT NULL,
                 key_data BLOB NOT NULL,
                 value INTEGER NULL,
                 vout INTEGER NULL
             );
             CREATE INDEX IF NOT EXISTS transaction_keys_key_data_idx
                 ON transaction_keys (key_data);
             CREATE INDEX IF NOT EXISTS transactions_status_height_idx
                 ON transactions (status, block_height, position_in_block);
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
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

    /// Deletes persisted mempool rows that are no longer present in the current mempool.
    fn delete_stale_mempool_transactions(
        &self,
        mempool: &Mempool,
    ) -> Result<(), SqliteStoreError> {
        let desired: std::collections::HashSet<Txid> =
            mempool.iter().map(|(txid, _)| *txid).collect();
        let persisted = self.load_persisted_mempool_txids()?;
        let tx = self.connection.unchecked_transaction()?;
        for txid in persisted {
            if desired.contains(&txid) {
                continue;
            }
            tx.execute(
                "DELETE FROM mempool_transactions WHERE txid = ?1",
                params![txid.to_string()],
            )?;
            tx.execute(
                "DELETE FROM transactions WHERE txid = ?1 AND status = 'mempool'",
                params![txid.to_string()],
            )?;
            tx.execute(
                "DELETE FROM transaction_keys WHERE txid = ?1",
                params![txid.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Saves the confirmed transaction rows and key edges for one accepted block.
    fn save_confirmed_block_transactions(
        &self,
        block: &Block,
        entry: &ChainIndexEntry,
    ) -> Result<(), SqliteStoreError> {
        let (transactions, edges) =
            self.build_confirmed_block_index(block, entry.block_hash, entry.height)?;
        let mut merged_transactions = Vec::with_capacity(transactions.len());
        for indexed in transactions {
            merged_transactions.push(self.merge_confirmed_transaction_metadata(indexed)?);
        }
        let tx = self.connection.unchecked_transaction()?;
        for indexed in &merged_transactions {
            self.delete_mempool_transaction_row(&tx, indexed.txid)?;
            self.upsert_indexed_transaction_row(&tx, indexed)?;
            self.delete_transaction_key_rows(&tx, indexed.txid)?;
        }
        for edge in &edges {
            self.insert_transaction_key_row(&tx, edge)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads the set of transaction ids currently persisted in the mempool table.
    fn load_persisted_mempool_txids(&self) -> Result<Vec<Txid>, SqliteStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT txid FROM mempool_transactions ORDER BY txid ASC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut txids = Vec::new();
        for row in rows {
            txids.push(parse_txid(&row?)?);
        }
        Ok(txids)
    }

    /// Merges any previously indexed metadata into a newly confirmed transaction row.
    ///
    /// This keeps fields like `first_seen_at` stable when a transaction moves
    /// from mempool indexing to confirmed indexing.
    fn merge_confirmed_transaction_metadata(
        &self,
        mut indexed: IndexedTransaction,
    ) -> Result<IndexedTransaction, SqliteStoreError> {
        if let Some(existing) = self.load_indexed_transaction(indexed.txid)? {
            indexed.first_seen_at = existing.first_seen_at;
        }
        Ok(indexed)
    }

    /// Deletes one persisted mempool row after its transaction left the mempool.
    fn delete_mempool_transaction_row(
        &self,
        tx: &rusqlite::Transaction<'_>,
        txid: Txid,
    ) -> Result<(), SqliteStoreError> {
        tx.execute(
            "DELETE FROM mempool_transactions WHERE txid = ?1",
            params![txid.to_string()],
        )?;
        Ok(())
    }

    /// Upserts one wallet-style transaction index row.
    fn upsert_indexed_transaction_row(
        &self,
        tx: &rusqlite::Transaction<'_>,
        indexed: &IndexedTransaction,
    ) -> Result<(), SqliteStoreError> {
        let transaction_bytes =
            bincode::serde::encode_to_vec(&indexed.transaction, bincode::config::standard())?;
        tx.execute(
            "INSERT OR REPLACE INTO transactions (
                 txid, transaction_bytes, block_hash, block_height, position_in_block,
                 status, first_seen_at, last_updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                indexed.txid.to_string(),
                transaction_bytes,
                indexed.block_hash.map(|value| value.to_string()),
                indexed.block_height.map(|value| value as i64),
                indexed.position_in_block.map(|value| value as i64),
                indexed.status.as_str(),
                indexed.first_seen_at,
                indexed.last_updated_at,
            ],
        )?;
        Ok(())
    }

    /// Deletes all persisted key-edge rows for one transaction before replacement.
    fn delete_transaction_key_rows(
        &self,
        tx: &rusqlite::Transaction<'_>,
        txid: Txid,
    ) -> Result<(), SqliteStoreError> {
        tx.execute(
            "DELETE FROM transaction_keys WHERE txid = ?1",
            params![txid.to_string()],
        )?;
        Ok(())
    }

    /// Inserts one per-key transaction edge row.
    fn insert_transaction_key_row(
        &self,
        tx: &rusqlite::Transaction<'_>,
        edge: &TransactionKeyEdge,
    ) -> Result<(), SqliteStoreError> {
        tx.execute(
            "INSERT INTO transaction_keys (
                 txid, key_role, key_data, value, vout
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                edge.txid.to_string(),
                edge.key_role.as_str(),
                edge.key_data,
                edge.value.map(|value| value as i64),
                edge.vout.map(|value| value as i64),
            ],
        )?;
        Ok(())
    }

    /// Builds one transaction's key edges using already-persisted transaction outputs.
    fn build_transaction_key_edges_for_persisted_context(
        &self,
        transaction: &Transaction,
    ) -> Result<Vec<TransactionKeyEdge>, SqliteStoreError> {
        let mut edges = Vec::new();
        let txid = transaction.txid();
        for input in &transaction.inputs {
            if let Some(previous_output) = self.load_output(input.previous_output)? {
                edges.push(TransactionKeyEdge {
                    txid,
                    key_role: TransactionKeyRole::Input,
                    key_data: previous_output.locking_data,
                    value: Some(previous_output.value),
                    vout: Some(input.previous_output.vout),
                });
            }
        }
        for (vout, output) in transaction.outputs.iter().enumerate() {
            edges.push(TransactionKeyEdge {
                txid,
                key_role: TransactionKeyRole::Output,
                key_data: output.locking_data.clone(),
                value: Some(output.value),
                vout: Some(vout as u32),
            });
        }
        Ok(edges)
    }

    /// Loads one referenced output from the persisted transaction index when known.
    fn load_output(&self, outpoint: OutPoint) -> Result<Option<TxOut>, SqliteStoreError> {
        let transaction_bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT transaction_bytes FROM transactions WHERE txid = ?1",
                params![outpoint.txid.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(transaction_bytes) = transaction_bytes else {
            return Ok(None);
        };
        let (transaction, _): (Transaction, usize) =
            bincode::serde::decode_from_slice(&transaction_bytes, bincode::config::standard())?;
        Ok(transaction.outputs.get(outpoint.vout as usize).cloned())
    }

    /// Builds confirmed transaction rows and key edges for one persisted block.
    fn build_confirmed_block_index(
        &self,
        block: &Block,
        block_hash: BlockHash,
        block_height: u64,
    ) -> Result<(Vec<IndexedTransaction>, Vec<TransactionKeyEdge>), SqliteStoreError> {
        let mut transactions = Vec::new();
        let mut edges = Vec::new();
        let mut known_outputs = std::collections::HashMap::<OutPoint, TxOut>::new();
        for (position, transaction) in block.transactions.iter().enumerate() {
            let txid = transaction.txid();
            transactions.push(IndexedTransaction {
                txid,
                transaction: transaction.clone(),
                block_hash: Some(block_hash),
                block_height: Some(block_height),
                position_in_block: Some(position as u32),
                status: IndexedTransactionStatus::Confirmed,
                first_seen_at: None,
                last_updated_at: None,
            });
            append_transaction_key_edges_with_fallback(
                self,
                transaction,
                &known_outputs,
                &mut edges,
            )?;
            remember_outputs(transaction, &mut known_outputs);
        }
        Ok((transactions, edges))
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

fn stored_peer_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPeer> {
    let source_text: String = row.get(2)?;
    let source = PeerSource::parse(&source_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid peer source: {source_text}"),
            )),
        )
    })?;
    Ok(StoredPeer {
        addr: row.get(0)?,
        node_name: row.get(1)?,
        source,
        advertised_tip_hash: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_block_hash(&value))
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{err:?}"),
                    )),
                )
            })?,
        advertised_height: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
        last_hello_at: row.get(5)?,
        last_seen_at: row.get(6)?,
        last_connect_at: row.get(7)?,
        last_success_at: row.get(8)?,
        last_error: row.get(9)?,
        connections: row.get::<_, i64>(10)? as u32,
        failed_connections: row.get::<_, i64>(11)? as u32,
        behavior_score: row.get(12)?,
        banned_until: row.get(13)?,
    })
}

/// Builds the persisted transaction index view from the current chain and mempool.
///
/// Confirmed transactions are indexed from the current best chain only, in
/// chain-height order with their block position, then pending mempool
/// transactions are added as unconfirmed rows. Side branches are intentionally
/// excluded so wallet-style history reflects the active chain and avoids
/// duplicate `txid` rows when the same transaction appears in competing forks.
/// Input edges are resolved through the outputs created by known chain and
/// mempool transactions so wallet lookups can match both spends and receipts by
/// locking key.
fn build_transaction_index(
    state: &NodeState,
) -> (Vec<IndexedTransaction>, Vec<TransactionKeyEdge>) {
    let mut transactions = Vec::new();
    let mut edges = Vec::new();
    let mut seen_txids = std::collections::HashSet::<Txid>::new();
    let mut known_outputs = std::collections::HashMap::<OutPoint, TxOut>::new();
    let entries = best_chain_entries(state);

    for entry in entries {
        let Some(block) = state.get_block(&entry.block_hash) else {
            continue;
        };
        for (position, transaction) in block.transactions.iter().enumerate() {
            let txid = transaction.txid();
            // TODO: Model duplicate txids explicitly in the wallet index.
            // Bitcoin-style coinbase transactions can repeat the same txid
            // across blocks, but the current wallet tables still key rows by
            // txid alone.
            if !seen_txids.insert(txid) {
                continue;
            }
            transactions.push(IndexedTransaction {
                txid,
                transaction: transaction.clone(),
                block_hash: Some(entry.block_hash),
                block_height: Some(entry.height),
                position_in_block: Some(position as u32),
                status: IndexedTransactionStatus::Confirmed,
                first_seen_at: None,
                last_updated_at: None,
            });
            append_transaction_key_edges(transaction, &known_outputs, &mut edges);
            remember_outputs(transaction, &mut known_outputs);
        }
    }

    let mut mempool_transactions: Vec<(Txid, Transaction)> = state
        .mempool()
        .iter()
        .map(|(txid, tx)| (*txid, tx.clone()))
        .collect();
    mempool_transactions.sort_by_key(|(txid, _)| txid.to_string());
    for (txid, transaction) in mempool_transactions {
        if !seen_txids.insert(txid) {
            continue;
        }
        transactions.push(IndexedTransaction {
            txid,
            transaction: transaction.clone(),
            block_hash: None,
            block_height: None,
            position_in_block: None,
            status: IndexedTransactionStatus::Mempool,
            first_seen_at: None,
            last_updated_at: None,
        });
        append_transaction_key_edges(&transaction, &known_outputs, &mut edges);
        remember_outputs(&transaction, &mut known_outputs);
    }

    (transactions, edges)
}

/// Returns whether `block_hash` currently lies on the active best chain.
fn best_chain_contains(state: &NodeState, block_hash: BlockHash) -> bool {
    best_chain_entries(state)
        .iter()
        .any(|entry| entry.block_hash == block_hash)
}

/// Returns the indexed blocks on the current best chain from genesis to tip.
fn best_chain_entries(state: &NodeState) -> Vec<ChainIndexEntry> {
    let mut entries = Vec::new();
    let mut cursor = state.chain().best_tip();
    while let Some(block_hash) = cursor {
        let Some(entry) = state.chain().get(&block_hash) else {
            break;
        };
        entries.push(entry.clone());
        cursor = entry.parent;
    }
    entries.reverse();
    entries
}

/// Records the input and output key relationships for one transaction.
fn append_transaction_key_edges(
    transaction: &Transaction,
    known_outputs: &std::collections::HashMap<OutPoint, TxOut>,
    edges: &mut Vec<TransactionKeyEdge>,
) {
    let txid = transaction.txid();
    for input in &transaction.inputs {
        if let Some(previous_output) = known_outputs.get(&input.previous_output) {
            edges.push(TransactionKeyEdge {
                txid,
                key_role: TransactionKeyRole::Input,
                key_data: previous_output.locking_data.clone(),
                value: Some(previous_output.value),
                vout: Some(input.previous_output.vout),
            });
        }
    }

    for (vout, output) in transaction.outputs.iter().enumerate() {
        edges.push(TransactionKeyEdge {
            txid,
            key_role: TransactionKeyRole::Output,
            key_data: output.locking_data.clone(),
            value: Some(output.value),
            vout: Some(vout as u32),
        });
    }
}

/// Records input and output key relationships, resolving inputs from block-local
/// outputs first and persisted transaction outputs second.
fn append_transaction_key_edges_with_fallback(
    store: &SqliteStore,
    transaction: &Transaction,
    known_outputs: &std::collections::HashMap<OutPoint, TxOut>,
    edges: &mut Vec<TransactionKeyEdge>,
) -> Result<(), SqliteStoreError> {
    let txid = transaction.txid();
    for input in &transaction.inputs {
        let previous_output = match known_outputs.get(&input.previous_output) {
            Some(output) => Some(output.clone()),
            None => store.load_output(input.previous_output)?,
        };
        if let Some(previous_output) = previous_output {
            edges.push(TransactionKeyEdge {
                txid,
                key_role: TransactionKeyRole::Input,
                key_data: previous_output.locking_data,
                value: Some(previous_output.value),
                vout: Some(input.previous_output.vout),
            });
        }
    }

    for (vout, output) in transaction.outputs.iter().enumerate() {
        edges.push(TransactionKeyEdge {
            txid,
            key_role: TransactionKeyRole::Output,
            key_data: output.locking_data.clone(),
            value: Some(output.value),
            vout: Some(vout as u32),
        });
    }
    Ok(())
}

/// Adds newly created outputs so later spends can resolve their locking key.
fn remember_outputs(
    transaction: &Transaction,
    known_outputs: &mut std::collections::HashMap<OutPoint, TxOut>,
) {
    let txid = transaction.txid();
    for (vout, output) in transaction.outputs.iter().enumerate() {
        known_outputs.insert(
            OutPoint {
                txid,
                vout: vout as u32,
            },
            output.clone(),
        );
    }
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
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        crypto,
        mempool::Mempool,
        peers::{PeerSource, StoredPeer},
        state::UtxoSet,
        tx_index::{IndexedTransaction, IndexedTransactionStatus, TransactionKeyRole},
        types::{
            Block, BlockHash, BlockHeader, ChainIndexEntry, OutPoint, Transaction, TxIn, TxOut,
            Txid, Utxo,
        },
    };

    use super::SqliteStore;

    static NEXT_TEMP_DB_ID: AtomicU64 = AtomicU64::new(0);

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

    fn mine_block_with_transactions(
        prev_blockhash: BlockHash,
        bits: u32,
        miner: &ed25519_dalek::VerifyingKey,
        uniqueness: u32,
        mut transactions: Vec<Transaction>,
    ) -> Block {
        let mut full_transactions = Vec::with_capacity(transactions.len() + 1);
        full_transactions.push(Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value: 50,
                locking_data: crypto::verifying_key_bytes(miner).to_vec(),
            }],
            lock_time: uniqueness,
        });
        full_transactions.append(&mut transactions);

        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions: full_transactions,
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
        let unique = NEXT_TEMP_DB_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!(
            "wobble-sqlite-test-{}-{}-{}.db",
            std::process::id(),
            nanos,
            unique,
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
    fn counts_persisted_chain_entries() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let first = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let first_hash = first.header.block_hash();
        let second = mine_block(first_hash, 0x207f_ffff, 1);
        let second_hash = second.header.block_hash();

        store
            .save_block_record(
                &first,
                &ChainIndexEntry {
                    block_hash: first_hash,
                    height: 0,
                    cumulative_work: 1,
                    parent: None,
                },
                Some(second_hash),
            )
            .unwrap();
        store
            .save_block_record(
                &second,
                &ChainIndexEntry {
                    block_hash: second_hash,
                    height: 1,
                    cumulative_work: 2,
                    parent: Some(first_hash),
                },
                Some(second_hash),
            )
            .unwrap();

        let count = store.count_chain_entries().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(count, 2);
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
    fn round_trips_persisted_peers() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let peers = vec![
            StoredPeer {
                addr: "127.0.0.1:9001".to_string(),
                node_name: Some("alpha".to_string()),
                source: PeerSource::Seed,
                advertised_tip_hash: Some(BlockHash::new([0x11; 32])),
                advertised_height: Some(7),
                last_hello_at: Some("2026-04-11T11:59:00Z".to_string()),
                last_seen_at: Some("2026-04-11T12:00:00Z".to_string()),
                last_connect_at: Some("2026-04-11T12:01:00Z".to_string()),
                last_success_at: Some("2026-04-11T12:02:00Z".to_string()),
                last_error: None,
                connections: 3,
                failed_connections: 1,
                behavior_score: 100,
                banned_until: None,
            },
            StoredPeer {
                addr: "127.0.0.1:9002".to_string(),
                node_name: None,
                source: PeerSource::Hello,
                advertised_tip_hash: None,
                advertised_height: None,
                last_hello_at: None,
                last_seen_at: None,
                last_connect_at: None,
                last_success_at: None,
                last_error: Some("timed out".to_string()),
                connections: 0,
                failed_connections: 2,
                behavior_score: 85,
                banned_until: Some("2026-04-12T00:00:00Z".to_string()),
            },
        ];

        store.save_peers(&peers).unwrap();

        let loaded = store.load_peers().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded, peers);
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

    #[test]
    fn read_only_open_loads_persisted_state() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let block = mine_block(BlockHash::default(), 0x207f_ffff, 17);
        let block_hash = block.header.block_hash();
        let mut state = crate::node_state::NodeState::new();
        state.accept_block(block).unwrap();
        store.save_node_state(&state).unwrap();
        drop(store);

        let read_only = SqliteStore::open_read_only(&path).unwrap();
        let rebuilt = read_only.load_node_state().unwrap();
        drop(read_only);
        fs::remove_file(&path).unwrap();

        assert_eq!(rebuilt.chain().best_tip(), Some(block_hash));
        assert_eq!(rebuilt.active_utxos().len(), state.active_utxos().len());
        assert_eq!(rebuilt.mempool().len(), state.mempool().len());
    }

    #[test]
    fn save_node_state_indexes_confirmed_and_mempool_transactions() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let miner = crypto::signing_key_from_bytes([3; 32]);

        let mut state = crate::node_state::NodeState::new();
        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            11,
            Vec::new(),
        );
        let genesis_txid = genesis.transactions[0].txid();
        state.accept_block(genesis).unwrap();

        let mut confirmed_payment = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: genesis_txid,
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 30,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 20,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 1,
        };
        confirmed_payment.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &confirmed_payment.signing_digest()).to_vec();

        let confirmed_block = mine_block_with_transactions(
            state.chain().best_tip().unwrap(),
            0x207f_ffff,
            &miner.verifying_key(),
            12,
            vec![confirmed_payment.clone()],
        );
        state.accept_block(confirmed_block).unwrap();

        let mut pending = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: confirmed_payment.txid(),
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 10,
                    locking_data: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 20,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
            ],
            lock_time: 2,
        };
        pending.inputs[0].unlocking_data =
            crypto::sign_message(&recipient, &pending.signing_digest()).to_vec();
        state.submit_transaction(pending.clone()).unwrap();

        store.save_node_state(&state).unwrap();

        let indexed = store.load_indexed_transactions().unwrap();
        let recipient_rows = store
            .load_transactions_for_keys(&[
                crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec()
            ])
            .unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(indexed.len(), 4);
        assert_eq!(indexed[0].status, IndexedTransactionStatus::Confirmed);
        assert_eq!(indexed[0].block_height, Some(0));
        assert_eq!(indexed[1].status, IndexedTransactionStatus::Confirmed);
        assert_eq!(indexed[1].block_height, Some(1));
        assert_eq!(indexed[2].txid, confirmed_payment.txid());
        assert_eq!(indexed[2].status, IndexedTransactionStatus::Confirmed);
        assert_eq!(indexed[2].block_height, Some(1));
        assert_eq!(indexed[3].txid, pending.txid());
        assert_eq!(indexed[3].status, IndexedTransactionStatus::Mempool);
        assert_eq!(indexed[3].block_height, None);
        assert_eq!(recipient_rows.len(), 2);
        assert_eq!(recipient_rows[0].txid, confirmed_payment.txid());
        assert_eq!(recipient_rows[1].txid, pending.txid());
    }

    #[test]
    fn save_node_state_indexes_only_best_chain_transactions() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([21; 32]);
        let recipient = crypto::signing_key_from_bytes([22; 32]);
        let miner_a = crypto::signing_key_from_bytes([23; 32]);
        let miner_b = crypto::signing_key_from_bytes([24; 32]);

        let mut state = crate::node_state::NodeState::new();
        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            41,
            Vec::new(),
        );
        let genesis_txid = genesis.transactions[0].txid();
        let genesis_hash = genesis.header.block_hash();
        state.accept_block(genesis).unwrap();

        let mut payment = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: genesis_txid,
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 30,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 20,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 42,
        };
        payment.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &payment.signing_digest()).to_vec();

        let branch_a = mine_block_with_transactions(
            genesis_hash,
            0x207f_ffff,
            &miner_a.verifying_key(),
            43,
            vec![payment.clone()],
        );
        let branch_a_hash = branch_a.header.block_hash();
        state.accept_block(branch_a).unwrap();

        let branch_b = mine_block_with_transactions(
            genesis_hash,
            0x201f_ffff,
            &miner_b.verifying_key(),
            44,
            vec![payment.clone()],
        );
        let branch_b_hash = branch_b.header.block_hash();
        state.accept_block(branch_b).unwrap();

        store.save_node_state(&state).unwrap();
        let indexed = store.load_indexed_transactions().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(state.chain().best_tip(), Some(branch_b_hash));
        assert_ne!(branch_a_hash, branch_b_hash);
        assert_eq!(indexed.len(), 3);
        assert_eq!(indexed[0].block_height, Some(0));
        assert_eq!(indexed[1].block_hash, Some(branch_b_hash));
        assert_eq!(indexed[2].txid, payment.txid());
        assert_eq!(indexed[2].block_hash, Some(branch_b_hash));
    }

    #[test]
    fn save_node_state_tolerates_duplicate_coinbase_txids_on_best_chain() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let miner = crypto::signing_key_from_bytes([41; 32]);
        let repeated_coinbase = Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value: 50,
                locking_data: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
            }],
            lock_time: 7,
        };

        let mut genesis = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::default(),
                merkle_root: [0; 32],
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: vec![repeated_coinbase.clone()],
        };
        genesis.header.merkle_root = genesis.merkle_root();
        while crate::consensus::validate_block(&genesis).is_err() {
            genesis.header.nonce = genesis.header.nonce.wrapping_add(1);
        }

        let mut child = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: genesis.header.block_hash(),
                merkle_root: [0; 32],
                time: 2,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: vec![repeated_coinbase],
        };
        child.header.merkle_root = child.merkle_root();
        while crate::consensus::validate_block(&child).is_err() {
            child.header.nonce = child.header.nonce.wrapping_add(1);
        }

        let duplicate_txid = genesis.transactions[0].txid();
        let mut state = crate::node_state::NodeState::new();
        state.accept_block(genesis).unwrap();
        state.accept_block(child).unwrap();

        store.save_node_state(&state).unwrap();
        let indexed = store.load_indexed_transactions().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].txid, duplicate_txid);
    }

    #[test]
    fn save_mempool_state_updates_mempool_and_transaction_index_without_blocks() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([51; 32]);
        let recipient = crypto::signing_key_from_bytes([52; 32]);
        let mut state = crate::node_state::NodeState::new();

        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            61,
            Vec::new(),
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        state.accept_block(genesis).unwrap();

        let mut pending = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: spendable,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 15,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 35,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 62,
        };
        pending.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &pending.signing_digest()).to_vec();
        state.submit_transaction(pending.clone()).unwrap();

        store.save_mempool_state(&state).unwrap();

        let reloaded_mempool = store.load_mempool().unwrap();
        let indexed = store.load_indexed_transactions().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(reloaded_mempool.get(&pending.txid()), Some(&pending));
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[0].block_height, Some(0));
        assert_eq!(indexed[1].txid, pending.txid());
        assert_eq!(indexed[1].status, IndexedTransactionStatus::Mempool);
    }

    #[test]
    fn save_mempool_transaction_loads_single_pending_transaction() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([91; 32]);
        let recipient = crypto::signing_key_from_bytes([92; 32]);

        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            93,
            Vec::new(),
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let mut state = crate::node_state::NodeState::new();
        state.accept_block(genesis).unwrap();
        store.save_node_state(&state).unwrap();

        let mut pending = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: spendable,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 10,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 40,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 94,
        };
        pending.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &pending.signing_digest()).to_vec();

        store.save_mempool_transaction(&pending).unwrap();

        let loaded = store.load_mempool_transaction(pending.txid()).unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded, Some(pending));
    }

    #[test]
    fn load_indexed_transaction_returns_saved_mempool_row() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([101; 32]);
        let recipient = crypto::signing_key_from_bytes([102; 32]);

        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            103,
            Vec::new(),
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let mut state = crate::node_state::NodeState::new();
        state.accept_block(genesis).unwrap();
        store.save_node_state(&state).unwrap();

        let mut pending = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: spendable,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 25,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 25,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 104,
        };
        pending.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &pending.signing_digest()).to_vec();

        store.save_mempool_transaction(&pending).unwrap();

        let indexed = store.load_indexed_transaction(pending.txid()).unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(indexed.as_ref().map(|row| row.txid), Some(pending.txid()));
        assert_eq!(
            indexed.as_ref().map(|row| row.status.clone()),
            Some(IndexedTransactionStatus::Mempool)
        );
        assert_eq!(indexed.as_ref().map(|row| row.block_hash), Some(None));
        assert_eq!(indexed.as_ref().map(|row| row.transaction.clone()), Some(pending));
    }

    #[test]
    fn save_accepted_block_updates_block_views_and_transaction_index() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([71; 32]);
        let recipient = crypto::signing_key_from_bytes([72; 32]);
        let miner = crypto::signing_key_from_bytes([73; 32]);
        let mut state = crate::node_state::NodeState::new();

        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            81,
            Vec::new(),
        );
        let genesis_hash = genesis.header.block_hash();
        let genesis_txid = genesis.transactions[0].txid();
        state.accept_block(genesis).unwrap();
        store.save_accepted_block(&state, genesis_hash).unwrap();

        let mut payment = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: genesis_txid,
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 20,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 30,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 82,
        };
        payment.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &payment.signing_digest()).to_vec();

        let child = mine_block_with_transactions(
            genesis_hash,
            0x207f_ffff,
            &miner.verifying_key(),
            83,
            vec![payment.clone()],
        );
        let child_hash = child.header.block_hash();
        state.accept_block(child).unwrap();

        store.save_accepted_block(&state, child_hash).unwrap();

        let stored_block = store.load_block(child_hash).unwrap();
        let stored_entry = store.load_chain_entry(child_hash).unwrap();
        let stored_tip = store.load_best_tip().unwrap();
        let reloaded_utxos = store.load_active_utxos().unwrap();
        let indexed = store.load_indexed_transactions().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert!(stored_block.is_some());
        assert_eq!(stored_entry.unwrap().block_hash, child_hash);
        assert_eq!(stored_tip, Some(child_hash));
        assert_eq!(reloaded_utxos.len(), state.active_utxos().len());
        assert_eq!(indexed.len(), 3);
        assert_eq!(indexed[2].txid, payment.txid());
        assert_eq!(indexed[2].block_hash, Some(child_hash));
        assert_eq!(indexed[2].status, IndexedTransactionStatus::Confirmed);
    }

    #[test]
    fn save_accepted_block_preserves_first_seen_at_for_confirmed_mempool_tx() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let sender = crypto::signing_key_from_bytes([81; 32]);
        let recipient = crypto::signing_key_from_bytes([82; 32]);
        let miner = crypto::signing_key_from_bytes([83; 32]);
        let mut state = crate::node_state::NodeState::new();

        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            91,
            Vec::new(),
        );
        let genesis_hash = genesis.header.block_hash();
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        state.accept_block(genesis).unwrap();
        store.save_accepted_block(&state, genesis_hash).unwrap();

        let mut payment = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: spendable,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 20,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 30,
                    locking_data: crypto::verifying_key_bytes(&sender.verifying_key()).to_vec(),
                },
            ],
            lock_time: 92,
        };
        payment.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &payment.signing_digest()).to_vec();
        let payment_txid = payment.txid();

        store
            .save_transaction_index(
                &[IndexedTransaction {
                    txid: payment_txid,
                    transaction: payment.clone(),
                    block_hash: None,
                    block_height: None,
                    position_in_block: None,
                    status: IndexedTransactionStatus::Mempool,
                    first_seen_at: Some("unix:123".to_string()),
                    last_updated_at: Some("unix:124".to_string()),
                }],
                &[],
            )
            .unwrap();

        let child = mine_block_with_transactions(
            genesis_hash,
            0x207f_ffff,
            &miner.verifying_key(),
            93,
            vec![payment],
        );
        let child_hash = child.header.block_hash();
        state.accept_block(child).unwrap();

        store.save_accepted_block(&state, child_hash).unwrap();

        let indexed = store.load_indexed_transaction(payment_txid).unwrap().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(indexed.block_hash, Some(child_hash));
        assert_eq!(indexed.status, IndexedTransactionStatus::Confirmed);
        assert_eq!(indexed.first_seen_at.as_deref(), Some("unix:123"));
    }

    #[test]
    fn saves_and_loads_peer_tip_metadata() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let peers = vec![StoredPeer {
            addr: "127.0.0.1:9001".to_string(),
            node_name: Some("seed".to_string()),
            source: PeerSource::Seed,
            advertised_tip_hash: Some(BlockHash::new([0x44; 32])),
            advertised_height: Some(17),
            last_hello_at: Some("2026-04-12T06:00:00Z".to_string()),
            last_seen_at: Some("2026-04-12T06:00:01Z".to_string()),
            last_connect_at: Some("2026-04-12T06:00:02Z".to_string()),
            last_success_at: Some("2026-04-12T06:00:03Z".to_string()),
            last_error: None,
            connections: 4,
            failed_connections: 1,
            behavior_score: 97,
            banned_until: None,
        }];

        store.save_peers(&peers).unwrap();
        let loaded = store.load_peers().unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded, peers);
    }

    #[test]
    fn save_peer_and_load_peer_round_trip() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let peer = StoredPeer {
            addr: "127.0.0.1:9101".to_string(),
            node_name: Some("alpha".to_string()),
            source: PeerSource::Hello,
            advertised_tip_hash: Some(BlockHash::new([0x55; 32])),
            advertised_height: Some(9),
            last_hello_at: Some("unix:1".to_string()),
            last_seen_at: Some("unix:2".to_string()),
            last_connect_at: Some("unix:3".to_string()),
            last_success_at: Some("unix:4".to_string()),
            last_error: None,
            connections: 2,
            failed_connections: 1,
            behavior_score: 99,
            banned_until: None,
        };

        store.save_peer(&peer).unwrap();
        let loaded = store.load_peer(&peer.addr).unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded, Some(peer));
    }

    #[test]
    fn load_transactions_for_keys_deduplicates_multi_key_wallet_matches() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let left = crypto::signing_key_from_bytes([10; 32]);
        let right = crypto::signing_key_from_bytes([11; 32]);
        let recipient = crypto::signing_key_from_bytes([12; 32]);

        let mut state = crate::node_state::NodeState::new();
        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &left.verifying_key(),
            31,
            Vec::new(),
        );
        let genesis_txid = genesis.transactions[0].txid();
        state.accept_block(genesis).unwrap();

        let mut payment = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: genesis_txid,
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 25,
                    locking_data: crypto::verifying_key_bytes(&right.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 25,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
            ],
            lock_time: 4,
        };
        payment.inputs[0].unlocking_data =
            crypto::sign_message(&left, &payment.signing_digest()).to_vec();
        state.submit_transaction(payment.clone()).unwrap();
        store.save_node_state(&state).unwrap();

        let wallet_rows = store
            .load_transactions_for_keys(&[
                crypto::verifying_key_bytes(&left.verifying_key()).to_vec(),
                crypto::verifying_key_bytes(&right.verifying_key()).to_vec(),
            ])
            .unwrap();
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(wallet_rows.len(), 2);
        assert_eq!(wallet_rows[0].txid, genesis_txid);
        assert_eq!(wallet_rows[1].txid, payment.txid());
    }

    #[test]
    fn transaction_key_index_tracks_input_and_output_roles() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let owner = crypto::signing_key_from_bytes([7; 32]);
        let recipient = crypto::signing_key_from_bytes([8; 32]);

        let mut state = crate::node_state::NodeState::new();
        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &owner.verifying_key(),
            21,
            Vec::new(),
        );
        let genesis_txid = genesis.transactions[0].txid();
        state.accept_block(genesis).unwrap();

        let mut spend = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: genesis_txid,
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value: 50,
                locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
            }],
            lock_time: 3,
        };
        spend.inputs[0].unlocking_data =
            crypto::sign_message(&owner, &spend.signing_digest()).to_vec();
        state.submit_transaction(spend.clone()).unwrap();
        store.save_node_state(&state).unwrap();

        let mut statement = store
            .connection
            .prepare(
                "SELECT key_role, key_data, value, vout
                 FROM transaction_keys
                 WHERE txid = ?1
                 ORDER BY key_role ASC, vout ASC NULLS LAST",
            )
            .unwrap();
        let rows = statement
            .query_map([spend.txid().to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(statement);
        drop(store);
        fs::remove_file(&path).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(
            TransactionKeyRole::parse(&rows[0].0),
            Some(TransactionKeyRole::Input)
        );
        assert_eq!(
            rows[0].1,
            crypto::verifying_key_bytes(&owner.verifying_key()).to_vec()
        );
        assert_eq!(rows[0].2, Some(50));
        assert_eq!(rows[0].3, Some(0));

        assert_eq!(
            TransactionKeyRole::parse(&rows[1].0),
            Some(TransactionKeyRole::Output)
        );
        assert_eq!(
            rows[1].1,
            crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec()
        );
        assert_eq!(rows[1].2, Some(50));
        assert_eq!(rows[1].3, Some(0));
    }
}
