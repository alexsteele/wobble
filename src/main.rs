use std::{
    env,
    path::{Path, PathBuf},
};

use tracing::info;

use wobble::{
    aliases::{self, AliasBook},
    client,
    consensus::BLOCK_SUBSIDY,
    crypto,
    home::NodeHome,
    logging, net,
    node_state::NodeState,
    peer::PeerConfig,
    peers,
    server::{MiningConfig, Server},
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
            if args.len() != 2 && args.len() != 4 {
                return Err(usage());
            }

            let home_override = parse_optional_home_flag(&args[2..])?;
            let home = resolve_node_home(home_override.as_deref())?;
            home.initialize()
                .map_err(|err| format!("home init failed: {err:?}"))?;
            let wallet = wallet::load_wallet(&home.wallet_path())
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            println!("initialized node home at {}", home.root().display());
            println!("state: {}", home.state_path().display());
            println!("wallet: {}", home.wallet_path().display());
            println!("aliases: {}", home.aliases_path().display());
            println!("peers: {}", home.peers_path().display());
            println!(
                "public: {}",
                encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
            );
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
            let path = parse_wallet_address_path(&args[2..])?;
            let wallet =
                wallet::load_wallet(&path).map_err(|err| format!("wallet load failed: {err:?}"))?;
            println!(
                "{}",
                encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
            );
            Ok(())
        }
        "wallet-balance" => {
            let local = parse_wallet_balance_paths(&args[2..])?;
            let wallet = wallet::load_wallet(&local.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let state = load_sqlite_state(&local.state_path)?;
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
            if args.len() < 4 {
                return Err(usage());
            }

            let listen_addr = &args[2];
            let network = args[3].clone();
            let serve_options = parse_serve_args(&args[4..])?;
            let home = resolve_node_home(serve_options.home_path.as_deref())?;
            let sqlite_path = home.state_path();
            let state = SqliteStore::open(&sqlite_path)
                .and_then(|store| store.load_node_state())
                .map_err(|err| format!("sqlite bootstrap failed: {err:?}"))?;
            let config = PeerConfig::new(network.clone(), serve_options.node_name.clone())
                .with_advertised_addr(listen_addr);
            let peer_endpoints = match serve_options.peers_path.as_ref() {
                Some(path) => peers::load_peer_endpoints(path)
                    .map_err(|err| format!("peer config load failed: {err:?}"))?,
                None => peers::load_peer_endpoints(&home.peers_path())
                    .map_err(|err| format!("peer config load failed: {err:?}"))?,
            };
            let mining = match serve_options.miner_wallet_path.as_ref() {
                Some(path) => {
                    let wallet = wallet::load_wallet(path)
                        .map_err(|err| format!("wallet load failed: {err:?}"))?;
                    let mut mining = MiningConfig::new(wallet.verifying_key());
                    if let Some(interval_ms) = serve_options.mining_interval_ms {
                        mining =
                            mining.with_interval(std::time::Duration::from_millis(interval_ms));
                    }
                    if let Some(max_transactions) = serve_options.mining_max_transactions {
                        mining = mining.with_max_transactions(max_transactions);
                    }
                    if let Some(bits) = serve_options.mining_bits {
                        mining = mining.with_bits(bits);
                    }
                    Some(mining)
                }
                None => None,
            };
            let mut server = Server::new(config, state)
                .with_peers(peer_endpoints.clone())
                .with_sqlite_path(&sqlite_path)
                .with_bootstrap_sync(true);
            if let Some(mining) = mining.clone() {
                server = server.with_mining(mining);
            }

            info!(
                command = "serve",
                home = %home.root().display(),
                sqlite_path = %sqlite_path.display(),
                listen_addr,
                network,
                peer_count = peer_endpoints.len(),
                mining = mining.is_some(),
                best_tip = %format_hash(server.state().chain().best_tip()),
                "starting server"
            );
            println!("serving sqlite {}", sqlite_path.display());
            println!("node home: {}", home.root().display());
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
            if let Some(path) = serve_options.peers_path {
                println!("peers file: {}", path.display());
            } else {
                println!("peers file: {}", home.peers_path().display());
            }
            if let Some(path) = serve_options.miner_wallet_path {
                println!("integrated mining: enabled");
                println!("miner wallet: {}", path.display());
                println!("mining reward: {}", BLOCK_SUBSIDY);
                println!(
                    "mining interval ms: {}",
                    serve_options.mining_interval_ms.unwrap_or(250)
                );
                println!(
                    "mining max transactions: {}",
                    serve_options.mining_max_transactions.unwrap_or(100)
                );
                println!(
                    "mining bits: {:#010x}",
                    serve_options.mining_bits.unwrap_or(0x207f_ffff)
                );
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
            if args.len() != 8 && args.len() != 9 && args.len() != 10 && args.len() != 11 {
                return Err(usage());
            }

            let sqlite_path = Path::new(&args[2]);
            let sender_wallet_path = Path::new(&args[3]);
            let recipient_verifying_key = parse_public_key_or_alias(&args[4])?;
            let amount = parse_u64(&args[5], "amount")?;
            let (uniqueness, peer_addr, network, node_name_args) =
                if let Ok(value) = parse_u32(&args[6], "uniqueness") {
                    (value, &args[7], args[8].clone(), &args[9..])
                } else {
                    (default_uniqueness(), &args[6], args[7].clone(), &args[8..])
                };
            let node_name = parse_optional_node_name_flag(node_name_args)?;
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
            let parsed = parse_local_submit_payment_args(&args[2..])?;
            let recipient_verifying_key = parse_public_key_or_alias(&parsed.recipient)?;
            let sender_wallet = wallet::load_wallet(&parsed.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(&parsed.state_path)?;
            let submitted = state
                .submit_payment(
                    sender_wallet.signing_key(),
                    &recipient_verifying_key,
                    parsed.amount,
                    parsed.uniqueness,
                )
                .map_err(|err| format!("submit failed: {err:?}"))?;
            save_sqlite_state(&parsed.state_path, &state)?;

            println!("queued payment {}", submitted);
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        "mine-coinbase" => {
            let parsed = parse_local_mine_coinbase_args(&args[2..])?;
            let miner_wallet = wallet::load_wallet(&parsed.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(&parsed.state_path)?;
            let block_hash = state
                .mine_block(
                    parsed.reward,
                    &miner_wallet.verifying_key(),
                    parsed.uniqueness,
                    parsed.bits,
                    0,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(&parsed.state_path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            if let Some(outpoint) = state.active_outpoints().last() {
                println!("latest outpoint: {}:{}", outpoint.txid, outpoint.vout);
            }
            Ok(())
        }
        "mine-pending" => {
            let parsed = parse_local_mine_pending_args(&args[2..])?;
            let miner_wallet = wallet::load_wallet(&parsed.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;

            let mut state = load_sqlite_state(&parsed.state_path)?;
            let block_hash = state
                .mine_block(
                    parsed.reward,
                    &miner_wallet.verifying_key(),
                    parsed.uniqueness,
                    parsed.bits,
                    parsed.max_transactions,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(&parsed.state_path, &state)?;

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
        "",
        "  High-level workflow:",
        "    wobble init [--home <dir>]",
        "    wobble serve <listen_addr> <network> [--home <dir>] [--node_name <name>] [--peers_path <path>] [--miner_wallet <path>] [--mining_interval_ms <ms>] [--mining_max_transactions <count>] [--mining_bits <bits>]",
        "    wobble submit-payment <recipient_public_key|@alias_book:name> <amount> [uniqueness] [--home <dir>]",
        "    wobble submit-payment-remote <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> [uniqueness] <peer_addr> <network> [--node_name <name>]",
        "    wobble wallet-address [--home <dir>]",
        "    wobble wallet-balance [--home <dir>]",
        "",
        "  Inspect state:",
        "    wobble get-tip <peer_addr> <network> [--node_name <name>]",
        "    wobble info <sqlite_path>",
        "    wobble balance <sqlite_path> <public_key>",
        "    wobble utxos <sqlite_path>",
        "",
        "  Mining and low-level transaction control:",
        "    wobble mine-pending <reward> <uniqueness> <max_transactions> [bits] [--home <dir>]",
        "    wobble mine-pending <sqlite_path> <reward> <miner_wallet> <uniqueness> <max_transactions> [bits]",
        "    wobble mine-pending-remote <reward> <miner_wallet> <uniqueness> <max_transactions> <peer_addr> <network> [--node_name <name>]",
        "    wobble mine-coinbase <reward> [uniqueness] [bits] [--home <dir>]",
        "    wobble mine-coinbase <sqlite_path> <reward> <miner_wallet> [uniqueness] [bits]",
        "    wobble submit-payment <sqlite_path> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> [uniqueness]",
        "    wobble submit-transfer <sqlite_path> <txid> <vout> <amount> <sender_wallet> <recipient_public_key>",
        "",
        "  Setup and helpers:",
        "    wobble create-wallet <wallet_path>",
        "    wobble create-alias-book <alias_book>",
        "    wobble alias-add <alias_book> <name> <public_key>",
        "    wobble alias-list <alias_book>",
        "    wobble wallet-address <wallet_path>",
        "    wobble wallet-balance <sqlite_path> <wallet_path>",
        "    wobble generate-key",
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
/// - `--home <dir>`
/// - `--node_name <name>`
/// - `--peers_path <path>`
/// - `--miner_wallet <path>`
/// - `--mining_interval_ms <ms>`
/// - `--mining_max_transactions <count>`
/// - `--mining_bits <bits>`
#[derive(Debug, PartialEq, Eq)]
struct ServeOptions {
    home_path: Option<PathBuf>,
    node_name: Option<String>,
    peers_path: Option<PathBuf>,
    miner_wallet_path: Option<PathBuf>,
    mining_interval_ms: Option<u64>,
    mining_max_transactions: Option<usize>,
    mining_bits: Option<u32>,
}

fn parse_serve_args(args: &[String]) -> Result<ServeOptions, String> {
    let mut home_path = None;
    let mut node_name = None;
    let mut peers_path = None;
    let mut miner_wallet_path = None;
    let mut mining_interval_ms = None;
    let mut mining_max_transactions = None;
    let mut mining_bits = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--home" => {
                if home_path.is_some() {
                    return Err("duplicate --home".to_string());
                }
                let Some(path) = args.get(index + 1) else {
                    return Err("missing value for --home".to_string());
                };
                home_path = Some(PathBuf::from(path));
                index += 2;
            }
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
                peers_path = Some(PathBuf::from(path));
                index += 2;
            }
            "--miner_wallet" => {
                if miner_wallet_path.is_some() {
                    return Err("duplicate --miner_wallet".to_string());
                }
                let Some(path) = args.get(index + 1) else {
                    return Err("missing value for --miner_wallet".to_string());
                };
                miner_wallet_path = Some(PathBuf::from(path));
                index += 2;
            }
            "--mining_interval_ms" => {
                if mining_interval_ms.is_some() {
                    return Err("duplicate --mining_interval_ms".to_string());
                }
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --mining_interval_ms".to_string());
                };
                mining_interval_ms = Some(parse_u64(value, "mining_interval_ms")?);
                index += 2;
            }
            "--mining_max_transactions" => {
                if mining_max_transactions.is_some() {
                    return Err("duplicate --mining_max_transactions".to_string());
                }
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --mining_max_transactions".to_string());
                };
                mining_max_transactions = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid mining_max_transactions: {value}"))?,
                );
                index += 2;
            }
            "--mining_bits" => {
                if mining_bits.is_some() {
                    return Err("duplicate --mining_bits".to_string());
                }
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --mining_bits".to_string());
                };
                mining_bits = Some(parse_bits(value)?);
                index += 2;
            }
            value => {
                return Err(format!("unexpected serve argument: {value}"));
            }
        }
    }

    if miner_wallet_path.is_none()
        && (mining_interval_ms.is_some()
            || mining_max_transactions.is_some()
            || mining_bits.is_some())
    {
        return Err("mining flags require --miner_wallet".to_string());
    }

    Ok(ServeOptions {
        home_path,
        node_name,
        peers_path,
        miner_wallet_path,
        mining_interval_ms,
        mining_max_transactions,
        mining_bits,
    })
}

fn parse_optional_home_flag(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--home" => Ok(Some(PathBuf::from(value))),
        [flag] if flag == "--home" => Err("missing value for --home".to_string()),
        [value] => Err(format!("unexpected argument: {value}")),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

fn split_trailing_optional_home_flag(
    args: &[String],
) -> Result<(&[String], Option<PathBuf>), String> {
    match args {
        [rest @ .., flag, value] if flag == "--home" => Ok((rest, Some(PathBuf::from(value)))),
        [rest @ .., flag] if flag == "--home" => Err("missing value for --home".to_string()),
        _ => Ok((args, None)),
    }
}

fn resolve_node_home(path: Option<&Path>) -> Result<NodeHome, String> {
    match path {
        Some(path) => Ok(NodeHome::new(path)),
        None => NodeHome::from_default_dir().map_err(|err| format!("home resolve failed: {err:?}")),
    }
}

#[derive(Debug, Clone)]
struct LocalNodePaths {
    state_path: PathBuf,
    wallet_path: PathBuf,
}

#[derive(Debug, Clone)]
struct LocalSubmitPaymentArgs {
    state_path: PathBuf,
    wallet_path: PathBuf,
    recipient: String,
    amount: u64,
    uniqueness: u32,
}

#[derive(Debug, Clone)]
struct LocalMineCoinbaseArgs {
    state_path: PathBuf,
    wallet_path: PathBuf,
    reward: u64,
    uniqueness: u32,
    bits: u32,
}

#[derive(Debug, Clone)]
struct LocalMinePendingArgs {
    state_path: PathBuf,
    wallet_path: PathBuf,
    reward: u64,
    uniqueness: u32,
    max_transactions: usize,
    bits: u32,
}

// TODO: The local command parsing is still more complicated than it should be.
// Consolidate the explicit-path and home-default forms behind a smaller shared
// command model instead of maintaining several shape-specific helpers here.
fn resolve_local_node_paths(home_override: Option<&Path>) -> Result<LocalNodePaths, String> {
    let home = resolve_node_home(home_override)?;
    Ok(LocalNodePaths {
        state_path: home.state_path(),
        wallet_path: home.wallet_path(),
    })
}

fn parse_wallet_address_path(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(resolve_local_node_paths(None)?.wallet_path),
        [flag] if flag == "--home" => Err("missing value for --home".to_string()),
        [flag, value] if flag == "--home" => {
            Ok(resolve_local_node_paths(Some(Path::new(value)))?.wallet_path)
        }
        [path] => Ok(PathBuf::from(path)),
        _ => Err(usage()),
    }
}

fn parse_wallet_balance_paths(args: &[String]) -> Result<LocalNodePaths, String> {
    match args {
        [] => resolve_local_node_paths(None),
        [flag, value] if flag == "--home" => resolve_local_node_paths(Some(Path::new(value))),
        [sqlite_path, wallet_path] => Ok(LocalNodePaths {
            state_path: PathBuf::from(sqlite_path),
            wallet_path: PathBuf::from(wallet_path),
        }),
        [flag] if flag == "--home" => Err("missing value for --home".to_string()),
        _ => Err(usage()),
    }
}

fn parse_submit_payment_paths(args: &[String]) -> Result<(LocalNodePaths, &[String]), String> {
    let (command_args, home_override) = split_trailing_optional_home_flag(args)?;
    if command_args.len() == 3 {
        return Ok((
            resolve_local_node_paths(home_override.as_deref())?,
            command_args,
        ));
    }
    if args.len() == 5 && args.get(3).map(String::as_str) != Some("--home") {
        return Ok((
            LocalNodePaths {
                state_path: PathBuf::from(&args[0]),
                wallet_path: PathBuf::from(&args[1]),
            },
            &args[2..],
        ));
    }
    Err(usage())
}

fn parse_local_submit_payment_args(args: &[String]) -> Result<LocalSubmitPaymentArgs, String> {
    let (local, command_args) = parse_submit_payment_paths(args)?;
    if command_args.len() < 2 || command_args.len() > 3 {
        return Err(usage());
    }
    Ok(LocalSubmitPaymentArgs {
        state_path: local.state_path,
        wallet_path: local.wallet_path,
        recipient: command_args[0].clone(),
        amount: parse_u64(&command_args[1], "amount")?,
        uniqueness: command_args
            .get(2)
            .map(|value| parse_u32(value, "uniqueness"))
            .transpose()?
            .unwrap_or_else(default_uniqueness),
    })
}

fn parse_local_mine_coinbase_args(args: &[String]) -> Result<LocalMineCoinbaseArgs, String> {
    let (command_args, home_override) = split_trailing_optional_home_flag(args)?;
    let (local, reward_arg, uniqueness_arg, bits_arg) =
        if !command_args.is_empty() && parse_u64(&command_args[0], "reward").is_ok() {
            if command_args.len() > 3 {
                return Err(usage());
            }
            (
                resolve_local_node_paths(home_override.as_deref())?,
                &command_args[0],
                command_args.get(1),
                command_args.get(2),
            )
        } else if (3..=5).contains(&command_args.len()) {
            (
                LocalNodePaths {
                    state_path: PathBuf::from(&command_args[0]),
                    wallet_path: PathBuf::from(&command_args[2]),
                },
                &command_args[1],
                command_args.get(3),
                command_args.get(4),
            )
        } else {
            return Err(usage());
        };
    Ok(LocalMineCoinbaseArgs {
        state_path: local.state_path,
        wallet_path: local.wallet_path,
        reward: parse_u64(reward_arg, "reward")?,
        uniqueness: uniqueness_arg
            .map(|value| parse_u32(value, "uniqueness"))
            .transpose()?
            .unwrap_or(0),
        bits: bits_arg
            .map(|value| parse_bits(value))
            .transpose()?
            .unwrap_or(0x207f_ffff),
    })
}

fn parse_local_mine_pending_args(args: &[String]) -> Result<LocalMinePendingArgs, String> {
    let (command_args, home_override) = split_trailing_optional_home_flag(args)?;
    let (local, reward_arg, uniqueness_arg, max_transactions_arg, bits_arg) =
        if command_args.len() == 3 || command_args.len() == 4 {
            (
                resolve_local_node_paths(home_override.as_deref())?,
                &command_args[0],
                &command_args[1],
                &command_args[2],
                command_args.get(3),
            )
        } else if command_args.len() == 5 || command_args.len() == 6 {
            (
                LocalNodePaths {
                    state_path: PathBuf::from(&command_args[0]),
                    wallet_path: PathBuf::from(&command_args[2]),
                },
                &command_args[1],
                &command_args[3],
                &command_args[4],
                command_args.get(5),
            )
        } else {
            return Err(usage());
        };
    Ok(LocalMinePendingArgs {
        state_path: local.state_path,
        wallet_path: local.wallet_path,
        reward: parse_u64(reward_arg, "reward")?,
        uniqueness: parse_u32(uniqueness_arg, "uniqueness")?,
        max_transactions: max_transactions_arg
            .parse::<usize>()
            .map_err(|_| format!("invalid max_transactions: {max_transactions_arg}"))?,
        bits: bits_arg
            .map(|value| parse_bits(value))
            .transpose()?
            .unwrap_or(0x207f_ffff),
    })
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

/// Returns a default transaction uniqueness value for user-facing CLI commands.
///
/// This keeps the common payment flow free from manual nonce-like inputs while
/// still producing distinct transactions when users omit an explicit override.
fn default_uniqueness() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after unix epoch")
        .subsec_nanos()
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
    use std::path::PathBuf;

    use super::{
        ServeOptions, parse_optional_home_flag, parse_optional_node_name_flag, parse_serve_args,
    };

    #[test]
    fn parse_serve_args_accepts_node_name_and_peer_file() {
        let args = vec![
            "--node_name".to_string(),
            "alpha".to_string(),
            "--peers_path".to_string(),
            "/tmp/peers.json".to_string(),
        ];

        let options = parse_serve_args(&args).unwrap();

        assert_eq!(
            options,
            ServeOptions {
                home_path: None,
                node_name: Some("alpha".to_string()),
                peers_path: Some(PathBuf::from("/tmp/peers.json")),
                miner_wallet_path: None,
                mining_interval_ms: None,
                mining_max_transactions: None,
                mining_bits: None,
            }
        );
    }

    #[test]
    fn parse_serve_args_accepts_peer_file_without_node_name() {
        let args = vec!["--peers_path".to_string(), "/tmp/peers.json".to_string()];

        let options = parse_serve_args(&args).unwrap();

        assert_eq!(
            options,
            ServeOptions {
                home_path: None,
                node_name: None,
                peers_path: Some(PathBuf::from("/tmp/peers.json")),
                miner_wallet_path: None,
                mining_interval_ms: None,
                mining_max_transactions: None,
                mining_bits: None,
            }
        );
    }

    #[test]
    fn parse_serve_args_accepts_integrated_mining_flags() {
        let args = vec![
            "--node_name".to_string(),
            "alpha".to_string(),
            "--home".to_string(),
            "/tmp/proposer".to_string(),
            "--miner_wallet".to_string(),
            "/tmp/miner.wallet".to_string(),
            "--mining_interval_ms".to_string(),
            "500".to_string(),
            "--mining_max_transactions".to_string(),
            "25".to_string(),
            "--mining_bits".to_string(),
            "0x207fffff".to_string(),
        ];

        let options = parse_serve_args(&args).unwrap();

        assert_eq!(
            options,
            ServeOptions {
                home_path: Some(PathBuf::from("/tmp/proposer")),
                node_name: Some("alpha".to_string()),
                peers_path: None,
                miner_wallet_path: Some(PathBuf::from("/tmp/miner.wallet")),
                mining_interval_ms: Some(500),
                mining_max_transactions: Some(25),
                mining_bits: Some(0x207f_ffff),
            }
        );
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
    fn parse_serve_args_rejects_duplicate_home_flag() {
        let args = vec![
            "--home".to_string(),
            "/tmp/a".to_string(),
            "--home".to_string(),
            "/tmp/b".to_string(),
        ];

        let err = parse_serve_args(&args).unwrap_err();

        assert_eq!(err, "duplicate --home");
    }

    #[test]
    fn parse_serve_args_rejects_mining_overrides_without_wallet() {
        let args = vec!["--mining_interval_ms".to_string(), "250".to_string()];

        let err = parse_serve_args(&args).unwrap_err();

        assert_eq!(err, "mining flags require --miner_wallet");
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

    #[test]
    fn parse_optional_home_flag_accepts_named_form() {
        let args = vec!["--home".to_string(), "/tmp/proposer".to_string()];

        let home = parse_optional_home_flag(&args).unwrap();

        assert_eq!(home, Some(PathBuf::from("/tmp/proposer")));
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
