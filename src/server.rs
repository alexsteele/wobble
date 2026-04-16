//! Server manages the chain, sync protocol, and mining (if enabled).

use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::{
    runtime::Handle as TokioHandle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use crate::{
    admin::{AdminRequest, AdminResponse, BalanceSummary, BootstrapSummary, StatusSummary},
    chain::ChainError,
    client::{ClientError, RequestError},
    consensus::BLOCK_SUBSIDY,
    mempool::MempoolError,
    mining::{MiningConfig, spawn_mining_loop},
    node_state::{NodeState, NodeStateError},
    peer::{self, PeerError},
    peers::{PeerSource, StoredPeer},
    sqlite_store::{self, SqliteStoreError},
    types::{Block, BlockHash, Txid},
    wire::{HelloMessage, PROTOCOL_VERSION, TipSummary, WireMessage},
};
/// Outbound relay destination configured for this server.
///
/// `node_name` is optional because some callers may know only the socket
/// address. When present, it lets the server avoid relaying an accepted object
/// straight back to the peer that just announced it on the current stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEndpoint {
    pub addr: String,
    pub node_name: Option<String>,
}

impl PeerEndpoint {
    pub fn new(addr: impl Into<String>, node_name: Option<String>) -> Self {
        Self {
            addr: addr.into(),
            node_name,
        }
    }
}

/// Server settings for one running node instance.
///
/// This is the single config object for the server. It carries both the local
/// protocol identity advertised to peers and the runtime listener settings used
/// when the process starts serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub network: String,
    pub node_name: Option<String>,
    pub advertised_addr: Option<String>,
    pub listen_addr: String,
    pub admin_addr: Option<String>,
    pub channel_capacity: usize,
}

impl ServerConfig {
    pub fn new(
        network: impl Into<String>,
        node_name: Option<String>,
        listen_addr: impl Into<String>,
    ) -> Self {
        Self {
            network: network.into(),
            node_name,
            advertised_addr: None,
            listen_addr: listen_addr.into(),
            admin_addr: None,
            channel_capacity: 64,
        }
    }

    /// Records the listener address this node should advertise during handshake.
    pub fn with_advertised_addr(mut self, advertised_addr: impl Into<String>) -> Self {
        self.advertised_addr = Some(advertised_addr.into());
        self
    }

    pub fn with_admin_addr(mut self, admin_addr: impl Into<String>) -> Self {
        self.admin_addr = Some(admin_addr.into());
        self
    }

    pub fn with_channel_capacity(mut self, channel_capacity: usize) -> Self {
        self.channel_capacity = channel_capacity;
        self
    }
}

/// Shared stop signal for one running server instance.
///
/// The server event loop and any active stream handlers poll this flag so callers can
/// stop a long-running node without depending on connection timing.
#[derive(Debug, Clone, Default)]
pub struct ServerControl {
    stop_requested: Arc<AtomicBool>,
}

impl ServerControl {
    /// Requests that the server stop serving as soon as its loops next poll.
    pub fn stop(&self) {
        self.stop_requested.store(true, Ordering::Relaxed);
    }
}

/// Cloneable event sender for the async server shell.
#[derive(Debug, Clone)]
pub struct ServerHandle {
    command_tx: mpsc::Sender<ServerEvent>,
    stop_requested: Arc<AtomicBool>,
}

impl ServerHandle {
    /// Requests one bootstrap pass before the server begins normal serving.
    async fn bootstrap(&self) -> Result<(), ServerHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(ServerEvent::Bootstrap { reply: reply_tx })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)?;
        reply_rx
            .await
            .map_err(|_| ServerHandleError::ResponseDropped)
    }

    /// Sends one peer message into the server event loop and waits for replies.
    pub(crate) async fn request_peer_message(
        &self,
        peer_id: String,
        origin: Option<RelayOrigin>,
        message: WireMessage,
    ) -> Result<Result<Vec<WireMessage>, ServerError>, ServerHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(ServerEvent::PeerMessage {
                peer_id,
                origin,
                message,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)?;
        reply_rx
            .await
            .map_err(|_| ServerHandleError::ResponseDropped)
    }

    /// Sends one admin request into the server event loop and waits for the response.
    pub async fn request_admin(
        &self,
        request: AdminRequest,
    ) -> Result<AdminResponse, ServerHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(ServerEvent::AdminRequest {
                request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)?;
        reply_rx
            .await
            .map_err(|_| ServerHandleError::ResponseDropped)
    }

    /// Records one new connected peer in the server event loop.
    pub(crate) async fn notify_peer_connected(
        &self,
        peer_id: String,
    ) -> Result<(), ServerHandleError> {
        self.command_tx
            .send(ServerEvent::PeerConnected { peer_id })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)
    }

    /// Records one disconnected peer in the server event loop.
    pub(crate) async fn notify_peer_disconnected(
        &self,
        peer_id: String,
    ) -> Result<(), ServerHandleError> {
        self.command_tx
            .send(ServerEvent::PeerDisconnected { peer_id })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)
    }

    /// Triggers one mining poll against the owned server state.
    pub(crate) async fn notify_mine_tick(&self) -> Result<(), ServerHandleError> {
        self.command_tx
            .send(ServerEvent::MineTick)
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)
    }

    /// Requests one follow-up sync check after a peer hello has been answered.
    pub(crate) async fn notify_hello_sync(
        &self,
        remote_hello: HelloMessage,
    ) -> Result<(), ServerHandleError> {
        self.command_tx
            .send(ServerEvent::HelloSync { remote_hello })
            .await
            .map_err(|_| ServerHandleError::SubmitClosed)
    }

    /// Requests server shutdown.
    pub fn stop(&self) {
        self.stop_requested.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stop_requested.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct Server {
    config: ServerConfig,
    state: NodeState,
    peers: HashMap<String, RuntimePeer>,
    connected_peers: HashSet<String>,
    sqlite_store: Option<sqlite_store::SqliteStore>,
    runtime_handle: Option<TokioHandle>,
    bootstrap_sync: bool,
    mining: Option<MiningConfig>,
    control: ServerControl,
}

/// Errors produced by the live server while handling protocol messages.
#[derive(Debug)]
pub enum ServerError {
    Peer(PeerError),
    SqlitePersist(SqliteStoreError),
    Sync(SyncError),
}

/// Errors produced while fetching and applying missing blocks from a peer.
#[derive(Debug)]
pub enum SyncError {
    Handshake(ClientError),
    Request(RequestError),
    MissingRemoteBlock(BlockHash),
    AcceptBlock(NodeStateError),
    SqlitePersist(SqliteStoreError),
}

/// Request/response errors for async tasks talking to the server event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerHandleError {
    SubmitClosed,
    ResponseDropped,
}

/// Origin identity learned from the remote `hello` on a live stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RelayOrigin {
    pub(crate) advertised_addr: Option<String>,
    pub(crate) node_name: Option<String>,
}

/// Event sent into the single-threaded server event loop.
///
/// Async listener, peer, admin, and mining tasks translate external activity
/// into these concrete events so the owned `Server` remains the only authority
/// for state mutation and policy decisions.
enum ServerEvent {
    Bootstrap {
        reply: oneshot::Sender<()>,
    },
    PeerConnected {
        peer_id: String,
    },
    PeerMessage {
        peer_id: String,
        origin: Option<RelayOrigin>,
        message: WireMessage,
        reply: oneshot::Sender<Result<Vec<WireMessage>, ServerError>>,
    },
    PeerDisconnected {
        peer_id: String,
    },
    AdminRequest {
        request: AdminRequest,
        reply: oneshot::Sender<AdminResponse>,
    },
    HelloSync {
        remote_hello: HelloMessage,
    },
    MineTick,
    Stop,
}

/// One runtime peer record combining endpoint identity, learned metadata, and
/// any future live outbound connection state.
#[derive(Debug)]
struct RuntimePeer {
    endpoint: PeerEndpoint,
    stored: StoredPeer,
    connection: Option<peer::PeerHandle>,
}

/// One spawned async peer transport task.
struct LivePeerTask {
    worker: JoinHandle<()>,
}

/// Retry budget for best-effort outbound relay dialing.
const RELAY_CONNECT_ATTEMPTS: usize = 3;
/// Small pause between relay dial attempts so listeners finishing a prior
/// request can accept the next inbound connection.
const RELAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);
/// Cooldown between configured-peer sync attempts to the same peer.
const CONFIGURED_PEER_SYNC_COOLDOWN: Duration = Duration::from_secs(5);

impl Server {
    pub fn new(config: ServerConfig, state: NodeState) -> Self {
        Self {
            config,
            state,
            peers: HashMap::new(),
            connected_peers: HashSet::new(),
            sqlite_store: None,
            runtime_handle: None,
            bootstrap_sync: false,
            mining: None,
            control: ServerControl::default(),
        }
    }

    /// Configures the peer addresses that should receive relayed transactions and blocks.
    pub fn with_peers(mut self, peers: Vec<PeerEndpoint>) -> Self {
        self.peers = peers
            .into_iter()
            .map(|peer| {
                (
                    peer.addr.clone(),
                    RuntimePeer {
                        endpoint: peer.clone(),
                        stored: StoredPeer::from_endpoint(peer, PeerSource::Seed),
                        connection: None,
                    },
                )
            })
            .collect();
        self
    }

