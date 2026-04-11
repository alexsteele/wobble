use std::{env, path::Path};

use ed25519_dalek::SigningKey;
use wobble::{
    crypto,
    node_state::NodeState,
    store,
    types::{BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid},
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
        "submit-transfer" => {
            if args.len() != 8 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let txid = parse_txid(&args[3])?;
            let vout = parse_u32(&args[4], "vout")?;
            let amount = parse_u64(&args[5], "amount")?;
            let secret_key = parse_signing_key(&args[6])?;
            let recipient_public_key = parse_public_key(&args[7])?;

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
                crypto::sign_message(&secret_key, &tx.signing_digest()).to_vec();
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
            let sender_signing_key = parse_signing_key(&args[3])?;
            let recipient_verifying_key = parse_public_key(&args[4])?;
            let amount = parse_u64(&args[5], "amount")?;
            let uniqueness = parse_u32(&args[6], "uniqueness")?;

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let submitted = state
                .submit_payment(
                    &sender_signing_key,
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
            let miner_verifying_key = parse_public_key(&args[4])?;
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

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let block_hash = state
                .mine_block(reward, &miner_verifying_key, uniqueness, bits, 0)
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
            let miner_verifying_key = parse_public_key(&args[4])?;
            let uniqueness = parse_u32(&args[5], "uniqueness")?;
            let max_transactions = args[6]
                .parse::<usize>()
                .map_err(|_| format!("invalid max_transactions: {}", args[6]))?;
            let bits = if let Some(raw_bits) = args.get(7) {
                parse_bits(raw_bits)?
            } else {
                0x207f_ffff
            };

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let block_hash = state
                .mine_block(
                    reward,
                    &miner_verifying_key,
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
        "  wobble utxos <snapshot>",
        "  wobble generate-key",
        "  wobble submit-payment <snapshot> <sender_secret_key> <recipient_public_key> <amount> <uniqueness>",
        "  wobble submit-transfer <snapshot> <txid> <vout> <amount> <sender_secret_key> <recipient_public_key>",
        "  wobble mine-coinbase <snapshot> <reward> <miner_public_key> [uniqueness] [bits]",
        "  wobble mine-pending <snapshot> <reward> <miner_public_key> <uniqueness> <max_transactions> [bits]",
    ]
    .join("\n")
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

fn parse_signing_key(value: &str) -> Result<SigningKey, String> {
    Ok(crypto::signing_key_from_bytes(parse_hex_array::<32>(
        value,
        "secret key",
    )?))
}

fn parse_public_key(value: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    crypto::parse_verifying_key(&parse_hex_vec(value, "public key")?)
        .ok_or_else(|| format!("invalid public key: {value}"))
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
