//! Local admin protocol used by the wobble CLI and future management tools.
//!
//! This module defines a small request/response protocol carried over a
//! localhost TCP socket. It intentionally sits outside the public peer protocol
//! so operator actions like status, balance queries, and local transaction
//! submission are not available to arbitrary peers.

use std::{
    io::{self, BufRead, BufReader, Write},
    net::TcpStream,
};

use serde::{Deserialize, Serialize};

use crate::{
    net,
    types::{BlockHash, Transaction, Txid},
};

/// Operator-facing snapshot of the local node state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSummary {
    pub tip: Option<BlockHash>,
    pub height: Option<u64>,
    pub branch_count: usize,
    pub mempool_size: usize,
    pub peer_count: usize,
    pub mining_enabled: bool,
}

/// Operator-facing balance result for one locking key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceSummary {
    pub amount: u64,
}

/// Result of an admin bootstrap mining request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapSummary {
    pub blocks_mined: u32,
    pub last_block_hash: Option<BlockHash>,
}

/// Local admin requests accepted by the node's admin socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AdminRequest {
    GetStatus,
    GetBalance { public_key: Vec<u8> },
    Bootstrap { public_key: Vec<u8>, blocks: u32 },
    SubmitTransaction { transaction: Transaction },
}

/// Local admin responses returned by the node's admin socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AdminResponse {
    Status(StatusSummary),
    Balance(BalanceSummary),
    Bootstrapped(BootstrapSummary),
    Submitted { txid: Txid },
    Error { message: String },
}

/// Errors returned while using the admin client protocol.
#[derive(Debug)]
pub enum AdminError {
    Connect(io::Error),
    Send(io::Error),
    Receive(io::Error),
    Server(String),
    UnexpectedResponse(AdminResponse),
}

/// Sends one newline-delimited admin request on a connected stream.
pub fn send_request(stream: &mut TcpStream, request: &AdminRequest) -> io::Result<()> {
    let mut line = serde_json::to_string(request)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Receives one newline-delimited admin response from a connected stream.
pub fn receive_response(stream: &mut TcpStream) -> io::Result<AdminResponse> {
    let cloned = stream.try_clone()?;
    receive_response_from_reader(BufReader::new(cloned))
}

/// Receives one admin response from a buffered line reader.
pub fn receive_response_from_reader<R: BufRead>(mut reader: R) -> io::Result<AdminResponse> {
    let mut line = String::new();
    let bytes_read = reader.read_line(&mut line)?;
    if bytes_read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "admin peer closed connection",
        ));
    }
    serde_json::from_str(line.trim_end_matches('\n'))
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Opens a one-shot localhost admin connection and requests node status.
pub fn request_status(admin_addr: &str) -> Result<StatusSummary, AdminError> {
    let mut stream = net::connect(admin_addr).map_err(AdminError::Connect)?;
    send_request(&mut stream, &AdminRequest::GetStatus).map_err(AdminError::Send)?;
    match receive_response(&mut stream).map_err(AdminError::Receive)? {
        AdminResponse::Status(summary) => Ok(summary),
        AdminResponse::Error { message } => Err(AdminError::Server(message)),
        other => Err(AdminError::UnexpectedResponse(other)),
    }
}

/// Opens a one-shot localhost admin connection and requests a balance.
pub fn request_balance(
    admin_addr: &str,
    public_key: Vec<u8>,
) -> Result<BalanceSummary, AdminError> {
    let mut stream = net::connect(admin_addr).map_err(AdminError::Connect)?;
    send_request(&mut stream, &AdminRequest::GetBalance { public_key })
        .map_err(AdminError::Send)?;
    match receive_response(&mut stream).map_err(AdminError::Receive)? {
        AdminResponse::Balance(summary) => Ok(summary),
        AdminResponse::Error { message } => Err(AdminError::Server(message)),
        other => Err(AdminError::UnexpectedResponse(other)),
    }
}

/// Opens a one-shot localhost admin connection and submits a signed transaction.
pub fn submit_transaction(admin_addr: &str, transaction: Transaction) -> Result<Txid, AdminError> {
    let mut stream = net::connect(admin_addr).map_err(AdminError::Connect)?;
    send_request(
        &mut stream,
        &AdminRequest::SubmitTransaction { transaction },
    )
    .map_err(AdminError::Send)?;
    match receive_response(&mut stream).map_err(AdminError::Receive)? {
        AdminResponse::Submitted { txid } => Ok(txid),
        AdminResponse::Error { message } => Err(AdminError::Server(message)),
        other => Err(AdminError::UnexpectedResponse(other)),
    }
}