    /// Configures the server to persist accepted blocks and chain metadata to SQLite.
    ///
    /// This stores the live server state in SQLite for restart and sync.
    pub fn with_sqlite_path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let store = sqlite_store::SqliteStore::open(&path)
            .expect("server sqlite store should open before serving");
        self.sqlite_store = Some(store);
        self
    }

    /// Enables configured-peer sync attempts while serving.
    ///
    /// This is intended for cold start or restart of a node that may have
    /// missed blocks while offline. Sync remains best effort: the server tries
    /// once at startup and then retries periodically while the serve loop is
    /// running.
    pub fn with_bootstrap_sync(mut self, enabled: bool) -> Self {
        self.bootstrap_sync = enabled;
        self
    }

    /// Enables the integrated testnet miner for this serving node.
    ///
    /// The integrated miner is intentionally simple: while serving, it polls
    /// for inbound connections and mines only when the local mempool is
    /// non-empty.
    pub fn with_mining(mut self, mining: MiningConfig) -> Self {
        self.mining = Some(mining);
        self
    }

    /// Starts the server and runs it until shutdown.
    ///
    /// - creates tokio runtime
    /// - binds socket listeners
    /// - starts mining
    /// - runs the server event loop
    pub fn start(self, ready: Option<std::sync::mpsc::Sender<ServerHandle>>) -> io::Result<NodeState> {
        info!(listen_addr = self.config.listen_addr, "Server::start");
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| io::Error::other(format!("async runtime init failed: {err}")))?;
        tokio_runtime.block_on(async move { self.serve_inner(ready).await })
    }

    /// Returns a cloneable control handle for this running server.
    pub fn control(&self) -> ServerControl {
        self.control.clone()
    }

    /// Binds the peer listener and optional admin listener, then runs the
    /// server until shutdown.
    ///
    /// The listener, peer tasks, and optional mining timer run asynchronously,
    /// while this consumed `Server` instance continues to own all blockchain
    /// state and persistence on one dedicated event loop thread.
    async fn serve_inner(
        mut self,
        ready: Option<std::sync::mpsc::Sender<ServerHandle>>,
    ) -> io::Result<NodeState> {
        let peer_listener = tokio::net::TcpListener::bind(&self.config.listen_addr).await?;
        let admin_listener = match self.config.admin_addr.as_deref() {
            Some(admin_addr) => Some(tokio::net::TcpListener::bind(admin_addr).await?),
            None => None,
        };
        self.runtime_handle = Some(tokio::runtime::Handle::current());
        let mining_interval = self.mining.as_ref().map(|config| config.interval);
        let bootstrap_pending = self.bootstrap_sync;
        let stop_requested = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = mpsc::channel(self.config.channel_capacity);
        let handle = ServerHandle {
            command_tx,
            stop_requested: stop_requested.clone(),
        };
        let worker = std::thread::spawn(move || self.run_event_loop(command_rx, stop_requested));

        if bootstrap_pending {
            handle
                .bootstrap()
                .await
                .map_err(|err| io::Error::other(format!("{err:?}")))?;
        }
        if let Some(ready) = ready {
            let _ = ready.send(handle.clone());
        }

        let mining_task = spawn_mining_loop(handle.clone(), mining_interval);
        let mut peers: HashMap<String, LivePeerTask> = HashMap::new();
        let mut next_inbound_peer_id = 0_u64;

        loop {
            if handle.is_stopped() {
                break;
            }
            tokio::select! {
                accept_result = peer_listener.accept() => {
                    let (stream, _) = accept_result?;
                    next_inbound_peer_id = next_inbound_peer_id.wrapping_add(1);
                    let peer_id = format!("peer-{next_inbound_peer_id}");
                    spawn_peer_task(&mut peers, handle.clone(), peer_id, stream);
                }
                accept_result = accept_optional_admin(admin_listener.as_ref()) => {
                    if let Some(stream) = accept_result? {
                        spawn_admin_task(handle.clone(), stream);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
            peers.retain(|_, peer| !peer.worker.is_finished());
        }

        let _ = handle.command_tx.try_send(ServerEvent::Stop);
        if let Some(task) = mining_task {
            task.abort();
        }
        peers.clear();

        let server = worker
            .join()
            .expect("server event loop thread should not panic");
        Ok(server.state)
    }

    /// Runs the authoritative server event loop on one dedicated thread.
    ///
    /// Every state mutation, persistence write, relay decision, bootstrap sync,
    /// and mining action flows through this loop so protocol authority stays in
    /// one place even though peer and admin sockets are handled asynchronously.
    fn run_event_loop(
        mut self,
        mut command_rx: mpsc::Receiver<ServerEvent>,
        stop_requested: Arc<AtomicBool>,
    ) -> Server {
        while let Some(event) = command_rx.blocking_recv() {
            if !self.handle_event(event) || stop_requested.load(Ordering::Relaxed) {
                break;
            }
        }
        self.disconnect();
        self
    }

    /// Dispatches one server event to the appropriate handler.
    ///
    /// The event loop stays intentionally small: it pulls one event from the
    /// channel, routes it here, and lets the dedicated handler deal with peer
    /// messages, admin requests, peer lifecycle, mining ticks, or shutdown.
    fn handle_event(&mut self, event: ServerEvent) -> bool {
        match event {
            ServerEvent::Bootstrap { reply } => {
                if self.bootstrap_sync {
                    self.sync_configured_peers_best_effort();
                    self.bootstrap_sync = false;
                }
                let _ = reply.send(());
                true
            }
            ServerEvent::PeerConnected { peer_id } => {
                self.connected_peers.insert(peer_id);
                true
            }
            ServerEvent::PeerMessage {
                peer_id,
                origin,
                message,
                reply,
            } => {
                let result = self.handle_peer_event(&peer_id, origin.as_ref(), message);
                let _ = reply.send(result);
                true
            }
            ServerEvent::PeerDisconnected { peer_id } => {
                self.connected_peers.remove(&peer_id);
                true
            }
            ServerEvent::AdminRequest { request, reply } => {
                let response = self.handle_admin_request(request);
                let _ = reply.send(response);
                true
            }
            ServerEvent::HelloSync { remote_hello } => {
                self.handle_hello_sync(remote_hello);
                true
            }
            ServerEvent::MineTick => {
                if let Err(err) = self.mine_pending_best_effort() {
                    warn!(error = %err, "integrated mining tick failed");
                }
                true
            }
            ServerEvent::Stop => false,
        }
    }

    /// Handles one async peer event against the authoritative server state.
    fn handle_peer_event(
        &mut self,
        _peer_id: &str,
        origin: Option<&RelayOrigin>,
        message: WireMessage,
    ) -> Result<Vec<WireMessage>, ServerError> {
        self.handle_peer_message(origin, message)
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    pub fn state(&self) -> &NodeState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut NodeState {
        &mut self.state
    }

    /// Handles one decoded wire message against the server's current node state.
    ///
    /// If the message mutates the live chain or mempool and SQLite persistence
    /// is enabled, the updated `NodeState` is saved after the protocol action
    /// succeeds.
    pub fn handle_message(
        &mut self,
        message: WireMessage,
    ) -> Result<Vec<WireMessage>, ServerError> {
        self.handle_peer_message(None, message)
    }

    /// Dispatches one decoded peer message to the appropriate server handler.
    ///
    /// Each supported wire message has a dedicated handler so handshake,
    /// queries, announcements, mining, relay, and sync behavior stay easy to
    /// follow as the protocol grows.
    fn handle_peer_message(
        &mut self,
        origin: Option<&RelayOrigin>,
        message: WireMessage,
    ) -> Result<Vec<WireMessage>, ServerError> {
        match message {
            WireMessage::Hello(remote_hello) => self.handle_peer_hello(remote_hello),
            WireMessage::GetTip => self.handle_peer_get_tip(),
            WireMessage::GetBlock { block_hash } => self.handle_peer_get_block(block_hash),
            WireMessage::AnnounceTx { transaction } => {
                self.handle_peer_announce_tx(transaction, origin)
            }
            WireMessage::AnnounceBlock { block } => self.handle_peer_announce_block(block, origin),
            WireMessage::MinePending(request) => self.handle_peer_mine_pending(request, origin),
            WireMessage::Tip(_) | WireMessage::Block { .. } | WireMessage::MinedBlock(_) => {
                Ok(Vec::new())
            }
        }
    }

    /// Handles one incoming `hello`, persists any advertised peer metadata,
    /// and optionally triggers a best-effort catch-up when the remote tip is
    /// ahead of local state.
    fn handle_peer_hello(
        &mut self,
        remote_hello: HelloMessage,
    ) -> Result<Vec<WireMessage>, ServerError> {
        let should_log_info = self.hello_advances_local_state(&remote_hello);
        let remote_node = remote_hello.node_name.as_deref().unwrap_or("unknown");
        let advertised_addr = remote_hello.advertised_addr.as_deref().unwrap_or("none");
        if should_log_info {
            info!(
                remote_node,
                advertised_addr,
                remote_tip = %format_hash(remote_hello.tip),
                remote_height = ?remote_hello.height,
                "received peer hello with potentially useful tip"
            );
        } else {
            debug!(
                remote_node,
                advertised_addr,
                remote_tip = %format_hash(remote_hello.tip),
                remote_height = ?remote_hello.height,
                "received peer hello"
            );
        }
        self.record_inbound_hello(None, &remote_hello)
            .map_err(ServerError::SqlitePersist)?;
        if remote_hello.network != self.config.network {
            return Err(ServerError::Peer(PeerError::NetworkMismatch {
                local: self.config.network.clone(),
                remote: remote_hello.network,
            }));
        }
        if remote_hello.version != PROTOCOL_VERSION {
            return Err(ServerError::Peer(PeerError::UnsupportedVersion(
                remote_hello.version,
            )));
        }
        Ok(vec![WireMessage::Hello(peer::local_hello(
            &self.config,
            &self.state,
        ))])
    }

    /// Runs one follow-up sync after the peer hello reply has already been sent.
    ///
    /// This avoids a handshake deadlock where both sides wait on hello-related
    /// work before either side can continue with the next protocol message.
    fn handle_hello_sync(&mut self, remote_hello: HelloMessage) {
        if !self.hello_advances_local_state(&remote_hello) {
            return;
        }
        let origin = RelayOrigin {
            advertised_addr: remote_hello.advertised_addr.clone(),
            node_name: remote_hello.node_name.clone(),
        };
        for block in self.sync_from_hello_best_effort(&remote_hello) {
            info!(
                block_hash = %format_hash(Some(block.header.block_hash())),
                "relaying block learned during hello-triggered sync"
            );
            self.relay_best_effort(&WireMessage::AnnounceBlock { block }, Some(&origin));
        }
    }

    /// Handles one `get_tip` query by returning the current best-tip summary.
    fn handle_peer_get_tip(&self) -> Result<Vec<WireMessage>, ServerError> {
        Ok(vec![WireMessage::Tip(self.state.tip_summary())])
    }

    /// Handles one `get_block` query by returning the indexed block when known.
    fn handle_peer_get_block(
        &self,
        block_hash: BlockHash,
    ) -> Result<Vec<WireMessage>, ServerError> {
        Ok(vec![WireMessage::Block {
            block: self.state.get_block(&block_hash).cloned(),
        }])
    }

    /// Handles one announced transaction, persists mempool changes, and relays
    /// the transaction only when it was new to this node.
    fn handle_peer_announce_tx(
        &mut self,
        transaction: crate::types::Transaction,
        origin: Option<&RelayOrigin>,
    ) -> Result<Vec<WireMessage>, ServerError> {
        self.handle_state_mutating_peer_message(
            WireMessage::AnnounceTx { transaction },
            origin,
            None,
        )
    }

    /// Handles one announced block, including the best-effort missing-parent
    /// sync path before retrying block validation.
    fn handle_peer_announce_block(
        &mut self,
        block: Block,
        origin: Option<&RelayOrigin>,
    ) -> Result<Vec<WireMessage>, ServerError> {
        let message = WireMessage::AnnounceBlock { block };
        if let Some(recovered) = self.try_handle_announced_block_with_sync(&message, origin)? {
            return Ok(recovered);
        }
        let block_hash = saved_block_hash(&message);
        self.handle_state_mutating_peer_message(message, origin, block_hash)
    }

    /// Handles one `mine_pending` request by mining a block from the current
    /// mempool and relaying the accepted block back out to peers.
    ///
    /// TODO: keep `mine_pending` as a debug/admin-oriented hook only. Real
    /// production mining policy should stay server-owned rather than exposed as
    /// a normal peer message.
    fn handle_peer_mine_pending(
        &mut self,
        request: crate::wire::MinePendingRequest,
        origin: Option<&RelayOrigin>,
    ) -> Result<Vec<WireMessage>, ServerError> {
        let message = WireMessage::MinePending(request.clone());
        log_inbound_message(&message, &self.state);
        let previous_best_tip = self.state.chain().best_tip();
        let previous_mempool_txids = mempool_txids(self.state.mempool());
        let miner = crate::crypto::parse_verifying_key(&request.miner_public_key)
            .ok_or(ServerError::Peer(PeerError::InvalidMinerPublicKey))?;
        let block_hash = self
            .state
            .mine_block(
                request.reward,
                &miner,
                request.uniqueness,
                request.bits,
                request.max_transactions,
            )
            .map_err(PeerError::MiningRejected)
            .map_err(ServerError::Peer)?;
        let replies = vec![WireMessage::MinedBlock(crate::wire::MinedBlock {
            block_hash,
        })];
        log_post_handle_state(&replies, &self.state);
        self.save_sqlite(
            &message,
            None,
            previous_best_tip,
            &previous_mempool_txids,
            &replies,
        )
        .map_err(ServerError::SqlitePersist)?;
        if let Some(relay) = self.relay_message_from_replies(&replies) {
            self.relay_best_effort(&relay, origin);
        }
        Ok(replies)
    }

    /// Applies one transaction or block announcement that mutates live state,
    /// persists the resulting changes, and relays any newly accepted object.
    fn handle_state_mutating_peer_message(
        &mut self,
        message: WireMessage,
        origin: Option<&RelayOrigin>,
        save_block_hash: Option<BlockHash>,
    ) -> Result<Vec<WireMessage>, ServerError> {
        log_inbound_message(&message, &self.state);
        let previous_best_tip = self.state.chain().best_tip();
        let previous_mempool_txids = mempool_txids(self.state.mempool());
        let relay = self.relay_message_before_handle(&message);
        let save_message = message.clone();
        let replies = match message {
            WireMessage::AnnounceTx { transaction } => {
                match self.state.submit_transaction(transaction) {
                    Ok(_) | Err(NodeStateError::Mempool(MempoolError::DuplicateTransaction(_))) => {
                    }
                    Err(err) => return Err(ServerError::Peer(PeerError::TransactionRejected(err))),
                }
                Vec::new()
            }
            WireMessage::AnnounceBlock { block } => {
                self.state
                    .accept_block(block)
                    .map_err(PeerError::BlockRejected)
                    .map_err(ServerError::Peer)?;
                Vec::new()
            }
            _ => unreachable!("state-mutating peer handler only supports tx/block announcements"),
        };
        log_post_handle_state(&replies, &self.state);
        self.save_sqlite(
            &save_message,
            save_block_hash,
            previous_best_tip,
            &previous_mempool_txids,
            &replies,
        )
        .map_err(ServerError::SqlitePersist)?;
        if let Some(relay) = relay.or_else(|| self.relay_message_from_replies(&replies)) {
            self.relay_best_effort(&relay, origin);
        }
        Ok(replies)
    }

    /// Handles announced blocks that arrive before their parents by syncing
    /// missing ancestors from the origin peer and retrying the block once.
    ///
    /// This keeps relay resilient when blocks are announced out of order or
    /// before the local node has finished catching up to that peer. If sync
    /// still does not make the parent available, the original validation error
    /// is returned unchanged.
    fn try_handle_announced_block_with_sync(
        &mut self,
        message: &WireMessage,
        origin: Option<&RelayOrigin>,
    ) -> Result<Option<Vec<WireMessage>>, ServerError> {
        let WireMessage::AnnounceBlock { block } = message else {
            return Ok(None);
        };

        let block_hash = block.header.block_hash();
        let parent_hash = block.header.prev_blockhash;

        let Some(peer) = origin.and_then(|origin| self.sync_peer_from_origin(origin)) else {
            return Ok(None);
        };

        let relay = self.relay_message_before_handle(message);
        log_inbound_message(message, &self.state);
        let previous_best_tip = self.state.chain().best_tip();
        let previous_mempool_txids = mempool_txids(self.state.mempool());

        match peer::handle_message(&self.config, &mut self.state, message.clone()) {
            Ok(replies) => {
                log_post_handle_state(&replies, &self.state);
                self.save_sqlite(
                    message,
                    Some(block_hash),
                    previous_best_tip,
                    &previous_mempool_txids,
                    &replies,
                )
                .map_err(ServerError::SqlitePersist)?;
                if let Some(relay) = relay.or_else(|| self.relay_message_from_replies(&replies)) {
                    self.relay_best_effort(&relay, origin);
                }
                return Ok(Some(replies));
            }
            Err(PeerError::BlockRejected(err))
                if announced_block_needs_ancestor_sync(&err, parent_hash) =>
            {
                info!(
                    peer_addr = %peer.addr,
                    block_hash = %format_hash(Some(block_hash)),
                    parent_hash = %format_hash(Some(parent_hash)),
                    "announced block missing parent locally; syncing ancestors before retry"
                );
            }
            Err(err) => return Err(ServerError::Peer(err)),
        }

        match self.sync_from_peer(&peer) {
            Ok(synced_blocks) => {
                info!(
                    peer_addr = %peer.addr,
                    synced_blocks = synced_blocks.len(),
                    block_hash = %format_hash(Some(block_hash)),
                    "retrying announced block after ancestor sync"
                );
            }
            Err(err) => {
                warn!(
                    peer_addr = %peer.addr,
                    block_hash = %format_hash(Some(block_hash)),
                    error = ?err,
                    "ancestor sync failed for announced block"
                );
            }
        }

        let replies = peer::handle_message(&self.config, &mut self.state, message.clone())
            .map_err(ServerError::Peer)?;
        log_post_handle_state(&replies, &self.state);
        self.save_sqlite(
            message,
            Some(block_hash),
            previous_best_tip,
            &previous_mempool_txids,
            &replies,
        )
        .map_err(ServerError::SqlitePersist)?;
        if let Some(relay) = relay.or_else(|| self.relay_message_from_replies(&replies)) {
            self.relay_best_effort(&relay, origin);
        }
        Ok(Some(replies))
    }

    /// Handles one localhost admin request against the live node state.
    fn handle_admin_request(&mut self, request: AdminRequest) -> AdminResponse {
        match request {
            AdminRequest::GetStatus => {
                let tip = self.state.tip_summary();
                AdminResponse::Status(StatusSummary {
                    tip: tip.tip,
                    height: tip.height,
                    branch_count: self.state.chain().branch_count(),
                    mempool_size: self.state.mempool().len(),
                    peer_count: self.connected_peers.len(),
                    mining_enabled: self.mining.is_some(),
                })
            }
            AdminRequest::GetBalance { public_key } => {
                let Some(owner) = crate::crypto::parse_verifying_key(&public_key) else {
                    return AdminResponse::Error {
                        message: "invalid public key".to_string(),
                    };
                };
                AdminResponse::Balance(BalanceSummary {
                    amount: self.state.balance_for_key(&owner),
                })
            }
            AdminRequest::Bootstrap { public_key, blocks } => {
                if crate::crypto::parse_verifying_key(&public_key).is_none() {
                    return AdminResponse::Error {
                        message: "invalid public key".to_string(),
                    };
                }
                let start_uniqueness = self
                    .state
                    .tip_summary()
                    .height
                    .and_then(|height| u32::try_from(height.saturating_add(1)).ok())
                    .unwrap_or(0);
                let mut last_block_hash = None;
                for offset in 0..blocks {
                    let uniqueness = start_uniqueness.saturating_add(offset);
                    match self.handle_message(WireMessage::MinePending(
                        crate::wire::MinePendingRequest {
                            reward: BLOCK_SUBSIDY,
                            miner_public_key: public_key.clone(),
                            uniqueness,
                            bits: 0x207f_ffff,
                            max_transactions: 0,
                        },
                    )) {
                        Ok(replies) => {
                            last_block_hash = replies.iter().find_map(|reply| match reply {
                                WireMessage::MinedBlock(result) => Some(result.block_hash),
                                _ => None,
                            });
                        }
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
                match self.handle_message(WireMessage::AnnounceTx { transaction }) {
                    Ok(_) => AdminResponse::Submitted { txid },
                    Err(err) => AdminResponse::Error {
                        message: format!("{err:?}"),
                    },
                }
            }
        }
    }

    /// Mines one block from the current mempool when integrated mining is
    /// enabled and there is pending work to confirm.
    ///
    /// This intentionally skips coinbase-only blocks so a quiet testnet node
    /// does not produce an endless stream of empty blocks.
    fn mine_pending_best_effort(&mut self) -> io::Result<()> {
        let Some(mining) = self.mining.as_mut() else {
            return Ok(());
        };
        if self.state.mempool().is_empty() {
            debug!("mine_pending skipped because mempool is empty");
            return Ok(());
        }

        let request = mining.next_request();
        debug!(
            uniqueness = request.uniqueness,
            reward = request.reward,
            max_transactions = request.max_transactions,
            bits = format_args!("{:#010x}", request.bits),
            mempool_size = self.state.mempool().len(),
            "mine_pending: submitting internal mine_pending request"
        );
        let replies = self
            .handle_message(WireMessage::MinePending(request))
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}")))?;
        if let Some(WireMessage::MinedBlock(result)) = replies.first() {
            info!(
                block_hash = %format_hash(Some(result.block_hash)),
                best_tip = %format_hash(self.state.chain().best_tip()),
                "mine_pending accepted block"
            );
        }
        Ok(())
    }

    /// Attempts a one-time startup sync from each configured peer.
    ///
    /// Failures are intentionally swallowed so a node can still come up and
    /// serve local traffic even if some peers are offline or return incomplete
    /// history. This also avoids reconnecting to peers that were contacted very
    /// recently or already advertised that they are not ahead of the local tip.
    pub fn sync_configured_peers_best_effort(&mut self) {
        let peers = self.select_sync_peers();
        for peer in &peers {
            if let Err(err) = self.sync_from_peer(peer) {
                let _ = self.record_peer_connect_failure(peer, &err);
                warn!(peer_addr = %peer.addr, error = ?err, "bootstrap sync from peer failed");
            }
        }
    }

    /// Selects the best currently-known sync candidates from configured peers.
    ///
    /// The first version is intentionally conservative: it picks at most one
    /// peer whose advertised height is ahead of the local node, and it falls
    /// back to one unknown peer only when the node has no useful tip metadata
    /// yet. Recently contacted peers stay on a short cooldown to avoid noisy
    /// reconnect loops.
    fn select_sync_peers(&self) -> Vec<PeerEndpoint> {
        let local_height = self.state.tip_summary().height.unwrap_or(0);
        let mut candidates: Vec<StoredPeer> = self
            .peers
            .values()
            .map(|peer| peer.stored.clone())
            .collect();
        candidates.sort_by(|left, right| {
            right
                .advertised_height
                .unwrap_or(0)
                .cmp(&left.advertised_height.unwrap_or(0))
                .then_with(|| left.addr.cmp(&right.addr))
        });
        if let Some(best) = candidates.into_iter().find(|peer| {
            peer.advertised_height.unwrap_or(0) > local_height && self.peer_is_ready_for_sync(peer)
        }) {
            return vec![best.endpoint()];
        }
        self.peer_endpoints()
            .iter()
            .find(|peer| {
                self.peers
                    .get(&peer.addr)
                    .map(|runtime| runtime.stored.advertised_height.is_none())
                    .unwrap_or(true)
                    && self.peer_endpoint_is_ready_for_sync(peer)
            })
            .cloned()
            .into_iter()
            .collect()
    }

    /// Fetches a missing remote tip segment from `peer` and applies it
    /// parent-first until this node reaches a known ancestor.
    ///
    /// If the remote tip is already indexed locally, this is a no-op. Current
    /// behavior reuses one outbound connection for the tip poll and any needed
    /// block requests in this sync pass. That keeps hello chatter low within
    /// one catch-up operation and also lets later relay traffic reuse the same
    /// connected peer when it stays healthy.
    fn sync_from_peer(&mut self, peer: &PeerEndpoint) -> Result<Vec<Block>, SyncError> {
        info!(peer_addr = %peer.addr, peer_node = ?peer.node_name, "starting sync from peer");
        let remote_tip_summary = self.request_tip_from_peer(peer)?;
        debug!(
            peer_addr = %peer.addr,
            remote_tip = %format_hash(remote_tip_summary.tip),
            remote_height = ?remote_tip_summary.height,
            "sync connection ready"
        );
        let Some(remote_tip) = remote_tip_summary.tip else {
            warn!(
                peer_addr = %peer.addr,
                "peer advertised no tip during sync"
            );
            return Ok(Vec::new());
        };
        if self.state.get_block(&remote_tip).is_some() {
            debug!(
                peer_addr = %peer.addr,
                remote_tip = %format_hash(Some(remote_tip)),
                "remote tip already known"
            );
            return Ok(Vec::new());
        }

        let mut missing_blocks = Vec::new();
        let mut next_hash = remote_tip;
        loop {
            if self.state.get_block(&next_hash).is_some() {
                break;
            }

            debug!(
                peer_addr = %peer.addr,
                block_hash = %format_hash(Some(next_hash)),
                "requesting block during sync"
            );
            let block = self
                .request_block_from_peer(peer, next_hash)?
                .ok_or(SyncError::MissingRemoteBlock(next_hash))?;
            let parent_hash = block.header.prev_blockhash;
            debug!(
                peer_addr = %peer.addr,
                block_hash = %format_hash(Some(block.header.block_hash())),
                parent_hash = %format_hash(Some(parent_hash)),
                "fetched block during sync"
            );
            missing_blocks.push(block);

            if parent_hash == BlockHash::default() {
                break;
            }
            next_hash = parent_hash;
        }

        let mut accepted_blocks = Vec::new();
        for block in missing_blocks.into_iter().rev() {
            if let Some(block) = self.accept_synced_block(block)? {
                accepted_blocks.push(block);
            }
        }

        if !accepted_blocks.is_empty() {
            self.save_full_state().map_err(SyncError::SqlitePersist)?;
        }

        info!(
            peer_addr = %peer.addr,
            accepted_blocks = accepted_blocks.len(),
            new_best_tip = %format_hash(self.state.chain().best_tip()),
            "completed sync from peer"
        );
        Ok(accepted_blocks)
    }

    /// Triggers a one-shot catch-up attempt when an inbound peer advertises a
    /// tip this node does not yet know and includes a listener address we can
    /// dial back for block fetches.
    ///
    /// This is best effort by design. A failed catch-up should not reject the
    /// handshake or stop normal message handling on the current stream.
    fn sync_from_hello_best_effort(&mut self, remote_hello: &HelloMessage) -> Vec<Block> {
        let Some(remote_tip) = remote_hello.tip else {
            return Vec::new();
        };
        if self.state.get_block(&remote_tip).is_some() {
            return Vec::new();
        }
        let Some(peer_addr) = remote_hello.advertised_addr.as_deref() else {
            debug!("remote hello advertised unknown tip without listener address");
            return Vec::new();
        };
        info!(
            peer_addr,
            remote_tip = %format_hash(Some(remote_tip)),
            remote_height = ?remote_hello.height,
            "starting hello-triggered sync"
        );
        let peer = PeerEndpoint::new(peer_addr.to_string(), remote_hello.node_name.clone());
        self.sync_from_peer(&peer).unwrap_or_default()
    }

    /// Returns whether one inbound hello advertises chain data this node does
    /// not already know, which makes it worth logging at info level and trying
    /// a follow-up sync.
    fn hello_advances_local_state(&self, remote_hello: &HelloMessage) -> bool {
        remote_hello
            .tip
            .is_some_and(|tip| self.state.get_block(&tip).is_none())
            || remote_hello.height.unwrap_or(0) > self.state.tip_summary().height.unwrap_or(0)
    }

    /// Applies one fetched block if it is not already indexed locally.
    fn accept_synced_block(&mut self, block: Block) -> Result<Option<Block>, SyncError> {
        let block_hash = block.header.block_hash();
        if self.state.get_block(&block_hash).is_some() {
            return Ok(None);
        }
        self.state
            .accept_block(block.clone())
            .map_err(SyncError::AcceptBlock)?;
        Ok(Some(block))
    }

    fn save_sqlite(
        &self,
        message: &WireMessage,
        request_block_hash: Option<BlockHash>,
        previous_best_tip: Option<BlockHash>,
        previous_mempool_txids: &HashSet<Txid>,
        replies: &[WireMessage],
    ) -> Result<(), SqliteStoreError> {
        let Some(store) = self.sqlite_store.as_ref() else {
            return Ok(());
        };
        if let WireMessage::AnnounceTx { transaction } = message {
            return store.save_mempool_transaction(transaction);
        }
        let Some(block_hash) = request_block_hash.or_else(|| mined_block_hash(replies)) else {
            // Non-block mutations that still changed local state keep using the
            // broader table-level saves until they get narrower helpers too.
            store.save_active_utxos(self.state.active_utxos())?;
            store.save_mempool(self.state.mempool())?;
            return Ok(());
        };
        if block_save_requires_full_snapshot(
            &self.state,
            block_hash,
            previous_best_tip,
            previous_mempool_txids,
        ) {
            // Reorgs and tip changes that prune unrelated mempool entries are
            // still subtle enough that we prefer the authoritative full save.
            return self.save_full_state();
        }
        // Simple best-tip extension: save just the accepted block effects.
        store.save_accepted_block(&self.state, block_hash)
    }

    /// Persists the full current node state when a bulk sync changed local history.
    fn save_full_state(&self) -> Result<(), SqliteStoreError> {
        let Some(store) = self.sqlite_store.as_ref() else {
            return Ok(());
        };
        store.save_node_state(&self.state)
    }

    /// Announces an accepted object to configured peers except the one that
    /// originated it on the current stream, when that peer identity is known.
    fn relay_best_effort(&mut self, message: &WireMessage, origin: Option<&RelayOrigin>) {
        for peer in self.peer_endpoints() {
            if peer_matches_origin(&peer, origin) {
                debug!(peer_addr = %peer.addr, "skipping relay to origin peer");
                continue;
            }
            debug!(
                peer_addr = %peer.addr,
                message = wire_message_name(message),
                "relaying message to peer"
            );
            let _ = self.announce_to_peer_best_effort(&peer, message);
        }
    }

    /// Decides whether an inbound message should be relayed if local handling succeeds.
    ///
    /// Relay happens only when this server does not already know the announced
    /// object. That keeps duplicate gossip idempotent and avoids forwarding an
    /// object we already had before this peer sent it.
    fn relay_message_before_handle(&self, message: &WireMessage) -> Option<WireMessage> {
        match message {
            WireMessage::AnnounceTx { transaction } => {
                if self.state.mempool().get(&transaction.txid()).is_some() {
                    None
                } else {
                    Some(message.clone())
                }
            }
            WireMessage::AnnounceBlock { block } => {
                if self.state.get_block(&block.header.block_hash()).is_some() {
                    None
                } else {
                    Some(message.clone())
                }
            }
            _ => None,
        }
    }

    /// Builds any relay message implied by successful local side effects.
    ///
    /// `mine_pending` returns only the mined block hash on the wire, so the
    /// server resolves that hash back into the concrete block from local state
    /// before announcing it to peers.
    fn relay_message_from_replies(&self, replies: &[WireMessage]) -> Option<WireMessage> {
        replies.iter().find_map(|reply| match reply {
            WireMessage::MinedBlock(result) => self
                .state
                .get_block(&result.block_hash)
                .cloned()
                .map(|block| WireMessage::AnnounceBlock { block }),
            _ => None,
        })
    }

    /// Resolves the best peer address to call back when an inbound stream
    /// announced an object that requires follow-up sync.
    ///
    /// The advertised listener address is preferred because it is the most
    /// direct way to reconnect to the exact peer that sent the object. When
    /// absent, a configured peer entry with the same node name is used.
    fn sync_peer_from_origin(&self, origin: &RelayOrigin) -> Option<PeerEndpoint> {
        if let Some(addr) = origin.advertised_addr.as_ref() {
            return Some(PeerEndpoint::new(addr.clone(), origin.node_name.clone()));
        }
        let node_name = origin.node_name.as_deref()?;
        self.peers
            .values()
            .find(|peer| peer.endpoint.node_name.as_deref() == Some(node_name))
            .map(|peer| peer.endpoint.clone())
    }

    /// Records peer metadata learned from one inbound `hello`.
    fn record_inbound_hello(
        &mut self,
        peer_addr: Option<std::net::SocketAddr>,
        remote_hello: &HelloMessage,
    ) -> Result<(), SqliteStoreError> {
        if self.sqlite_store.is_none() {
            return Ok(());
        }
        let addr = remote_hello
            .advertised_addr
            .clone()
            .or_else(|| peer_addr.map(|addr| addr.to_string()))
            .unwrap_or_else(|| "unknown".to_string());
        self.update_persisted_peer(
            &PeerEndpoint::new(addr, remote_hello.node_name.clone()),
            PeerSource::Hello,
            |peer| {
                peer.advertised_tip_hash = remote_hello.tip;
                peer.advertised_height = remote_hello.height;
                peer.last_hello_at = Some(now_timestamp_string());
                peer.last_seen_at = Some(now_timestamp_string());
            },
        )
    }

    /// Records the start of one outbound sync attempt to a configured peer.
    fn record_peer_connect_attempt(&mut self, peer: &PeerEndpoint) -> Result<(), SqliteStoreError> {
        if self.sqlite_store.is_none() {
            return Ok(());
        }
        self.update_persisted_peer(peer, PeerSource::Seed, |stored| {
            stored.last_connect_at = Some(now_timestamp_string());
        })
    }

    /// Records a successful outbound handshake and the peer's advertised tip.
    fn record_peer_connect_success(
        &mut self,
        peer: &PeerEndpoint,
        remote_hello: &HelloMessage,
    ) -> Result<(), SqliteStoreError> {
        if self.sqlite_store.is_none() {
            return Ok(());
        }
        self.update_persisted_peer(peer, PeerSource::Seed, |stored| {
            stored.node_name = remote_hello
                .node_name
                .clone()
                .or_else(|| stored.node_name.clone());
            stored.advertised_tip_hash = remote_hello.tip;
            stored.advertised_height = remote_hello.height;
            stored.last_hello_at = Some(now_timestamp_string());
            stored.last_seen_at = Some(now_timestamp_string());
            stored.last_connect_at = Some(now_timestamp_string());
            stored.last_success_at = Some(now_timestamp_string());
            stored.last_error = None;
            stored.connections = stored.connections.saturating_add(1);
        })
    }

    /// Records a failed outbound sync attempt for one configured peer.
    fn record_peer_connect_failure(
        &mut self,
        peer: &PeerEndpoint,
        err: &SyncError,
    ) -> Result<(), SqliteStoreError> {
        if self.sqlite_store.is_none() {
            return Ok(());
        }
        self.update_persisted_peer(peer, PeerSource::Seed, |stored| {
            stored.last_connect_at = Some(now_timestamp_string());
            stored.last_error = Some(format!("{err:?}"));
            stored.failed_connections = stored.failed_connections.saturating_add(1);
        })
    }

    /// Loads, updates, and rewrites the persisted peer set with one upserted peer.
    fn update_persisted_peer<F>(
        &mut self,
        endpoint: &PeerEndpoint,
        source: PeerSource,
        update: F,
    ) -> Result<(), SqliteStoreError>
    where
        F: FnOnce(&mut StoredPeer),
    {
        let store = self
            .sqlite_store
            .as_ref()
            .expect("peer persistence requires an open sqlite store");
        let mut peer = store
            .load_peer(&endpoint.addr)?
            .unwrap_or_else(|| StoredPeer::from_endpoint(endpoint.clone(), source));
        if endpoint.node_name.is_some() {
            peer.node_name = endpoint.node_name.clone();
        }
        update(&mut peer);
        self.peers
            .entry(peer.addr.clone())
            .and_modify(|runtime| {
                runtime.endpoint = peer.endpoint();
                runtime.stored = peer.clone();
            })
            .or_insert_with(|| RuntimePeer {
                endpoint: peer.endpoint(),
                stored: peer.clone(),
                connection: None,
            });
        store.save_peer(&peer)
    }

    /// Returns whether the local server should spend one periodic sync attempt
    /// on this peer now, rather than immediately retrying a recent connection.
    fn peer_is_ready_for_sync(&self, peer: &StoredPeer) -> bool {
        peer.last_connect_at
            .as_deref()
            .and_then(parse_timestamp_string)
            .map(|seconds| {
                current_unix_seconds().saturating_sub(seconds)
                    >= CONFIGURED_PEER_SYNC_COOLDOWN.as_secs()
            })
            .unwrap_or(true)
    }

    /// Applies the same sync cooldown check starting from a configured endpoint.
    fn peer_endpoint_is_ready_for_sync(&self, peer: &PeerEndpoint) -> bool {
        self.peers
            .get(&peer.addr)
            .map(|runtime| self.peer_is_ready_for_sync(&runtime.stored))
            .unwrap_or(true)
    }

    /// Ensures the runtime peer map has one entry for `endpoint`.
    fn ensure_runtime_peer(&mut self, endpoint: &PeerEndpoint, source: PeerSource) {
        self.peers
            .entry(endpoint.addr.clone())
            .or_insert_with(|| RuntimePeer {
                endpoint: endpoint.clone(),
                stored: StoredPeer::from_endpoint(endpoint.clone(), source),
                connection: None,
            });
    }

    /// Opens and caches one outbound async peer connection if none is live.
    ///
    /// On success the connection stays attached to the runtime peer record for
    /// later relay and sync requests. Failed connects still update peer
    /// metadata so selection logic can back off noisy peers.
    fn ensure_peer_connection(&mut self, peer: &PeerEndpoint) -> Result<(), SyncError> {
        self.ensure_runtime_peer(peer, PeerSource::Seed);
        if self
            .peers
            .get(&peer.addr)
            .and_then(|runtime| runtime.connection.as_ref())
            .is_some()
        {
            return Ok(());
        }
        let _ = self.record_peer_connect_attempt(peer);
        let runtime = self
            .runtime_handle
            .as_ref()
            .expect("async runtime handle should exist before outbound peer use")
            .clone();
        let local_hello = peer::local_hello(&self.config, &self.state);
        match peer::connect_peer(&runtime, peer.addr.clone(), local_hello) {
            Ok((connection, remote_hello)) => {
                self.record_peer_connect_success(peer, &remote_hello)
                    .map_err(SyncError::SqlitePersist)?;
                self.peers
                    .get_mut(&peer.addr)
                    .expect("runtime peer should exist before caching connection")
                    .connection = Some(connection);
                Ok(())
            }
            Err(err) => {
                let sync_err = SyncError::Handshake(err);
                let _ = self.record_peer_connect_failure(peer, &sync_err);
                Err(sync_err)
            }
        }
    }

    /// Drops one cached outbound connection after a transport error so the next
    /// request can reconnect cleanly.
    fn clear_peer_connection(&mut self, peer_addr: &str) {
        if let Some(runtime_peer) = self.peers.get_mut(peer_addr) {
            if let Some(connection) = runtime_peer.connection.take() {
                connection.shutdown();
            }
        }
    }

    /// Sends one `get_tip` request, reconnecting once if a cached connection died.
    fn request_tip_from_peer(&mut self, peer: &PeerEndpoint) -> Result<TipSummary, SyncError> {
        for attempt in 0..2 {
            self.ensure_peer_connection(peer)?;
            let request = {
                let connection = self
                    .peers
                    .get_mut(&peer.addr)
                    .and_then(|runtime| runtime.connection.as_ref())
                    .expect("connection should exist after ensure_peer_connection");
                connection.request_tip()
            };
            match request {
                Ok(summary) => {
                    if self.sqlite_store.is_some() {
                        self.update_persisted_peer(peer, PeerSource::Seed, |stored| {
                            stored.advertised_tip_hash = summary.tip;
                            stored.advertised_height = summary.height;
                            stored.last_seen_at = Some(now_timestamp_string());
                            stored.last_error = None;
                        })
                        .map_err(SyncError::SqlitePersist)?;
                    } else if let Some(runtime_peer) = self.peers.get_mut(&peer.addr) {
                        runtime_peer.stored.advertised_tip_hash = summary.tip;
                        runtime_peer.stored.advertised_height = summary.height;
                        runtime_peer.stored.last_seen_at = Some(now_timestamp_string());
                        runtime_peer.stored.last_error = None;
                    }
                    return Ok(summary);
                }
                Err(_err) if attempt == 0 => self.clear_peer_connection(&peer.addr),
                Err(err) => return Err(SyncError::Request(err)),
            }
        }
        unreachable!("peer tip request should return or error within bounded retries");
    }

    /// Requests one block from a cached connection, reconnecting once after a
    /// broken transport so long sync walks do not redial for every block.
    fn request_block_from_peer(
        &mut self,
        peer: &PeerEndpoint,
        block_hash: BlockHash,
    ) -> Result<Option<Block>, SyncError> {
        for attempt in 0..2 {
            self.ensure_peer_connection(peer)?;
            let request = {
                let connection = self
                    .peers
                    .get_mut(&peer.addr)
                    .and_then(|runtime| runtime.connection.as_ref())
                    .expect("connection should exist after ensure_peer_connection");
                connection.request_block(block_hash)
            };
            match request {
                Ok(block) => return Ok(block),
                Err(_err) if attempt == 0 => self.clear_peer_connection(&peer.addr),
                Err(err) => return Err(SyncError::Request(err)),
            }
        }
        unreachable!("peer block request should return or error within bounded retries");
    }

    /// Announces one object to a peer, reconnecting once if needed.
    ///
    /// Relay currently closes the outbound connection after each announcement.
    /// `announce_tx` and `announce_block` are one-way messages, so without an
    /// explicit ack there is no strong liveness signal that a reused socket is
    /// still attached to the remote server after a restart.
    fn announce_to_peer_best_effort(
        &mut self,
        peer: &PeerEndpoint,
        message: &WireMessage,
    ) -> io::Result<()> {
        let mut last_error = None;
        for attempt in 0..RELAY_CONNECT_ATTEMPTS {
            match self.announce_to_peer_once(peer, message) {
                Ok(()) => {
                    self.clear_peer_connection(&peer.addr);
                    return Ok(());
                }
                Err(err) => {
                    last_error = Some(err);
                    self.clear_peer_connection(&peer.addr);
                    if attempt + 1 < RELAY_CONNECT_ATTEMPTS {
                        thread::sleep(RELAY_CONNECT_RETRY_DELAY);
                    }
                }
            }
        }
        Err(last_error.expect("relay attempts should record an error"))
    }

    /// Sends one announcement over the peer's cached outbound connection.
    fn announce_to_peer_once(
        &mut self,
        peer: &PeerEndpoint,
        message: &WireMessage,
    ) -> io::Result<()> {
        self.ensure_peer_connection(peer)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{err:?}")))?;
        let connection = self
            .peers
            .get_mut(&peer.addr)
            .and_then(|runtime| runtime.connection.as_ref())
            .expect("connection should exist after ensure_peer_connection");
        connection.announce(message)
    }

    /// Returns the configured peer endpoints in a stable address order.
    fn peer_endpoints(&self) -> Vec<PeerEndpoint> {
        let mut peers: Vec<_> = self
            .peers
            .values()
            .map(|peer| peer.endpoint.clone())
            .collect();
        peers.sort_by(|left, right| left.addr.cmp(&right.addr));
        peers
    }

    /// Disconnects this server from all currently cached outbound peers.
    ///
    /// This gives callers an explicit way to tear down long-lived peer
    /// sessions without relying on server drop timing. The current testnet
    /// harness uses it to make multi-node teardown deterministic.
    pub fn disconnect(&mut self) {
        for runtime_peer in self.peers.values_mut() {
            if let Some(connection) = runtime_peer.connection.take() {
                connection.shutdown();
            }
        }
    }
}

/// Runs one async admin transport and forwards requests into the server event loop.
async fn run_admin(handle: ServerHandle, stream: tokio::net::TcpStream) -> io::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await?;
        if bytes_read == 0 {
            return Ok(());
        }
        let request: AdminRequest = serde_json::from_str(line.trim_end_matches('\n'))
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let response = handle
            .request_admin(request)
            .await
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, format!("{err:?}")))?;
        let mut response_line = serde_json::to_string(&response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        response_line.push('\n');
        tokio::io::AsyncWriteExt::write_all(&mut writer, response_line.as_bytes()).await?;
        tokio::io::AsyncWriteExt::flush(&mut writer).await?;
    }
}

/// Accepts one optional admin connection without duplicating select branches.
async fn accept_optional_admin(
    admin_listener: Option<&tokio::net::TcpListener>,
) -> io::Result<Option<tokio::net::TcpStream>> {
    let Some(admin_listener) = admin_listener else {
        std::future::pending::<()>().await;
        unreachable!();
    };
    let (stream, _) = admin_listener.accept().await?;
    Ok(Some(stream))
}

/// Spawns one async peer send/recv task owned by the active server runtime.
fn spawn_peer_task(
    peers: &mut HashMap<String, LivePeerTask>,
    handle: ServerHandle,
    peer_id: String,
    stream: tokio::net::TcpStream,
) {
    let peer_id_for_task = peer_id.clone();
    let worker = tokio::spawn(async move {
        let _ = handle.notify_peer_connected(peer_id_for_task.clone()).await;
        peer::serve_peer_stream(peer_id_for_task.clone(), handle.clone(), stream).await;
        let _ = handle.notify_peer_disconnected(peer_id_for_task).await;
    });
    peers.insert(peer_id, LivePeerTask { worker });
}

/// Spawns one async admin transport task owned by the active server runtime.
fn spawn_admin_task(handle: ServerHandle, stream: tokio::net::TcpStream) {
    tokio::spawn(async move {
        let _ = run_admin(handle, stream).await;
    });
}

/// Returns a small stable timestamp string for persisted peer metadata.
fn now_timestamp_string() -> String {
    format_timestamp_string(current_unix_seconds())
}

/// Returns the current unix timestamp as seconds for peer metadata bookkeeping.
fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_secs()
}

