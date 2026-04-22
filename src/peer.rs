//! Peer protocol and transport helpers for node-to-node communication.
//!
//! This module validates peer handshakes, handles wire-message protocol rules,
//! and owns the low-level peer transport tasks. `server` remains the sole
//! authority for chain state and sync policy, while `peer` owns the duplex
//! socket loops that:
//! - complete the initial handshake
//! - read unsolicited peer messages
//! - write outbound requests and announcements
//! - forward inbound activity back into the server event queue
//! - match one in-flight request/response at a time for sync operations

use std::{
    io,
    sync::mpsc,
    time::Duration,
};

use crate::{
    client::{ClientError, RequestError},
    mempool::MempoolError,
    node_state::{NodeState, NodeStateError},
    server::{RelayOrigin, ServerConfig, ServerHandle},
    types::{Block, BlockHash},
    wire::{HelloMessage, MinedBlock, PROTOCOL_VERSION, TipSummary, WireMessage},
};
#[cfg(test)]
use crate::net;
use tokio::{
    net::TcpStream,
    runtime::Handle,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::Instant,
};

/// Reasons a remote peer message was rejected at the protocol layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    NetworkMismatch { local: String, remote: String },
    UnsupportedVersion(u32),
    TransactionRejected(NodeStateError),
    BlockRejected(NodeStateError),
    InvalidMinerPublicKey,
    MiningRejected(NodeStateError),
}

/// Builds the local `hello` payload from configuration and current node state.
pub fn local_hello(config: &ServerConfig, state: &NodeState) -> HelloMessage {
    let tip = state.tip_summary();
    HelloMessage {
        network: config.network.clone(),
        version: PROTOCOL_VERSION,
        node_name: config.node_name.clone(),
        advertised_addr: config.advertised_addr.clone(),
        tip: tip.tip,
        height: tip.height,
    }
}

/// One live peer handle owned by an async transport task.
#[derive(Debug)]
pub(crate) struct PeerHandle {
    command_tx: UnboundedSender<PeerCommand>,
}

impl PeerHandle {
    /// Requests the peer's current best tip over the live async connection.
    pub(crate) fn request_tip(&self) -> Result<TipSummary, RequestError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .send(PeerCommand::RequestTip { reply: reply_tx })
            .map_err(|_| RequestError::Receive(io::Error::new(io::ErrorKind::BrokenPipe, "outbound peer closed")))?;
        reply_rx
            .recv_timeout(OUTBOUND_PEER_IO_TIMEOUT)
            .map_err(|_| RequestError::Receive(io::Error::new(io::ErrorKind::TimedOut, "outbound peer tip request timed out")))?
    }

    /// Requests one block over the live async connection.
    pub(crate) fn request_block(
        &self,
        block_hash: BlockHash,
    ) -> Result<Option<Block>, RequestError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .send(PeerCommand::RequestBlock {
                block_hash,
                reply: reply_tx,
            })
            .map_err(|_| RequestError::Receive(io::Error::new(io::ErrorKind::BrokenPipe, "outbound peer closed")))?;
        reply_rx
            .recv_timeout(OUTBOUND_PEER_IO_TIMEOUT)
            .map_err(|_| RequestError::Receive(io::Error::new(io::ErrorKind::TimedOut, "outbound peer block request timed out")))?
    }

    /// Sends one announcement over the live async connection.
    pub(crate) fn announce(&self, message: &WireMessage) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .send(PeerCommand::Announce {
                message: message.clone(),
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "outbound peer closed"))?;
        reply_rx
            .recv_timeout(OUTBOUND_PEER_IO_TIMEOUT)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "outbound peer announce timed out"))?
    }

    /// Requests that the owned outbound peer task stop.
    pub(crate) fn shutdown(&self) {
        let _ = self.command_tx.send(PeerCommand::Shutdown);
    }
}

