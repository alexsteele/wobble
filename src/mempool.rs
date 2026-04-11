//! In-memory pool of transactions waiting to be mined into a block.
//!
//! The mempool admits only transactions that are valid against the current
//! active UTXO set and that do not conflict with other pending transactions by
//! spending the same input. It does not yet support package acceptance or
//! parent-child dependency chains, so every admitted transaction must stand on
//! its own against the active chain state.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    state::{UtxoSet, ValidationError},
    types::{OutPoint, Transaction, Txid},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MempoolError {
    DuplicateTransaction(Txid),
    ConflictingInput(OutPoint),
    InvalidTransaction(ValidationError),
}

/// Pending transactions keyed by transaction id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mempool {
    transactions: HashMap<Txid, Transaction>,
}

impl Mempool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    pub fn get(&self, txid: &Txid) -> Option<&Transaction> {
        self.transactions.get(txid)
    }

    /// Validates `tx` against the current active UTXO state before admission.
    ///
    /// Admission rejects duplicate transaction ids, transactions that fail
    /// active-chain validation, and transactions whose inputs are already
    /// claimed by another pending transaction.
    ///
    /// Gap: this does not yet admit dependent transactions whose parents live
    /// only in the mempool.
    pub fn submit(&mut self, utxos: &UtxoSet, tx: Transaction) -> Result<Txid, MempoolError> {
        let txid = tx.txid();
        if self.transactions.contains_key(&txid) {
            return Err(MempoolError::DuplicateTransaction(txid));
        }

        utxos
            .validate_transaction(&tx)
            .map_err(MempoolError::InvalidTransaction)?;
        for input in &tx.inputs {
            if self.transactions.values().any(|pending| {
                pending
                    .inputs
                    .iter()
                    .any(|spent| spent.previous_output == input.previous_output)
            }) {
                return Err(MempoolError::ConflictingInput(input.previous_output));
            }
        }
        self.transactions.insert(txid, tx);
        Ok(txid)
    }

    /// Selects transactions that remain valid when applied in order.
    ///
    /// This is a defensive filter for stale entries after chain movement.
    /// Admission-time conflict checks should normally prevent multiple pending
    /// transactions from claiming the same active-chain input.
    pub fn collect_valid(
        &self,
        utxos: &UtxoSet,
        max_count: usize,
    ) -> (Vec<Transaction>, Vec<Txid>) {
        let mut selected = Vec::new();
        let mut selected_ids = Vec::new();
        let mut candidate = utxos.clone();

        let mut entries: Vec<(Txid, Transaction)> = self
            .transactions
            .iter()
            .map(|(txid, tx)| (*txid, tx.clone()))
            .collect();
        entries.sort_by_key(|(txid, _)| txid.to_string());

        for (txid, tx) in entries {
            if selected.len() >= max_count {
                break;
            }

            if candidate.apply_transaction(&tx, 0).is_ok() {
                selected.push(tx);
                selected_ids.push(txid);
            }
        }

        (selected, selected_ids)
    }

    /// Drops transactions that no longer validate against the active UTXO set.
    ///
    /// Newly accepted blocks and reorgs can make pending transactions stale by
    /// consuming or replacing the UTXOs they referenced. This pass removes
    /// those stale entries so the mempool continues to reflect the active tip.
    ///
    /// Gap: because package dependencies are not modeled yet, pruning only
    /// considers direct validity against the active chain.
    pub fn prune_invalid(&mut self, utxos: &UtxoSet) {
        self.transactions
            .retain(|_, tx| utxos.validate_transaction(tx).is_ok());
    }

    pub fn remove_many(&mut self, txids: &[Txid]) {
        for txid in txids {
            self.transactions.remove(txid);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::crypto;
    use crate::types::{OutPoint, Transaction, TxIn, TxOut, Txid, Utxo};

    use super::{Mempool, MempoolError};

    fn spendable_utxo(value: u64) -> Utxo {
        let signing_key = crypto::signing_key_from_bytes([7; 32]);
        Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 0,
            },
            output: TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(&signing_key.verifying_key()).to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        }
    }

    fn tx(previous_output: OutPoint, value: u64, uniqueness: u32) -> Transaction {
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([9; 32]).verifying_key(),
                )
                .to_vec(),
            }],
            lock_time: uniqueness,
        };
        let signing_key = crypto::signing_key_from_bytes([7; 32]);
        tx.inputs[0].unlocking_data =
            crypto::sign_message(&signing_key, &tx.signing_digest()).to_vec();
        tx
    }

    #[test]
    fn admits_valid_transaction() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();

        let tx = tx(utxo.outpoint, 10, 1);
        let txid = mempool.submit(&utxos, tx.clone()).unwrap();

        assert_eq!(txid, tx.txid());
        assert_eq!(mempool.len(), 1);
    }

    #[test]
    fn rejects_duplicate_transaction() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();
        let tx = tx(utxo.outpoint, 10, 1);

        mempool.submit(&utxos, tx.clone()).unwrap();

        assert_eq!(
            mempool.submit(&utxos, tx.clone()),
            Err(MempoolError::DuplicateTransaction(tx.txid()))
        );
    }

    #[test]
    fn rejects_transaction_that_conflicts_with_pending_input() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();
        let left = tx(utxo.outpoint, 6, 1);
        let right = tx(utxo.outpoint, 6, 2);

        mempool.submit(&utxos, left.clone()).unwrap();

        assert_eq!(
            mempool.submit(&utxos, right.clone()),
            Err(MempoolError::ConflictingInput(utxo.outpoint))
        );
    }

    #[test]
    fn prunes_transaction_invalidated_by_updated_utxos() {
        let utxo = spendable_utxo(10);
        let mut before = crate::state::UtxoSet::new();
        before.insert(utxo.clone());
        let after = crate::state::UtxoSet::new();
        let mut mempool = Mempool::new();
        let pending = tx(utxo.outpoint, 8, 1);

        mempool.submit(&before, pending).unwrap();
        assert_eq!(mempool.len(), 1);

        // The referenced outpoint no longer exists in the updated active UTXO
        // view, so the mempool should discard this stale spend.
        mempool.prune_invalid(&after);

        assert!(mempool.is_empty());
    }

    #[test]
    fn collects_only_transactions_that_remain_valid_together() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();
        let left = tx(utxo.outpoint, 6, 1);
        let right = tx(utxo.outpoint, 6, 2);

        // This test bypasses normal submit() so collect_valid() still covers
        // the stale-conflict case defensively.
        mempool.transactions.insert(left.txid(), left.clone());
        mempool.transactions.insert(right.txid(), right.clone());

        let (selected, selected_ids) = mempool.collect_valid(&utxos, usize::MAX);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected_ids.len(), 1);
    }
}
