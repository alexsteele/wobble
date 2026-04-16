//! Integrated local mining policy and background mining loop.
//!
//! This module keeps the simple testnet mining configuration separate from the
//! server's startup and event-dispatch logic. The server still owns node state
//! and decides how mined blocks are applied, but mining cadence and request
//! construction live here.

use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use tokio::task::JoinHandle;

use crate::{consensus::BLOCK_SUBSIDY, server::ServerHandle};

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

/// Spawns the integrated mining tick loop when mining is configured.
pub(crate) fn spawn_mining_loop(
    handle: ServerHandle,
    interval: Option<Duration>,
) -> Option<JoinHandle<()>> {
    let interval = interval?;
    Some(tokio::spawn(async move {
        while !handle.is_stopped() {
            tokio::time::sleep(interval).await;
            if handle.notify_mine_tick().await.is_err() {
                break;
            }
        }
    }))
}