/// Opens one persistent outbound peer connection and returns a synchronous handle
/// plus the remote hello learned during handshake.
pub(crate) fn connect_peer(
    runtime: &Handle,
    peer_id: String,
    peer_addr: String,
    local_hello: HelloMessage,
    server: Option<ServerHandle>,
) -> Result<(PeerHandle, HelloMessage), ClientError> {
    let (command_tx, command_rx) = unbounded_channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    runtime.spawn(async move {
        let ready = open_peer_connection(peer_addr, local_hello).await;
        match ready {
            Ok((remote_hello, reader, writer)) => {
                let _ = ready_tx.send(Ok(remote_hello.clone()));
                run_peer_task(
                    peer_id,
                    server,
                    reader,
                    writer,
                    Some(command_rx),
                    RelayOrigin {
                        advertised_addr: remote_hello.advertised_addr.clone(),
                        node_name: remote_hello.node_name.clone(),
                    },
                )
                .await;
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err));
            }
        }
    });
    let remote_hello = ready_rx
        .recv_timeout(OUTBOUND_PEER_IO_TIMEOUT)
        .map_err(|_| ClientError::ReceiveHello(io::Error::new(io::ErrorKind::TimedOut, "outbound peer handshake timed out")))??;
    Ok((PeerHandle { command_tx }, remote_hello))
}

/// Commands sent from the server thread into one live outbound peer task.
enum PeerCommand {
    RequestTip {
        reply: mpsc::SyncSender<Result<TipSummary, RequestError>>,
    },
    RequestBlock {
        block_hash: BlockHash,
        reply: mpsc::SyncSender<Result<Option<Block>, RequestError>>,
    },
    Announce {
        message: WireMessage,
        reply: mpsc::SyncSender<io::Result<()>>,
    },
    Shutdown,
}

/// One outstanding request waiting for the next matching peer response.
///
/// We intentionally allow only one in-flight request at a time per live peer
/// session. That keeps response matching simple while still supporting the
/// current sync flow of serialized `get_tip`/`get_block` walks.
enum PendingRequest {
    Tip {
        reply: mpsc::SyncSender<Result<TipSummary, RequestError>>,
    },
    Block {
        reply: mpsc::SyncSender<Result<Option<Block>, RequestError>>,
    },
}

/// Connects and completes the outbound peer handshake.
async fn open_peer_connection(
    peer_addr: String,
    local_hello: HelloMessage,
)-> Result<
    (
        HelloMessage,
        tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
    ),
    ClientError,
> {
    let stream =
        timeout_io(TcpStream::connect(&peer_addr))
            .await
            .map_err(ClientError::Connect)?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);

    send_async_message(&mut writer, &WireMessage::Hello(local_hello))
        .await
        .map_err(ClientError::SendHello)?;
    let remote_hello_message = receive_async_message(&mut reader)
        .await
        .map_err(ClientError::ReceiveHello)?;
    let WireMessage::Hello(remote_hello) = remote_hello_message else {
        return Err(ClientError::UnexpectedHandshake(remote_hello_message));
    };

    Ok((remote_hello, reader, writer))
}

