//! In-memory chain state built from the current unspent transaction outputs.
//!
//! This module validates transactions against the UTXO set and applies their
//! state transitions by consuming spent outputs and creating new ones.
//! It does not validate proof-of-work, block hashes, merkle roots, or fork
//! choice; those belong in a higher-level consensus layer.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{
    crypto,
    types::{Amount, BlockHeight, OutPoint, Transaction, TxOut, Utxo},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    EmptyOutputs,
    MissingInput(OutPoint),
    DuplicateInput(OutPoint),
    InvalidLockingData(OutPoint),
    InvalidUnlockingData(OutPoint),
    SignatureMismatch(OutPoint),
    OutputValueOverflow,
    InputValueOverflow,
    Overspend {
        input_value: Amount,
        output_value: Amount,
    },
}

/// In-memory view of the current unspent transaction outputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UtxoSet {
    entries: HashMap<OutPoint, Utxo>,
}

impl UtxoSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, outpoint: &OutPoint) -> Option<&Utxo> {
        self.entries.get(outpoint)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&OutPoint, &Utxo)> {
        self.entries.iter()
    }

    pub fn insert(&mut self, utxo: Utxo) -> Option<Utxo> {
        self.entries.insert(utxo.outpoint, utxo)
    }

    pub fn validate_transaction(&self, tx: &Transaction) -> Result<(), ValidationError> {
        self.validate_transaction_inner(tx).map(|_| ())
    }

    /// Returns the fee paid by `tx` after validating its inputs and outputs.
    ///
    /// For non-coinbase transactions, the fee is the total referenced input
    /// value minus the total output value. Any value not assigned to outputs is
    /// implicitly paid to the miner as a transaction fee.
    pub fn transaction_fee(&self, tx: &Transaction) -> Result<Amount, ValidationError> {
        let validation = self.validate_transaction_inner(tx)?;
        Ok(validation.fee)
    }

    /// Validates `tx`, removes any spent inputs, and inserts its newly created outputs.
    pub fn apply_transaction(
        &mut self,
        tx: &Transaction,
        height: BlockHeight,
    ) -> Result<(), ValidationError> {
        let validation = self.validate_transaction_inner(tx)?;

        if !tx.is_coinbase() {
            for outpoint in validation.spent_inputs {
                self.entries.remove(&outpoint);
            }
        }

        let txid = tx.txid();
        for (vout, output) in tx.outputs.iter().cloned().enumerate() {
            let vout = u32::try_from(vout).expect("output index exceeds u32");
            let utxo = Utxo {
                outpoint: OutPoint { txid, vout },
                output,
                created_at_height: height,
                is_coinbase: tx.is_coinbase(),
            };
            self.entries.insert(utxo.outpoint, utxo);
        }

        Ok(())
    }

    fn validate_transaction_inner(
        &self,
        tx: &Transaction,
    ) -> Result<ValidationResult, ValidationError> {
        // Reject transactions that create no new spendable outputs.
        if tx.outputs.is_empty() {
            return Err(ValidationError::EmptyOutputs);
        }

        let output_value = sum_outputs(&tx.outputs)?;

        // Coinbase creates value directly and does not consume prior UTXOs.
        if tx.is_coinbase() {
            return Ok(ValidationResult {
                spent_inputs: Vec::new(),
                fee: 0,
            });
        }

        let mut seen_inputs = HashSet::new();
        let mut input_value = 0_u64;
        let signing_digest = tx.signing_digest();

        // Resolve each referenced input exactly once and accumulate its value.
        for input in &tx.inputs {
            let outpoint = input.previous_output;
            if !seen_inputs.insert(outpoint) {
                return Err(ValidationError::DuplicateInput(outpoint));
            }

            let utxo = self
                .entries
                .get(&outpoint)
                .ok_or(ValidationError::MissingInput(outpoint))?;

            let verifying_key = crypto::parse_verifying_key(&utxo.output.locking_data)
                .ok_or(ValidationError::InvalidLockingData(outpoint))?;
            let signature = crypto::parse_signature(&input.unlocking_data)
                .ok_or(ValidationError::InvalidUnlockingData(outpoint))?;
            if !crypto::verify_message(&signature, &verifying_key, &signing_digest) {
                return Err(ValidationError::SignatureMismatch(outpoint));
            }

            input_value = input_value
                .checked_add(utxo.output.value)
                .ok_or(ValidationError::InputValueOverflow)?;
        }

        // Enforce conservation of value for non-coinbase transactions.
        if input_value < output_value {
            return Err(ValidationError::Overspend {
                input_value,
                output_value,
            });
        }

        Ok(ValidationResult {
            spent_inputs: tx
                .inputs
                .iter()
                .map(|input| input.previous_output)
                .collect(),
            fee: input_value - output_value,
        })
    }
}

