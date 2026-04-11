//! Integrated in-memory node state for the active chain tip.
//!
//! This module combines the chain index and active UTXO set so a node can
//! accept blocks and advance the current best chain. It keeps an in-memory
//! UTXO snapshot per indexed block so the active view can switch across forks.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    chain::{ChainError, ChainIndex},
    consensus::{self, ConsensusError},
    crypto,
    mempool::{Mempool, MempoolError},
    state::{UtxoSet, ValidationError},
    types::{
        Amount, Block, BlockHash, BlockHeader, BlockHeight, OutPoint, Transaction, TxIn, TxOut,
        Txid,
    },
};
use ed25519_dalek::{SigningKey, VerifyingKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStateError {
    Chain(ChainError),
    Consensus(ConsensusError),
    ConsensusFee(ValidationError),
    Mempool(MempoolError),
    InsufficientFunds {
        requested: Amount,
        available: Amount,
    },
    MissingIndexedBlock(BlockHash),
    MissingParentState(BlockHash),
}

/// Tracks indexed blocks together with the active-chain UTXO view.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeState {
    chain: ChainIndex,
    active_utxos: UtxoSet,
    mempool: Mempool,
    blocks: HashMap<BlockHash, Block>,
    utxo_snapshots: HashMap<BlockHash, UtxoSet>,
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

    pub fn mempool(&self) -> &Mempool {
        &self.mempool
    }

    pub fn get_block(&self, hash: &BlockHash) -> Option<&Block> {
        self.blocks.get(hash)
    }

    pub fn active_outpoints(&self) -> Vec<OutPoint> {
        let mut outpoints: Vec<OutPoint> = self
            .active_utxos
            .iter()
            .map(|(outpoint, _)| *outpoint)
            .collect();
        outpoints.sort_by_key(|outpoint| (outpoint.txid.to_string(), outpoint.vout));
        outpoints
    }

    /// Returns the total active-chain balance locked to `owner`.
    pub fn balance_for_key(&self, owner: &VerifyingKey) -> Amount {
        let owner_bytes = crypto::verifying_key_bytes(owner);
        self.active_utxos
            .iter()
            .filter(|(_, utxo)| utxo.output.locking_data == owner_bytes)
            .map(|(_, utxo)| utxo.output.value)
            .sum()
    }

    /// Validates and queues a pending transaction against the active chain state.
    pub fn submit_transaction(&mut self, tx: Transaction) -> Result<Txid, NodeStateError> {
        self.mempool
            .submit(&self.active_utxos, tx)
            .map_err(NodeStateError::Mempool)
    }

    /// Builds a signed payment transaction by selecting active UTXOs locked to
    /// `sender_signing_key`, then submits it to the mempool.
    ///
    /// If the selected inputs exceed `amount`, a change output is created back
    /// to the sender's verifying key.
    pub fn submit_payment(
        &mut self,
        sender_signing_key: &SigningKey,
        recipient_verifying_key: &VerifyingKey,
        amount: Amount,
        uniqueness: u32,
    ) -> Result<Txid, NodeStateError> {
        let sender_verifying_key = sender_signing_key.verifying_key();
        let sender_locking_data = crypto::verifying_key_bytes(&sender_verifying_key).to_vec();
        let mut selected = Vec::new();
        let mut total = 0_u64;

        // Find active UTXOs controlled by the sender and gather enough value
        // to fund the requested payment plus any change output.
        for outpoint in self.active_outpoints() {
            let Some(utxo) = self.active_utxos.get(&outpoint) else {
                continue;
            };
            if utxo.output.locking_data != sender_locking_data {
                continue;
            }

            selected.push((outpoint, utxo.output.value));
            total = total.saturating_add(utxo.output.value);
            if total >= amount {
                break;
            }
        }

        if total < amount {
            return Err(NodeStateError::InsufficientFunds {
                requested: amount,
                available: total,
            });
        }

        let mut outputs = vec![TxOut {
            value: amount,
            locking_data: crypto::verifying_key_bytes(recipient_verifying_key).to_vec(),
        }];
        let change = total - amount;
        if change > 0 {
            outputs.push(TxOut {
                value: change,
                locking_data: sender_locking_data.clone(),
            });
        }

        let mut tx = Transaction {
            version: 1,
            inputs: selected
                .into_iter()
                .map(|(previous_output, _)| TxIn {
                    previous_output,
                    unlocking_data: Vec::new(),
                })
                .collect(),
            outputs,
            lock_time: uniqueness,
        };
        let signature = crypto::sign_message(sender_signing_key, &tx.signing_digest()).to_vec();
        for input in &mut tx.inputs {
            input.unlocking_data = signature.clone();
        }

        self.submit_transaction(tx)
    }

    /// Mines a block on the current best tip using the coinbase plus currently
    /// valid pending transactions, then accepts it into node state.
    pub fn mine_block(
        &mut self,
        subsidy: u64,
        miner_verifying_key: &VerifyingKey,
        uniqueness: u32,
        bits: u32,
        max_transactions: usize,
    ) -> Result<BlockHash, NodeStateError> {
        let prev = self.chain.best_tip().unwrap_or_default();
        let (pending, included_ids) = self
            .mempool
            .collect_valid(&self.active_utxos, max_transactions);
        let total_fees = pending
            .iter()
            .try_fold(0_u64, |total, tx| {
                self.active_utxos.transaction_fee(tx).and_then(|fee| {
                    total
                        .checked_add(fee)
                        .ok_or(ValidationError::InputValueOverflow)
                })
            })
            .map_err(NodeStateError::ConsensusFee)?;
        let block = mine_block_from_transactions(
            prev,
            subsidy + total_fees,
            miner_verifying_key,
            uniqueness,
            bits,
            pending,
        );
        let block_hash = block.header.block_hash();
        self.accept_block(block)?;
        self.mempool.remove_many(&included_ids);
        Ok(block_hash)
    }

    /// Indexes `block`, derives its UTXO state from its parent snapshot, and
    /// updates the active view if this block becomes the best tip.
    ///
    /// If the accepted block advances or replaces the active tip, the mempool
    /// is pruned afterward so any transactions invalidated by the new active
    /// chain are dropped.
    pub fn accept_block(&mut self, block: Block) -> Result<(), NodeStateError> {
        let entry = self
            .chain
            .insert_block(&block)
            .map_err(NodeStateError::Chain)?;
        let block_hash = entry.block_hash;
        self.blocks.entry(block_hash).or_insert(block);
        self.ensure_snapshot(block_hash, entry.parent, entry.height)?;

        if self.chain.best_tip() == Some(block_hash) {
            self.active_utxos = self
                .utxo_snapshots
                .get(&block_hash)
                .ok_or(NodeStateError::MissingParentState(block_hash))?
                .clone();
            // Revalidate pending transactions against the new active tip after
            // the UTXO view changes. This keeps stale spends from surviving a
            // newly accepted block or reorg.
            self.mempool.prune_invalid(&self.active_utxos);
        }

        Ok(())
    }

    /// Materializes the UTXO view for `block_hash` by cloning its parent's
    /// snapshot and applying the block at `height`.
    fn ensure_snapshot(
        &mut self,
        block_hash: BlockHash,
        parent: Option<BlockHash>,
        height: BlockHeight,
    ) -> Result<(), NodeStateError> {
        if self.utxo_snapshots.contains_key(&block_hash) {
            return Ok(());
        }

        let mut utxos = match parent {
            None => UtxoSet::new(),
            Some(parent_hash) => self
                .utxo_snapshots
                .get(&parent_hash)
                .ok_or(NodeStateError::MissingParentState(parent_hash))?
                .clone(),
        };

        let block = self
            .blocks
            .get(&block_hash)
            .ok_or(NodeStateError::MissingIndexedBlock(block_hash))?;
        consensus::apply_block(&mut utxos, block, height).map_err(NodeStateError::Consensus)?;
        self.utxo_snapshots.insert(block_hash, utxos);

        Ok(())
    }
}