/// Runs one persistent peer session for sync, relay, and unsolicited inbound
/// messages on the same socket.
///
/// The session owns the low-level duplex transport. It forwards inbound peer
/// messages into the server event loop and also serves one in-flight
/// request/response at a time for sync operations.
async fn run_peer_task(
    peer_id: String,
    handle: Option<ServerHandle>,
    mut reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut command_rx: Option<UnboundedReceiver<PeerCommand>>,
    mut origin: RelayOrigin,
) {
    if let Some(handle) = handle.as_ref() {
        let _ = handle.notify_peer_connected(peer_id.clone()).await;
    }
    let mut pending_request = None;
    let mut pending_deadline = None;
    loop {
        tokio::select! {
            command = recv_peer_command(&mut command_rx), if command_rx.is_some() => {
                match command {
                    Some(command) => {
                        let keep_running = handle_peer_command(
                            &mut writer,
                            command,
                            &mut pending_request,
                            &mut pending_deadline,
                        ).await;
                        if !keep_running {
                            break;
                        }
                    }
                    None => {
                        command_rx = None;
                    }
                }
            }
            read = receive_async_message(&mut reader) => {
                let message = match read {
                    Ok(message) => message,
                    Err(err) => {
                        fail_pending_request(&mut pending_request, err, None);
                        break;
                    }
                };
                if handle_inbound_message(
                    &peer_id,
                    handle.as_ref(),
                    &mut writer,
                    &mut pending_request,
                    &mut pending_deadline,
                    &mut origin,
                    message,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            _ = sleep_until_deadline(pending_deadline), if pending_deadline.is_some() => {
                fail_pending_request_with_error(
                    &mut pending_request,
                    RequestError::Receive(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "peer request timed out",
                    )),
                );
                pending_deadline = None;
            }
        }
    }
    fail_pending_request(
        &mut pending_request,
        io::Error::new(io::ErrorKind::BrokenPipe, "peer session closed"),
        Some(io::ErrorKind::BrokenPipe),
    );
    if let Some(handle) = handle.as_ref() {
        let _ = handle.notify_peer_disconnected(peer_id).await;
    }
}

/// Sends one outbound peer command onto the live session writer.
///
/// Request commands install a single pending waiter so the next matching
/// inbound response can be delivered back to the caller.
async fn handle_peer_command(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    command: PeerCommand,
    pending_request: &mut Option<PendingRequest>,
    pending_deadline: &mut Option<Instant>,
) -> bool {
    match command {
        PeerCommand::RequestTip { reply } => {
            if pending_request.is_some() {
                let _ = reply.send(Err(RequestError::Receive(io::Error::other(
                    "peer already has a pending request",
                ))));
                return true;
            }
            match send_async_message(writer, &WireMessage::GetTip).await {
                Ok(()) => {
                    *pending_request = Some(PendingRequest::Tip { reply });
                    *pending_deadline = Some(Instant::now() + OUTBOUND_PEER_IO_TIMEOUT);
                    true
                }
                Err(err) => {
                    let _ = reply.send(Err(RequestError::Send(err)));
                    false
                }
            }
        }
        PeerCommand::RequestBlock { block_hash, reply } => {
            if pending_request.is_some() {
                let _ = reply.send(Err(RequestError::Receive(io::Error::other(
                    "peer already has a pending request",
                ))));
                return true;
            }
            match send_async_message(writer, &WireMessage::GetBlock { block_hash }).await {
                Ok(()) => {
                    *pending_request = Some(PendingRequest::Block { reply });
                    *pending_deadline = Some(Instant::now() + OUTBOUND_PEER_IO_TIMEOUT);
                    true
                }
                Err(err) => {
                    let _ = reply.send(Err(RequestError::Send(err)));
                    false
                }
            }
        }
        PeerCommand::Announce { message, reply } => {
            let result = send_async_message(writer, &message).await;
            let keep_running = result.is_ok();
            let _ = reply.send(result);
            keep_running
        }
        PeerCommand::Shutdown => false,
    }
}

/// Handles one inbound wire message on a live peer session.
///
/// Matching request responses are delivered to the waiting caller. All other
/// peer messages are forwarded into the server event loop so `Server` remains
/// the only protocol/state authority.
async fn handle_inbound_message(
    peer_id: &str,
    handle: Option<&ServerHandle>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    pending_request: &mut Option<PendingRequest>,
    pending_deadline: &mut Option<Instant>,
    origin: &mut RelayOrigin,
    message: WireMessage,
) -> io::Result<()> {
    if try_complete_pending_request(pending_request, message.clone()) {
        *pending_deadline = None;
        return Ok(());
    }

    let mut tip_for_follow_up_sync = None;
    if let WireMessage::Hello(remote_hello) = &message {
        *origin = RelayOrigin {
            advertised_addr: remote_hello.advertised_addr.clone(),
            node_name: remote_hello.node_name.clone(),
        };
    } else if let WireMessage::AnnounceTip(summary) = &message {
        tip_for_follow_up_sync = Some(summary.clone());
    }

    let Some(handle) = handle else {
        return Ok(());
    };

    let replies = handle
        .request_peer_message(peer_id.to_string(), Some(origin.clone()), message)
        .await
        .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, format!("{err:?}")))?
        .map_err(|err| io::Error::other(format!("{err:?}")))?;
    for reply in replies {
        send_async_message(writer, &reply).await?;
    }
    if let Some(summary) = tip_for_follow_up_sync {
        let _ = handle.notify_tip_sync(origin.clone(), summary).await;
    }
    Ok(())
}

/// Attempts to match one inbound response against the current pending request.
///
/// Returns `true` when the message was consumed as a request response and
/// should not also be forwarded into the server event loop.
fn try_complete_pending_request(
    pending_request: &mut Option<PendingRequest>,
    message: WireMessage,
) -> bool {
    let Some(pending) = pending_request.take() else {
        return false;
    };
    match (pending, message) {
        (PendingRequest::Tip { reply }, WireMessage::Tip(summary)) => {
            let _ = reply.send(Ok(summary));
            true
        }
        (PendingRequest::Block { reply }, WireMessage::Block { block }) => {
            let _ = reply.send(Ok(block));
            true
        }
        (pending, message) => {
            *pending_request = Some(pending);
            let _ = message;
            false
        }
    }
}

