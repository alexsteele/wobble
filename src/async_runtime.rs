//! Async runtime scaffolding for the next-generation node server.
//!
//! This module defines the task roles, command channels, and control handle
//! that the future Tokio-based server runtime will use. The important design
//! boundary is that runtime tasks perform I/O and coordination, while a single
//! state-owning task remains the only place that mutates `NodeState`.

use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{
    admin::{AdminRequest, AdminResponse, BalanceSummary, BootstrapSummary, StatusSummary},
    consensus::BLOCK_SUBSIDY,
    crypto,
    node_state::NodeState,
    peer::{self, PeerConfig},
    types::{Block, BlockHash, Txid},
    wire::WireMessage,
};

/// Stable identifier for one peer known to the runtime layer.
pub type PeerId = String;

/// Monotonic identifier for one mining job.
pub type MiningJobId = u64;

/// Command sent into the single state-owning task.
///
/// Peer tasks, admin handlers, and the miner all enter the system through this
/// command stream so state mutation stays serialized.
pub enum StateCommand {
    InboundPeerMessage {
        peer_id: PeerId,
        message: WireMessage,
        reply: oneshot::Sender<StateResponse>,
    },
    AdminRequest {
        request: AdminRequest,
        reply: oneshot::Sender<AdminResponse>,
    },
    MinerFoundBlock {
        job_id: MiningJobId,
        block: Block,
    },
    PeerDisconnected {
        peer_id: PeerId,
    },
}

/// Direct response returned by the state task to one caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateResponse {
    None,
    PeerReplies(Vec<WireMessage>),
}

/// Asynchronous side effects emitted by the state task.
///
/// These effects are routed by the runtime to peer tasks, persistence, or the
/// miner without giving those workers direct mutable access to `NodeState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEffect {
    Relay {
        peers: Vec<PeerId>,
        message: WireMessage,
    },
    Persist,
    StartMiningJob {
        job: MiningJob,
    },
    StopMining,
    DisconnectPeer {
        peer_id: PeerId,
    },
}

/// Command delivered to one live peer task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerCommand {
    Send(WireMessage),
    Disconnect,
}

/// Mining work item prepared by the state task.
///
/// The miner thread receives a fully-assembled candidate block and searches
/// nonce space for that exact template. If a newer job replaces it, the old
/// `job_id` becomes stale and its result should be ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiningJob {
    pub job_id: MiningJobId,
    pub parent: Option<BlockHash>,
    pub block: Block,
    pub txids: Vec<Txid>,
}

/// Command sent from the state task to the dedicated miner thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinerCommand {
    StartJob(MiningJob),
    Stop,
}

/// Event sent from the miner thread back to the state task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinerEvent {
    FoundBlock {
        job_id: MiningJobId,
        block: Block,
    },
}

/// Basic channel sizing for the future runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub state_channel_capacity: usize,
    pub effect_channel_capacity: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            state_channel_capacity: 256,
            effect_channel_capacity: 256,
        }
    }
}

/// Errors returned while using the async server handle.
#[derive(Debug)]
pub enum ServerHandleError {
    SubmitClosed,
    ResponseDropped,
}

/// Cloneable control handle exposed to tests and callers.
///
/// This owns the shutdown signal plus the main state-command sender. The final
/// async runtime can grow richer helper methods on top of this surface without
/// changing who owns state.
#[derive(Debug, Clone)]
pub struct ServerHandle {
    shutdown_tx: watch::Sender<bool>,
    state_tx: mpsc::Sender<StateCommand>,
}

impl ServerHandle {
    /// Requests a clean runtime shutdown.
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Returns whether shutdown has already been requested.
    pub fn is_stopped(&self) -> bool {
        *self.shutdown_tx.borrow()
    }

    /// Sends one command to the state-owning task.
    pub async fn submit(
        &self,
        command: StateCommand,
    ) -> Result<(), mpsc::error::SendError<StateCommand>> {
        self.state_tx.send(command).await
    }

