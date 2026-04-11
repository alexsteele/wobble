use std::{env, path::Path};

use wobble::{
    node_state::NodeState,
    store,
    types::{BlockHash, OutPoint, Transaction, TxIn, TxOut},
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
                        "{}:{} value={} coinbase={}",
                        outpoint.txid, outpoint.vout, utxo.output.value, utxo.is_coinbase
                    );
                }
            }
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
            let uniqueness = parse_u32(&args[6], "uniqueness")?;
            let lock_tag = parse_u32(&args[7], "lock_tag")?;

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let tx = Transaction {
                version: 1,
                inputs: vec![TxIn {
                    previous_output: OutPoint { txid, vout },
                    unlocking_data: uniqueness.to_le_bytes().to_vec(),
                }],
                outputs: vec![TxOut {
                    value: amount,
                    locking_data: lock_tag.to_le_bytes().to_vec(),
                }],
                lock_time: uniqueness,
            };
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
            let sender_lock_tag = parse_u32(&args[3], "sender_lock_tag")?;
            let recipient_lock_tag = parse_u32(&args[4], "recipient_lock_tag")?;
            let amount = parse_u64(&args[5], "amount")?;
            let uniqueness = parse_u32(&args[6], "uniqueness")?;

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let submitted = state
                .submit_payment(sender_lock_tag, recipient_lock_tag, amount, uniqueness)
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
            let uniqueness = parse_u32(&args[4], "uniqueness")?;
            let bits = if let Some(raw_bits) = args.get(5) {
                parse_bits(raw_bits)?
            } else {
                0x207f_ffff
            };

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let block_hash = state
                .mine_block(reward, uniqueness, bits, 0)
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
            if args.len() != 6 && args.len() != 7 {
                return Err(usage());
            }

            let path = Path::new(&args[2]);
            let reward = parse_u64(&args[3], "reward")?;
            let uniqueness = parse_u32(&args[4], "uniqueness")?;
            let max_transactions = args[5]
                .parse::<usize>()
                .map_err(|_| format!("invalid max_transactions: {}", args[5]))?;
            let bits = if let Some(raw_bits) = args.get(6) {
                parse_bits(raw_bits)?
            } else {
                0x207f_ffff
            };

            let mut state =
                store::load_node_state(path).map_err(|err| format!("load failed: {err:?}"))?;
            let block_hash = state
                .mine_block(reward, uniqueness, bits, max_transactions)
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
        "  wobble submit-payment <snapshot> <sender_lock_tag> <recipient_lock_tag> <amount> <uniqueness>",
        "  wobble submit-transfer <snapshot> <txid> <vout> <amount> <uniqueness> <lock_tag>",
        "  wobble mine-coinbase <snapshot> <reward> <uniqueness> [bits]",
        "  wobble mine-pending <snapshot> <reward> <uniqueness> <max_transactions> [bits]",
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

fn parse_txid(value: &str) -> Result<wobble::types::Txid, String> {
    if value.len() != 64 {
        return Err(format!("invalid txid length: {value}"));
    }

    let mut bytes = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk).map_err(|_| format!("invalid txid: {value}"))?;
        bytes[index] = u8::from_str_radix(hex, 16).map_err(|_| format!("invalid txid: {value}"))?;
    }

    Ok(wobble::types::Txid::new(bytes))
}

fn format_hash(hash: Option<BlockHash>) -> String {
    hash.map(|value| value.to_string())
        .unwrap_or_else(|| "<none>".to_string())
}