/// Formats one unix-seconds timestamp in the compact persisted peer format.
fn format_timestamp_string(seconds: u64) -> String {
    format!("unix:{seconds}")
}

/// Parses one compact persisted peer timestamp string.
fn parse_timestamp_string(value: &str) -> Option<u64> {
    value.strip_prefix("unix:")?.parse().ok()
}

fn peer_matches_origin(peer: &PeerEndpoint, origin: Option<&RelayOrigin>) -> bool {
    let Some(origin) = origin else {
        return false;
    };
    if let Some(origin_addr) = origin.advertised_addr.as_deref() {
        return peer.addr == origin_addr;
    }
    peer.node_name.as_deref() == origin.node_name.as_deref()
}

fn saved_block_hash(message: &WireMessage) -> Option<BlockHash> {
    match message {
        WireMessage::AnnounceBlock { block } => Some(block.header.block_hash()),
        _ => None,
    }
}

/// Returns whether one accepted block should fall back to a full-state save.
fn block_save_requires_full_snapshot(
    state: &NodeState,
    block_hash: BlockHash,
    previous_best_tip: Option<BlockHash>,
    previous_mempool_txids: &HashSet<Txid>,
) -> bool {
    let Some(entry) = state.chain().get(&block_hash) else {
        return true;
    };
    if state.chain().best_tip() != Some(block_hash) {
        return false;
    }
    let extends_previous_tip = match (previous_best_tip, entry.parent) {
        (None, None) => true,
        (Some(previous), Some(parent)) => parent == previous,
        _ => false,
    };
    if !extends_previous_tip {
        // Switching to a different branch is still handled by the full snapshot path.
        return true;
    }
    let Some(block) = state.get_block(&block_hash) else {
        return true;
    };
    let current_mempool_txids = mempool_txids(state.mempool());
    let removed_txids: HashSet<Txid> = previous_mempool_txids
        .difference(&current_mempool_txids)
        .copied()
        .collect();
    let included_txids: HashSet<Txid> = block
        .transactions
        .iter()
        .skip(1)
        .map(|transaction| transaction.txid())
        .collect();
    // If the tip change pruned anything beyond the block's own confirmed
    // transactions, fall back to the broader save path.
    removed_txids != included_txids
}

