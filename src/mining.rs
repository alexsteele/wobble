//! Integrated local mining policy and cancellable mining worker.
//!
//! This module owns the mining-side runtime concerns: candidate solving,
//! cancellation, and background worker lifecycle.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use ed25519_dalek::VerifyingKey;

use crate::{
    consensus::{self, BLOCK_SUBSIDY},
    crypto,
    server::ServerHandle,
    types::{Block, BlockHash, BlockHeader, Transaction, TxOut},
};

/// Background testnet mining configuration for a serving node.
#[derive(Debug, Clone)]
pub struct MiningConfig {
    pub miner_verifying_key: VerifyingKey,
    pub interval: Duration,
    pub max_transactions: usize,
    pub bits: u32,
    next_uniqueness: u32,
}

impl MiningConfig {
    /// Builds a minimal integrated mining policy for local testnet use.
    ///
    /// Mining uses the standard block subsidy, mines only when the mempool is
    /// non-empty, and increments `uniqueness` on each mined block so coinbase
    /// transactions remain distinct.
    pub fn new(miner_verifying_key: VerifyingKey) -> Self {
        Self {
            miner_verifying_key,
            interval: Duration::from_millis(250),
            max_transactions: 100,
            bits: 0x207f_ffff,
            next_uniqueness: 0,
        }
    }

    /// Sets the poll interval for the integrated miner loop.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Caps how many mempool transactions a mined block may include.
    pub fn with_max_transactions(mut self, max_transactions: usize) -> Self {
        self.max_transactions = max_transactions;
        self
    }

    /// Sets the compact proof-of-work target used by the integrated miner.
    pub fn with_bits(mut self, bits: u32) -> Self {
        self.bits = bits;
        self
    }

    /// Builds the next internal `mine_pending` request for this miner.
    pub fn next_request(&mut self) -> crate::wire::MinePendingRequest {
        let request = crate::wire::MinePendingRequest {
            reward: BLOCK_SUBSIDY,
            miner_public_key: crate::crypto::verifying_key_bytes(&self.miner_verifying_key)
                .to_vec(),
            uniqueness: self.next_uniqueness,
            bits: self.bits,
            max_transactions: self.max_transactions,
        };
        self.next_uniqueness = self.next_uniqueness.wrapping_add(1);
        request
    }
}

/// Unsolved block candidate assembled by the server and submitted to the miner.
pub type Candidate = Block;

/// Handle for one background miner worker.
///
/// The server submits already-assembled block candidates here. Each submit
/// replaces current work. The worker solves at most one candidate at a time
/// and sends solved blocks back to the server event loop.
#[derive(Debug)]
pub struct Miner {
    command_tx: mpsc::Sender<MinerCommand>,
    generation: Arc<AtomicU64>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Miner {
    /// Starts one background miner that returns solved blocks through `handle`.
    pub fn start(handle: ServerHandle) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = generation.clone();
        let worker = thread::spawn(move || run_miner_worker(command_rx, worker_generation, handle));
        Self {
            command_tx,
            generation,
            worker: Some(worker),
        }
    }

    /// Replaces current work with `candidate`.
    pub fn submit(&self, candidate: Candidate) -> Result<(), MinerError> {
        let generation = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.command_tx
            .send(MinerCommand::Submit {
                generation,
                candidate,
            })
            .map_err(|_| MinerError::SubmitClosed)
    }

    /// Cancels the current attempt but leaves the miner running.
    pub fn cancel(&self) -> Result<(), MinerError> {
        let generation = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.command_tx
            .send(MinerCommand::Cancel { generation })
            .map_err(|_| MinerError::SubmitClosed)
    }

