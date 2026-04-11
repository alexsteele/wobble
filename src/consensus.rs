//! Block-level consensus checks layered above UTXO state transitions.
//!
//! This module validates header proof-of-work, block structure, and merkle
//! commitments, then applies transactions atomically through the state layer.
//! Bitcoin-style difficulty uses a compact "scientific notation" encoding in
//! `bits` to represent the full 256-bit target threshold. Bigger targets are
//! easier to satisfy; smaller targets are harder and therefore mean higher
//! difficulty.

use crate::{
    state::{UtxoSet, ValidationError},
    types::{Block, BlockHash, BlockHeight},
};

/// Expanded 256-bit proof-of-work threshold represented as a big-endian integer.
///
/// A block header is valid only if its hash, interpreted the same way, is less
/// than or equal to this target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PowTarget([u8; 32]);

impl PowTarget {
    /// Parses Bitcoin-style compact difficulty encoding into a full target.
    ///
    /// The high byte is an exponent and the low three bytes are a mantissa. This is
    /// effectively a base-256 scientific-notation form for the target:
    /// `target = mantissa * 256^(exponent - 3)`.
    ///
    /// Example: `0x1d00ffff` means exponent `0x1d` and mantissa `0x00ffff`, which
    /// expands into Bitcoin's historical maximum target.
    pub(crate) fn from_compact(bits: u32) -> Option<Self> {
        let exponent = (bits >> 24) as usize;
        let mantissa = bits & 0x007f_ffff;
        let is_negative = (bits & 0x0080_0000) != 0;

        if is_negative || mantissa == 0 {
            return None;
        }

        let mantissa_bytes = [
            ((mantissa >> 16) & 0xff) as u8,
            ((mantissa >> 8) & 0xff) as u8,
            (mantissa & 0xff) as u8,
        ];

        let mut target = [0_u8; 32];
        if exponent <= 3 {
            let value = mantissa >> (8 * (3 - exponent));
            let bytes = value.to_be_bytes();
            target[28..].copy_from_slice(&bytes);
            return Some(Self(target));
        }

        let offset = 32usize.checked_sub(exponent)?;
        if offset + 3 > 32 {
            return None;
        }

        target[offset..offset + 3].copy_from_slice(&mantissa_bytes);
        Some(Self(target))
    }

    /// Returns true if `hash` satisfies this proof-of-work threshold.
    pub(crate) fn is_met_by(&self, hash: &BlockHash) -> bool {
        hash.as_bytes() <= &self.0
    }

    /// Returns the amount of chain-selection work represented by this target.
    pub(crate) fn work(&self) -> u128 {
        let leading = u128::from_be_bytes(self.0[..16].try_into().expect("slice length is fixed"));
        let trailing = u128::from_be_bytes(self.0[16..].try_into().expect("slice length is fixed"));

        let denominator = if leading == 0 {
            1
        } else {
            leading.saturating_add(1)
        };
        let base_work = u128::MAX / denominator;

        if leading == 0 && trailing != u128::MAX {
            return base_work.saturating_sub(trailing / 2);
        }

        base_work
    }

    #[cfg(test)]
    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsensusError {
    EmptyBlock,
    BadMerkleRoot,
    FirstTransactionNotCoinbase,
    MultipleCoinbaseTransactions,
    InvalidPowTarget,
    InsufficientProofOfWork,
    Transaction(ValidationError),
}

/// Validates block structure and applies it atomically to the provided UTXO set.
pub fn apply_block(
    utxos: &mut UtxoSet,
    block: &Block,
    height: BlockHeight,
) -> Result<(), ConsensusError> {
    validate_block(block)?;

    let mut candidate = utxos.clone();
    for tx in &block.transactions {
        candidate
            .apply_transaction(tx, height)
            .map_err(ConsensusError::Transaction)?;
    }

    *utxos = candidate;
    Ok(())
}

/// Validates the block rules that can be checked in isolation.
///
/// This includes proof-of-work, basic block shape, coinbase placement, and the
/// merkle commitment from the header to the transaction list.
/// It does not check parent linkage, height-dependent subsidy rules, difficulty
/// retargeting, median-time-past, or fork choice, because those require chain
/// context beyond the block itself.
pub fn validate_block(block: &Block) -> Result<(), ConsensusError> {
    validate_header_pow(block)?;

    if block.transactions.is_empty() {
        return Err(ConsensusError::EmptyBlock);
    }

    if !block.transactions[0].is_coinbase() {
        return Err(ConsensusError::FirstTransactionNotCoinbase);
    }

    if block.transactions[1..].iter().any(|tx| tx.is_coinbase()) {
        return Err(ConsensusError::MultipleCoinbaseTransactions);
    }

    if block.header.merkle_root != block.merkle_root() {
        return Err(ConsensusError::BadMerkleRoot);
    }

    Ok(())
}

/// Expands the compact target from `bits` and checks that the header hash is
/// at or below that target.
///
/// In Bitcoin-style proof of work, `bits` is a compact encoding of the full
/// 256-bit target threshold. A header is valid only if its hash is numerically
/// less than or equal to that target. Smaller targets are harder to satisfy
/// and therefore represent higher difficulty.
fn validate_header_pow(block: &Block) -> Result<(), ConsensusError> {
    let target =
        PowTarget::from_compact(block.header.bits).ok_or(ConsensusError::InvalidPowTarget)?;
    let hash = block.header.block_hash();

    if !target.is_met_by(&hash) {
        return Err(ConsensusError::InsufficientProofOfWork);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        crypto,
        state::UtxoSet,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut, Txid},
    };