/// Returns the current mempool transaction ids as a set for save-path decisions.
fn mempool_txids(mempool: &crate::mempool::Mempool) -> HashSet<Txid> {
    mempool.iter().map(|(txid, _)| *txid).collect()
}

fn mined_block_hash(replies: &[WireMessage]) -> Option<BlockHash> {
    replies.iter().find_map(|reply| match reply {
        WireMessage::MinedBlock(result) => Some(result.block_hash),
        _ => None,
    })
}

/// Returns whether an announced block rejection should trigger ancestor sync.
///
/// Two local validation errors are recoverable here:
/// - `MissingParent`, when the node already has a chain but not this branch
/// - `GenesisHasParent`, when an empty node hears about a child block before
///   learning the remote genesis
fn announced_block_needs_ancestor_sync(err: &NodeStateError, parent_hash: BlockHash) -> bool {
    (matches!(err, NodeStateError::Chain(ChainError::MissingParent(_)))
        && parent_hash != BlockHash::default())
        || (matches!(err, NodeStateError::Chain(ChainError::GenesisHasParent))
            && parent_hash != BlockHash::default())
}

/// Emits a concise log event for one inbound message before it is handled.
fn log_inbound_message(message: &WireMessage, state: &NodeState) {
    match message {
        WireMessage::GetTip => debug!(
            best_tip = %format_hash(state.chain().best_tip()),
            "handling get_tip"
        ),
        WireMessage::GetBlock { block_hash } => debug!(
            block_hash = %format_hash(Some(*block_hash)),
            "handling get_block"
        ),
        WireMessage::AnnounceTx { transaction } => info!(
            txid = %transaction.txid(),
            input_count = transaction.inputs.len(),
            output_count = transaction.outputs.len(),
            "handling announced transaction"
        ),
        WireMessage::AnnounceBlock { block } => info!(
            block_hash = %format_hash(Some(block.header.block_hash())),
            tx_count = block.transactions.len(),
            "handling announced block"
        ),
        WireMessage::MinePending(request) => info!(
            reward = request.reward,
            max_transactions = request.max_transactions,
            bits = format_args!("{:#010x}", request.bits),
            "handling mine_pending request"
        ),
        WireMessage::Hello(_)
        | WireMessage::Tip(_)
        | WireMessage::Block { .. }
        | WireMessage::MinedBlock(_) => {}
    }
}

