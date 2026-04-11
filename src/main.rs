use std::{env, path::Path};

use wobble::{
    consensus,
    node_state::NodeState,
    store,
    types::{Block, BlockHash, BlockHeader, Transaction, TxOut},
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
            let prev = state.chain().best_tip().unwrap_or_default();
            let block = mine_coinbase_block(prev, reward, uniqueness, bits);
            let block_hash = block.header.block_hash();
            state
                .accept_block(block)
                .map_err(|err| format!("block rejected: {err:?}"))?;
            store::save_node_state(path, &state).map_err(|err| format!("save failed: {err:?}"))?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
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
        "  wobble mine-coinbase <snapshot> <reward> <uniqueness> [bits]",
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

fn format_hash(hash: Option<BlockHash>) -> String {
    hash.map(|value| value.to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

fn coinbase_transaction(reward: u64, uniqueness: u32) -> Transaction {
    Transaction {
        version: 1,
        inputs: Vec::new(),
        outputs: vec![TxOut {
            value: reward,
            locking_data: uniqueness.to_le_bytes().to_vec(),
        }],
        lock_time: uniqueness,
    }
}

fn mine_coinbase_block(
    prev_blockhash: BlockHash,
    reward: u64,
    uniqueness: u32,
    bits: u32,
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
        transactions: vec![coinbase_transaction(reward, uniqueness)],
    };
    block.header.merkle_root = block.merkle_root();

    loop {
        if consensus::validate_block(&block).is_ok() {
            return block;
        }
        block.header.nonce = block.header.nonce.wrapping_add(1);
    }
}
