//! In-memory chain index for linking validated blocks into competing branches.
//!
//! This module tracks parent pointers, heights, cumulative work, and the
//! current best tip. It does not own full UTXO state or perform reorgs yet.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    consensus::{self, ConsensusError, PowTarget},
    types::{Block, BlockHash, ChainIndexEntry},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    InvalidBlock(ConsensusError),
    MissingParent(BlockHash),
    GenesisHasParent,
    WorkOverflow,
}

/// Index of known blocks keyed by hash, with best-tip tracking by cumulative work.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainIndex {
    entries: HashMap<BlockHash, ChainIndexEntry>,
    best_tip: Option<BlockHash>,
}

impl ChainIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn best_tip(&self) -> Option<BlockHash> {
        self.best_tip
    }

    pub fn get(&self, hash: &BlockHash) -> Option<&ChainIndexEntry> {
        self.entries.get(hash)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&BlockHash, &ChainIndexEntry)> {
        self.entries.iter()
    }

    /// Returns how many known branch heads currently exist in the index.
    ///
    /// A branch head is any indexed block with no known child. This count is
    /// `1` for a simple single-chain view and grows when the node is tracking
    /// competing side branches.
    pub fn branch_count(&self) -> usize {
        let mut non_heads = std::collections::HashSet::new();
        for entry in self.entries.values() {
            if let Some(parent) = entry.parent {
                non_heads.insert(parent);
            }
        }
        self.entries
            .keys()
            .filter(|hash| !non_heads.contains(hash))
            .count()
    }

    /// Validates `block` in isolation, links it to its parent, and updates the
    /// best tip if the new branch has more cumulative work.
    pub fn insert_block(&mut self, block: &Block) -> Result<ChainIndexEntry, ChainError> {
        consensus::validate_block(block).map_err(ChainError::InvalidBlock)?;

        let block_hash = block.header.block_hash();
        if let Some(entry) = self.entries.get(&block_hash) {
            return Ok(entry.clone());
        }

        let parent_hash = block.header.prev_blockhash;
        let work = block_work(block)?;

        let (height, cumulative_work, parent) = if self.entries.is_empty() {
            if parent_hash != BlockHash::default() {
                return Err(ChainError::GenesisHasParent);
            }
            (0, work, None)
        } else {
            let parent = self
                .entries
                .get(&parent_hash)
                .ok_or(ChainError::MissingParent(parent_hash))?;
            let height = parent.height + 1;
            let cumulative_work = parent
                .cumulative_work
                .checked_add(work)
                .ok_or(ChainError::WorkOverflow)?;
            (height, cumulative_work, Some(parent_hash))
        };

        let entry = ChainIndexEntry {
            block_hash,
            height,
            cumulative_work,
            parent,
        };
        self.entries.insert(block_hash, entry.clone());
        self.update_best_tip(block_hash);

        Ok(entry)
    }

    fn update_best_tip(&mut self, candidate_hash: BlockHash) {
        let candidate = self
            .entries
            .get(&candidate_hash)
            .expect("candidate entry was inserted");

        let should_replace = match self.best_tip {
            None => true,
            Some(current_hash) => {
                let current = self
                    .entries
                    .get(&current_hash)
                    .expect("best tip must exist in entries");
                candidate.cumulative_work > current.cumulative_work
            }
        };

        if should_replace {
            self.best_tip = Some(candidate_hash);
        }
    }
}

/// Returns the amount of chain-selection work contributed by this block.
///
/// Work is derived from the expanded proof-of-work target and summed across a
/// branch to determine the best tip.
pub fn block_work(block: &Block) -> Result<u128, ChainError> {
    let target = PowTarget::from_compact(block.header.bits)
        .ok_or(ChainError::InvalidBlock(ConsensusError::InvalidPowTarget))?;
    Ok(target.work())
}

#[cfg(test)]
mod tests {
    use crate::types::{Block, BlockHash, BlockHeader, Transaction, TxOut};

    use super::{ChainError, ChainIndex};

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
    fn inserts_genesis_and_sets_best_tip() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let mut index = ChainIndex::new();

        let entry = index.insert_block(&genesis).unwrap();

        assert_eq!(entry.height, 0);
        assert_eq!(entry.parent, None);
        assert_eq!(index.best_tip(), Some(genesis_hash));
    }

    #[test]
    fn rejects_genesis_with_parent() {
        let genesis = mine_block(BlockHash::new([1; 32]), 0x207f_ffff, 0);
        let mut index = ChainIndex::new();

        assert_eq!(
            index.insert_block(&genesis),
            Err(ChainError::GenesisHasParent)
        );
    }

    #[test]
    fn rejects_missing_parent_for_non_genesis_block() {
        let block = mine_block(BlockHash::new([2; 32]), 0x207f_ffff, 1);
        let mut index = ChainIndex::new();
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        index.insert_block(&genesis).unwrap();

        assert_eq!(
            index.insert_block(&block),
            Err(ChainError::MissingParent(BlockHash::new([2; 32])))
        );
    }

    #[test]
    fn extends_chain_from_parent() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let child = mine_block(genesis_hash, 0x207f_ffff, 1);
        let child_hash = child.header.block_hash();
        let mut index = ChainIndex::new();

        index.insert_block(&genesis).unwrap();
        let entry = index.insert_block(&child).unwrap();

        assert_eq!(entry.height, 1);
        assert_eq!(entry.parent, Some(genesis_hash));
        assert_eq!(index.best_tip(), Some(child_hash));
    }

    #[test]
    fn prefers_branch_with_more_cumulative_work() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let easier = mine_block(genesis_hash, 0x207f_ffff, 1);
        let harder = mine_block(genesis_hash, 0x1f00ffff, 2);
        let harder_hash = harder.header.block_hash();
        let mut index = ChainIndex::new();

        index.insert_block(&genesis).unwrap();
        index.insert_block(&easier).unwrap();
        index.insert_block(&harder).unwrap();

        assert_eq!(index.best_tip(), Some(harder_hash));
    }

    #[test]
    fn counts_known_branch_heads() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let genesis_hash = genesis.header.block_hash();
        let child = mine_block(genesis_hash, 0x207f_ffff, 1);
        let sibling = mine_block(genesis_hash, 0x207f_ffff, 2);
        let mut index = ChainIndex::new();

        index.insert_block(&genesis).unwrap();
        assert_eq!(index.branch_count(), 1);

        index.insert_block(&child).unwrap();
        assert_eq!(index.branch_count(), 1);

        index.insert_block(&sibling).unwrap();
        assert_eq!(index.branch_count(), 2);
    }

    #[test]
    fn accepts_duplicate_inserts_idempotently() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        let mut index = ChainIndex::new();

        let first = index.insert_block(&genesis).unwrap();
        let second = index.insert_block(&genesis).unwrap();

        assert_eq!(first, second);
        assert_eq!(index.len(), 1);
    }
}