    /// Cancels current work and stops the miner thread.
    pub fn stop(mut self) -> Result<(), MinerError> {
        let _ = self.generation.fetch_add(1, Ordering::Relaxed);
        let _ = self.command_tx.send(MinerCommand::Stop);
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| MinerError::WorkerPanicked)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum MinerError {
    SubmitClosed,
    WorkerPanicked,
}

enum MinerCommand {
    Submit {
        generation: u64,
        candidate: Candidate,
    },
    Cancel {
        generation: u64,
    },
    Stop,
}

/// Builds a coinbase transaction paying `reward` to `miner_verifying_key`.
///
/// `uniqueness` is stored in `lock_time` so locally mined coinbases stay
/// distinct even when they otherwise have the same shape.
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

/// Builds one unsolved candidate block from the selected transactions.
///
/// The server owns transaction selection and parent-tip choice. This helper
/// just assembles the candidate block shape the miner will solve.
pub fn build_candidate(
    prev_blockhash: BlockHash,
    reward: u64,
    miner_verifying_key: &VerifyingKey,
    uniqueness: u32,
    bits: u32,
    mut transactions: Vec<Transaction>,
) -> Candidate {
    let mut txns = Vec::with_capacity(transactions.len() + 1);
    txns.push(coinbase_transaction(
        reward,
        miner_verifying_key,
        uniqueness,
    ));
    txns.append(&mut transactions);

    let mut block = Block {
        header: BlockHeader {
            version: 1,
            prev_blockhash,
            merkle_root: [0; 32],
            time: 1,
            bits,
            nonce: 0,
        },
        transactions: txns,
    };
    block.header.merkle_root = block.merkle_root();

    block
}

/// Solves one candidate by searching nonces until the header satisfies `bits`.
pub fn solve_candidate(mut candidate: Candidate) -> Block {
    loop {
        if consensus::validate_block(&candidate).is_ok() {
            return candidate;
        }
        candidate.header.nonce = candidate.header.nonce.wrapping_add(1);
    }
}

/// Builds and mines one block by searching nonces until the header satisfies `bits`.
///
/// This remains as the synchronous convenience helper used by existing tests
/// and direct state-mutating paths.
pub fn mine_block(
    prev_blockhash: BlockHash,
    reward: u64,
    miner_verifying_key: &VerifyingKey,
    uniqueness: u32,
    bits: u32,
    transactions: Vec<Transaction>,
) -> Block {
    solve_candidate(build_candidate(
        prev_blockhash,
        reward,
        miner_verifying_key,
        uniqueness,
        bits,
        transactions,
    ))
}

const CANCEL_CHECK_INTERVAL: u32 = 4096;

fn run_miner_worker(
    command_rx: mpsc::Receiver<MinerCommand>,
    generation: Arc<AtomicU64>,
    handle: ServerHandle,
) {
    let mut current: Option<(u64, Candidate)> = None;

    loop {
        if current.is_none() {
            match command_rx.recv() {
                Ok(MinerCommand::Submit {
                    generation,
                    candidate,
                }) => current = Some((generation, candidate)),
                Ok(MinerCommand::Cancel { .. }) => {}
                Ok(MinerCommand::Stop) | Err(_) => break,
            }
            continue;
        }

        let mut replace_current = false;
        {
            let (candidate_generation, candidate) =
                current.as_mut().expect("current candidate should exist while mining");
            loop {
                if generation.load(Ordering::Relaxed) != *candidate_generation {
                    replace_current = true;
                    break;
                }
                if consensus::validate_block(candidate).is_ok() {
                    let solved = candidate.clone();
                    let _ = handle.notify_mined_block(solved);
                    current = None;
                    replace_current = false;
                    break;
                }
                candidate.header.nonce = candidate.header.nonce.wrapping_add(1);
                if candidate.header.nonce % CANCEL_CHECK_INTERVAL == 0 {
                    break;
                }
            }
        }

        while let Ok(command) = command_rx.try_recv() {
            match command {
                MinerCommand::Submit {
                    generation,
                    candidate,
                } => {
                    current = Some((generation, candidate));
                    replace_current = false;
                }
                MinerCommand::Cancel { generation } => {
                    let _ = generation;
                    current = None;
                    replace_current = false;
                }
                MinerCommand::Stop => return,
            }
        }

        if replace_current {
            current = None;
        }
    }
}