/// Receives one optional command from the live peer control channel.
async fn recv_peer_command(
    command_rx: &mut Option<UnboundedReceiver<PeerCommand>>,
) -> Option<PeerCommand> {
    let Some(command_rx) = command_rx.as_mut() else {
        return None;
    };
    command_rx.recv().await
}

/// Sleeps until the current pending request deadline fires.
async fn sleep_until_deadline(deadline: Option<Instant>) {
    let Some(deadline) = deadline else {
        std::future::pending::<()>().await;
        unreachable!();
    };
    tokio::time::sleep_until(deadline).await;
}

/// Fails one pending request because the peer session encountered an I/O error.
fn fail_pending_request(
    pending_request: &mut Option<PendingRequest>,
    err: io::Error,
    kind: Option<io::ErrorKind>,
) {
    let request_err = RequestError::Receive(match kind {
        Some(kind) => io::Error::new(kind, err.to_string()),
        None => err,
    });
    fail_pending_request_with_error(pending_request, request_err);
}

/// Delivers one explicit request error to the current pending waiter, if any.
fn fail_pending_request_with_error(
    pending_request: &mut Option<PendingRequest>,
    err: RequestError,
) {
    let Some(pending) = pending_request.take() else {
        return;
    };
    match pending {
        PendingRequest::Tip { reply } => {
            let _ = reply.send(Err(err));
        }
        PendingRequest::Block { reply } => {
            let _ = reply.send(Err(err));
        }
    }
}

/// Sends one wire message over an async writer with the standard peer timeout.
async fn send_async_message(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    message: &WireMessage,
) -> io::Result<()> {
    let line = message
        .to_json_line()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    timeout_io(tokio::io::AsyncWriteExt::write_all(writer, line.as_bytes())).await?;
    timeout_io(tokio::io::AsyncWriteExt::flush(writer)).await?;
    Ok(())
}

/// Receives one wire message from an async buffered reader with the standard peer timeout.
async fn receive_async_message(
    reader: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> io::Result<WireMessage> {
    let mut line = String::new();
    let bytes_read = timeout_io(tokio::io::AsyncBufReadExt::read_line(reader, &mut line)).await?;
    if bytes_read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed connection",
        ));
    }
    WireMessage::from_json_line(&line)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Applies one standard timeout around an async peer I/O operation.
async fn timeout_io<F, T>(future: F) -> io::Result<T>
where
    F: std::future::Future<Output = io::Result<T>>,
{
    tokio::time::timeout(OUTBOUND_PEER_IO_TIMEOUT, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "peer I/O timed out"))?
}

/// Runs one async peer stream and forwards decoded messages into the server.
pub(crate) async fn serve_peer_stream(
    peer_id: String,
    handle: ServerHandle,
    stream: tokio::net::TcpStream,
) {
    let (reader, writer) = stream.into_split();
    run_peer_task(
        peer_id,
        Some(handle),
        tokio::io::BufReader::new(reader),
        writer,
        None,
        RelayOrigin::default(),
    )
    .await;
}

/// Opens a short-lived outbound relay connection, completes the handshake, and
/// sends one announcement message.
#[cfg(test)]
pub(crate) fn relay_to_peer(
    peer_addr: &str,
    config: &ServerConfig,
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
                    std::thread::sleep(RELAY_CONNECT_RETRY_DELAY);
                }
            }
        }
    }

    Err(last_error.expect("relay attempts should record an error"))
}

