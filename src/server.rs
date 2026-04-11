//! Minimal peer server that owns node state and protocol configuration.
//!
//! This module ties together the raw TCP transport in `net`, the message
//! semantics in `peer`, and the mutable blockchain state in `NodeState`.
//! The first version is intentionally single-threaded and handles one stream at
//! a time so the protocol loop stays easy to reason about during early
//! networking work. It now also supports an optional bootstrap sync pass that
//! can pull missing blocks from configured peers before the main accept loop.
//!
//! When integrated mining is enabled, the server reuses the existing
//! `MinePending` message path instead of maintaining a separate mining engine.
//! The serve loop polls for inbound connections, and during idle intervals it
//! builds an internal `MinePending` request from `MiningConfig` and feeds that
//! request back through normal message handling. That keeps mining,
//! persistence, relay, and logging behavior aligned with externally triggered
//! `mine_pending` requests.

use std::{
    io,
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::PathBuf,
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{
    client::{self, ClientError, RequestError},
    consensus::BLOCK_SUBSIDY,
    net,
    node_state::{NodeState, NodeStateError},
    peer::{self, PeerConfig, PeerError},
    sqlite_store::{self, SqliteStoreError},
    types::{Block, BlockHash},
    wire::{HelloMessage, WireMessage},
};
use ed25519_dalek::VerifyingKey;

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

/// Origin identity learned from the remote `hello` on a live stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RelayOrigin {
    advertised_addr: Option<String>,
    node_name: Option<String>,
}

/// Owns local protocol configuration and mutable node state for networking.
#[derive(Debug, Clone)]
pub struct Server {
    config: PeerConfig,
    state: NodeState,
    peers: Vec<PeerEndpoint>,
    sqlite_path: Option<PathBuf>,
    bootstrap_sync: bool,
    mining: Option<MiningConfig>,
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

    fn next_request(&mut self) -> crate::wire::MinePendingRequest {
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

/// Retry budget for best-effort outbound relay dialing.
const RELAY_CONNECT_ATTEMPTS: usize = 3;
/// Small pause between relay dial attempts so listeners finishing a prior
/// request can accept the next inbound connection.
const RELAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);

impl Server {
    pub fn new(config: PeerConfig, state: NodeState) -> Self {
        Self {
            config,
            state,
            peers: Vec::new(),
            sqlite_path: None,
            bootstrap_sync: false,
            mining: None,
        }
    }

    /// Configures the peer addresses that should receive relayed transactions and blocks.
    pub fn with_peers(mut self, peers: Vec<PeerEndpoint>) -> Self {
        self.peers = peers;
        self
    }

    /// Configures the server to persist accepted blocks and chain metadata to SQLite.
    ///
    /// This stores the live server state in SQLite for restart and sync.
    pub fn with_sqlite_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.sqlite_path = Some(path.into());
        self
    }

    /// Enables a one-time sync attempt from configured peers before serving.
    ///
    /// This is intended for cold start or restart of a node that may have
    /// missed blocks while offline. Sync is still best effort and currently
    /// runs only once at startup rather than as a background loop.
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

    pub fn config(&self) -> &PeerConfig {
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
        self.handle_message_from_peer(message, None)
    }

    /// Handles one decoded wire message and optionally suppresses relay back to
    /// the named peer that sent it on the current stream.
    fn handle_message_from_peer(
        &mut self,
        message: WireMessage,
        origin: Option<&RelayOrigin>,
    ) -> Result<Vec<WireMessage>, ServerError> {
        log_inbound_message(&message, &self.state);
        let should_persist = message_mutates_state(&message);
        let relay = self.relay_message_before_handle(&message);
        let sqlite_block_hash = persisted_block_hash(&message);
        let replies = peer::handle_message(&self.config, &mut self.state, message)
            .map_err(ServerError::Peer)?;
        log_post_handle_state(&replies, &self.state);
        if should_persist {
            self.persist_sqlite(sqlite_block_hash, &replies)
                .map_err(ServerError::SqlitePersist)?;
        }
        if let Some(relay) = relay.or_else(|| self.relay_message_from_replies(&replies)) {
            self.relay_best_effort(&relay, origin);
        }
        Ok(replies)
    }