fn coinbase_transaction(
    reward: u64,
    miner_verifying_key: &VerifyingKey,
    uniqueness: u32,
) -> Transaction {
    Transaction {
        version: 1,
        inputs: Vec::new(),
        outputs: vec![TxOut {
            value: reward,
            locking_data: crypto::verifying_key_bytes(miner_verifying_key).to_vec(),
        }],
        lock_time: uniqueness,
    }
}

fn mine_block_from_transactions(
    prev_blockhash: BlockHash,
    reward: u64,
    miner_verifying_key: &VerifyingKey,
    uniqueness: u32,
    bits: u32,
    mut transactions: Vec<Transaction>,
) -> Block {
    let mut full_transactions = Vec::with_capacity(transactions.len() + 1);
    full_transactions.push(coinbase_transaction(
        reward,
        miner_verifying_key,
        uniqueness,
    ));
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
        if consensus::validate_block(&block).is_ok() {
            return block;
        }
        block.header.nonce = block.header.nonce.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use crate::consensus::BLOCK_SUBSIDY;
    use crate::{
        crypto,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
    };
    use ed25519_dalek::{SigningKey, VerifyingKey};

    use super::{NodeState, NodeStateError};

    fn signing_key(seed: u8) -> SigningKey {
        crypto::signing_key_from_bytes([seed; 32])
    }

    fn coinbase(value: u64, owner: &VerifyingKey, uniqueness: u32) -> Transaction {
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(owner).to_vec(),
            }],
            lock_time: uniqueness,
        }
    }

    fn spend(
        previous_output: OutPoint,
        signer: &SigningKey,
        recipient: &VerifyingKey,
        value: u64,
        uniqueness: u32,
    ) -> Transaction {
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(recipient).to_vec(),
            }],
            lock_time: uniqueness,
        };
        tx.inputs[0].unlocking_data = crypto::sign_message(signer, &tx.signing_digest()).to_vec();
        tx
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
        let miner = signing_key(1);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &miner.verifying_key(), 0)],
        );
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
        let sender = signing_key(1);
        let recipient = signing_key(2);
        let miner = signing_key(3);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &sender.verifying_key(), 0)],
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let child = mine_block(
            genesis.header.block_hash(),
            0x207f_ffff,
            vec![
                coinbase(50, &miner.verifying_key(), 1),
                spend(spendable, &sender, &recipient.verifying_key(), 30, 2),
            ],
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
        let owner = signing_key(1);
        let alt = signing_key(2);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &owner.verifying_key(), 0)],
        );
        let genesis_hash = genesis.header.block_hash();
        let active_child = mine_block(
            genesis_hash,
            0x207f_ffff,
            vec![coinbase(50, &owner.verifying_key(), 1)],
        );
        let side_child = mine_block(
            genesis_hash,
            0x207f_ffff,
            vec![coinbase(50, &alt.verifying_key(), 2)],
        );
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
    fn reorgs_active_utxos_when_better_branch_arrives() {
        let owner = signing_key(1);
        let alt = signing_key(2);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &owner.verifying_key(), 0)],
        );
        let genesis_hash = genesis.header.block_hash();
        let easy_child = mine_block(
            genesis_hash,
            0x207f_ffff,
            vec![coinbase(50, &owner.verifying_key(), 1)],
        );
        let harder_child = mine_block(
            genesis_hash,
            0x1f00ffff,
            vec![coinbase(50, &alt.verifying_key(), 2)],
        );
        let harder_hash = harder_child.header.block_hash();
        let mut state = NodeState::new();

        state.accept_block(genesis).unwrap();
        state.accept_block(easy_child).unwrap();
        state.accept_block(harder_child.clone()).unwrap();

        assert_eq!(state.chain().best_tip(), Some(harder_hash));
        assert!(
            state
                .active_utxos()
                .get(&OutPoint {
                    txid: harder_child.transactions[0].txid(),
                    vout: 0
                })
                .is_some()
        );
    }

    #[test]
    fn mines_pending_transactions_from_mempool() {
        let sender = signing_key(1);
        let recipient = signing_key(2);
        let miner = signing_key(3);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &sender.verifying_key(), 0)],
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();

        state
            .submit_transaction(spend(spendable, &sender, &recipient.verifying_key(), 30, 2))
            .unwrap();
        let block_hash = state
            .mine_block(BLOCK_SUBSIDY, &miner.verifying_key(), 3, 0x207f_ffff, 100)
            .unwrap();
        let block = state
            .get_block(&block_hash)
            .expect("mined block is indexed");

        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.transactions[0].outputs[0].value, BLOCK_SUBSIDY + 20);
        assert!(state.active_utxos().get(&spendable).is_none());
        assert_eq!(state.mempool().len(), 0);
    }

    #[test]
    fn prunes_pending_transaction_spent_by_new_best_block() {
        let sender = signing_key(1);
        let recipient = signing_key(2);
        let rival = signing_key(3);
        let miner = signing_key(4);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &sender.verifying_key(), 0)],
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let competing_block = mine_block(
            genesis.header.block_hash(),
            0x207f_ffff,
            vec![
                coinbase(50, &miner.verifying_key(), 1),
                spend(spendable, &sender, &rival.verifying_key(), 45, 2),
            ],
        );
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();

        state
            .submit_transaction(spend(spendable, &sender, &recipient.verifying_key(), 30, 3))
            .unwrap();
        assert_eq!(state.mempool().len(), 1);

        // Once the best chain consumes the same outpoint, the pending spend is
        // no longer valid against the active UTXO set and should be pruned.
        state.accept_block(competing_block).unwrap();

        assert!(state.mempool().is_empty());
    }

    #[test]
    fn prunes_pending_transaction_after_reorg_replaces_its_input() {
        let owner = signing_key(1);
        let pending_recipient = signing_key(2);
        let active_branch_miner = signing_key(3);
        let alt_branch_miner = signing_key(4);
        let replacement_owner = signing_key(5);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &owner.verifying_key(), 0)],
        );
        let genesis_hash = genesis.header.block_hash();
        let easy_child = mine_block(
            genesis_hash,
            0x207f_ffff,
            vec![coinbase(50, &active_branch_miner.verifying_key(), 1)],
        );
        let harder_child = mine_block(
            genesis_hash,
            0x1f00ffff,
            vec![coinbase(50, &replacement_owner.verifying_key(), 2)],
        );
        let follow_on = mine_block(
            harder_child.header.block_hash(),
            0x1f00ffff,
            vec![coinbase(50, &alt_branch_miner.verifying_key(), 3)],
        );
        let easy_child_coinbase = OutPoint {
            txid: easy_child.transactions[0].txid(),
            vout: 0,
        };
        let mut state = NodeState::new();

        state.accept_block(genesis).unwrap();
        state.accept_block(easy_child).unwrap();
        state
            .submit_transaction(spend(
                easy_child_coinbase,
                &active_branch_miner,
                &pending_recipient.verifying_key(),
                40,
                4,
            ))
            .unwrap();
        assert_eq!(state.mempool().len(), 1);

        // The harder branch becomes the best tip and removes the easy-child
        // coinbase from the active chain, so the pending spend becomes stale.
        state.accept_block(harder_child).unwrap();
        state.accept_block(follow_on).unwrap();

        assert!(state.mempool().is_empty());
    }

    #[test]
    fn submits_payment_with_change_output() {
        let sender = signing_key(1);
        let recipient = signing_key(2);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &sender.verifying_key(), 0)],
        );
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();

        let txid = state
            .submit_payment(&sender, &recipient.verifying_key(), 30, 7)
            .unwrap();
        let tx = state
            .mempool()
            .get(&txid)
            .expect("submitted payment is in mempool");

        assert_eq!(tx.inputs.len(), 1);
        assert_eq!(tx.outputs.len(), 2);
        assert_eq!(tx.outputs[0].value, 30);
        assert_eq!(tx.outputs[1].value, 20);
    }

    #[test]
    fn rejects_payment_when_sender_funds_are_insufficient() {
        let sender = signing_key(1);
        let recipient = signing_key(2);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &sender.verifying_key(), 0)],
        );
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();

        assert_eq!(
            state.submit_payment(&recipient, &sender.verifying_key(), 30, 7),
            Err(NodeStateError::InsufficientFunds {
                requested: 30,
                available: 0,
            })
        );
    }

    #[test]
    fn reports_balance_for_public_key() {
        let sender = signing_key(1);
        let recipient = signing_key(2);
        let miner = signing_key(3);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            vec![coinbase(50, &sender.verifying_key(), 0)],
        );
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state
            .submit_payment(&sender, &recipient.verifying_key(), 30, 1)
            .unwrap();
        state
            .mine_block(BLOCK_SUBSIDY, &miner.verifying_key(), 2, 0x207f_ffff, 100)
            .unwrap();

        assert_eq!(state.balance_for_key(&sender.verifying_key()), 20);
        assert_eq!(state.balance_for_key(&recipient.verifying_key()), 30);
        assert_eq!(state.balance_for_key(&miner.verifying_key()), 50);
    }
}