#[cfg(test)]
fn relay_to_peer_once(
    peer_addr: &str,
    config: &ServerConfig,
    state: &NodeState,
    message: &WireMessage,
) -> io::Result<()> {
    let mut stream = net::connect(peer_addr)?;
    stream.set_read_timeout(Some(OUTBOUND_PEER_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(OUTBOUND_PEER_IO_TIMEOUT))?;
    net::send_message(&mut stream, &WireMessage::Hello(local_hello(config, state)))?;
    let _ = net::receive_message(&mut stream)?;
    net::send_message(&mut stream, message)?;
    Ok(())
}

const OUTBOUND_PEER_IO_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const RELAY_CONNECT_ATTEMPTS: usize = 3;
#[cfg(test)]
const RELAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Handles one incoming wire message and returns any immediate protocol replies.
///
/// Current behavior:
/// - `hello` must match the local network and supported protocol version
/// - `get_tip` returns the current best-tip summary
/// - `get_block` returns the indexed block when known
/// - `announce_tx` validates and inserts the transaction into the live mempool
/// - `announce_block` validates and accepts the block into the local chain state
/// - `mine_pending` mines a block from the current mempool for local testnet use
/// - other messages are ignored for now and will be handled in later network slices
///
/// Gap: this does not yet fan accepted objects out to other peers, track peer
/// state, or request missing parents automatically.
pub fn handle_message(
    config: &ServerConfig,
    state: &mut NodeState,
    message: WireMessage,
) -> Result<Vec<WireMessage>, PeerError> {
    match message {
        WireMessage::Hello(remote) => {
            if remote.network != config.network {
                return Err(PeerError::NetworkMismatch {
                    local: config.network.clone(),
                    remote: remote.network,
                });
            }
            if remote.version != PROTOCOL_VERSION {
                return Err(PeerError::UnsupportedVersion(remote.version));
            }

            Ok(vec![WireMessage::Hello(local_hello(config, state))])
        }
        WireMessage::GetTip => Ok(vec![WireMessage::Tip(state.tip_summary())]),
        WireMessage::AnnounceTip(_) => Ok(Vec::new()),
        WireMessage::GetBlock { block_hash } => Ok(vec![WireMessage::Block {
            block: state.get_block(&block_hash).cloned(),
        }]),
        WireMessage::AnnounceTx { transaction } => {
            match state.submit_transaction(transaction) {
                Ok(_) => {}
                Err(NodeStateError::Mempool(MempoolError::DuplicateTransaction(_))) => {}
                Err(err) => return Err(PeerError::TransactionRejected(err)),
            }
            Ok(Vec::new())
        }
        WireMessage::AnnounceBlock { block } => {
            state
                .accept_block(block)
                .map_err(PeerError::BlockRejected)?;
            Ok(Vec::new())
        }
        WireMessage::MinePending(request) => {
            let miner = crate::crypto::parse_verifying_key(&request.miner_public_key)
                .ok_or(PeerError::InvalidMinerPublicKey)?;
            let block_hash = state
                .mine_block(
                    request.reward,
                    &miner,
                    request.uniqueness,
                    request.bits,
                    request.max_transactions,
                )
                .map_err(PeerError::MiningRejected)?;
            Ok(vec![WireMessage::MinedBlock(MinedBlock { block_hash })])
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        node_state::NodeState,
        peer::{PeerError, handle_message, local_hello},
        server::ServerConfig,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut},
        wire::{
            HelloMessage, MinePendingRequest, MinedBlock, PROTOCOL_VERSION, TipSummary, WireMessage,
        },
    };

    fn coinbase(value: u64, owner: &ed25519_dalek::VerifyingKey, uniqueness: u32) -> Transaction {
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value,
                locking_data: crate::crypto::verifying_key_bytes(owner).to_vec(),
            }],
            lock_time: uniqueness,
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
                locking_data: crate::crypto::verifying_key_bytes(recipient).to_vec(),
            }],
            lock_time: uniqueness,
        };
        tx.inputs[0].unlocking_data =
            crate::crypto::sign_message(signer, &tx.signing_digest()).to_vec();
        tx
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

    #[test]
    fn local_hello_reports_current_tip_state() {
        let owner = crate::crypto::signing_key_from_bytes([1; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = ServerConfig::new(
            "wobble-local",
            Some("alpha".to_string()),
            "127.0.0.1:9000",
        )
            .with_advertised_addr("127.0.0.1:9000");

        let hello = local_hello(&config, &state);

        assert_eq!(hello.network, "wobble-local");
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert_eq!(hello.node_name, Some("alpha".to_string()));
        assert_eq!(hello.advertised_addr, Some("127.0.0.1:9000".to_string()));
        assert_eq!(hello.tip, Some(genesis_hash));
        assert_eq!(hello.height, Some(0));
    }

    #[test]
    fn get_tip_returns_tip_summary() {
        let owner = crate::crypto::signing_key_from_bytes([1; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        let replies = handle_message(&config, &mut state, WireMessage::GetTip).unwrap();

        assert_eq!(
            replies,
            vec![WireMessage::Tip(TipSummary {
                tip: Some(genesis_hash),
                height: Some(0),
            })]
        );
    }

    #[test]
    fn get_block_returns_known_block() {
        let owner = crate::crypto::signing_key_from_bytes([1; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let genesis_hash = genesis.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis.clone()).unwrap();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        let replies = handle_message(
            &config,
            &mut state,
            WireMessage::GetBlock {
                block_hash: genesis_hash,
            },
        )
        .unwrap();

        assert_eq!(
            replies,
            vec![WireMessage::Block {
                block: Some(genesis)
            }]
        );
    }

    #[test]
    fn hello_rejects_network_mismatch() {
        let mut state = NodeState::new();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        let err = handle_message(
            &config,
            &mut state,
            WireMessage::Hello(HelloMessage {
                network: "other-net".to_string(),
                version: PROTOCOL_VERSION,
                node_name: None,
                advertised_addr: None,
                tip: None,
                height: None,
            }),
        )
        .unwrap_err();

        assert_eq!(
            err,
            PeerError::NetworkMismatch {
                local: "wobble-local".to_string(),
                remote: "other-net".to_string(),
            }
        );
    }

    #[test]
    fn hello_rejects_unsupported_version() {
        let mut state = NodeState::new();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        let err = handle_message(
            &config,
            &mut state,
            WireMessage::Hello(HelloMessage {
                network: "wobble-local".to_string(),
                version: PROTOCOL_VERSION + 1,
                node_name: None,
                advertised_addr: None,
                tip: None,
                height: None,
            }),
        )
        .unwrap_err();

        assert_eq!(err, PeerError::UnsupportedVersion(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn announce_tx_adds_transaction_to_mempool() {
        let sender = crate::crypto::signing_key_from_bytes([1; 32]);
        let recipient = crate::crypto::signing_key_from_bytes([2; 32]);
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
        let tx = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let txid = tx.txid();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        let replies = handle_message(
            &config,
            &mut state,
            WireMessage::AnnounceTx { transaction: tx },
        )
        .unwrap();

        assert!(replies.is_empty());
        assert!(state.mempool().get(&txid).is_some());
    }

    #[test]
    fn announce_block_accepts_block_into_local_state() {
        let owner = crate::crypto::signing_key_from_bytes([1; 32]);
        let genesis = mine_block(BlockHash::default(), 0x207f_ffff, &owner.verifying_key(), 0);
        let child = mine_block(
            genesis.header.block_hash(),
            0x207f_ffff,
            &owner.verifying_key(),
            1,
        );
        let child_hash = child.header.block_hash();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        let replies = handle_message(
            &config,
            &mut state,
            WireMessage::AnnounceBlock { block: child },
        )
        .unwrap();

        assert!(replies.is_empty());
        assert_eq!(state.chain().best_tip(), Some(child_hash));
    }

    #[test]
    fn mine_pending_mines_announced_transaction_into_block() {
        let sender = crate::crypto::signing_key_from_bytes([1; 32]);
        let recipient = crate::crypto::signing_key_from_bytes([2; 32]);
        let miner = crate::crypto::signing_key_from_bytes([3; 32]);
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
        let tx = spend(spendable, &sender, &recipient.verifying_key(), 30, 1);
        let txid = tx.txid();
        let mut state = NodeState::new();
        state.accept_block(genesis).unwrap();
        let config = ServerConfig::new("wobble-local", None, "127.0.0.1:9000");

        handle_message(
            &config,
            &mut state,
            WireMessage::AnnounceTx { transaction: tx },
        )
        .unwrap();

        let replies = handle_message(
            &config,
            &mut state,
            WireMessage::MinePending(MinePendingRequest {
                reward: crate::consensus::BLOCK_SUBSIDY,
                miner_public_key: crate::crypto::verifying_key_bytes(&miner.verifying_key())
                    .to_vec(),
                uniqueness: 2,
                bits: 0x207f_ffff,
                max_transactions: 10,
            }),
        )
        .unwrap();

        let [WireMessage::MinedBlock(MinedBlock { block_hash })] = replies.as_slice() else {
            panic!("expected a single mined block response");
        };
        let mined = state.get_block(block_hash).expect("mined block indexed");

        assert_eq!(state.chain().best_tip(), Some(*block_hash));
        assert!(state.mempool().get(&txid).is_none());
        assert_eq!(mined.transactions.len(), 2);
        assert_eq!(mined.transactions[1].txid(), txid);
    }
}
