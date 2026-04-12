//! Async runtime scaffolding for the next-generation node server.
//!
//! This module defines the task roles, command channels, and control handle
//! that the future Tokio-based server runtime will use. The important design
//! boundary is that runtime tasks perform I/O and coordination, while a single
//! state-owning task remains the only place that mutates `NodeState`.

use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    admin::{AdminRequest, AdminResponse},
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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            state_channel_capacity: 256,
        }
    }
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
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use crate::{
        admin::AdminRequest,
        async_runtime::{RuntimeConfig, ServerRuntime, StateCommand},
        wire::WireMessage,
    };

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
}