    /// Sends one peer message through the state task and waits for the direct response.
    pub async fn request_peer_message(
        &self,
        peer_id: PeerId,
        message: WireMessage,
    ) -> Result<StateResponse, ServerHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.submit(StateCommand::InboundPeerMessage {
            peer_id,
            message,
            reply: reply_tx,
        })
        .await
        .map_err(|_| ServerHandleError::SubmitClosed)?;
        reply_rx.await.map_err(|_| ServerHandleError::ResponseDropped)
    }

    /// Sends one admin request through the state task and waits for the response.
    pub async fn request_admin(
        &self,
        request: AdminRequest,
    ) -> Result<AdminResponse, ServerHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.submit(StateCommand::AdminRequest {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| ServerHandleError::SubmitClosed)?;
        reply_rx.await.map_err(|_| ServerHandleError::ResponseDropped)
    }

    /// Notifies the state task that one peer disconnected.
    pub async fn notify_peer_disconnected(
        &self,
        peer_id: PeerId,
    ) -> Result<(), ServerHandleError> {
        self.submit(StateCommand::PeerDisconnected { peer_id })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)
    }

    /// Delivers one mined block candidate back to the state task.
    pub async fn notify_miner_found_block(
        &self,
        job_id: MiningJobId,
        block: Block,
    ) -> Result<(), ServerHandleError> {
        self.submit(StateCommand::MinerFoundBlock { job_id, block })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)
    }
}

/// Scaffolding for the future Tokio-based runtime shell.
///
/// This type currently owns the core command receiver and shutdown watch used
/// by tests and by later runtime tasks. Future steps can add peer registries,
/// task sets, and effect routing here without changing the control surface.
pub struct ServerRuntime {
    config: RuntimeConfig,
    handle: ServerHandle,
    shutdown_rx: watch::Receiver<bool>,
    state_rx: mpsc::Receiver<StateCommand>,
}

/// Running async state loop plus the control and effect channels it drives.
///
/// This is the small integration surface that tests and future runtime code
/// can hold onto while the real peer, admin, and miner tasks are still being
/// ported. It keeps the spawned state owner, its control handle, and emitted
/// effects together so callers do not have to juggle those pieces separately.
pub struct SpawnedStateTask {
    handle: ServerHandle,
    effects_rx: mpsc::Receiver<StateEffect>,
    worker: JoinHandle<StateTask>,
}

impl SpawnedStateTask {
    /// Returns a cloneable handle for sending commands into the state task.
    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    /// Requests a clean shutdown of the spawned state loop.
    pub fn stop(&self) {
        self.handle.stop();
    }

    /// Sends one peer message through the state task and waits for its response.
    pub async fn request_peer_message(
        &self,
        peer_id: PeerId,
        message: WireMessage,
    ) -> Result<StateResponse, ServerHandleError> {
        self.handle.request_peer_message(peer_id, message).await
    }

    /// Sends one admin request through the state task and waits for its response.
    pub async fn request_admin(
        &self,
        request: AdminRequest,
    ) -> Result<AdminResponse, ServerHandleError> {
        self.handle.request_admin(request).await
    }

    /// Notifies the state task that one peer disconnected.
    pub async fn notify_peer_disconnected(
        &self,
        peer_id: PeerId,
    ) -> Result<(), ServerHandleError> {
        self.handle.notify_peer_disconnected(peer_id).await
    }

    /// Delivers one mined block candidate back to the state task.
    pub async fn notify_miner_found_block(
        &self,
        job_id: MiningJobId,
        block: Block,
    ) -> Result<(), ServerHandleError> {
        self.handle.notify_miner_found_block(job_id, block).await
    }

    /// Receives the next asynchronous effect emitted by the state task.
    pub async fn recv_effect(&mut self) -> Option<StateEffect> {
        self.effects_rx.recv().await
    }

    /// Waits for the state task to exit and returns its final state owner.
    pub async fn join(self) -> StateTask {
        self.worker
            .await
            .expect("state task should not panic during tests")
    }
}