/// Opens a one-shot localhost admin connection and mines initial funding blocks.
pub fn bootstrap_funds(
    admin_addr: &str,
    public_key: Vec<u8>,
    blocks: u32,
) -> Result<BootstrapSummary, AdminError> {
    let mut stream = net::connect(admin_addr).map_err(AdminError::Connect)?;
    send_request(&mut stream, &AdminRequest::Bootstrap { public_key, blocks })
        .map_err(AdminError::Send)?;
    match receive_response(&mut stream).map_err(AdminError::Receive)? {
        AdminResponse::Bootstrapped(summary) => Ok(summary),
        AdminResponse::Error { message } => Err(AdminError::Server(message)),
        other => Err(AdminError::UnexpectedResponse(other)),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    use crate::{
        admin::{
            AdminRequest, AdminResponse, BalanceSummary, BootstrapSummary, StatusSummary,
            bootstrap_funds, request_balance, request_status, submit_transaction,
        },
        types::{BlockHash, Transaction},
    };

    fn connected_listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        (listener, addr)
    }

    fn sample_transaction() -> Transaction {
        Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: Vec::new(),
            lock_time: 7,
        }
    }

    #[test]
    fn request_status_returns_status_summary() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: AdminRequest = serde_json::from_str(line.trim_end_matches('\n')).unwrap();
            assert_eq!(request, AdminRequest::GetStatus);

            let mut line = serde_json::to_string(&AdminResponse::Status(StatusSummary {
                tip: Some(BlockHash::new([0x11; 32])),
                height: Some(5),
                branch_count: 2,
                mempool_size: 2,
                peer_count: 3,
                mining_enabled: true,
            }))
            .unwrap();
            line.push('\n');
            stream.write_all(line.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let status = request_status(&addr).unwrap();
        worker.join().unwrap();

        assert_eq!(status.height, Some(5));
        assert_eq!(status.branch_count, 2);
        assert_eq!(status.mempool_size, 2);
        assert_eq!(status.peer_count, 3);
        assert!(status.mining_enabled);
    }

    #[test]
    fn request_balance_returns_balance_summary() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: AdminRequest = serde_json::from_str(line.trim_end_matches('\n')).unwrap();
            assert_eq!(
                request,
                AdminRequest::GetBalance {
                    public_key: vec![0x11; 32]
                }
            );

            let mut line =
                serde_json::to_string(&AdminResponse::Balance(BalanceSummary { amount: 42 }))
                    .unwrap();
            line.push('\n');
            stream.write_all(line.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let balance = request_balance(&addr, vec![0x11; 32]).unwrap();
        worker.join().unwrap();

        assert_eq!(balance.amount, 42);
    }

    #[test]
    fn submit_transaction_returns_submitted_txid() {
        let (listener, addr) = connected_listener();
        let transaction = sample_transaction();
        let txid = transaction.txid();

        let worker = thread::spawn({
            let transaction = transaction.clone();
            move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: AdminRequest =
                    serde_json::from_str(line.trim_end_matches('\n')).unwrap();
                assert_eq!(request, AdminRequest::SubmitTransaction { transaction });

                let mut line = serde_json::to_string(&AdminResponse::Submitted { txid }).unwrap();
                line.push('\n');
                stream.write_all(line.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let submitted = submit_transaction(&addr, transaction).unwrap();
        worker.join().unwrap();

        assert_eq!(submitted, txid);
    }

    #[test]
    fn bootstrap_funds_returns_bootstrap_summary() {
        let (listener, addr) = connected_listener();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: AdminRequest = serde_json::from_str(line.trim_end_matches('\n')).unwrap();
            assert_eq!(
                request,
                AdminRequest::Bootstrap {
                    public_key: vec![0x11; 32],
                    blocks: 3,
                }
            );

            let mut line = serde_json::to_string(&AdminResponse::Bootstrapped(BootstrapSummary {
                blocks_mined: 3,
                last_block_hash: Some(BlockHash::new([0x22; 32])),
            }))
            .unwrap();
            line.push('\n');
            stream.write_all(line.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let summary = bootstrap_funds(&addr, vec![0x11; 32], 3).unwrap();
        worker.join().unwrap();

        assert_eq!(summary.blocks_mined, 3);
        assert_eq!(summary.last_block_hash, Some(BlockHash::new([0x22; 32])));
    }
}
