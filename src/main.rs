use std::{env, path::Path};

use wobble::{
    aliases::{self, AliasBook},
    crypto, net,
    node_state::NodeState,
    peer::PeerConfig,
    server::Server,
    sqlite_store::SqliteStore,
    store,
    types::{BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid},
    wallet::{self, Wallet},
    wire::{HelloMessage, MinePendingRequest, PROTOCOL_VERSION, WireMessage},
};

fn main() {
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
            store::save_node_state(path, &state).map_err(|err| format!("save failed: {err:?}"))?;
            println!("initialized snapshot at {}", path.display());
            Ok(())
        }
        "info" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            println!("snapshot: {}", path.display());
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
            let state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            println!("{}", state.balance_for_key(&owner));
            Ok(())
        }
        "utxos" => {
            if args.len() != 3 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
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

            let snapshot_path = Path::new(&args[2]);
            let wallet_path = Path::new(&args[3]);
            let wallet = wallet::load_wallet(wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let state = store::load_node_state(snapshot_path)
                .map_err(|err| format!("load failed: {err:?}"))?;
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
            if args.len() != 5 && args.len() != 6 {
                return Err(usage());
            }

            let snapshot_path = Path::new(&args[2]);
            let listen_addr = &args[3];
            let network = args[4].clone();
            let node_name = args.get(5).cloned();
            let sqlite_path = snapshot_path.with_extension("sqlite");
            let state = store::load_node_state(snapshot_path)
                .map_err(|err| format!("load failed: {err:?}"))?;
            validate_sqlite_bootstrap(&sqlite_path, &state)?;
            let mut server = Server::new(PeerConfig::new(network.clone(), node_name), state)
                .with_snapshot_path(snapshot_path)
                .with_sqlite_path(&sqlite_path);

            println!("serving snapshot {}", snapshot_path.display());
            println!("sqlite store: {}", sqlite_path.display());
            println!("listen addr: {listen_addr}");
            println!("network: {network}");
            if let Some(name) = server.config().node_name.as_deref() {
                println!("node name: {name}");
            }
            println!(
                "best tip: {}",
                format_hash(server.state().chain().best_tip())
            );
            if sqlite_path.exists() {
                println!("sqlite bootstrap: validated");
            }

            server
                .serve(listen_addr)
                .map_err(|err| format!("server failed: {err}"))
        }
        "get-tip" => {
            if args.len() != 4 && args.len() != 5 {
                return Err(usage());
            }

            let peer_addr = &args[2];
            let network = args[3].clone();
            let node_name = args.get(4).cloned();
            let mut stream =
                net::connect(peer_addr).map_err(|err| format!("connect failed: {err}"))?;

            let hello = WireMessage::Hello(HelloMessage {
                network,
                version: PROTOCOL_VERSION,
                node_name,
                tip: None,
                height: None,
            });
            net::send_message(&mut stream, &hello)
                .map_err(|err| format!("send hello failed: {err}"))?;
            net::send_message(&mut stream, &WireMessage::GetTip)
                .map_err(|err| format!("send get_tip failed: {err}"))?;

            let remote_hello = net::receive_message(&mut stream)
                .map_err(|err| format!("receive hello failed: {err}"))?;
            let remote_tip = net::receive_message(&mut stream)
                .map_err(|err| format!("receive tip failed: {err}"))?;

            match remote_hello {
                WireMessage::Hello(message) => {
                    println!("peer network: {}", message.network);
                    println!("peer version: {}", message.version);
                    if let Some(name) = message.node_name {
                        println!("peer node name: {name}");
                    }
                }
                other => {
                    return Err(format!("unexpected first response: {other:?}"));
                }
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
            if args.len() != 9 && args.len() != 10 {
                return Err(usage());
            }

            let snapshot_path = Path::new(&args[2]);
            let sender_wallet_path = Path::new(&args[3]);
            let recipient_verifying_key = parse_public_key_or_alias(&args[4])?;
            let amount = parse_u64(&args[5], "amount")?;
            let uniqueness = parse_u32(&args[6], "uniqueness")?;
            let peer_addr = &args[7];
            let network = args[8].clone();
            let node_name = args.get(9).cloned();
            let sender_wallet = wallet::load_wallet(sender_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut local_state = store::load_node_state(snapshot_path)
                .map_err(|err| format!("load failed: {err:?}"))?;

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

            let mut stream =
                net::connect(peer_addr).map_err(|err| format!("connect failed: {err}"))?;
            let hello = WireMessage::Hello(HelloMessage {
                network,
                version: PROTOCOL_VERSION,
                node_name,
                tip: None,
                height: None,
            });
            net::send_message(&mut stream, &hello)
                .map_err(|err| format!("send hello failed: {err}"))?;
            let remote_hello = net::receive_message(&mut stream)
                .map_err(|err| format!("receive hello failed: {err}"))?;
            if !matches!(remote_hello, WireMessage::Hello(_)) {
                return Err(format!("unexpected handshake response: {remote_hello:?}"));
            }
            net::send_message(&mut stream, &WireMessage::AnnounceTx { transaction })
                .map_err(|err| format!("send transaction failed: {err}"))?;

            println!("submitted payment {} to {}", txid, peer_addr);
            Ok(())
        }
        "mine-pending-remote" => {
            if args.len() != 8 && args.len() != 9 {
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
            let node_name = args.get(8).cloned();
            let miner_wallet = wallet::load_wallet(miner_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut stream =
                net::connect(peer_addr).map_err(|err| format!("connect failed: {err}"))?;

            let hello = WireMessage::Hello(HelloMessage {
                network,
                version: PROTOCOL_VERSION,
                node_name,
                tip: None,
                height: None,
            });
            net::send_message(&mut stream, &hello)
                .map_err(|err| format!("send hello failed: {err}"))?;
            let remote_hello = net::receive_message(&mut stream)
                .map_err(|err| format!("receive hello failed: {err}"))?;
            if !matches!(remote_hello, WireMessage::Hello(_)) {
                return Err(format!("unexpected handshake response: {remote_hello:?}"));
            }

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

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
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
            store::save_node_state(path, &state).map_err(|err| format!("save failed: {err:?}"))?;

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

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let submitted = state
                .submit_payment(
                    sender_wallet.signing_key(),
                    &recipient_verifying_key,
                    amount,
                    uniqueness,
                )
                .map_err(|err| format!("submit failed: {err:?}"))?;
            store::save_node_state(path, &state).map_err(|err| format!("save failed: {err:?}"))?;

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

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let block_hash = state
                .mine_block(reward, &miner_wallet.verifying_key(), uniqueness, bits, 0)
                .map_err(|err| format!("block rejected: {err:?}"))?;
            store::save_node_state(path, &state).map_err(|err| format!("save failed: {err:?}"))?;

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

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let block_hash = state
                .mine_block(
                    reward,
                    &miner_wallet.verifying_key(),
                    uniqueness,
                    bits,
                    max_transactions,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            store::save_node_state(path, &state).map_err(|err| format!("save failed: {err:?}"))?;

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
        "  wobble init <snapshot>",
        "  wobble info <snapshot>",
        "  wobble balance <snapshot> <public_key>",
        "  wobble utxos <snapshot>",
        "  wobble generate-key",
        "  wobble create-wallet <wallet_path>",
        "  wobble wallet-address <wallet_path>",
        "  wobble wallet-balance <snapshot> <wallet_path>",
        "  wobble create-alias-book <alias_book>",
        "  wobble alias-add <alias_book> <name> <public_key>",
        "  wobble alias-list <alias_book>",
        "  wobble serve <snapshot> <listen_addr> <network> [node_name]",
        "  wobble get-tip <peer_addr> <network> [node_name]",
        "  wobble submit-payment-remote <snapshot> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> <uniqueness> <peer_addr> <network> [node_name]",
        "  wobble mine-pending-remote <reward> <miner_wallet> <uniqueness> <max_transactions> <peer_addr> <network> [node_name]",
        "  wobble submit-payment <snapshot> <sender_wallet> <recipient_public_key|@alias_book:name> <amount> <uniqueness>",
        "  wobble submit-transfer <snapshot> <txid> <vout> <amount> <sender_wallet> <recipient_public_key>",
        "  wobble mine-coinbase <snapshot> <reward> <miner_wallet> [uniqueness] [bits]",
        "  wobble mine-pending <snapshot> <reward> <miner_wallet> <uniqueness> <max_transactions> [bits]",
    ]
    .join("\n")
}

fn validate_sqlite_bootstrap(sqlite_path: &Path, snapshot_state: &NodeState) -> Result<(), String> {
    if !sqlite_path.exists() {
        return Ok(());
    }

    let store =
        SqliteStore::open(sqlite_path).map_err(|err| format!("sqlite load failed: {err:?}"))?;
    let sqlite_best_tip = store
        .load_best_tip()
        .map_err(|err| format!("sqlite best tip load failed: {err:?}"))?;
    let snapshot_best_tip = snapshot_state.chain().best_tip();
    if sqlite_best_tip != snapshot_best_tip {
        return Err(format!(
            "sqlite best tip mismatch: snapshot={} sqlite={}",
            format_hash(snapshot_best_tip),
            format_hash(sqlite_best_tip)
        ));
    }

    let sqlite_utxos = store
        .load_active_utxos()
        .map_err(|err| format!("sqlite active UTXO load failed: {err:?}"))?;
    if !utxo_sets_match(snapshot_state.active_utxos(), &sqlite_utxos) {
        return Err(format!(
            "sqlite active UTXO mismatch: snapshot={} sqlite={}",
            snapshot_state.active_utxos().len(),
            sqlite_utxos.len()
        ));
    }

    let sqlite_mempool = store
        .load_mempool()
        .map_err(|err| format!("sqlite mempool load failed: {err:?}"))?;
    if !mempools_match(snapshot_state.mempool(), &sqlite_mempool) {
        return Err(format!(
            "sqlite mempool mismatch: snapshot={} sqlite={}",
            snapshot_state.mempool().len(),
            sqlite_mempool.len()
        ));
    }

    Ok(())
}

fn utxo_sets_match(left: &wobble::state::UtxoSet, right: &wobble::state::UtxoSet) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter()
        .all(|(outpoint, utxo)| right.get(outpoint) == Some(utxo))
}

fn mempools_match(left: &wobble::mempool::Mempool, right: &wobble::mempool::Mempool) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter().all(|(txid, tx)| right.get(txid) == Some(tx))
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
