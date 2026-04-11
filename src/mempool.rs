//! In-memory pool of transactions waiting to be mined into a block.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    state::{UtxoSet, ValidationError},
    types::{Transaction, Txid},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MempoolError {
    DuplicateTransaction(Txid),
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
    pub fn submit(&mut self, utxos: &UtxoSet, tx: Transaction) -> Result<Txid, MempoolError> {
        let txid = tx.txid();
        if self.transactions.contains_key(&txid) {
            return Err(MempoolError::DuplicateTransaction(txid));
        }

        utxos
            .validate_transaction(&tx)
            .map_err(MempoolError::InvalidTransaction)?;
        self.transactions.insert(txid, tx);
        Ok(txid)
    }

    /// Selects transactions that remain valid when applied in order.
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

    pub fn remove_many(&mut self, txids: &[Txid]) {
        for txid in txids {
            self.transactions.remove(txid);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::{OutPoint, Transaction, TxIn, TxOut, Txid, Utxo};

    use super::{Mempool, MempoolError};

    fn spendable_utxo(value: u64) -> Utxo {
        Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 0,
            },
            output: TxOut {
                value,
                locking_data: vec![0x51],
            },
            created_at_height: 1,
            is_coinbase: false,
        }
    }

    fn tx(previous_output: OutPoint, value: u64, uniqueness: u32) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: uniqueness.to_le_bytes().to_vec(),
            }],
            outputs: vec![TxOut {
                value,
                locking_data: vec![0x52],
            }],
            lock_time: uniqueness,
        }
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
    fn collects_only_transactions_that_remain_valid_together() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();
        let left = tx(utxo.outpoint, 6, 1);
        let right = tx(utxo.outpoint, 6, 2);

        mempool.submit(&utxos, left.clone()).unwrap();
        mempool.submit(&utxos, right.clone()).unwrap();

        let (selected, selected_ids) = mempool.collect_valid(&utxos, usize::MAX);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected_ids.len(), 1);
    }
}