/// Emits a concise log event after a state transition succeeds.
fn log_post_handle_state(replies: &[WireMessage], state: &NodeState) {
    if let Some(WireMessage::MinedBlock(result)) = replies.first() {
        info!(
            block_hash = %format_hash(Some(result.block_hash)),
            best_tip = %format_hash(state.chain().best_tip()),
            mempool_size = state.mempool().len(),
            "mined block"
        );
    }
}

/// Returns a short stable label for a wire message variant.
fn wire_message_name(message: &WireMessage) -> &'static str {
    match message {
        WireMessage::Hello(_) => "hello",
        WireMessage::GetTip => "get_tip",
        WireMessage::Tip(_) => "tip",
        WireMessage::AnnounceTx { .. } => "announce_tx",
        WireMessage::AnnounceBlock { .. } => "announce_block",
        WireMessage::GetBlock { .. } => "get_block",
        WireMessage::Block { .. } => "block",
        WireMessage::MinePending(_) => "mine_pending",
        WireMessage::MinedBlock(_) => "mined_block",
    }
}

/// Formats an optional block hash for structured logs.
fn format_hash(hash: Option<BlockHash>) -> String {
    hash.map(|hash| hash.to_string())
        .unwrap_or_else(|| "none".to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        net::TcpListener,
        path::PathBuf,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
        net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream},
    };

    use crate::{
        admin::{AdminRequest, AdminResponse},
        chain::ChainError,
        crypto,
        mining::MiningConfig,
        net,
        node_state::{NodeState, NodeStateError},
        peer::relay_to_peer,
        peers::{PeerSource, StoredPeer},
        server::{PeerEndpoint, Server, ServerConfig, ServerHandle},
        sqlite_store::SqliteStore,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
        wire::{HelloMessage, PROTOCOL_VERSION, TipSummary, WireMessage},
    };

    use super::{RelayOrigin, current_unix_seconds, format_timestamp_string, peer_matches_origin};

    fn temp_sqlite_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-server-test-{}-{}.sqlite",
            std::process::id(),
            nanos
        ));
        path
    }

    fn server_config(node_name: Option<&str>) -> ServerConfig {
        server_config_at(node_name, "127.0.0.1:9000")
    }

    fn server_config_at(node_name: Option<&str>, listen_addr: &str) -> ServerConfig {
        ServerConfig::new(
            "wobble-local",
            node_name.map(|name| name.to_string()),
            listen_addr,
        )
    }

    struct TestRuntime {
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Drop for TestRuntime {
        fn drop(&mut self) {
            if let Some(shutdown_tx) = self.shutdown_tx.take() {
                let _ = shutdown_tx.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn attach_runtime(server: &mut Server) -> TestRuntime {
        let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let handle = runtime.handle().clone();
            handle_tx.send(handle).unwrap();
            runtime.block_on(async {
                let _ = shutdown_rx.await;
            });
        });
        server.runtime_handle = Some(handle_rx.recv().unwrap());
        TestRuntime {
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

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

    fn mine_block_with_transactions(
        prev_blockhash: BlockHash,
        bits: u32,
        miner: &ed25519_dalek::VerifyingKey,
        uniqueness: u32,
        mut transactions: Vec<Transaction>,
    ) -> Block {
        let mut full_transactions = Vec::with_capacity(transactions.len() + 1);
        full_transactions.push(coinbase(50, miner, uniqueness));
        full_transactions.append(&mut transactions);

        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions: full_transactions,
        };
        block.header.merkle_root = block.merkle_root();

        loop {
            if crate::consensus::validate_block(&block).is_ok() {
                return block;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
    }

    fn spend(
        previous_output: OutPoint,
        signer: &ed25519_dalek::SigningKey,
        recipient: &ed25519_dalek::VerifyingKey,
        value: u64,
        uniqueness: u32,
    ) -> Transaction {
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output,
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value,
                locking_data: crypto::verifying_key_bytes(recipient).to_vec(),
            }],
            lock_time: uniqueness,
        };
        tx.inputs[0].unlocking_data = crypto::sign_message(signer, &tx.signing_digest()).to_vec();
        tx
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_relays_announced_transaction_to_configured_peer() {
        let sender = crypto::signing_key_from_bytes([61; 32]);
        let recipient = crypto::signing_key_from_bytes([62; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let transaction = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();

        let remote_listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = remote_listener.local_addr().unwrap();
        let server_addr = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let server = Server::new(
            server_config(Some("alpha"))
                .with_advertised_addr(server_addr.to_string())
                .with_channel_capacity(4),
            state,
        )
        .with_peers(vec![PeerEndpoint::new(
            remote_addr.to_string(),
            Some("remote".to_string()),
        )]);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let serve_task = tokio::spawn(async move {
            server
                .serve_inner(Some(ready_tx))
                .await
                .unwrap()
        });

        let remote_task = tokio::spawn(async move {
            let (stream, _) = remote_listener.accept().await.unwrap();
            let mut reader = AsyncBufReader::new(stream);
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).await.unwrap();
            let hello = WireMessage::from_json_line(&hello_line).unwrap();
            let WireMessage::Hello(_) = hello else {
                panic!("expected hello");
            };
            let mut stream = reader.into_inner();
            let response = WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION,
                node_name: Some("remote".to_string()),
                advertised_addr: None,
                tip: None,
                height: None,
            });
            stream
                .write_all(response.to_json_line().unwrap().as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
            let mut reader = AsyncBufReader::new(stream);
            let mut tx_line = String::new();
            reader.read_line(&mut tx_line).await.unwrap();
            WireMessage::from_json_line(&tx_line).unwrap()
        });

        let mut client = loop {
            match TokioTcpStream::connect(server_addr).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let hello = WireMessage::Hello(HelloMessage {
            network: "wobble-local".to_string(),
            version: PROTOCOL_VERSION,
            node_name: Some("client".to_string()),
            advertised_addr: None,
            tip: None,
            height: None,
        });
        client
            .write_all(hello.to_json_line().unwrap().as_bytes())
            .await
            .unwrap();
        client.flush().await.unwrap();
        let mut reader = AsyncBufReader::new(client);
        let mut hello_reply = String::new();
        reader.read_line(&mut hello_reply).await.unwrap();
        let mut client = reader.into_inner();
        client
            .write_all(
                WireMessage::AnnounceTx {
                    transaction: transaction.clone(),
                }
                .to_json_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
        drop(client);

        let relayed = remote_task.await.unwrap();
        assert_eq!(
            relayed,
            WireMessage::AnnounceTx {
                transaction: transaction.clone(),
            }
        );

        let stop_handle: ServerHandle = ready_rx.recv().unwrap();
        stop_handle.stop();
        let final_state = serve_task.await.unwrap();
        assert!(final_state.mempool().get(&transaction.txid()).is_some());
    }

    #[test]
    fn bootstrap_uniqueness_advances_with_existing_height() {
        let owner = crypto::signing_key_from_bytes([8; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let mut server = Server::new(server_config(None), NodeState::new());
        server.state_mut().accept_block(genesis).unwrap();

        let response = server.handle_admin_request(AdminRequest::Bootstrap {
            public_key: crypto::verifying_key_bytes(&owner.verifying_key()).to_vec(),
            blocks: 1,
        });

        let AdminResponse::Bootstrapped(summary) = response else {
            panic!("expected bootstrapped response");
        };
        let block_hash = summary
            .last_block_hash
            .expect("bootstrap should mine one block");
        let block = server
            .state()
            .get_block(&block_hash)
            .expect("mined block should be indexed");

        assert_eq!(block.transactions[0].lock_time, 1);
    }

    #[test]
    fn persists_mempool_to_sqlite_after_transaction_message() {
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let transaction = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let txid = transaction.txid();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let sqlite_path = temp_sqlite_path();
        let mut server = Server::new(server_config(None), state).with_sqlite_path(&sqlite_path);

        server
            .handle_message(WireMessage::AnnounceTx { transaction })
            .unwrap();

        let store = SqliteStore::open(&sqlite_path).unwrap();
        let loaded_mempool = store.load_mempool().unwrap();
        drop(store);
        fs::remove_file(&sqlite_path).unwrap();

        assert_eq!(
            loaded_mempool.get(&txid),
            server.state().mempool().get(&txid)
        );
    }

    #[test]
    fn persists_block_metadata_and_active_utxos_to_sqlite() {
        let owner = crypto::signing_key_from_bytes([1; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let genesis_hash = genesis.header.block_hash();
        let sqlite_path = temp_sqlite_path();
        let mut server =
            Server::new(server_config(None), NodeState::new()).with_sqlite_path(&sqlite_path);

        server
            .handle_message(WireMessage::AnnounceBlock {
                block: genesis.clone(),
            })
            .unwrap();

        let store = SqliteStore::open(&sqlite_path).unwrap();
        let loaded_block = store.load_block(genesis_hash).unwrap();
        let loaded_entry = store.load_chain_entry(genesis_hash).unwrap();
        let loaded_best_tip = store.load_best_tip().unwrap();
        let loaded_utxos = store.load_active_utxos().unwrap();
        drop(store);
        fs::remove_file(&sqlite_path).unwrap();

        assert_eq!(loaded_block, Some(genesis));
        assert_eq!(
            loaded_entry,
            server.state().chain().get(&genesis_hash).cloned()
        );
        assert_eq!(loaded_best_tip, server.state().chain().best_tip());
        assert_eq!(loaded_utxos.len(), server.state().active_utxos().len());
    }

    #[test]
    fn reorg_block_falls_back_to_full_snapshot_save() {
        let miner_a = crypto::signing_key_from_bytes([11; 32]);
        let miner_b = crypto::signing_key_from_bytes([12; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &miner_a.verifying_key(),
            0,
        );
        let genesis_hash = genesis.header.block_hash();
        let easy_child = mine_block(genesis_hash, 0x207f_ffff, &miner_a.verifying_key(), 1);
        let easy_child_hash = easy_child.header.block_hash();
        let harder_child = mine_block(genesis_hash, 0x201f_ffff, &miner_b.verifying_key(), 2);
        let harder_child_hash = harder_child.header.block_hash();
        let sqlite_path = temp_sqlite_path();
        let mut server =
            Server::new(server_config(None), NodeState::new()).with_sqlite_path(&sqlite_path);

        server
            .handle_message(WireMessage::AnnounceBlock { block: genesis })
            .unwrap();
        server
            .handle_message(WireMessage::AnnounceBlock { block: easy_child })
            .unwrap();
        server
            .handle_message(WireMessage::AnnounceBlock {
                block: harder_child.clone(),
            })
            .unwrap();

        let store = SqliteStore::open(&sqlite_path).unwrap();
        let reloaded = store.load_node_state().unwrap();
        let persisted_harder = store.load_block(harder_child_hash).unwrap();
        let persisted_tip = store.load_best_tip().unwrap();
        drop(store);
        fs::remove_file(&sqlite_path).unwrap();

        assert_eq!(server.state().chain().best_tip(), Some(harder_child_hash));
        assert_ne!(easy_child_hash, harder_child_hash);
        assert_eq!(persisted_tip, Some(harder_child_hash));
        assert_eq!(persisted_harder, Some(harder_child));
        assert_eq!(reloaded.chain().best_tip(), Some(harder_child_hash));
        assert_eq!(
            reloaded.active_utxos().len(),
            server.state().active_utxos().len()
        );
        assert_eq!(
            reloaded.active_outpoints(),
            server.state().active_outpoints()
        );
        assert_eq!(reloaded.mempool().len(), server.state().mempool().len());
    }

    #[test]
    fn block_that_prunes_unrelated_mempool_tx_falls_back_to_full_snapshot_save() {
        let sender = crypto::signing_key_from_bytes([21; 32]);
        let recipient_a = crypto::signing_key_from_bytes([22; 32]);
        let recipient_b = crypto::signing_key_from_bytes([23; 32]);
        let miner = crypto::signing_key_from_bytes([24; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let pending = spend(spendable, &sender, &recipient_a.verifying_key(), 30, 1);
        let confirmed = spend(spendable, &sender, &recipient_b.verifying_key(), 30, 2);
        let confirmed_txid = confirmed.txid();
        let child = mine_block_with_transactions(
            genesis.header.block_hash(),
            0x207f_ffff,
            &miner.verifying_key(),
            3,
            vec![confirmed.clone()],
        );
        let child_hash = child.header.block_hash();
        let sqlite_path = temp_sqlite_path();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state.submit_transaction(pending.clone()).unwrap();
        let mut server = Server::new(server_config(None), state).with_sqlite_path(&sqlite_path);
        server.save_full_state().unwrap();

        server
            .handle_message(WireMessage::AnnounceBlock { block: child })
            .unwrap();

        let store = SqliteStore::open(&sqlite_path).unwrap();
        let reloaded = store.load_node_state().unwrap();
        let persisted_tip = store.load_best_tip().unwrap();
        let persisted_pending = store.load_mempool_transaction(pending.txid()).unwrap();
        let persisted_confirmed = store.load_indexed_transaction(confirmed_txid).unwrap();
        drop(store);
        fs::remove_file(&sqlite_path).unwrap();

        assert_eq!(server.state().chain().best_tip(), Some(child_hash));
        assert_eq!(persisted_tip, Some(child_hash));
        assert!(server.state().mempool().get(&pending.txid()).is_none());
        assert!(persisted_pending.is_none());
        assert_eq!(reloaded.chain().best_tip(), Some(child_hash));
        assert_eq!(reloaded.mempool().len(), 0);
        assert_eq!(
            persisted_confirmed.as_ref().map(|row| row.block_hash),
            Some(Some(child_hash))
        );
    }

    #[test]
    fn select_sync_peers_prefers_highest_advertised_height_ahead_of_local_tip() {
        let mut state = NodeState::new();
        let owner = crypto::signing_key_from_bytes([1; 32]);
        state
            .accept_block(mine_block(
                BlockHash::default(),
                0x207f_ffff,
                &owner.verifying_key(),
                0,
            ))
            .unwrap();
        let mut server = Server::new(server_config(None), state).with_peers(vec![
            PeerEndpoint::new("127.0.0.1:9001", Some("alpha".to_string())),
            PeerEndpoint::new("127.0.0.1:9002", Some("beta".to_string())),
            PeerEndpoint::new("127.0.0.1:9003", Some("gamma".to_string())),
        ]);
        server.peers.get_mut("127.0.0.1:9001").unwrap().stored = StoredPeer {
            addr: "127.0.0.1:9001".to_string(),
            node_name: Some("alpha".to_string()),
            source: PeerSource::Seed,
            advertised_tip_hash: Some(BlockHash::new([0x11; 32])),
            advertised_height: Some(1),
            ..StoredPeer::from_endpoint(
                PeerEndpoint::new("127.0.0.1:9001", Some("alpha".to_string())),
                PeerSource::Seed,
            )
        };
        server.peers.get_mut("127.0.0.1:9002").unwrap().stored = StoredPeer {
            addr: "127.0.0.1:9002".to_string(),
            node_name: Some("beta".to_string()),
            source: PeerSource::Seed,
            advertised_tip_hash: Some(BlockHash::new([0x22; 32])),
            advertised_height: Some(4),
            ..StoredPeer::from_endpoint(
                PeerEndpoint::new("127.0.0.1:9002", Some("beta".to_string())),
                PeerSource::Seed,
            )
        };
        server.peers.get_mut("127.0.0.1:9003").unwrap().stored = StoredPeer {
            addr: "127.0.0.1:9003".to_string(),
            node_name: Some("gamma".to_string()),
            source: PeerSource::Seed,
            advertised_tip_hash: Some(BlockHash::new([0x33; 32])),
            advertised_height: Some(3),
            ..StoredPeer::from_endpoint(
                PeerEndpoint::new("127.0.0.1:9003", Some("gamma".to_string())),
                PeerSource::Seed,
            )
        };

        let selected = server.select_sync_peers();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].addr, "127.0.0.1:9002");
        assert_eq!(selected[0].node_name.as_deref(), Some("beta"));
    }

    #[test]
    fn select_sync_peers_falls_back_to_first_configured_peer_without_tip_metadata() {
        let server = Server::new(server_config(None), NodeState::new()).with_peers(vec![
            PeerEndpoint::new("127.0.0.1:9001", Some("alpha".to_string())),
            PeerEndpoint::new("127.0.0.1:9002", Some("beta".to_string())),
        ]);

        let selected = server.select_sync_peers();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].addr, "127.0.0.1:9001");
        assert_eq!(selected[0].node_name.as_deref(), Some("alpha"));
    }

    #[test]
    fn select_sync_peers_skips_recently_contacted_unknown_peer() {
        let mut server =
            Server::new(server_config(None), NodeState::new()).with_peers(vec![PeerEndpoint::new(
                "127.0.0.1:9001",
                Some("alpha".to_string()),
            )]);
        server.peers.get_mut("127.0.0.1:9001").unwrap().stored = StoredPeer {
            last_connect_at: Some(format_timestamp_string(current_unix_seconds())),
            ..StoredPeer::from_endpoint(
                PeerEndpoint::new("127.0.0.1:9001", Some("alpha".to_string())),
                PeerSource::Seed,
            )
        };

        let selected = server.select_sync_peers();

        assert!(selected.is_empty());
    }

    #[test]
    fn relay_policy_skips_known_transaction_announcements() {
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let transaction = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state.submit_transaction(transaction.clone()).unwrap();
        let server = Server::new(server_config(None), state);

        let relay = server.relay_message_before_handle(&WireMessage::AnnounceTx { transaction });

        assert_eq!(relay, None);
    }

    #[test]
    fn relay_best_effort_skips_origin_peer_by_node_name() {
        let server = Server::new(server_config(Some("alpha")), NodeState::new()).with_peers(vec![
            PeerEndpoint::new("127.0.0.1:1", Some("beta".to_string())),
            PeerEndpoint::new("127.0.0.1:2", Some("gamma".to_string())),
        ]);

        let selected: Vec<_> = server
            .peers
            .values()
            .map(|peer| &peer.endpoint)
            .filter(|peer| peer.node_name.as_deref() != Some("beta"))
            .map(|peer| peer.addr.as_str())
            .collect();

        assert_eq!(selected, vec!["127.0.0.1:2"]);
    }

    #[test]
    fn peer_match_prefers_advertised_addr_over_node_name() {
        let peer = PeerEndpoint::new("127.0.0.1:2", Some("gamma".to_string()));
        let origin = RelayOrigin {
            advertised_addr: Some("127.0.0.1:2".to_string()),
            node_name: Some("beta".to_string()),
        };

        assert!(peer_matches_origin(&peer, Some(&origin)));
    }

    #[test]
    fn mined_block_reply_relays_the_concrete_block_from_state() {
        let sender = crypto::signing_key_from_bytes([1; 32]);
        let recipient = crypto::signing_key_from_bytes([2; 32]);
        let miner = crypto::signing_key_from_bytes([3; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let transaction = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state.submit_transaction(transaction.clone()).unwrap();
        let mut server = Server::new(server_config(None), state);

        let replies = server
            .handle_message(WireMessage::MinePending(crate::wire::MinePendingRequest {
                reward: 50,
                miner_public_key: crypto::verifying_key_bytes(&miner.verifying_key()).to_vec(),
                uniqueness: 2,
                bits: 0x207f_ffff,
                max_transactions: 10,
            }))
            .unwrap();
        let relay = server.relay_message_from_replies(&replies);

        let Some(WireMessage::AnnounceBlock { block }) = relay else {
            panic!("expected mined block relay");
        };
        let [WireMessage::MinedBlock(crate::wire::MinedBlock { block_hash })] = replies.as_slice()
        else {
            panic!("expected mined block reply");
        };
        assert_eq!(block.header.block_hash(), *block_hash);
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.transactions[1], transaction);
    }

    #[test]
    fn sync_from_peer_fetches_and_accepts_missing_tip_block() {
        let owner = crypto::signing_key_from_bytes([9; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let child = mine_block(
            genesis.header.block_hash(),
            0x207f_ffff,
            &owner.verifying_key(),
            1,
        );
        let child_hash = child.header.block_hash();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let peer_addr = addr.clone();
        let served_child = child.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: PROTOCOL_VERSION,
                    node_name: Some("remote".to_string()),
                    advertised_addr: Some(addr.clone()),
                    tip: Some(child_hash),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            assert_eq!(
                request,
                WireMessage::GetTip,
                "sync sessions poll the current remote tip before fetching blocks"
            );
            net::send_message(
                &mut stream,
                &WireMessage::Tip(TipSummary {
                    tip: Some(child_hash),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            let WireMessage::GetBlock { block_hash } = request else {
                panic!("expected get_block request");
            };
            assert_eq!(block_hash, child_hash);
            net::send_message(
                &mut stream,
                &WireMessage::Block {
                    block: Some(served_child),
                },
            )
            .unwrap();
        });

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let mut server = Server::new(server_config(Some("local")), state);
        let _runtime = attach_runtime(&mut server);

        server
            .sync_from_peer(&PeerEndpoint::new(peer_addr, Some("remote".to_string())))
            .unwrap();
        worker.join().unwrap();

        assert_eq!(server.state().chain().best_tip(), Some(child_hash));
        assert!(server.state().get_block(&child_hash).is_some());
    }

    #[test]
    fn sync_from_peer_errors_when_remote_tip_block_is_missing() {
        let owner = crypto::signing_key_from_bytes([10; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let missing_hash = BlockHash::new([0x44; 32]);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let peer_addr = addr.clone();
        let missing_hash_for_thread = missing_hash;
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: PROTOCOL_VERSION,
                    node_name: Some("remote".to_string()),
                    advertised_addr: Some(addr.clone()),
                    tip: Some(missing_hash_for_thread),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            assert_eq!(
                request,
                WireMessage::GetTip,
                "sync sessions poll the current remote tip before fetching blocks"
            );
            net::send_message(
                &mut stream,
                &WireMessage::Tip(TipSummary {
                    tip: Some(missing_hash_for_thread),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            let WireMessage::GetBlock { block_hash } = request else {
                panic!("expected get_block request");
            };
            assert_eq!(block_hash, missing_hash_for_thread);
            net::send_message(&mut stream, &WireMessage::Block { block: None }).unwrap();
        });

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let mut server = Server::new(server_config(Some("local")), state);
        let _runtime = attach_runtime(&mut server);

        let err = server
            .sync_from_peer(&PeerEndpoint::new(peer_addr, Some("remote".to_string())))
            .unwrap_err();
        worker.join().unwrap();

        assert!(matches!(err, super::SyncError::MissingRemoteBlock(hash) if hash == missing_hash));
    }

    #[test]
    fn announced_block_syncs_missing_parent_then_retries_acceptance() {
        let owner = crypto::signing_key_from_bytes([11; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let child = mine_block(
            genesis.header.block_hash(),
            0x207f_ffff,
            &owner.verifying_key(),
            1,
        );
        let child_hash = child.header.block_hash();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let peer_addr = addr.clone();
        let served_genesis = genesis.clone();
        let served_child = child.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: PROTOCOL_VERSION,
                    node_name: Some("remote".to_string()),
                    advertised_addr: Some(addr.clone()),
                    tip: Some(child_hash),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            assert_eq!(
                request,
                WireMessage::GetTip,
                "ancestor sync first polls the advertised tip on the reused connection"
            );
            net::send_message(
                &mut stream,
                &WireMessage::Tip(TipSummary {
                    tip: Some(child_hash),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            let WireMessage::GetBlock { block_hash } = request else {
                panic!("expected child get_block request");
            };
            assert_eq!(block_hash, child_hash);
            net::send_message(
                &mut stream,
                &WireMessage::Block {
                    block: Some(served_child),
                },
            )
            .unwrap();
            let request = net::receive_message(&mut stream).unwrap();
            let WireMessage::GetBlock { block_hash } = request else {
                panic!("expected genesis get_block request");
            };
            assert_eq!(block_hash, served_genesis.header.block_hash());
            net::send_message(
                &mut stream,
                &WireMessage::Block {
                    block: Some(served_genesis),
                },
            )
            .unwrap();
        });

        let mut server = Server::new(server_config(Some("local")), NodeState::new());
        let _runtime = attach_runtime(&mut server);
        let origin = RelayOrigin {
            advertised_addr: Some(peer_addr),
            node_name: Some("remote".to_string()),
        };

        let replies = server
            .handle_peer_message(
                Some(&origin),
                WireMessage::AnnounceBlock {
                    block: child.clone(),
                },
            )
            .unwrap();
        worker.join().unwrap();

        assert!(replies.is_empty());
        assert_eq!(server.state().chain().best_tip(), Some(child_hash));
        assert!(
            server
                .state()
                .get_block(&genesis.header.block_hash())
                .is_some()
        );
        assert!(server.state().get_block(&child_hash).is_some());
    }

    #[test]
    fn announced_competing_genesis_still_returns_original_rejection() {
        let local_owner = crypto::signing_key_from_bytes([12; 32]);
        let remote_owner = crypto::signing_key_from_bytes([13; 32]);
        let local_genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &local_owner.verifying_key(),
            0,
        );
        let remote_genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &remote_owner.verifying_key(),
            1,
        );
        let mut state = NodeState::new();
        state.accept_block(local_genesis).unwrap();
        let mut server = Server::new(server_config(Some("local")), state);
        let origin = RelayOrigin {
            advertised_addr: Some("127.0.0.1:9999".to_string()),
            node_name: Some("remote".to_string()),
        };

        let err = server
            .handle_peer_message(
                Some(&origin),
                WireMessage::AnnounceBlock {
                    block: remote_genesis,
                },
            )
            .unwrap_err();

        assert!(matches!(
            err,
            super::ServerError::Peer(crate::peer::PeerError::BlockRejected(
                NodeStateError::Chain(ChainError::MissingParent(hash))
            )) if hash == BlockHash::default()
        ));
    }

    #[test]
    fn relay_handshake_advertises_current_tip() {
        let owner = crypto::signing_key_from_bytes([12; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let genesis_hash = genesis.header.block_hash();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut stream).unwrap();
            let WireMessage::Hello(hello) = hello else {
                panic!("expected hello");
            };
            assert_eq!(hello.tip, Some(genesis_hash));
            assert_eq!(hello.height, Some(0));
            net::send_message(
                &mut stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: PROTOCOL_VERSION,
                    node_name: Some("remote".to_string()),
                    advertised_addr: None,
                    tip: None,
                    height: None,
                }),
            )
            .unwrap();
            let message = net::receive_message(&mut stream).unwrap();
            assert_eq!(message, WireMessage::GetTip);
        });

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();

        relay_to_peer(
            &addr,
            &server_config(Some("local")),
            &state,
            &WireMessage::GetTip,
        )
        .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn relay_best_effort_reconnects_for_each_announcement() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let worker = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let reader_stream = stream.try_clone().unwrap();
                let mut reader = io::BufReader::new(reader_stream);
                let hello = net::receive_message_from_reader(&mut reader).unwrap();
                let WireMessage::Hello(_) = hello else {
                    panic!("expected hello");
                };
                net::send_message(
                    &mut stream,
                    &WireMessage::Hello(HelloMessage {
                        network: "wobble-local".to_string(),
                        version: PROTOCOL_VERSION,
                        node_name: Some("remote".to_string()),
                        advertised_addr: None,
                        tip: None,
                        height: None,
                    }),
                )
                .unwrap();
                assert!(matches!(
                    net::receive_message_from_reader(&mut reader).unwrap(),
                    WireMessage::AnnounceTx { .. }
                ));
            }
        });

        let mut server = Server::new(server_config(Some("local")), NodeState::new())
            .with_peers(vec![PeerEndpoint::new(addr, Some("remote".to_string()))]);
        let _runtime = attach_runtime(&mut server);
        let transaction = coinbase(
            50,
            &crypto::signing_key_from_bytes([19; 32]).verifying_key(),
            0,
        );

        server.relay_best_effort(
            &WireMessage::AnnounceTx {
                transaction: transaction.clone(),
            },
            None,
        );
        server.relay_best_effort(&WireMessage::AnnounceTx { transaction }, None);

        worker.join().unwrap();
    }

    #[test]
    fn integrated_miner_mines_pending_transaction_when_enabled() {
        let sender = crypto::signing_key_from_bytes([14; 32]);
        let recipient = crypto::signing_key_from_bytes([15; 32]);
        let miner = crypto::signing_key_from_bytes([16; 32]);
        let genesis = mine_block(
            BlockHash::default(),
            0x207f_ffff,
            &sender.verifying_key(),
            0,
        );
        let spendable = OutPoint {
            txid: genesis.transactions[0].txid(),
            vout: 0,
        };
        let transaction = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let txid = transaction.txid();

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        state.submit_transaction(transaction).unwrap();

        let mut server = Server::new(server_config(None), state).with_mining(
            MiningConfig::new(miner.verifying_key()).with_interval(Duration::from_millis(1)),
        );

        server.mine_pending_best_effort().unwrap();

        assert!(server.state().mempool().is_empty());
        let tip = server.state().chain().best_tip().unwrap();
        let block = server.state().get_block(&tip).unwrap();
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.transactions[1].txid(), txid);
    }

    #[test]
    fn start_stops_when_handle_requests_shutdown() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            Server::new(
                server_config_at(None, "127.0.0.1:0").with_channel_capacity(4),
                NodeState::new(),
            )
            .start(Some(ready_tx))
            .unwrap()
        });

        let handle = ready_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(100));
        handle.stop();

        let stopped = worker.join().unwrap();
        assert!(stopped.chain().best_tip().is_none());
    }
}