impl ServerRuntime {
    /// Builds a new runtime shell with its command channels and stop handle.
    pub fn new(config: RuntimeConfig) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (state_tx, state_rx) = mpsc::channel(config.state_channel_capacity);
        let handle = ServerHandle {
            shutdown_tx,
            state_tx,
        };
        Self {
            config,
            handle,
            shutdown_rx,
            state_rx,
        }
    }

    /// Returns the cloneable control handle for this runtime.
    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    /// Returns the current runtime config.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Waits for the next command that should be handled by the state task.
    ///
    /// This is primarily scaffolding for early tests. The eventual runtime
    /// will drive a dedicated state task from this receiver.
    pub async fn recv_state_command(&mut self) -> Option<StateCommand> {
        self.state_rx.recv().await
    }

    /// Waits until the runtime receives a stop request.
    pub async fn wait_for_stop(&mut self) {
        while !*self.shutdown_rx.borrow() {
            if self.shutdown_rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Consumes this runtime shell and spawns the async state loop.
    pub fn spawn_state_task(self, task: StateTask) -> SpawnedStateTask {
        let (effects_tx, effects_rx) = mpsc::channel(self.config.effect_channel_capacity);
        let handle = self.handle.clone();
        let worker = tokio::spawn(run_state_task(
            task,
            self.state_rx,
            self.shutdown_rx,
            effects_tx,
        ));
        SpawnedStateTask {
            handle,
            effects_rx,
            worker,
        }
    }
}

/// Serialized state owner for the future async runtime.
///
/// This wraps the existing synchronous `NodeState` and protocol handlers behind
/// one command-processing surface so peer and admin tasks do not mutate chain
/// state directly.
pub struct StateTask {
    config: PeerConfig,
    state: NodeState,
    peer_count: usize,
    mining_enabled: bool,
}

impl StateTask {
    /// Builds a state owner around the current synchronous node model.
    pub fn new(config: PeerConfig, state: NodeState) -> Self {
        Self {
            config,
            state,
            peer_count: 0,
            mining_enabled: false,
        }
    }

    /// Returns the current immutable node state snapshot.
    pub fn state(&self) -> &NodeState {
        &self.state
    }

    /// Records how many peers the runtime currently considers connected.
    pub fn set_peer_count(&mut self, peer_count: usize) {
        self.peer_count = peer_count;
    }

    /// Records whether mining is currently enabled in the runtime.
    pub fn set_mining_enabled(&mut self, mining_enabled: bool) {
        self.mining_enabled = mining_enabled;
    }

    /// Processes one state command, sends any direct reply, and returns
    /// asynchronous side effects for the runtime to route.
    pub fn handle_command(&mut self, command: StateCommand) -> Vec<StateEffect> {
        match command {
            StateCommand::InboundPeerMessage {
                peer_id: _peer_id,
                message,
                reply,
            } => {
                let replies = peer::handle_message(&self.config, &mut self.state, message)
                    .map(StateResponse::PeerReplies)
                    .unwrap_or(StateResponse::None);
                let _ = reply.send(replies);
                Vec::new()
            }
            StateCommand::AdminRequest { request, reply } => {
                let _ = reply.send(self.handle_admin_request(request));
                Vec::new()
            }
            StateCommand::MinerFoundBlock { job_id: _, block } => {
                let _ = self.state.accept_block(block);
                vec![StateEffect::Persist]
            }
            StateCommand::PeerDisconnected { peer_id: _peer_id } => Vec::new(),
        }
    }

    /// Handles one admin request against the serialized node state.
    fn handle_admin_request(&mut self, request: AdminRequest) -> AdminResponse {
        match request {
            AdminRequest::GetStatus => {
                let tip = self.state.tip_summary();
                AdminResponse::Status(StatusSummary {
                    tip: tip.tip,
                    height: tip.height,
                    branch_count: self.state.chain().branch_count(),
                    mempool_size: self.state.mempool().len(),
                    peer_count: self.peer_count,
                    mining_enabled: self.mining_enabled,
                })
            }
            AdminRequest::GetBalance { public_key } => {
                let Some(owner) = crypto::parse_verifying_key(&public_key) else {
                    return AdminResponse::Error {
                        message: "invalid public key".to_string(),
                    };
                };
                AdminResponse::Balance(BalanceSummary {
                    amount: self.state.balance_for_key(&owner),
                })
            }
            AdminRequest::Bootstrap { public_key, blocks } => {
                let Some(owner) = crypto::parse_verifying_key(&public_key) else {
                    return AdminResponse::Error {
                        message: "invalid public key".to_string(),
                    };
                };
                let start_uniqueness = self
                    .state
                    .tip_summary()
                    .height
                    .and_then(|height| u32::try_from(height.saturating_add(1)).ok())
                    .unwrap_or(0);
                let mut last_block_hash = None;
                for offset in 0..blocks {
                    let uniqueness = start_uniqueness.saturating_add(offset);
                    match self
                        .state
                        .mine_block(BLOCK_SUBSIDY, &owner, uniqueness, 0x207f_ffff, 0)
                    {
                        Ok(block_hash) => last_block_hash = Some(block_hash),
                        Err(err) => {
                            return AdminResponse::Error {
                                message: format!("{err:?}"),
                            };
                        }
                    }
                }
                AdminResponse::Bootstrapped(BootstrapSummary {
                    blocks_mined: blocks,
                    last_block_hash,
                })
            }
            AdminRequest::SubmitTransaction { transaction } => {
                let txid = transaction.txid();
                match self.state.submit_transaction(transaction) {
                    Ok(_) => AdminResponse::Submitted { txid },
                    Err(err) => AdminResponse::Error {
                        message: format!("{err:?}"),
                    },
                }
            }
        }
    }
}

/// Drives the serialized state owner until shutdown or channel close.
async fn run_state_task(
    mut task: StateTask,
    mut state_rx: mpsc::Receiver<StateCommand>,
    mut shutdown_rx: watch::Receiver<bool>,
    effects_tx: mpsc::Sender<StateEffect>,
) -> StateTask {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            command = state_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                for effect in task.handle_command(command) {
                    if effects_tx.send(effect).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    task
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use crate::{
        admin::{AdminRequest, AdminResponse},
        async_runtime::{
            RuntimeConfig, ServerRuntime, StateCommand, StateEffect, StateResponse, StateTask,
        },
        crypto,
        node_state::NodeState,
        peer::PeerConfig,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
        wire::{TipSummary, WireMessage},
    };

    fn coinbase(value: u64, owner: &ed25519_dalek::VerifyingKey, uniqueness: u32) -> Transaction {
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

    fn mine_block(
        prev_blockhash: BlockHash,
        bits: u32,
        owner: &ed25519_dalek::VerifyingKey,
        uniqueness: u32,
    ) -> Block {
        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions: vec![coinbase(50, owner, uniqueness)],
        };
        block.header.merkle_root = block.merkle_root();

        loop {
            if crate::consensus::validate_block(&block).is_ok() {
                return block;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_handle_stop_marks_runtime_stopped() {
        let mut runtime = ServerRuntime::new(RuntimeConfig::default());
        let handle = runtime.handle();

        handle.stop();
        runtime.wait_for_stop().await;

        assert!(handle.is_stopped());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_handle_submits_state_commands() {
        let mut runtime = ServerRuntime::new(RuntimeConfig::default());
        let handle = runtime.handle();
        let (reply_tx, _reply_rx) = oneshot::channel();

        handle
            .submit(StateCommand::InboundPeerMessage {
                peer_id: "peer-1".to_string(),
                message: WireMessage::GetTip,
                reply: reply_tx,
            })
            .await
            .unwrap();

        let command = runtime.recv_state_command().await;
        match command {
            Some(StateCommand::InboundPeerMessage {
                peer_id, message, ..
            }) => {
                assert_eq!(peer_id, "peer-1");
                assert_eq!(message, WireMessage::GetTip);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admin_requests_use_same_state_command_channel() {
        let mut runtime = ServerRuntime::new(RuntimeConfig::default());
        let handle = runtime.handle();
        let (reply_tx, _reply_rx) = oneshot::channel();

        handle
            .submit(StateCommand::AdminRequest {
                request: AdminRequest::GetStatus,
                reply: reply_tx,
            })
            .await
            .unwrap();

        assert!(matches!(
            runtime.recv_state_command().await,
            Some(StateCommand::AdminRequest { .. })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_handle_round_trips_peer_message_through_state_task() {
        let runtime = ServerRuntime::new(RuntimeConfig::default());
        let task = StateTask::new(PeerConfig::new("wobble-local", None), NodeState::new());
        let worker = runtime.spawn_state_task(task);

        let response = worker
            .request_peer_message("peer-1".to_string(), WireMessage::GetTip)
            .await
            .unwrap();

        worker.stop();
        let _task = worker.join().await;

        assert_eq!(
            response,
            StateResponse::PeerReplies(vec![WireMessage::Tip(TipSummary {
                tip: None,
                height: None,
            })])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_handle_round_trips_admin_request_through_state_task() {
        let runtime = ServerRuntime::new(RuntimeConfig::default());
        let mut task =
            StateTask::new(PeerConfig::new("wobble-local", Some("node".to_string())), NodeState::new());
        task.set_peer_count(2);
        let worker = runtime.spawn_state_task(task);

        let response = worker.request_admin(AdminRequest::GetStatus).await.unwrap();

        worker.stop();
        let _task = worker.join().await;

        assert_eq!(
            response,
            AdminResponse::Status(crate::admin::StatusSummary {
                tip: None,
                height: None,
                branch_count: 0,
                mempool_size: 0,
                peer_count: 2,
                mining_enabled: false,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn miner_found_block_emits_persist_effect() {
        let owner = crypto::signing_key_from_bytes([9; 32]);
        let block = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let runtime = ServerRuntime::new(RuntimeConfig::default());
        let task = StateTask::new(PeerConfig::new("wobble-local", None), NodeState::new());
        let mut worker = runtime.spawn_state_task(task);

        worker.notify_miner_found_block(7, block).await.unwrap();

        assert_eq!(worker.recv_effect().await, Some(StateEffect::Persist));
        worker.stop();
        let task = worker.join().await;
        assert!(task.state().chain().best_tip().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawned_state_task_exposes_handle_for_shared_callers() {
        let runtime = ServerRuntime::new(RuntimeConfig::default());
        let task = StateTask::new(PeerConfig::new("wobble-local", None), NodeState::new());
        let worker = runtime.spawn_state_task(task);
        let handle = worker.handle();

        let response = handle.request_admin(AdminRequest::GetStatus).await.unwrap();

        worker.stop();
        let _task = worker.join().await;

        assert!(matches!(response, AdminResponse::Status(_)));
    }

    #[test]
    fn state_task_handles_get_tip_via_peer_command() {
        let mut task = StateTask::new(PeerConfig::new("wobble-local", None), NodeState::new());
        let (reply_tx, reply_rx) = oneshot::channel();

        let effects = task.handle_command(StateCommand::InboundPeerMessage {
            peer_id: "peer-1".to_string(),
            message: WireMessage::GetTip,
            reply: reply_tx,
        });

        assert!(effects.is_empty());
        assert_eq!(
            reply_rx.blocking_recv().unwrap(),
            StateResponse::PeerReplies(vec![WireMessage::Tip(TipSummary {
                tip: None,
                height: None,
            })])
        );
    }

    #[test]
    fn state_task_reports_status_through_admin_request() {
        let mut task =
            StateTask::new(PeerConfig::new("wobble-local", Some("node".to_string())), NodeState::new());
        task.set_peer_count(3);
        task.set_mining_enabled(true);
        let (reply_tx, reply_rx) = oneshot::channel();

        let effects = task.handle_command(StateCommand::AdminRequest {
            request: AdminRequest::GetStatus,
            reply: reply_tx,
        });

        assert!(effects.is_empty());
        assert_eq!(
            reply_rx.blocking_recv().unwrap(),
            AdminResponse::Status(crate::admin::StatusSummary {
                tip: None,
                height: None,
                branch_count: 0,
                mempool_size: 0,
                peer_count: 3,
                mining_enabled: true,
            })
        );
    }

    #[test]
    fn state_task_submits_transaction_through_admin_request() {
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &sender.verifying_key(), 0);
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let mut transaction = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: spendable,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value: 30,
                locking_data: crypto::verifying_key_bytes(&recipient.verifying_key()).to_vec(),
            }],
            lock_time: 1,
        };
        transaction.inputs[0].unlocking_data =
            crypto::sign_message(&sender, &transaction.signing_digest()).to_vec();

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let mut task = StateTask::new(PeerConfig::new("wobble-local", None), state);
        let expected_txid = transaction.txid();
        let (reply_tx, reply_rx) = oneshot::channel();

        let effects = task.handle_command(StateCommand::AdminRequest {
            request: AdminRequest::SubmitTransaction { transaction },
            reply: reply_tx,
        });

        assert!(effects.is_empty());
        assert_eq!(
            reply_rx.blocking_recv().unwrap(),
            AdminResponse::Submitted { txid: expected_txid }
        );
        assert!(task.state().mempool().get(&expected_txid).is_some());
    }
}
