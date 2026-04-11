//! Simple file-backed persistence for node state snapshots.
//!
//! This stores the current in-memory `NodeState` as a single binary blob. It is
//! intentionally simple and suited to local development, not production use.

use std::{fs, io, path::Path};

use crate::node_state::NodeState;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<bincode::error::EncodeError> for StoreError {
    fn from(error: bincode::error::EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<bincode::error::DecodeError> for StoreError {
    fn from(error: bincode::error::DecodeError) -> Self {
        Self::Decode(error)
    }
}

/// Writes the full node snapshot to `path`, replacing any previous file.
pub fn save_node_state(path: &Path, state: &NodeState) -> Result<(), StoreError> {
    let bytes = bincode::serde::encode_to_vec(state, bincode::config::standard())?;
    fs::write(path, bytes)?;
    Ok(())
}

/// Loads a previously saved node snapshot from `path`.
pub fn load_node_state(path: &Path) -> Result<NodeState, StoreError> {
    let bytes = fs::read(path)?;
    let (state, _): (NodeState, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        node_state::NodeState,
        types::{Block, BlockHash, BlockHeader, Transaction, TxOut},
    };

    use super::{load_node_state, save_node_state};

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

    fn temp_snapshot_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-store-test-{}-{}.bin",
            std::process::id(),
            nanos
        ));
        path
    }

    #[test]
    fn saves_and_loads_node_state_round_trip() {
        let mut state = NodeState::new();
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, 0);
        state.accept_block(genesis).unwrap();

        let path = temp_snapshot_path();
        save_node_state(&path, &state).unwrap();
        let loaded = load_node_state(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded.chain().best_tip(), state.chain().best_tip());
        assert_eq!(
            format!("{:?}", loaded.active_utxos()),
            format!("{:?}", state.active_utxos())
        );
    }
}
