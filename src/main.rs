use std::{env, path::Path};

use tracing::info;

use wobble::{
    aliases::{self, AliasBook},
    client, crypto, logging, net,
    node_state::NodeState,
    peer::PeerConfig,
    peers,
    server::Server,
    sqlite_store::SqliteStore,
    types::{BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid},
    wallet::{self, Wallet},
    wire::{MinePendingRequest, WireMessage},
};

fn main() {
    logging::init();
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let Some(command) = args.get(1).map(String::as_str) else {
        return Err(usage());
    };

    match command {
        "init" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let state = NodeState::new();
            let store =
                SqliteStore::open(path).map_err(|err| format!("sqlite open failed: {err:?}"))?;
            store
                .save_node_state(&state)
                .map_err(|err| format!("sqlite save failed: {err:?}"))?;
            println!("initialized sqlite node state at {}", path.display());
            Ok(())
        }
        "info" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let state = load_sqlite_state(path)?;
            println!("sqlite: {}", path.display());
            println!("indexed blocks: {}", state.chain().len());
            println!("best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        "balance" => {
            if args.len() != 4 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let owner = parse_public_key(&args[3])?;
            let state = load_sqlite_state(path)?;
            println!("{}", state.balance_for_key(&owner));
            Ok(())
        }
        "utxos" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let state = load_sqlite_state(path)?;
            for outpoint in state.active_outpoints() {
                if let Some(utxo) = state.active_utxos().get(&outpoint) {
                    println!(
                        "{}:{} value={} coinbase={} owner={}",
                        outpoint.txid,
                        outpoint.vout,
                        utxo.output.value,
                        utxo.is_coinbase,
                        encode_hex(&utxo.output.locking_data)
                    );
                }
            }
            Ok(())
        }
        "generate-key" => {
            if args.len() != 2 {
                return Err(usage());
            }

            let signing_key = crypto::generate_signing_key();
            println!("secret: {}", encode_hex(&signing_key.to_bytes()));
            println!(
                "public: {}",
                encode_hex(&crypto::verifying_key_bytes(&signing_key.verifying_key()))
            );
            Ok(())
        }
        "create-wallet" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let wallet = Wallet::generate();
            wallet::save_wallet(path, &wallet)
                .map_err(|err| format!("wallet save failed: {err:?}"))?;
            println!("created wallet at {}", path.display());
            println!(
                "public: {}",
                encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
            );
            Ok(())
        }
        "wallet-address" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let wallet =
                wallet::load_wallet(path).map_err(|err| format!("wallet load failed: {err:?}"))?;
            println!(
                "{}",
                encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
            );
            Ok(())
        }
        "wallet-balance" => {
            if args.len() != 4 {
                return Err(usage());
            }

            let sqlite_path = Path::new(&args[2]);
            let wallet_path = Path::new(&args[3]);
            let wallet = wallet::load_wallet(wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let state = load_sqlite_state(sqlite_path)?;
            println!("{}", state.balance_for_key(&wallet.verifying_key()));
            Ok(())
        }
        "create-alias-book" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            aliases::save_alias_book(path, &AliasBook::new())
                .map_err(|err| format!("alias save failed: {err:?}"))?;
            println!("created alias book at {}", path.display());
            Ok(())
        }
        "alias-add" => {
            if args.len() != 5 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let alias = args[3].clone();
            let public_key = parse_public_key(&args[4])?;
            let mut book = if path.exists() {
                aliases::load_alias_book(path)
                    .map_err(|err| format!("alias load failed: {err:?}"))?
            } else {
                AliasBook::new()
            };
            book.insert(alias.clone(), public_key);
            aliases::save_alias_book(path, &book)
                .map_err(|err| format!("alias save failed: {err:?}"))?;
            println!("saved alias {} in {}", alias, path.display());
            Ok(())
        }
        "alias-list" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let book = aliases::load_alias_book(path)
                .map_err(|err| format!("alias load failed: {err:?}"))?;
            for (alias, public_key) in book.entries() {
                println!(
                    "{} {}",
                    alias,
                    encode_hex(&crypto::verifying_key_bytes(&public_key))
                );
            }
            Ok(())
        }
        "serve" => {
            if args.len() < 5 {
                return Err(usage());
            }

            let sqlite_path = Path::new(&args[2]);
            let listen_addr = &args[3];
            let network = args[4].clone();
            let (node_name, peers_path) = parse_serve_args(&args[5..])?;
            let state = SqliteStore::open(sqlite_path)
                .and_then(|store| store.load_node_state())
                .map_err(|err| format!("sqlite bootstrap failed: {err:?}"))?;
            let config =
                PeerConfig::new(network.clone(), node_name).with_advertised_addr(listen_addr);
            let peer_endpoints = match peers_path {
                Some(path) => peers::load_peer_endpoints(path)
                    .map_err(|err| format!("peer config load failed: {err:?}"))?,
                None => Vec::new(),
            };
            let mut server = Server::new(config, state)
                .with_peers(peer_endpoints.clone())
                .with_sqlite_path(sqlite_path)
                .with_bootstrap_sync(true);

            info!(
                command = "serve",
                sqlite_path = %sqlite_path.display(),
                listen_addr,
                network,
                peer_count = peer_endpoints.len(),
                best_tip = %format_hash(server.state().chain().best_tip()),
                "starting server"
            );
            println!("serving sqlite {}", sqlite_path.display());
            println!("bootstrap source: sqlite");
            println!("listen addr: {listen_addr}");
            println!("network: {network}");
            if let Some(name) = server.config().node_name.as_deref() {
                println!("node name: {name}");
            }
            println!(
                "best tip: {}",
                format_hash(server.state().chain().best_tip())
            );
            if let Some(path) = peers_path {
                println!("peers file: {}", path.display());
            }
            println!("configured peers: {}", peer_endpoints.len());
            for peer in &peer_endpoints {
                match peer.node_name.as_deref() {
                    Some(name) => println!("peer: {} ({name})", peer.addr),
                    None => println!("peer: {}", peer.addr),
                }
            }

            server
                .serve(listen_addr)
                .map_err(|err| format!("server failed: {err}"))
        }
        "get-tip" => {
            if args.len() != 4 && args.len() != 6 {
                return Err(usage());
            }

            let peer_addr = &args[2];
            let network = args[3].clone();
            let node_name = parse_optional_node_name_flag(&args[4..])?;
            let config = PeerConfig::new(network, node_name);
            let (mut stream, remote_hello) = client::connect_and_handshake(peer_addr, &config)
                .map_err(|err| format!("handshake failed: {err:?}"))?;
            net::send_message(&mut stream, &WireMessage::GetTip)
                .map_err(|err| format!("send get_tip failed: {err}"))?;

            let remote_tip = net::receive_message(&mut stream)
                .map_err(|err| format!("receive tip failed: {err}"))?;

            println!("peer network: {}", remote_hello.network);
            println!("peer version: {}", remote_hello.version);
            if let Some(name) = remote_hello.node_name {
                println!("peer node name: {name}");
            }

            match remote_tip {
                WireMessage::Tip(summary) => {
                    println!("peer tip: {}", format_hash(summary.tip));
                    println!(
                        "peer height: {}",
                        summary
                            .height
                            .map(|height| height.to_string())
                            .unwrap_or_else(|| "<none>".to_string())
                    );
                }
                other => {
                    return Err(format!("unexpected second response: {other:?}"));
                }
            }

            Ok(())
        }
        "submit-payment-remote" => {
            if args.len() != 9 && args.len() != 11 {
                return Err(usage());
            }

            let sqlite_path = Path::new(&args[2]);
            let sender_wallet_path = Path::new(&args[3]);
            let recipient_verifying_key = parse_public_key_or_alias(&args[4])?;
            let amount = parse_u64(&args[5], "amount")?;
            let uniqueness = parse_u32(&args[6], "uniqueness")?;
            let peer_addr = &args[7];
            let network = args[8].clone();
            let node_name = parse_optional_node_name_flag(&args[9..])?;
            let config = PeerConfig::new(network, node_name);
            let sender_wallet = wallet::load_wallet(sender_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut local_state = load_sqlite_state(sqlite_path)?;

            let txid = local_state
                .submit_payment(
                    sender_wallet.signing_key(),
                    &recipient_verifying_key,
                    amount,
                    uniqueness,
                )
                .map_err(|err| format!("build failed: {err:?}"))?;
            let transaction =
                local_state.mempool().get(&txid).cloned().ok_or_else(|| {
                    format!("built transaction {txid} missing from local mempool")
                })?;

            let (mut stream, _) = client::connect_and_handshake(peer_addr, &config)
                .map_err(|err| format!("handshake failed: {err:?}"))?;
            net::send_message(&mut stream, &WireMessage::AnnounceTx { transaction })
                .map_err(|err| format!("send transaction failed: {err}"))?;

            println!("submitted payment {} to {}", txid, peer_addr);
            Ok(())
        }
        "mine-pending-remote" => {
            if args.len() != 8 && args.len() != 10 {
                return Err(usage());
            }

            let reward = parse_u64(&args[2], "reward")?;
            let miner_wallet_path = Path::new(&args[3]);
            let uniqueness = parse_u32(&args[4], "uniqueness")?;
            let max_transactions = args[5]
                .parse::<usize>()
                .map_err(|_| format!("invalid max_transactions: {}", args[5]))?;
            let peer_addr = &args[6];
            let network = args[7].clone();
            let node_name = parse_optional_node_name_flag(&args[8..])?;
            let config = PeerConfig::new(network, node_name);
            let miner_wallet = wallet::load_wallet(miner_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let (mut stream, _) = client::connect_and_handshake(peer_addr, &config)
                .map_err(|err| format!("handshake failed: {err:?}"))?;

            net::send_message(
                &mut stream,
                &WireMessage::MinePending(MinePendingRequest {
                    reward,
                    miner_public_key: crypto::verifying_key_bytes(&miner_wallet.verifying_key())
                        .to_vec(),
                    uniqueness,
                    bits: 0x207f_ffff,
                    max_transactions,
                }),
            )
            .map_err(|err| format!("send mine_pending failed: {err}"))?;

            let response = net::receive_message(&mut stream)
                .map_err(|err| format!("receive mined block failed: {err}"))?;
            match response {
                WireMessage::MinedBlock(result) => {
                    println!("mined block {}", result.block_hash);
                    Ok(())
                }
                other => Err(format!("unexpected mine response: {other:?}")),
            }
        }
        "submit-transfer" => {
            if args.len() != 8 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let txid = parse_txid(&args[3])?;
            let vout = parse_u32(&args[4], "vout")?;
            let amount = parse_u64(&args[5], "amount")?;
            let sender_wallet_path = Path::new(&args[6]);
            let recipient_public_key = parse_public_key(&args[7])?;
            let sender_wallet = wallet::load_wallet(sender_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(path)?;
            let mut tx = Transaction {
                version: 1,
                inputs: vec![TxIn {
                    previous_output: OutPoint { txid, vout },
                    unlocking_data: Vec::new(),
                }],
                outputs: vec![TxOut {
                    value: amount,
                    locking_data: crypto::verifying_key_bytes(&recipient_public_key).to_vec(),
                }],
                lock_time: 0,
            };
            tx.inputs[0].unlocking_data =
                crypto::sign_message(sender_wallet.signing_key(), &tx.signing_digest()).to_vec();
            let submitted = state
                .submit_transaction(tx)
                .map_err(|err| format!("submit failed: {err:?}"))?;
            save_sqlite_state(path, &state)?;

            println!("queued transaction {}", submitted);
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        "submit-payment" => {
            if args.len() != 7 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let sender_wallet_path = Path::new(&args[3]);
            let recipient_verifying_key = parse_public_key_or_alias(&args[4])?;
            let amount = parse_u64(&args[5], "amount")?;
            let uniqueness = parse_u32(&args[6], "uniqueness")?;
            let sender_wallet = wallet::load_wallet(sender_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(path)?;
            let submitted = state
                .submit_payment(
                    sender_wallet.signing_key(),
                    &recipient_verifying_key,
                    amount,
                    uniqueness,
                )
                .map_err(|err| format!("submit failed: {err:?}"))?;
            save_sqlite_state(path, &state)?;

            println!("queued payment {}", submitted);
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        "mine-coinbase" => {
            if args.len() != 5 && args.len() != 6 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let reward = parse_u64(&args[3], "reward")?;
            let miner_wallet_path = Path::new(&args[4]);
            let uniqueness = if args.len() == 6 {
                parse_u32(&args[5], "uniqueness")?
            } else {
                0
            };
            let bits = if let Some(raw_bits) = args.get(6) {
                parse_bits(raw_bits)?
            } else {
                0x207f_ffff
            };
            let miner_wallet = wallet::load_wallet(miner_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(path)?;
            let block_hash = state
                .mine_block(reward, &miner_wallet.verifying_key(), uniqueness, bits, 0)
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            if let Some(outpoint) = state.active_outpoints().last() {
                println!("latest outpoint: {}:{}", outpoint.txid, outpoint.vout);
            }
            Ok(())
        }
        "mine-pending" => {
            if args.len() != 7 && args.len() != 8 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let reward = parse_u64(&args[3], "reward")?;
            let miner_wallet_path = Path::new(&args[4]);
            let uniqueness = parse_u32(&args[5], "uniqueness")?;
            let max_transactions = args[6]
                .parse::<usize>()
                .map_err(|_| format!("invalid max_transactions: {}", args[6]))?;
            let bits = if let Some(raw_bits) = args.get(7) {
                parse_bits(raw_bits)?
            } else {
                0x207f_ffff
            };
            let miner_wallet = wallet::load_wallet(miner_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(path)?;
            let block_hash = state
                .mine_block(
                    reward,
                    &miner_wallet.verifying_key(),
                    uniqueness,
                    bits,
                    max_transactions,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    [
        "usage:",
        "  wobble init <sqlite_path>",
        "  wobble info <sqlite_path>",
        "  wobble balance <sqlite_path> <public_key>",
        "  wobble utxos <sqlite_path>",
        "  wobble generate-key",
        "  wobble create-wallet <wallet_path>",
        "  wobble wallet-address <wallet_path>",
        "  wobble wallet-balance <sqlite_path> <wallet_path>",
        "  wobble create-alias-book <alias_book>",
        "  wobble alias-add <alias_book> <name> <public_key>",
        "  wobble alias-list <alias_book>",
        "  wobble serve <sqlite_path> <listen_addr> <network> [--node_name <name>] [--peers_path <path>]",
        "  wobble get-tip <peer_addr> <network> [--node_name <name>]",
        "  wobble submit-payment-remote <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> <uniqueness> <peer_addr> <network> [--node_name <name>]",
        "  wobble mine-pending-remote <reward> <miner_wallet> <uniqueness> <max_transactions> <peer_addr> <network> [--node_name <name>]",
        "  wobble submit-payment <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> <uniqueness>",
        "  wobble submit-transfer <sqlite_path> <txid> <vout> <amount> <sender_wallet> <recipient_public_key>",
        "  wobble mine-coinbase <sqlite_path> <reward> <miner_wallet> [uniqueness] [bits]",
        "  wobble mine-pending <sqlite_path> <reward> <miner_wallet> <uniqueness> <max_transactions> [bits]",
    ]
    .join("\n")
}

fn load_sqlite_state(path: &Path) -> Result<NodeState, String> {
    SqliteStore::open(path)
        .and_then(|store| store.load_node_state())
        .map_err(|err| format!("sqlite load failed: {err:?}"))
}

/// Parses optional `serve` arguments without changing the existing required prefix.
///
/// Supported forms:
/// - `--node_name <name>`
/// - `--peers_path <path>`
/// - both flags in either order
fn parse_serve_args(args: &[String]) -> Result<(Option<String>, Option<&Path>), String> {
    let mut node_name = None;
    let mut peers_path = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--node_name" => {
                if node_name.is_some() {
                    return Err("duplicate --node_name".to_string());
                }
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --node_name".to_string());
                };
                node_name = Some(value.to_string());
                index += 2;
            }
            "--peers_path" => {
                if peers_path.is_some() {
                    return Err("duplicate --peers_path".to_string());
                }
                let Some(path) = args.get(index + 1) else {
                    return Err("missing value for --peers_path".to_string());
                };
                peers_path = Some(Path::new(path));
                index += 2;
            }
            value => {
                return Err(format!("unexpected serve argument: {value}"));
            }
        }
    }

    Ok((node_name, peers_path))
}

/// Parses an optional `--node_name <name>` flag used by remote CLI commands.
fn parse_optional_node_name_flag(args: &[String]) -> Result<Option<String>, String> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--node_name" => Ok(Some(value.clone())),
        [flag] if flag == "--node_name" => Err("missing value for --node_name".to_string()),
        [value] => Err(format!("unexpected argument: {value}")),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

fn save_sqlite_state(path: &Path, state: &NodeState) -> Result<(), String> {
    let store = SqliteStore::open(path).map_err(|err| format!("sqlite open failed: {err:?}"))?;
    store
        .save_node_state(state)
        .map_err(|err| format!("sqlite save failed: {err:?}"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid {name}: {value}"))
}

fn parse_u32(value: &str, name: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("invalid {name}: {value}"))
}

fn parse_bits(value: &str) -> Result<u32, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).map_err(|_| format!("invalid bits: {value}"))
    } else {
        parse_u32(value, "bits")
    }
}

fn parse_txid(value: &str) -> Result<Txid, String> {
    Ok(Txid::new(parse_hex_array::<32>(value, "txid")?))
}

fn format_hash(hash: Option<BlockHash>) -> String {
    hash.map(|value| value.to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

fn parse_public_key(value: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    crypto::parse_verifying_key(&parse_hex_vec(value, "public key")?)
        .ok_or_else(|| format!("invalid public key: {value}"))
}

fn parse_public_key_or_alias(value: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    if let Some(spec) = value.strip_prefix('@') {
        let (book_path, alias) = spec
            .split_once(':')
            .ok_or_else(|| format!("invalid alias reference: {value}"))?;
        let book = aliases::load_alias_book(Path::new(book_path))
            .map_err(|err| format!("alias load failed: {err:?}"))?;
        book.resolve(alias)
            .map_err(|err| format!("alias resolve failed: {err:?}"))
    } else {
        parse_public_key(value)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{parse_optional_node_name_flag, parse_serve_args};

    #[test]
    fn parse_serve_args_accepts_node_name_and_peer_file() {
        let args = vec![
            "--node_name".to_string(),
            "alpha".to_string(),
            "--peers_path".to_string(),
            "/tmp/peers.json".to_string(),
        ];

        let (node_name, peers_path) = parse_serve_args(&args).unwrap();

        assert_eq!(node_name, Some("alpha".to_string()));
        assert_eq!(peers_path, Some(Path::new("/tmp/peers.json")));
    }

    #[test]
    fn parse_serve_args_accepts_peer_file_without_node_name() {
        let args = vec!["--peers_path".to_string(), "/tmp/peers.json".to_string()];

        let (node_name, peers_path) = parse_serve_args(&args).unwrap();

        assert_eq!(node_name, None);
        assert_eq!(peers_path, Some(Path::new("/tmp/peers.json")));
    }

    #[test]
    fn parse_serve_args_rejects_missing_peer_file_value() {
        let args = vec![
            "--node_name".to_string(),
            "alpha".to_string(),
            "--peers_path".to_string(),
        ];

        let err = parse_serve_args(&args).unwrap_err();

        assert_eq!(err, "missing value for --peers_path");
    }

    #[test]
    fn parse_serve_args_rejects_positional_node_name() {
        let args = vec!["alpha".to_string()];

        let err = parse_serve_args(&args).unwrap_err();

        assert_eq!(err, "unexpected serve argument: alpha");
    }

    #[test]
    fn parse_serve_args_rejects_duplicate_node_name_flag() {
        let args = vec![
            "--node_name".to_string(),
            "alpha".to_string(),
            "--node_name".to_string(),
            "beta".to_string(),
        ];

        let err = parse_serve_args(&args).unwrap_err();

        assert_eq!(err, "duplicate --node_name");
    }

    #[test]
    fn parse_optional_node_name_flag_accepts_named_form() {
        let args = vec!["--node_name".to_string(), "alpha".to_string()];

        let node_name = parse_optional_node_name_flag(&args).unwrap();

        assert_eq!(node_name, Some("alpha".to_string()));
    }

    #[test]
    fn parse_optional_node_name_flag_rejects_old_positional_form() {
        let args = vec!["alpha".to_string()];

        let err = parse_optional_node_name_flag(&args).unwrap_err();

        assert_eq!(err, "unexpected argument: alpha");
    }
}

fn parse_hex_vec(value: &str, name: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err(format!("invalid {name}: {value}"));
    }

    let mut bytes = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks_exact(2) {
        let hex = std::str::from_utf8(chunk).map_err(|_| format!("invalid {name}: {value}"))?;
        let byte = u8::from_str_radix(hex, 16).map_err(|_| format!("invalid {name}: {value}"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

fn parse_hex_array<const N: usize>(value: &str, name: &str) -> Result<[u8; N], String> {
    let bytes = parse_hex_vec(value, name)?;
    bytes
        .try_into()
        .map_err(|_| format!("invalid {name}: {value}"))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