    use super::{ConsensusError, PowTarget, apply_block, validate_block};

    // `uniqueness` perturbs the coinbase so separate test blocks do not reuse
    // the same transaction id for otherwise identical rewards.
    fn coinbase(value: u64, uniqueness: u32) -> Transaction {
        let miner = crypto::signing_key_from_bytes([uniqueness as u8 + 1; 32]);
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
            }],
            lock_time: uniqueness,
        }
    }

    fn spend(previous_output: OutPoint, value: u64) -> Transaction {
        let owner = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([9; 32]);
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
            }],
            lock_time: 0,
        };
        tx.inputs[0].unlocking_data = crypto::sign_message(&owner, &tx.signing_digest()).to_vec();
        tx
    }

    fn mine_block(transactions: Vec<Transaction>) -> Block {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0; 32]),
                merkle_root: [0; 32],
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions,
        };
        block.header.merkle_root = block.merkle_root();
        mine_header_pow(&mut block);
        block
    }

    fn mine_header_pow(block: &mut Block) {
        loop {
            if super::validate_header_pow(block).is_ok() {
                return;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
    }

    #[test]
    fn applies_valid_block_atomically() {
        let genesis = mine_block(vec![coinbase(50, 0)]);
        let mut utxos = UtxoSet::new();
        apply_block(&mut utxos, &genesis, 0).unwrap();

        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let block = mine_block(vec![coinbase(50, 1), spend(spendable, 30)]);

        apply_block(&mut utxos, &block, 1).unwrap();

        assert!(utxos.get(&spendable).is_none());
        assert_eq!(utxos.len(), 2);
    }

    #[test]
    fn rejects_block_without_coinbase_first() {
        let mut invalid = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0; 32]),
                merkle_root: [0; 32],
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: vec![spend(
                OutPoint {
                    txid: Txid::new([1; 32]),
                    vout: 0,
                },
                1,
            )],
        };
        invalid.header.merkle_root = invalid.merkle_root();
        mine_header_pow(&mut invalid);

        assert_eq!(
            validate_block(&invalid),
            Err(ConsensusError::FirstTransactionNotCoinbase)
        );
    }

    #[test]
    fn rejects_empty_block() {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0; 32]),
                merkle_root: crate::crypto::double_sha256(&[]),
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: Vec::new(),
        };
        mine_header_pow(&mut block);

        assert_eq!(validate_block(&block), Err(ConsensusError::EmptyBlock));
    }

    #[test]
    fn rejects_multiple_coinbases() {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash: BlockHash::new([0; 32]),
                merkle_root: [0; 32],
                time: 1,
                bits: 0x207f_ffff,
                nonce: 0,
            },
            transactions: vec![coinbase(50, 0), coinbase(25, 1)],
        };
        block.header.merkle_root = block.merkle_root();
        mine_header_pow(&mut block);

        assert_eq!(
            validate_block(&block),
            Err(ConsensusError::MultipleCoinbaseTransactions)
        );
    }

    #[test]
    fn rejects_bad_merkle_root() {
        let mut block = mine_block(vec![coinbase(50, 0)]);
        block.header.merkle_root = [0xff; 32];
        mine_header_pow(&mut block);

        assert_eq!(validate_block(&block), Err(ConsensusError::BadMerkleRoot));
    }

    #[test]
    fn rejects_insufficient_proof_of_work() {
        let mut block = mine_block(vec![coinbase(50, 0)]);
        block.header.bits = 0x0100_0001;

        assert_eq!(
            validate_block(&block),
            Err(ConsensusError::InsufficientProofOfWork)
        );
    }

    #[test]
    fn rejects_invalid_pow_target() {
        let mut block = mine_block(vec![coinbase(50, 0)]);
        block.header.bits = 0;

        assert_eq!(
            validate_block(&block),
            Err(ConsensusError::InvalidPowTarget)
        );
    }

    #[test]
    fn block_application_is_atomic_on_transaction_failure() {
        let valid = mine_block(vec![coinbase(50, 0)]);
        let mut utxos = UtxoSet::new();
        apply_block(&mut utxos, &valid, 0).unwrap();
        let before = utxos.clone();

        let missing = OutPoint {
            txid: Txid::new([9; 32]),
            vout: 0,
        };
        let invalid = mine_block(vec![coinbase(50, 1), spend(missing, 10)]);

        assert!(matches!(
            apply_block(&mut utxos, &invalid, 1),
            Err(ConsensusError::Transaction(_))
        ));
        assert_eq!(utxos.len(), before.len());
        assert_eq!(format!("{utxos:?}"), format!("{before:?}"));
    }

    #[test]
    fn expands_compact_target_with_large_exponent() {
        let target = PowTarget::from_compact(0x1d00ffff).unwrap();

        assert_eq!(target.as_bytes()[3], 0x00);
        assert_eq!(target.as_bytes()[4], 0xff);
        assert_eq!(target.as_bytes()[5], 0xff);
        assert!(target.as_bytes()[..3].iter().all(|byte| *byte == 0));
        assert!(target.as_bytes()[6..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn expands_compact_target_with_small_exponent() {
        let target = PowTarget::from_compact(0x0300ff00).unwrap();

        assert_eq!(target.as_bytes()[30], 0xff);
        assert_eq!(target.as_bytes()[31], 0x00);
        assert!(target.as_bytes()[..30].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_invalid_compact_targets() {
        assert_eq!(PowTarget::from_compact(0), None);
        assert_eq!(PowTarget::from_compact(0x1d800001), None);
        assert_eq!(PowTarget::from_compact(0x21010000), None);
    }
}