struct ValidationResult {
    spent_inputs: Vec<OutPoint>,
    fee: Amount,
}

fn sum_outputs(outputs: &[TxOut]) -> Result<Amount, ValidationError> {
    let mut total = 0_u64;
    for output in outputs {
        total = total
            .checked_add(output.value)
            .ok_or(ValidationError::OutputValueOverflow)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use crate::{
        crypto,
        types::{OutPoint, Transaction, TxIn, TxOut, Txid, Utxo},
    };

    use super::{UtxoSet, ValidationError};

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

    fn spending_transaction(input: OutPoint, outputs: Vec<TxOut>) -> Transaction {
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: input,
                unlocking_data: Vec::new(),
            }],
            outputs,
            lock_time: 0,
        };
        let signing_key = crypto::signing_key_from_bytes([7; 32]);
        let signature = crypto::sign_message(&signing_key, &tx.signing_digest());
        tx.inputs[0].unlocking_data = signature.to_vec();
        tx
    }

    #[test]
    fn validates_and_applies_standard_transaction() {
        let utxo = spendable_utxo(50);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());

        let tx = spending_transaction(
            utxo.outpoint,
            vec![
                TxOut {
                    value: 30,
                    locking_data: vec![0x52],
                },
                TxOut {
                    value: 20,
                    locking_data: vec![0x53],
                },
            ],
        );

        assert_eq!(set.transaction_fee(&tx).unwrap(), 0);
        set.apply_transaction(&tx, 2).unwrap();

        assert!(set.get(&utxo.outpoint).is_none());
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn rejects_missing_input() {
        let set = UtxoSet::new();
        let missing = OutPoint {
            txid: Txid::new([0x99; 32]),
            vout: 0,
        };
        let tx = spending_transaction(
            missing,
            vec![TxOut {
                value: 1,
                locking_data: vec![0x51],
            }],
        );

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::MissingInput(missing))
        );
    }

    #[test]
    fn rejects_empty_outputs() {
        let utxo = spendable_utxo(10);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());

        let tx = spending_transaction(utxo.outpoint, Vec::new());

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::EmptyOutputs)
        );
    }

    #[test]
    fn rejects_duplicate_inputs() {
        let utxo = spendable_utxo(10);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());

        let mut tx = Transaction {
            version: 1,
            inputs: vec![
                TxIn {
                    previous_output: utxo.outpoint,
                    unlocking_data: Vec::new(),
                },
                TxIn {
                    previous_output: utxo.outpoint,
                    unlocking_data: Vec::new(),
                },
            ],
            outputs: vec![TxOut {
                value: 10,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([9; 32]).verifying_key(),
                )
                .to_vec(),
            }],
            lock_time: 0,
        };
        let signing_key = crypto::signing_key_from_bytes([7; 32]);
        let signature = crypto::sign_message(&signing_key, &tx.signing_digest()).to_vec();
        tx.inputs[0].unlocking_data = signature.clone();
        tx.inputs[1].unlocking_data = signature;

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::DuplicateInput(utxo.outpoint))
        );
    }

    #[test]
    fn rejects_overspend() {
        let utxo = spendable_utxo(10);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());

        let tx = spending_transaction(
            utxo.outpoint,
            vec![TxOut {
                value: 11,
                locking_data: vec![0x51],
            }],
        );

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::Overspend {
                input_value: 10,
                output_value: 11,
            })
        );
    }

    #[test]
    fn reports_transaction_fee() {
        let utxo = spendable_utxo(10);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());

        let tx = spending_transaction(
            utxo.outpoint,
            vec![TxOut {
                value: 7,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([9; 32]).verifying_key(),
                )
                .to_vec(),
            }],
        );

        assert_eq!(set.transaction_fee(&tx).unwrap(), 3);
    }

    #[test]
    fn reports_transaction_fee_for_multiple_inputs() {
        let owner = crypto::signing_key_from_bytes([7; 32]);
        // Two inputs contribute 10 + 15 units of spendable value.
        let left = Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 0,
            },
            output: TxOut {
                value: 10,
                locking_data: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        };
        let right = Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x20; 32]),
                vout: 0,
            },
            output: TxOut {
                value: 15,
                locking_data: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        };
        let recipient = crypto::signing_key_from_bytes([9; 32]);
        let change_owner = crypto::signing_key_from_bytes([11; 32]);
        let mut set = UtxoSet::new();
        set.insert(left.clone());
        set.insert(right.clone());

        let mut tx = Transaction {
            version: 1,
            inputs: vec![
                TxIn {
                    previous_output: left.outpoint,
                    unlocking_data: Vec::new(),
                },
                TxIn {
                    previous_output: right.outpoint,
                    unlocking_data: Vec::new(),
                },
            ],
            // Outputs assign 20 units to the recipient and 3 back as change.
            outputs: vec![
                TxOut {
                    value: 20,
                    locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
                },
                TxOut {
                    value: 3,
                    locking_data: crypto::verifying_key_bytes(&change_owner.verifying_key())
                        .to_vec(),
                },
            ],
            lock_time: 0,
        };
        let signature = crypto::sign_message(&owner, &tx.signing_digest()).to_vec();
        tx.inputs[0].unlocking_data = signature.clone();
        tx.inputs[1].unlocking_data = signature;

        // Fee = total inputs (25) - total outputs (23) = 2.
        assert_eq!(set.transaction_fee(&tx).unwrap(), 2);
    }

    #[test]
    fn rejects_output_value_overflow() {
        let utxo = spendable_utxo(u64::MAX);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());

        let tx = spending_transaction(
            utxo.outpoint,
            vec![
                TxOut {
                    value: u64::MAX,
                    locking_data: vec![0x51],
                },
                TxOut {
                    value: 1,
                    locking_data: vec![0x52],
                },
            ],
        );

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::OutputValueOverflow)
        );
    }

    #[test]
    fn rejects_input_value_overflow() {
        let left = Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x10; 32]),
                vout: 0,
            },
            output: TxOut {
                value: u64::MAX,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([7; 32]).verifying_key(),
                )
                .to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        };
        let right = Utxo {
            outpoint: OutPoint {
                txid: Txid::new([0x20; 32]),
                vout: 0,
            },
            output: TxOut {
                value: 1,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([7; 32]).verifying_key(),
                )
                .to_vec(),
            },
            created_at_height: 1,
            is_coinbase: false,
        };
        let mut set = UtxoSet::new();
        set.insert(left.clone());
        set.insert(right.clone());

        let tx = Transaction {
            version: 1,
            inputs: vec![
                TxIn {
                    previous_output: left.outpoint,
                    unlocking_data: Vec::new(),
                },
                TxIn {
                    previous_output: right.outpoint,
                    unlocking_data: Vec::new(),
                },
            ],
            outputs: vec![TxOut {
                value: 1,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([9; 32]).verifying_key(),
                )
                .to_vec(),
            }],
            lock_time: 0,
        };
        let mut tx = tx;
        let signing_key = crypto::signing_key_from_bytes([7; 32]);
        let signature = crypto::sign_message(&signing_key, &tx.signing_digest()).to_vec();
        tx.inputs[0].unlocking_data = signature.clone();
        tx.inputs[1].unlocking_data = signature;

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::InputValueOverflow)
        );
    }

    #[test]
    fn applies_coinbase_without_prior_inputs() {
        let mut set = UtxoSet::new();
        let coinbase = Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value: 50,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([7; 32]).verifying_key(),
                )
                .to_vec(),
            }],
            lock_time: 0,
        };

        set.apply_transaction(&coinbase, 1).unwrap();

        let created = set
            .get(&OutPoint {
                txid: coinbase.txid(),
                vout: 0,
            })
            .expect("coinbase output exists");
        assert!(created.is_coinbase);
        assert_eq!(created.created_at_height, 1);
    }

    #[test]
    fn rejects_invalid_unlocking_data() {
        let utxo = spendable_utxo(10);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());
        let tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: utxo.outpoint,
                unlocking_data: vec![0xaa],
            }],
            outputs: vec![TxOut {
                value: 10,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([9; 32]).verifying_key(),
                )
                .to_vec(),
            }],
            lock_time: 0,
        };

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::InvalidUnlockingData(utxo.outpoint))
        );
    }

    #[test]
    fn rejects_signature_mismatch() {
        let utxo = spendable_utxo(10);
        let mut set = UtxoSet::new();
        set.insert(utxo.clone());
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: utxo.outpoint,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value: 10,
                locking_data: crypto::verifying_key_bytes(
                    &crypto::signing_key_from_bytes([9; 32]).verifying_key(),
                )
                .to_vec(),
            }],
            lock_time: 0,
        };
        let wrong_key = crypto::signing_key_from_bytes([8; 32]);
        tx.inputs[0].unlocking_data =
            crypto::sign_message(&wrong_key, &tx.signing_digest()).to_vec();

        assert_eq!(
            set.validate_transaction(&tx),
            Err(ValidationError::SignatureMismatch(utxo.outpoint))
        );
    }
}
