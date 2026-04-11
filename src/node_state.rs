//! Integrated in-memory node state for the active chain tip.
//!
//! This module combines the chain index and active UTXO set so a node can
//! accept blocks and advance the current best chain. Side branches are indexed,
//! but reorging UTXO state onto a new best branch is not implemented yet.

use std::collections::HashMap;

use crate::{
    chain::{ChainError, ChainIndex},
    consensus::{self, ConsensusError},
    state::UtxoSet,
    types::{Block, BlockHash},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStateError {
    Chain(ChainError),
    Consensus(ConsensusError),
    MissingIndexedBlock(BlockHash),
    ReorgRequired {
        from: Option<BlockHash>,
        to: BlockHash,
    },
}

/// Tracks indexed blocks together with the active-chain UTXO view.
#[derive(Debug, Clone, Default)]
pub struct NodeState {
    chain: ChainIndex,
    active_utxos: UtxoSet,
    blocks: HashMap<BlockHash, Block>,
}

impl NodeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn chain(&self) -> &ChainIndex {
        &self.chain
    }

    pub fn active_utxos(&self) -> &UtxoSet {
        &self.active_utxos
    }

    pub fn get_block(&self, hash: &BlockHash) -> Option<&Block> {
        self.blocks.get(hash)
    }

    /// Indexes `block` and applies it to active state if it extends the current best tip.
    ///
    /// If the block would make a side branch become best, the chain metadata is updated
    /// but UTXO reorging is deferred and reported as `ReorgRequired`.
    pub fn accept_block(&mut self, block: Block) -> Result<(), NodeStateError> {
        let previous_best = self.chain.best_tip();
        let entry = self
            .chain
            .insert_block(&block)
            .map_err(NodeStateError::Chain)?;
        let block_hash = entry.block_hash;
        self.blocks.entry(block_hash).or_insert(block);

        let new_best = self.chain.best_tip();
        if new_best != Some(block_hash) {
            return Ok(());
        }

        match previous_best {
            None => {
                let best_block = self
                    .blocks
                    .get(&block_hash)
                    .ok_or(NodeStateError::MissingIndexedBlock(block_hash))?;
                consensus::apply_block(&mut self.active_utxos, best_block, entry.height)
                    .map_err(NodeStateError::Consensus)?;
                Ok(())
            }
            Some(previous_tip) if entry.parent == Some(previous_tip) => {
                let best_block = self
                    .blocks
                    .get(&block_hash)
                    .ok_or(NodeStateError::MissingIndexedBlock(block_hash))?;
                consensus::apply_block(&mut self.active_utxos, best_block, entry.height)
                    .map_err(NodeStateError::Consensus)?;
                Ok(())
            }
            _ => Err(NodeStateError::ReorgRequired {
                from: previous_best,
                to: block_hash,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut};

    use super::{NodeState, NodeStateError};

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

    fn spend(previous_output: OutPoint, value: u64, uniqueness: u32) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: vec![uniqueness as u8],
            }],
            outputs: vec![TxOut {
                value,
                locking_data: vec![0x52],
            }],
            lock_time: uniqueness,
        }
    }

    fn mine_block(prev_blockhash: BlockHash, bits: u32, transactions: Vec<Transaction>) -> Block {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions,
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
    fn accepts_genesis_and_populates_utxos() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, vec![coinbase(50, 0)]);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();

        state.accept_block(genesis.clone()).unwrap();

        assert_eq!(state.chain().best_tip(), Some(genesis_hash));
        assert!(
            state
                .active_utxos()
                .get(&OutPoint {
                    txid: genesis.transactions[0].txid(),
                    vout: 0
                })
                .is_some()
        );
    }

    #[test]
    fn accepts_direct_extension_of_best_tip() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, vec![coinbase(50, 0)]);
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let child = mine_block(
            genesis.header.block_hash(),
            0x207f_ffff,
            vec![coinbase(50, 1), spend(spendable, 30, 2)],
        );
        let mut state = NodeState::new();

        state.accept_block(genesis).unwrap();
        state.accept_block(child.clone()).unwrap();

        assert!(state.active_utxos().get(&spendable).is_none());
        assert!(
            state
                .active_utxos()
                .get(&OutPoint {
                    txid: child.transactions[1].txid(),
                    vout: 0
                })
                .is_some()
        );
    }

    #[test]
    fn indexes_side_branch_without_mutating_active_utxos() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, vec![coinbase(50, 0)]);
        let genesis_hash = genesis.header.block_hash();
        let active_child = mine_block(genesis_hash, 0x207f_ffff, vec![coinbase(50, 1)]);
        let side_child = mine_block(genesis_hash, 0x207f_ffff, vec![coinbase(50, 2)]);
        let active_tip_hash = active_child.header.block_hash();
        let side_tip_hash = side_child.header.block_hash();
        let mut state = NodeState::new();

        state.accept_block(genesis).unwrap();
        state.accept_block(active_child).unwrap();
        let before = format!("{:?}", state.active_utxos());

        state.accept_block(side_child).unwrap();

        assert_eq!(state.chain().best_tip(), Some(active_tip_hash));
        assert!(state.get_block(&side_tip_hash).is_some());
        assert_eq!(format!("{:?}", state.active_utxos()), before);
    }

    #[test]
    fn reports_when_reorg_would_be_required() {
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, vec![coinbase(50, 0)]);
        let genesis_hash = genesis.header.block_hash();
        let easy_child = mine_block(genesis_hash, 0x207f_ffff, vec![coinbase(50, 1)]);
        let harder_child = mine_block(genesis_hash, 0x1f00ffff, vec![coinbase(50, 2)]);
        let easy_hash = easy_child.header.block_hash();
        let harder_hash = harder_child.header.block_hash();
        let mut state = NodeState::new();

        state.accept_block(genesis).unwrap();
        state.accept_block(easy_child).unwrap();

        assert_eq!(
            state.accept_block(harder_child),
            Err(NodeStateError::ReorgRequired {
                from: Some(easy_hash),
                to: harder_hash,
            })
        );
    }
}
