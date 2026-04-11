//! In-memory pool of transactions waiting to be mined into a block.
//!
//! The mempool admits only transactions that are valid against the current
//! active UTXO set. If a new transaction conflicts with a pending transaction,
//! it may replace that transaction only when it spends exactly the same input
//! set and pays a strictly higher fee. It does not yet support package
//! acceptance or parent-child dependency chains, so every admitted transaction
//! must stand on its own against the active chain state.
//!
//! The pool also enforces a fixed transaction-count limit. When full, it keeps
//! the higher-fee transactions and evicts the current lowest-fee entry only if
//! the incoming transaction pays strictly more.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    state::{UtxoSet, ValidationError},
    types::{Amount, OutPoint, Transaction, Txid},
};

/// Maximum number of transactions retained in the mempool at once.
pub const MAX_MEMPOOL_TRANSACTIONS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MempoolError {
    DuplicateTransaction(Txid),
    ConflictingInput(OutPoint),
    ReplacementFeeTooLow { existing: u64, replacement: u64 },
    CapacityFeeTooLow { lowest: u64, candidate: u64 },
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

    pub fn iter(&self) -> impl Iterator<Item = (&Txid, &Transaction)> {
        self.transactions.iter()
    }

    /// Rebuilds a mempool from previously persisted transactions without
    /// reapplying admission policy.
    ///
    /// This is used only for trusted local persistence restore paths. Live
    /// network and CLI admission must continue to go through `submit`.
    pub fn from_persisted(transactions: Vec<(Txid, Transaction)>) -> Self {
        Self {
            transactions: transactions.into_iter().collect(),
        }
    }

    /// Validates `tx` against the current active UTXO state before admission.
    ///
    /// Admission rejects duplicate transaction ids, transactions that fail
    /// active-chain validation, and transactions with non-replaceable input
    /// conflicts against existing mempool entries.
    ///
    /// If a new transaction spends exactly the same inputs as an existing
    /// pending transaction, the higher-fee transaction wins and replaces the
    /// lower-fee one. Partial-overlap conflicts are still rejected because the
    /// mempool does not track dependency packages or more advanced policy.
    ///
    /// When the mempool is already at capacity, the incoming transaction must
    /// also pay a strictly higher fee than the current lowest-fee retained
    /// transaction; otherwise it is rejected.
    ///
    /// Gap: this does not yet admit dependent transactions whose parents live
    /// only in the mempool, and eviction is based only on direct transaction
    /// fees rather than package-aware policy.
    pub fn submit(&mut self, utxos: &UtxoSet, tx: Transaction) -> Result<Txid, MempoolError> {
        let txid = tx.txid();
        if self.transactions.contains_key(&txid) {
            return Err(MempoolError::DuplicateTransaction(txid));
        }

        let replacement_fee = utxos
            .transaction_fee(&tx)
            .map_err(MempoolError::InvalidTransaction)?;
        let conflicting_ids = self.conflicting_transactions(&tx);
        if !conflicting_ids.is_empty() {
            if !self.conflicts_are_replaceable(&tx, &conflicting_ids) {
                let outpoint = first_conflicting_input(&tx, &self.transactions)
                    .expect("conflicting transaction set should expose an input");
                return Err(MempoolError::ConflictingInput(outpoint));
            }

            let existing_fee = conflicting_ids
                .iter()
                .map(|existing_txid| {
                    let pending = self
                        .transactions
                        .get(existing_txid)
                        .expect("conflicting transaction must exist");
                    utxos
                        .transaction_fee(pending)
                        .expect("pending transactions should remain valid against active UTXOs")
                })
                .max()
                .expect("replaceable conflicts should include at least one transaction");
            if replacement_fee <= existing_fee {
                return Err(MempoolError::ReplacementFeeTooLow {
                    existing: existing_fee,
                    replacement: replacement_fee,
                });
            }
        }

        let displaced = self.capacity_eviction_candidate(utxos, &conflicting_ids);
        if let Some((evicted_txid, evicted_fee)) = displaced {
            if replacement_fee <= evicted_fee {
                return Err(MempoolError::CapacityFeeTooLow {
                    lowest: evicted_fee,
                    candidate: replacement_fee,
                });
            }
            self.transactions.remove(&evicted_txid);
        }

        self.remove_many(&conflicting_ids);

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

    fn conflicting_transactions(&self, tx: &Transaction) -> Vec<Txid> {
        self.transactions
            .iter()
            .filter(|(_, pending)| transactions_conflict(tx, pending))
            .map(|(txid, _)| *txid)
            .collect()
    }

    fn conflicts_are_replaceable(&self, tx: &Transaction, conflicting_ids: &[Txid]) -> bool {
        conflicting_ids.iter().all(|txid| {
            let pending = self
                .transactions
                .get(txid)
                .expect("conflicting transaction must exist");
            same_input_set(tx, pending)
        })
    }

    fn capacity_eviction_candidate(
        &self,
        utxos: &UtxoSet,
        conflicting_ids: &[Txid],
    ) -> Option<(Txid, Amount)> {
        let survivor_count = self
            .transactions
            .len()
            .saturating_sub(conflicting_ids.len());
        if survivor_count < MAX_MEMPOOL_TRANSACTIONS {
            return None;
        }

        self.transactions
            .iter()
            .filter(|(txid, _)| !conflicting_ids.contains(txid))
            .map(|(txid, tx)| {
                let fee = utxos
                    .transaction_fee(tx)
                    .expect("pending transactions should remain valid against active UTXOs");
                (*txid, fee)
            })
            .min_by_key(|(txid, fee)| (*fee, txid.to_string()))
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

    use super::{MAX_MEMPOOL_TRANSACTIONS, Mempool, MempoolError};

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

    fn tx_with_inputs(inputs: Vec<OutPoint>, value: u64, uniqueness: u32) -> Transaction {
        let mut tx = Transaction {
            version: 1,
            inputs: inputs
                .into_iter()
                .map(|previous_output| TxIn {
                    previous_output,
                    unlocking_data: Vec::new(),
                })
                .collect(),
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
        let signature = crypto::sign_message(&signing_key, &tx.signing_digest()).to_vec();
        for input in &mut tx.inputs {
            input.unlocking_data = signature.clone();
        }
        tx
    }

    fn indexed_txid(index: usize) -> Txid {
        let mut bytes = [0_u8; 32];
        bytes[0] = (index & 0xff) as u8;
        bytes[1] = ((index >> 8) & 0xff) as u8;
        Txid::new(bytes)
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
        let left_utxo = spendable_utxo(10);
        let right_utxo = Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x20; 32]),
                vout: 0,
            },
            output: left_utxo.output.clone(),
            created_at_height: 1,
            is_coinbase: false,
        };
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(left_utxo.clone());
        utxos.insert(right_utxo.clone());
        let mut mempool = Mempool::new();
        let left = tx(left_utxo.outpoint, 6, 1);
        let right = tx_with_inputs(vec![left_utxo.outpoint, right_utxo.outpoint], 12, 2);

        mempool.submit(&utxos, left.clone()).unwrap();

        assert_eq!(
            mempool.submit(&utxos, right.clone()),
            Err(MempoolError::ConflictingInput(left_utxo.outpoint))
        );
    }

    #[test]
    fn replaces_pending_transaction_with_higher_fee_on_same_inputs() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();
        let lower_fee = tx(utxo.outpoint, 9, 1);
        let higher_fee = tx(utxo.outpoint, 7, 2);

        mempool.submit(&utxos, lower_fee.clone()).unwrap();
        let replacement_id = mempool.submit(&utxos, higher_fee.clone()).unwrap();

        assert_eq!(replacement_id, higher_fee.txid());
        assert_eq!(mempool.len(), 1);
        assert!(mempool.get(&lower_fee.txid()).is_none());
        assert!(mempool.get(&higher_fee.txid()).is_some());
    }

    #[test]
    fn rejects_replacement_when_fee_is_not_higher() {
        let utxo = spendable_utxo(10);
        let mut utxos = crate::state::UtxoSet::new();
        utxos.insert(utxo.clone());
        let mut mempool = Mempool::new();
        let existing = tx(utxo.outpoint, 8, 1);
        let replacement = tx(utxo.outpoint, 8, 2);

        mempool.submit(&utxos, existing.clone()).unwrap();

        assert_eq!(
            mempool.submit(&utxos, replacement),
            Err(MempoolError::ReplacementFeeTooLow {
                existing: 2,
                replacement: 2,
            })
        );
        assert!(mempool.get(&existing.txid()).is_some());
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
    fn evicts_lowest_fee_transaction_when_full_and_candidate_pays_more() {
        let owner = crypto::signing_key_from_bytes([7; 32]);
        let recipient = crypto::signing_key_from_bytes([9; 32]);
        let mut utxos = crate::state::UtxoSet::new();
        let mut mempool = Mempool::new();
        let mut lowest_fee_txid = None;

        for index in 0..MAX_MEMPOOL_TRANSACTIONS {
            let outpoint = OutPoint {
                txid: indexed_txid(index),
                vout: 0,
            };
            let utxo = Utxo {
                outpoint,
                output: TxOut {
                    value: 100,
                    locking_data: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
                },
                created_at_height: 1,
                is_coinbase: false,
            };
            utxos.insert(utxo);

            // Fees range from 1 up to MAX_MEMPOOL_TRANSACTIONS.
            let mut tx = Transaction {
                version: 1,
                inputs: vec![TxIn {
                    previous_output: outpoint,
                    unlocking_data: Vec::new(),
                }],
                outputs: vec![TxOut {
                    value: 99_u64.saturating_sub(index as u64),
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                }],
                lock_time: index as u32,
            };
            tx.inputs[0].unlocking_data =
                crypto::sign_message(&owner, &tx.signing_digest()).to_vec();
            let txid = tx.txid();
            mempool.submit(&utxos, tx).unwrap();

            if index == 0 {
                lowest_fee_txid = Some(txid);
            }
        }

        let candidate_outpoint = OutPoint {
            txid: Txid::new([0xfe; 32]),
            vout: 0,
        };
        utxos.insert(Utxo {
            outpoint: candidate_outpoint,
            output: TxOut {
                value: 100,
                locking_data: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        });
        let mut candidate = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: candidate_outpoint,
                unlocking_data: Vec::new(),
            }],
            // Candidate fee is 50, which should displace the current floor fee of 1.
            outputs: vec![TxOut {
                value: 50,
                locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
            }],
            lock_time: u32::MAX,
        };
        candidate.inputs[0].unlocking_data =
            crypto::sign_message(&owner, &candidate.signing_digest()).to_vec();
        let candidate_id = candidate.txid();

        mempool.submit(&utxos, candidate).unwrap();

        assert_eq!(mempool.len(), MAX_MEMPOOL_TRANSACTIONS);
        assert!(
            mempool
                .get(&lowest_fee_txid.expect("lowest fee txid recorded"))
                .is_none()
        );
        assert!(mempool.get(&candidate_id).is_some());
    }

    #[test]
    fn rejects_candidate_when_full_and_fee_is_not_above_floor() {
        let owner = crypto::signing_key_from_bytes([7; 32]);
        let mut utxos = crate::state::UtxoSet::new();
        let mut mempool = Mempool::new();

        for index in 0..MAX_MEMPOOL_TRANSACTIONS {
            let outpoint = OutPoint {
                txid: indexed_txid(index),
                vout: 0,
            };
            utxos.insert(Utxo {
                outpoint,
                output: TxOut {
                    value: 10,
                    locking_data: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
                },
                created_at_height: 1,
                is_coinbase: false,
            });

            // Every retained transaction pays fee 1.
            let tx = tx(outpoint, 9, index as u32);
            mempool.submit(&utxos, tx).unwrap();
        }

        let candidate_outpoint = OutPoint {
            txid: Txid::new([0xfd; 32]),
            vout: 0,
        };
        utxos.insert(Utxo {
            outpoint: candidate_outpoint,
            output: TxOut {
                value: 10,
                locking_data: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        });
        let candidate = tx(candidate_outpoint, 9, u32::MAX);

        assert_eq!(
            mempool.submit(&utxos, candidate),
            Err(MempoolError::CapacityFeeTooLow {
                lowest: 1,
                candidate: 1,
            })
        );
        assert_eq!(mempool.len(), MAX_MEMPOOL_TRANSACTIONS);
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

fn transactions_conflict(left: &Transaction, right: &Transaction) -> bool {
    left.inputs.iter().any(|left_input| {
        right
            .inputs
            .iter()
            .any(|right_input| right_input.previous_output == left_input.previous_output)
    })
}

fn same_input_set(left: &Transaction, right: &Transaction) -> bool {
    left.inputs.len() == right.inputs.len()
        && left.inputs.iter().all(|left_input| {
            right
                .inputs
                .iter()
                .any(|right_input| right_input.previous_output == left_input.previous_output)
        })
}

fn first_conflicting_input(
    tx: &Transaction,
    pending: &HashMap<Txid, Transaction>,
) -> Option<OutPoint> {
    for input in &tx.inputs {
        if pending.values().any(|other| {
            other
                .inputs
                .iter()
                .any(|spent| spent.previous_output == input.previous_output)
        }) {
            return Some(input.previous_output);
        }
    }
    None
}