    /// Serves a single connected stream until the peer closes the connection or
    /// sends an invalid protocol message.
    ///
    /// Current behavior is request-response oriented: each received message is
    /// handled immediately and any resulting replies are written back in order.
    /// Gap: this does not yet track per-peer state or initiate background relay.
    pub fn handle_stream(&mut self, mut stream: TcpStream) -> io::Result<()> {
        let reader_stream = stream.try_clone()?;
        let mut reader = io::BufReader::new(reader_stream);
        let mut origin = RelayOrigin::default();
        let peer_addr = stream.peer_addr().ok();

        info!(peer_addr = ?peer_addr, "accepted peer stream");

        loop {
            let message = match net::receive_message_from_reader(&mut reader) {
                Ok(message) => message,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            };

            if let WireMessage::Hello(remote_hello) = &message {
                info!(
                    peer_addr = ?peer_addr,
                    remote_node = remote_hello.node_name.as_deref().unwrap_or("unknown"),
                    advertised_addr = remote_hello.advertised_addr.as_deref().unwrap_or("none"),
                    remote_tip = %format_hash(remote_hello.tip),
                    remote_height = ?remote_hello.height,
                    "received peer hello"
                );
                origin = RelayOrigin {
                    advertised_addr: remote_hello.advertised_addr.clone(),
                    node_name: remote_hello.node_name.clone(),
                };
                let replies = peer::handle_message(&self.config, &mut self.state, message.clone())
                    .map_err(|err| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}"))
                    })?;
                for reply in replies {
                    net::send_message(&mut stream, &reply)?;
                }
                let synced_blocks = self.sync_from_hello_best_effort(remote_hello);
                for block in synced_blocks {
                    info!(
                        block_hash = %format_hash(Some(block.header.block_hash())),
                        "relaying block learned during hello-triggered sync"
                    );
                    self.relay_best_effort(&WireMessage::AnnounceBlock { block }, Some(&origin));
                }
                continue;
            }
            let replies = self
                .handle_message_from_peer(message, Some(&origin))
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}")))?;
            for reply in replies {
                net::send_message(&mut stream, &reply)?;
            }
        }
    }

    /// Binds a listener and serves inbound peers sequentially.
    ///
    /// This is enough for local manual testing. Later versions can move to
    /// concurrent connection handling once shared-state policy is clear.
    pub fn serve<A: ToSocketAddrs>(&mut self, addr: A) -> io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        self.serve_listener(listener)
    }

    /// Accepts inbound peer connections from an existing listener.
    pub fn serve_listener(&mut self, listener: TcpListener) -> io::Result<()> {
        if self.bootstrap_sync {
            info!(peer_count = self.peers.len(), "starting bootstrap sync");
            self.sync_configured_peers_best_effort();
        }
        info!(listen_addr = ?listener.local_addr().ok(), "server listening");
        if self.mining.is_none() {
            for stream in listener.incoming() {
                self.handle_stream(stream?)?;
            }
            return Ok(());
        }

        listener.set_nonblocking(true)?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.handle_stream(stream)?,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.mine_pending_best_effort()?;
                    let interval = self
                        .mining
                        .as_ref()
                        .map(|config| config.interval)
                        .unwrap_or(Duration::from_millis(50));
                    thread::sleep(interval);
                }
                Err(err) => return Err(err),
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
    /// history.
    pub fn sync_configured_peers_best_effort(&mut self) {
        let peers = self.peers.clone();
        for peer in &peers {
            if let Err(err) = self.sync_from_peer(peer) {
                warn!(peer_addr = %peer.addr, error = ?err, "bootstrap sync from peer failed");
            }
        }
    }

    /// Fetches a missing remote tip segment from `peer` and applies it
    /// parent-first until this node reaches a known ancestor.
    ///
    /// If the remote tip is already indexed locally, this is a no-op. Current
    /// behavior reconnects per block request for simplicity; later versions can
    /// keep one stream open while downloading a range.
    fn sync_from_peer(&mut self, peer: &PeerEndpoint) -> Result<Vec<Block>, SyncError> {
        info!(peer_addr = %peer.addr, peer_node = ?peer.node_name, "starting sync from peer");
        let (_, remote_hello) = client::connect_and_handshake(&peer.addr, &self.config)
            .map_err(SyncError::Handshake)?;
        let Some(remote_tip) = remote_hello.tip else {
            debug!(peer_addr = %peer.addr, "peer has no advertised tip");
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

            let block = client::request_block(&peer.addr, &self.config, next_hash)
                .map_err(SyncError::Request)?
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
            self.persist_full_state()
                .map_err(SyncError::SqlitePersist)?;
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

    fn persist_sqlite(
        &self,
        request_block_hash: Option<BlockHash>,
        replies: &[WireMessage],
    ) -> Result<(), SqliteStoreError> {
        let Some(path) = self.sqlite_path.as_deref() else {
            return Ok(());
        };
        let Some(block_hash) = request_block_hash.or_else(|| mined_block_hash(replies)) else {
            let store = sqlite_store::SqliteStore::open(path)?;
            store.save_active_utxos(self.state.active_utxos())?;
            store.save_mempool(self.state.mempool())?;
            return Ok(());
        };
        let store = sqlite_store::SqliteStore::open(path)?;
        store.save_active_utxos(self.state.active_utxos())?;
        store.save_mempool(self.state.mempool())?;
        let Some(block) = self.state.get_block(&block_hash) else {
            return Ok(());
        };
        let Some(entry) = self.state.chain().get(&block_hash) else {
            return Ok(());
        };
        store.save_block_record(block, entry, self.state.chain().best_tip())
    }

    /// Persists the full current node state when a bulk sync changed local history.
    fn persist_full_state(&self) -> Result<(), SqliteStoreError> {
        let Some(path) = self.sqlite_path.as_deref() else {
            return Ok(());
        };
        let store = sqlite_store::SqliteStore::open(path)?;
        store.save_node_state(&self.state)
    }

    /// Announces an accepted object to configured peers except the one that
    /// originated it on the current stream, when that peer identity is known.
    fn relay_best_effort(&self, message: &WireMessage, origin: Option<&RelayOrigin>) {
        for peer in &self.peers {
            if peer_matches_origin(peer, origin) {
                debug!(peer_addr = %peer.addr, "skipping relay to origin peer");
                continue;
            }
            debug!(
                peer_addr = %peer.addr,
                message = wire_message_name(message),
                "relaying message to peer"
            );
            let _ = relay_to_peer(&peer.addr, &self.config, &self.state, message);
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

fn message_mutates_state(message: &WireMessage) -> bool {
    matches!(
        message,
        WireMessage::AnnounceTx { .. }
            | WireMessage::AnnounceBlock { .. }
            | WireMessage::MinePending(..)
    )
}

fn persisted_block_hash(message: &WireMessage) -> Option<BlockHash> {
    match message {
        WireMessage::AnnounceBlock { block } => Some(block.header.block_hash()),
        _ => None,
    }
}

fn mined_block_hash(replies: &[WireMessage]) -> Option<BlockHash> {
    replies.iter().find_map(|reply| match reply {
        WireMessage::MinedBlock(result) => Some(result.block_hash),
        _ => None,
    })
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

/// Opens a short-lived outbound relay connection, completes the handshake, and
/// sends one announcement message.
///
/// Relay is intentionally best effort. A small bounded retry helps with local
/// testnet timing where the destination listener may still be unwinding a
/// previous connection before it accepts the next one.
fn relay_to_peer(
    peer_addr: &str,
    config: &PeerConfig,
    state: &NodeState,
    message: &WireMessage,
) -> io::Result<()> {
    let mut last_error = None;
    for attempt in 0..RELAY_CONNECT_ATTEMPTS {
        match relay_to_peer_once(peer_addr, config, state, message) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if attempt + 1 < RELAY_CONNECT_ATTEMPTS {
                    thread::sleep(RELAY_CONNECT_RETRY_DELAY);
                }
            }
        }
    }

    Err(last_error.expect("relay attempts should record an error"))
}

fn relay_to_peer_once(
    peer_addr: &str,
    config: &PeerConfig,
    state: &NodeState,
    message: &WireMessage,
) -> io::Result<()> {
    let mut stream = net::connect(peer_addr)?;
    net::send_message(
        &mut stream,
        &WireMessage::Hello(peer::local_hello(config, state)),
    )?;
    let _ = net::receive_message(&mut stream)?;
    net::send_message(&mut stream, message)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use crate::{
        crypto, net,
        node_state::NodeState,
        peer::PeerConfig,
        server::{MiningConfig, PeerEndpoint, Server},
        sqlite_store::SqliteStore,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
        wire::{HelloMessage, PROTOCOL_VERSION, TipSummary, WireMessage},
    };

    use super::{RelayOrigin, peer_matches_origin, relay_to_peer};

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn read_line(reader: &mut BufReader<TcpStream>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

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

    #[test]
    fn responds_to_hello_and_get_tip_over_stream() {
        let mut server = Server::new(
            PeerConfig::new("wobble-local", Some("alpha".to_string())),
            NodeState::new(),
        );
        let (mut client, server_stream) = connected_pair();

        let worker = thread::spawn(move || server.handle_stream(server_stream));

        client
            .write_all(
                b"{\"type\":\"hello\",\"data\":{\"network\":\"wobble-local\",\"version\":1,\"node_name\":\"beta\",\"tip\":null,\"height\":null}}\n",
            )
            .unwrap();
        client.write_all(b"{\"type\":\"get_tip\"}\n").unwrap();
        client.flush().unwrap();

        let mut reader = BufReader::new(client);
        let hello = WireMessage::from_json_line(&read_line(&mut reader)).unwrap();
        let tip = WireMessage::from_json_line(&read_line(&mut reader)).unwrap();

        assert_eq!(
            hello,
            WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION,
                node_name: Some("alpha".to_string()),
                advertised_addr: None,
                tip: None,
                height: None,
            })
        );
        assert_eq!(
            tip,
            WireMessage::Tip(TipSummary {
                tip: None,
                height: None,
            })
        );

        drop(reader);
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn rejects_invalid_handshake_over_stream() {
        let mut server = Server::new(PeerConfig::new("wobble-local", None), NodeState::new());
        let (mut client, server_stream) = connected_pair();

        let worker = thread::spawn(move || server.handle_stream(server_stream));

        client
            .write_all(
                b"{\"type\":\"hello\",\"data\":{\"network\":\"other-net\",\"version\":1,\"node_name\":null,\"tip\":null,\"height\":null}}\n",
            )
            .unwrap();
        client.flush().unwrap();

        let result = worker.join().unwrap();

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn announced_transaction_reaches_server_mempool_end_to_end() {
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
        let mut server = Server::new(PeerConfig::new("wobble-local", None), state);
        let (mut client, server_stream) = connected_pair();

        let worker = thread::spawn(move || {
            server.handle_stream(server_stream).unwrap();
            server
        });

        // Handshake first so the server accepts later relay messages on this connection.
        net::send_message(
            &mut client,
            &WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION,
                node_name: Some("client".to_string()),
                advertised_addr: None,
                tip: None,
                height: None,
            }),
        )
        .unwrap();
        let remote_hello = net::receive_message(&mut client).unwrap();
        assert!(matches!(remote_hello, WireMessage::Hello(_)));

        net::send_message(&mut client, &WireMessage::AnnounceTx { transaction }).unwrap();
        drop(client);

        let server = worker.join().unwrap();

        assert!(server.state().mempool().get(&txid).is_some());
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
        let mut server = Server::new(PeerConfig::new("wobble-local", None), state)
            .with_sqlite_path(&sqlite_path);

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
        let mut server = Server::new(PeerConfig::new("wobble-local", None), NodeState::new())
            .with_sqlite_path(&sqlite_path);

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
        let server = Server::new(PeerConfig::new("wobble-local", None), state);

        let relay = server.relay_message_before_handle(&WireMessage::AnnounceTx { transaction });

        assert_eq!(relay, None);
    }

    #[test]
    fn relay_best_effort_skips_origin_peer_by_node_name() {
        let server = Server::new(
            PeerConfig::new("wobble-local", Some("alpha".to_string())),
            NodeState::new(),
        )
        .with_peers(vec![
            PeerEndpoint::new("127.0.0.1:1", Some("beta".to_string())),
            PeerEndpoint::new("127.0.0.1:2", Some("gamma".to_string())),
        ]);

        let selected: Vec<_> = server
            .peers
            .iter()
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
        let mut server = Server::new(PeerConfig::new("wobble-local", None), state);

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
            let (mut hello_stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut hello_stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut hello_stream,
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
            drop(hello_stream);

            let (mut block_stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut block_stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut block_stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: PROTOCOL_VERSION,
                    node_name: Some("remote".to_string()),
                    advertised_addr: Some(addr),
                    tip: Some(child_hash),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut block_stream).unwrap();
            let WireMessage::GetBlock { block_hash } = request else {
                panic!("expected get_block request");
            };
            assert_eq!(block_hash, child_hash);
            net::send_message(
                &mut block_stream,
                &WireMessage::Block {
                    block: Some(served_child),
                },
            )
            .unwrap();
        });

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let mut server = Server::new(
            PeerConfig::new("wobble-local", Some("local".to_string())),
            state,
        );

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
            let (mut hello_stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut hello_stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut hello_stream,
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
            drop(hello_stream);

            let (mut block_stream, _) = listener.accept().unwrap();
            let hello = net::receive_message(&mut block_stream).unwrap();
            assert!(matches!(hello, WireMessage::Hello(_)));
            net::send_message(
                &mut block_stream,
                &WireMessage::Hello(HelloMessage {
                    network: "wobble-local".to_string(),
                    version: PROTOCOL_VERSION,
                    node_name: Some("remote".to_string()),
                    advertised_addr: Some(addr),
                    tip: Some(missing_hash_for_thread),
                    height: Some(1),
                }),
            )
            .unwrap();
            let request = net::receive_message(&mut block_stream).unwrap();
            let WireMessage::GetBlock { block_hash } = request else {
                panic!("expected get_block request");
            };
            assert_eq!(block_hash, missing_hash_for_thread);
            net::send_message(&mut block_stream, &WireMessage::Block { block: None }).unwrap();
        });

        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let mut server = Server::new(
            PeerConfig::new("wobble-local", Some("local".to_string())),
            state,
        );

        let err = server
            .sync_from_peer(&PeerEndpoint::new(peer_addr, Some("remote".to_string())))
            .unwrap_err();
        worker.join().unwrap();

        assert!(matches!(err, super::SyncError::MissingRemoteBlock(hash) if hash == missing_hash));
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
            &PeerConfig::new("wobble-local", Some("local".to_string())),
            &state,
            &WireMessage::GetTip,
        )
        .unwrap();
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

        let mut server = Server::new(PeerConfig::new("wobble-local", None), state).with_mining(
            MiningConfig::new(miner.verifying_key()).with_interval(Duration::from_millis(1)),
        );

        server.mine_pending_best_effort().unwrap();

        assert!(server.state().mempool().is_empty());
        let tip = server.state().chain().best_tip().unwrap();
        let block = server.state().get_block(&tip).unwrap();
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.transactions[1].txid(), txid);
    }
}
