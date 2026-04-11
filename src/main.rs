//! CLI entrypoint for wobble.
//!
//! The CLI centers the normal node workflow around a local node home and a
//! running server. Lower-level chain mutation and remote control commands still
//! exist for development and testing, but they are grouped under explicit
//! subcommands so the main user path stays small and predictable. The `serve`
//! command reads defaults from the node home's config file and treats CLI flags
//! as temporary overrides.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use tracing::info;

use wobble::{
    aliases::{self, AliasBook},
    client,
    consensus::BLOCK_SUBSIDY,
    crypto,
    home::{NodeConfig, NodeHome},
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

/// Parses the typed wobble command line and dispatches to the selected action.
#[derive(Debug, Parser)]
#[command(name = "wobble")]
#[command(about = "Bitcoin-inspired blockchain prototype")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Groups the primary user workflow ahead of lower-level admin and debug tools.
#[derive(Debug, Subcommand)]
enum Command {
    /// Initializes a node home with default state, wallet, aliases, and peers.
    Init(HomeArg),
    /// Starts the local node server.
    Serve(ServeCommand),
    /// Prints the local node wallet public key.
    WalletAddress(HomeArg),
    /// Prints the local node wallet balance.
    WalletBalance(HomeArg),
    /// Builds and queues a local payment from the node wallet.
    SubmitPayment(SubmitPaymentCommand),
    /// Reads chain or peer state without mutating local data.
    Inspect {
        #[command(subcommand)]
        command: InspectCommand,
    },
    /// Manages helper files like wallets and alias books.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Runs lower-level development and test commands.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

/// Shared option for commands that operate on the default node home layout.
#[derive(Debug, Args, Clone)]
struct HomeArg {
    /// Overrides the default node home directory.
    #[arg(long)]
    home: Option<PathBuf>,
}

/// Configures server startup and optional integrated mining.
#[derive(Debug, Args)]
struct ServeCommand {
    /// Overrides the default node home directory.
    #[arg(long)]
    home: Option<PathBuf>,
    /// Socket address to bind, for example `127.0.0.1:9001`.
    #[arg(long)]
    listen_addr: Option<String>,
    /// Network name used during peer handshake.
    #[arg(long)]
    network: Option<String>,
    /// Optional node name advertised during handshake.
    #[arg(long)]
    node_name: Option<String>,
    /// Optional peer list JSON file. Defaults to the node home's peers file.
    #[arg(long)]
    peers_path: Option<PathBuf>,
    /// Enables integrated mining using the wallet at this path.
    #[arg(long)]
    miner_wallet: Option<PathBuf>,
    /// Integrated miner polling interval in milliseconds.
    #[arg(long, requires = "miner_wallet")]
    mining_interval_ms: Option<u64>,
    /// Maximum mempool transactions to include per mined block.
    #[arg(long, requires = "miner_wallet")]
    mining_max_transactions: Option<usize>,
    /// Compact proof-of-work difficulty target for integrated mining.
    #[arg(long, requires = "miner_wallet")]
    mining_bits: Option<String>,
}

/// Builds a payment using the local node wallet and stores it in local state.
#[derive(Debug, Args)]
struct SubmitPaymentCommand {
    /// Recipient public key hex or `@alias_book:name`.
    recipient: String,
    /// Number of coins to send.
    amount: u64,
    /// Optional transaction uniqueness override.
    #[arg(long)]
    uniqueness: Option<u32>,
    /// Overrides the default node home directory.
    #[arg(long)]
    home: Option<PathBuf>,
}

/// Read-only inspection commands for local stores and peers.
#[derive(Debug, Subcommand)]
enum InspectCommand {
    /// Prints chain summary from a local sqlite state file.
    Info(InfoCommand),
    /// Prints the balance for a public key from a local sqlite state file.
    Balance(BalanceCommand),
    /// Prints the active UTXO set from a local sqlite state file.
    Utxos(UtxosCommand),
    /// Connects to a peer and prints its advertised best tip.
    Tip(GetTipCommand),
}

#[derive(Debug, Args)]
struct InfoCommand {
    sqlite_path: PathBuf,
}

#[derive(Debug, Args)]
struct BalanceCommand {
    sqlite_path: PathBuf,
    public_key: String,
}

#[derive(Debug, Args)]
struct UtxosCommand {
    sqlite_path: PathBuf,
}

#[derive(Debug, Args)]
struct GetTipCommand {
    peer_addr: String,
    network: String,
    #[arg(long)]
    node_name: Option<String>,
}

/// Helper commands for creating wallets, alias books, and raw keys.
#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Generates a random signing key and prints both halves.
    GenerateKey,
    /// Creates a wallet file at the requested path.
    CreateWallet(CreateWalletCommand),
    /// Creates an empty alias book JSON file.
    CreateAliasBook(CreateAliasBookCommand),
    /// Adds or replaces an alias entry in an alias book.
    AliasAdd(AliasAddCommand),
    /// Lists all alias book entries.
    AliasList(AliasListCommand),
}

#[derive(Debug, Args)]
struct CreateWalletCommand {
    wallet_path: PathBuf,
}

#[derive(Debug, Args)]
struct CreateAliasBookCommand {
    alias_book: PathBuf,
}

#[derive(Debug, Args)]
struct AliasAddCommand {
    alias_book: PathBuf,
    name: String,
    public_key: String,
}

#[derive(Debug, Args)]
struct AliasListCommand {
    alias_book: PathBuf,
}

/// Lower-level commands for development, test harnesses, and protocol poking.
#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Mines a coinbase-only block directly into a local sqlite state file.
    MineCoinbase(MineCoinbaseCommand),
    /// Mines a block from the local mempool directly into a local sqlite state file.
    MinePending(MinePendingCommand),
    /// Builds and queues a payment directly in a local sqlite state file.
    SubmitPayment(LocalSubmitPaymentCommand),
    /// Connects to a remote node and submits a built payment transaction.
    SubmitPaymentRemote(SubmitPaymentRemoteCommand),
    /// Connects to a remote node and asks it to mine pending transactions.
    MinePendingRemote(MinePendingRemoteCommand),
    /// Builds and submits a manual single-input transfer.
    SubmitTransfer(SubmitTransferCommand),
}

#[derive(Debug, Args)]
struct MineCoinbaseCommand {
    #[arg(long)]
    state_path: PathBuf,
    #[arg(long)]
    wallet_path: PathBuf,
    #[arg(long)]
    reward: u64,
    #[arg(long, default_value_t = 0)]
    uniqueness: u32,
    #[arg(long, default_value = "0x207fffff")]
    bits: String,
}

#[derive(Debug, Args)]
struct MinePendingCommand {
    #[arg(long)]
    state_path: PathBuf,
    #[arg(long)]
    wallet_path: PathBuf,
    #[arg(long)]
    reward: u64,
    #[arg(long)]
    uniqueness: u32,
    #[arg(long)]
    max_transactions: usize,
    #[arg(long, default_value = "0x207fffff")]
    bits: String,
}

#[derive(Debug, Args)]
struct LocalSubmitPaymentCommand {
    #[arg(long)]
    state_path: PathBuf,
    #[arg(long)]
    wallet_path: PathBuf,
    #[arg(long)]
    recipient: String,
    #[arg(long)]
    amount: u64,
    #[arg(long)]
    uniqueness: Option<u32>,
}

#[derive(Debug, Args)]
struct SubmitPaymentRemoteCommand {
    #[arg(long)]
    sqlite_path: PathBuf,
    #[arg(long)]
    sender_wallet: PathBuf,
    #[arg(long)]
    recipient: String,
    #[arg(long)]
    amount: u64,
    #[arg(long)]
    peer_addr: String,
    #[arg(long)]
    network: String,
    #[arg(long)]
    uniqueness: Option<u32>,
    #[arg(long)]
    node_name: Option<String>,
}

#[derive(Debug, Args)]
struct MinePendingRemoteCommand {
    #[arg(long)]
    reward: u64,
    #[arg(long)]
    miner_wallet: PathBuf,
    #[arg(long)]
    uniqueness: u32,
    #[arg(long)]
    max_transactions: usize,
    #[arg(long)]
    peer_addr: String,
    #[arg(long)]
    network: String,
    #[arg(long, default_value = "0x207fffff")]
    bits: String,
    #[arg(long)]
    node_name: Option<String>,
}

#[derive(Debug, Args)]
struct SubmitTransferCommand {
    #[arg(long)]
    state_path: PathBuf,
    #[arg(long)]
    txid: String,
    #[arg(long)]
    vout: u32,
    #[arg(long)]
    amount: u64,
    #[arg(long)]
    sender_wallet: PathBuf,
    #[arg(long)]
    recipient_public_key: String,
}

fn main() {
    logging::init();
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

/// Parses the CLI and executes the selected command.
fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(command) => run_init(command),
        Command::Serve(command) => run_serve(command),
        Command::WalletAddress(command) => run_wallet_address(command),
        Command::WalletBalance(command) => run_wallet_balance(command),
        Command::SubmitPayment(command) => run_submit_payment(command),
        Command::Inspect { command } => run_inspect(command),
        Command::Admin { command } => run_admin(command),
        Command::Debug { command } => run_debug(command),
    }
}

/// Initializes a node home and prints the generated local identity information.
fn run_init(command: HomeArg) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    home.initialize()
        .map_err(|err| format!("home init failed: {err:?}"))?;
    let config = home
        .load_config()
        .map_err(|err| format!("config load failed: {err:?}"))?;
    let wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    println!("initialized node home at {}", home.root().display());
    println!("state: {}", home.state_path().display());
    println!("wallet: {}", home.wallet_path().display());
    println!("aliases: {}", home.aliases_path().display());
    println!("peers: {}", home.peers_path().display());
    println!("config: {}", home.config_path().display());
    println!("listen addr: {}", config.listen_addr);
    println!("network: {}", config.network);
    if let Some(name) = config.node_name.as_deref() {
        println!("node name: {name}");
    }
    println!(
        "public: {}",
        encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
    );
    Ok(())
}

/// Starts the server from the local node home and optionally enables integrated mining.
fn run_serve(command: ServeCommand) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let config_file = home
        .load_config()
        .map_err(|err| format!("config load failed: {err:?}"))?;
    let runtime = resolve_serve_runtime(&config_file, &command);
    let sqlite_path = home.state_path();
    let state = SqliteStore::open(&sqlite_path)
        .and_then(|store| store.load_node_state())
        .map_err(|err| format!("sqlite bootstrap failed: {err:?}"))?;
    let config = PeerConfig::new(runtime.network.clone(), runtime.node_name.clone())
        .with_advertised_addr(&runtime.listen_addr);
    let peer_path = command
        .peers_path
        .clone()
        .unwrap_or_else(|| home.peers_path());
    let peer_endpoints = peers::load_peer_endpoints(&peer_path)
        .map_err(|err| format!("peer config load failed: {err:?}"))?;
    let mining_bits = command.mining_bits.as_deref().map(parse_bits).transpose()?;
    let mining = match command.miner_wallet.as_ref() {
        Some(path) => {
            let wallet =
                wallet::load_wallet(path).map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut mining = MiningConfig::new(wallet.verifying_key());
            if let Some(interval_ms) = command.mining_interval_ms {
                mining = mining.with_interval(std::time::Duration::from_millis(interval_ms));
            }
            if let Some(max_transactions) = command.mining_max_transactions {
                mining = mining.with_max_transactions(max_transactions);
            }
            if let Some(bits) = mining_bits {
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
        listen_addr = runtime.listen_addr,
        network = runtime.network,
        peer_count = peer_endpoints.len(),
        mining = mining.is_some(),
        best_tip = %format_hash(server.state().chain().best_tip()),
        "starting server"
    );
    println!("serving sqlite {}", sqlite_path.display());
    println!("node home: {}", home.root().display());
    println!("bootstrap source: sqlite");
    println!("config: {}", home.config_path().display());
    println!("listen addr: {}", runtime.listen_addr);
    println!("network: {}", runtime.network);
    if let Some(name) = server.config().node_name.as_deref() {
        println!("node name: {name}");
    }
    println!(
        "best tip: {}",
        format_hash(server.state().chain().best_tip())
    );
    println!("peers file: {}", peer_path.display());
    if let Some(path) = command.miner_wallet {
        println!("integrated mining: enabled");
        println!("miner wallet: {}", path.display());
        println!("mining reward: {}", BLOCK_SUBSIDY);
        println!(
            "mining interval ms: {}",
            command.mining_interval_ms.unwrap_or(250)
        );
        println!(
            "mining max transactions: {}",
            command.mining_max_transactions.unwrap_or(100)
        );
        println!("mining bits: {:#010x}", mining_bits.unwrap_or(0x207f_ffff));
    }
    println!("configured peers: {}", peer_endpoints.len());
    for peer in &peer_endpoints {
        match peer.node_name.as_deref() {
            Some(name) => println!("peer: {} ({name})", peer.addr),
            None => println!("peer: {}", peer.addr),
        }
    }

    server
        .serve(&runtime.listen_addr)
        .map_err(|err| format!("server failed: {err}"))
}

/// Prints the local node wallet address from the selected home.
fn run_wallet_address(command: HomeArg) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    println!(
        "{}",
        encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
    );
    Ok(())
}

/// Prints the balance for the local node wallet from the selected home.
fn run_wallet_balance(command: HomeArg) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    let state = load_sqlite_state(&home.state_path())?;
    println!("{}", state.balance_for_key(&wallet.verifying_key()));
    Ok(())
}

/// Builds a payment from the local node wallet and persists it into local state.
fn run_submit_payment(command: SubmitPaymentCommand) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let recipient_verifying_key = parse_public_key_or_alias(&command.recipient)?;
    let sender_wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    let mut state = load_sqlite_state(&home.state_path())?;
    let submitted = state
        .submit_payment(
            sender_wallet.signing_key(),
            &recipient_verifying_key,
            command.amount,
            command.uniqueness.unwrap_or_else(default_uniqueness),
        )
        .map_err(|err| format!("submit failed: {err:?}"))?;
    save_sqlite_state(&home.state_path(), &state)?;

    println!("queued payment {}", submitted);
    println!("mempool txs: {}", state.mempool().len());
    Ok(())
}

fn run_inspect(command: InspectCommand) -> Result<(), String> {
    match command {
        InspectCommand::Info(command) => {
            let state = load_sqlite_state(&command.sqlite_path)?;
            println!("sqlite: {}", command.sqlite_path.display());
            println!("indexed blocks: {}", state.chain().len());
            println!("best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        InspectCommand::Balance(command) => {
            let owner = parse_public_key(&command.public_key)?;
            let state = load_sqlite_state(&command.sqlite_path)?;
            println!("{}", state.balance_for_key(&owner));
            Ok(())
        }
        InspectCommand::Utxos(command) => {
            let state = load_sqlite_state(&command.sqlite_path)?;
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
        InspectCommand::Tip(command) => {
            let config = PeerConfig::new(command.network, command.node_name);
            let (mut stream, remote_hello) =
                client::connect_and_handshake(&command.peer_addr, &config)
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
                    Ok(())
                }
                other => Err(format!("unexpected second response: {other:?}")),
            }
        }
    }
}

fn run_admin(command: AdminCommand) -> Result<(), String> {
    match command {
        AdminCommand::GenerateKey => {
            let signing_key = crypto::generate_signing_key();
            println!("secret: {}", encode_hex(&signing_key.to_bytes()));
            println!(
                "public: {}",
                encode_hex(&crypto::verifying_key_bytes(&signing_key.verifying_key()))
            );
            Ok(())
        }
        AdminCommand::CreateWallet(command) => {
            let wallet = Wallet::generate();
            wallet::save_wallet(&command.wallet_path, &wallet)
                .map_err(|err| format!("wallet save failed: {err:?}"))?;
            println!("created wallet at {}", command.wallet_path.display());
            println!(
                "public: {}",
                encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
            );
            Ok(())
        }
        AdminCommand::CreateAliasBook(command) => {
            aliases::save_alias_book(&command.alias_book, &AliasBook::new())
                .map_err(|err| format!("alias save failed: {err:?}"))?;
            println!("created alias book at {}", command.alias_book.display());
            Ok(())
        }
        AdminCommand::AliasAdd(command) => {
            let public_key = parse_public_key(&command.public_key)?;
            let mut book = if command.alias_book.exists() {
                aliases::load_alias_book(&command.alias_book)
                    .map_err(|err| format!("alias load failed: {err:?}"))?
            } else {
                AliasBook::new()
            };
            book.insert(command.name.clone(), public_key);
            aliases::save_alias_book(&command.alias_book, &book)
                .map_err(|err| format!("alias save failed: {err:?}"))?;
            println!(
                "saved alias {} in {}",
                command.name,
                command.alias_book.display()
            );
            Ok(())
        }
        AdminCommand::AliasList(command) => {
            let book = aliases::load_alias_book(&command.alias_book)
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
    }
}

fn run_debug(command: DebugCommand) -> Result<(), String> {
    match command {
        DebugCommand::MineCoinbase(command) => {
            let miner_wallet = wallet::load_wallet(&command.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut state = load_sqlite_state(&command.state_path)?;
            let block_hash = state
                .mine_block(
                    command.reward,
                    &miner_wallet.verifying_key(),
                    command.uniqueness,
                    parse_bits(&command.bits)?,
                    0,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(&command.state_path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            if let Some(outpoint) = state.active_outpoints().last() {
                println!("latest outpoint: {}:{}", outpoint.txid, outpoint.vout);
            }
            Ok(())
        }
        DebugCommand::MinePending(command) => {
            let miner_wallet = wallet::load_wallet(&command.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut state = load_sqlite_state(&command.state_path)?;
            let block_hash = state
                .mine_block(
                    command.reward,
                    &miner_wallet.verifying_key(),
                    command.uniqueness,
                    parse_bits(&command.bits)?,
                    command.max_transactions,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(&command.state_path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        DebugCommand::SubmitPayment(command) => {
            let recipient_verifying_key = parse_public_key_or_alias(&command.recipient)?;
            let sender_wallet = wallet::load_wallet(&command.wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut state = load_sqlite_state(&command.state_path)?;
            let submitted = state
                .submit_payment(
                    sender_wallet.signing_key(),
                    &recipient_verifying_key,
                    command.amount,
                    command.uniqueness.unwrap_or_else(default_uniqueness),
                )
                .map_err(|err| format!("submit failed: {err:?}"))?;
            save_sqlite_state(&command.state_path, &state)?;

            println!("queued payment {}", submitted);
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        DebugCommand::SubmitPaymentRemote(command) => {
            let recipient_verifying_key = parse_public_key_or_alias(&command.recipient)?;
            let config = PeerConfig::new(command.network, command.node_name);
            let sender_wallet = wallet::load_wallet(&command.sender_wallet)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut local_state = load_sqlite_state(&command.sqlite_path)?;
            let txid = local_state
                .submit_payment(
                    sender_wallet.signing_key(),
                    &recipient_verifying_key,
                    command.amount,
                    command.uniqueness.unwrap_or_else(default_uniqueness),
                )
                .map_err(|err| format!("build failed: {err:?}"))?;
            let transaction =
                local_state.mempool().get(&txid).cloned().ok_or_else(|| {
                    format!("built transaction {txid} missing from local mempool")
                })?;

            let (mut stream, _) = client::connect_and_handshake(&command.peer_addr, &config)
                .map_err(|err| format!("handshake failed: {err:?}"))?;
            net::send_message(&mut stream, &WireMessage::AnnounceTx { transaction })
                .map_err(|err| format!("send transaction failed: {err}"))?;

            println!("submitted payment {} to {}", txid, command.peer_addr);
            Ok(())
        }
        DebugCommand::MinePendingRemote(command) => {
            let config = PeerConfig::new(command.network, command.node_name);
            let miner_wallet = wallet::load_wallet(&command.miner_wallet)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let (mut stream, _) = client::connect_and_handshake(&command.peer_addr, &config)
                .map_err(|err| format!("handshake failed: {err:?}"))?;

            net::send_message(
                &mut stream,
                &WireMessage::MinePending(MinePendingRequest {
                    reward: command.reward,
                    miner_public_key: crypto::verifying_key_bytes(&miner_wallet.verifying_key())
                        .to_vec(),
                    uniqueness: command.uniqueness,
                    bits: parse_bits(&command.bits)?,
                    max_transactions: command.max_transactions,
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
        DebugCommand::SubmitTransfer(command) => {
            let txid = parse_txid(&command.txid)?;
            let sender_wallet = wallet::load_wallet(&command.sender_wallet)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let recipient_public_key = parse_public_key(&command.recipient_public_key)?;

            let mut state = load_sqlite_state(&command.state_path)?;
            let mut tx = Transaction {
                version: 1,
                inputs: vec![TxIn {
                    previous_output: OutPoint {
                        txid,
                        vout: command.vout,
                    },
                    unlocking_data: Vec::new(),
                }],
                outputs: vec![TxOut {
                    value: command.amount,
                    locking_data: crypto::verifying_key_bytes(&recipient_public_key).to_vec(),
                }],
                lock_time: 0,
            };
            tx.inputs[0].unlocking_data =
                crypto::sign_message(sender_wallet.signing_key(), &tx.signing_digest()).to_vec();
            let submitted = state
                .submit_transaction(tx)
                .map_err(|err| format!("submit failed: {err:?}"))?;
            save_sqlite_state(&command.state_path, &state)?;

            println!("queued transaction {}", submitted);
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
    }
}

fn load_sqlite_state(path: &Path) -> Result<NodeState, String> {
    SqliteStore::open(path)
        .and_then(|store| store.load_node_state())
        .map_err(|err| format!("sqlite load failed: {err:?}"))
}

fn resolve_node_home(path: Option<&Path>) -> Result<NodeHome, String> {
    match path {
        Some(path) => Ok(NodeHome::new(path)),
        None => NodeHome::from_default_dir().map_err(|err| format!("home resolve failed: {err:?}")),
    }
}

/// Effective serve settings after merging home config defaults with CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeRuntime {
    listen_addr: String,
    network: String,
    node_name: Option<String>,
}

/// Resolves server runtime settings from node-home config plus CLI overrides.
fn resolve_serve_runtime(config: &NodeConfig, command: &ServeCommand) -> ServeRuntime {
    ServeRuntime {
        listen_addr: command
            .listen_addr
            .clone()
            .unwrap_or_else(|| config.listen_addr.clone()),
        network: command
            .network
            .clone()
            .unwrap_or_else(|| config.network.clone()),
        node_name: command
            .node_name
            .clone()
            .or_else(|| config.node_name.clone()),
    }
}

fn save_sqlite_state(path: &Path, state: &NodeState) -> Result<(), String> {
    let store = SqliteStore::open(path).map_err(|err| format!("sqlite open failed: {err:?}"))?;
    store
        .save_node_state(state)
        .map_err(|err| format!("sqlite save failed: {err:?}"))
}

fn parse_bits(value: &str) -> Result<u32, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).map_err(|_| format!("invalid bits: {value}"))
    } else {
        value
            .parse::<u32>()
            .map_err(|_| format!("invalid bits: {value}"))
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use wobble::home::NodeConfig;

    use super::{
        Cli, Command, DebugCommand, InspectCommand, MinePendingCommand, ServeCommand,
        SubmitPaymentCommand, resolve_serve_runtime,
    };

    #[test]
    fn parses_high_level_submit_payment_with_optional_uniqueness() {
        let cli = Cli::try_parse_from([
            "wobble",
            "submit-payment",
            "@/tmp/book.json:alice",
            "25",
            "--uniqueness",
            "7",
            "--home",
            "/tmp/node",
        ])
        .unwrap();

        match cli.command {
            Command::SubmitPayment(SubmitPaymentCommand {
                recipient,
                amount,
                uniqueness,
                home,
            }) => {
                assert_eq!(recipient, "@/tmp/book.json:alice");
                assert_eq!(amount, 25);
                assert_eq!(uniqueness, Some(7));
                assert_eq!(home.unwrap(), PathBuf::from("/tmp/node"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_serve_with_integrated_mining_flags() {
        let cli = Cli::try_parse_from([
            "wobble",
            "serve",
            "--home",
            "/tmp/proposer",
            "--listen-addr",
            "127.0.0.1:9001",
            "--network",
            "wobble-local",
            "--node-name",
            "alpha",
            "--peers-path",
            "/tmp/peers.json",
            "--miner-wallet",
            "/tmp/miner.wallet",
            "--mining-interval-ms",
            "500",
            "--mining-max-transactions",
            "25",
            "--mining-bits",
            "0x207fffff",
        ])
        .unwrap();

        match cli.command {
            Command::Serve(ServeCommand {
                home,
                listen_addr,
                network,
                node_name,
                peers_path,
                miner_wallet,
                mining_interval_ms,
                mining_max_transactions,
                mining_bits,
            }) => {
                assert_eq!(home.unwrap(), PathBuf::from("/tmp/proposer"));
                assert_eq!(listen_addr.as_deref(), Some("127.0.0.1:9001"));
                assert_eq!(network.as_deref(), Some("wobble-local"));
                assert_eq!(node_name.as_deref(), Some("alpha"));
                assert_eq!(peers_path.unwrap(), PathBuf::from("/tmp/peers.json"));
                assert_eq!(miner_wallet.unwrap(), PathBuf::from("/tmp/miner.wallet"));
                assert_eq!(mining_interval_ms, Some(500));
                assert_eq!(mining_max_transactions, Some(25));
                assert_eq!(mining_bits.as_deref(), Some("0x207fffff"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_debug_mine_pending_named_args() {
        let cli = Cli::try_parse_from([
            "wobble",
            "debug",
            "mine-pending",
            "--state-path",
            "/tmp/node.sqlite",
            "--wallet-path",
            "/tmp/miner.wallet",
            "--reward",
            "50",
            "--uniqueness",
            "3",
            "--max-transactions",
            "100",
        ])
        .unwrap();

        match cli.command {
            Command::Debug {
                command:
                    DebugCommand::MinePending(MinePendingCommand {
                        state_path,
                        wallet_path,
                        reward,
                        uniqueness,
                        max_transactions,
                        bits,
                    }),
            } => {
                assert_eq!(state_path, PathBuf::from("/tmp/node.sqlite"));
                assert_eq!(wallet_path, PathBuf::from("/tmp/miner.wallet"));
                assert_eq!(reward, 50);
                assert_eq!(uniqueness, 3);
                assert_eq!(max_transactions, 100);
                assert_eq!(bits, "0x207fffff");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_inspect_tip_with_optional_node_name() {
        let cli = Cli::try_parse_from([
            "wobble",
            "inspect",
            "tip",
            "127.0.0.1:9002",
            "wobble-local",
            "--node-name",
            "observer",
        ])
        .unwrap();

        match cli.command {
            Command::Inspect {
                command: InspectCommand::Tip(command),
            } => {
                assert_eq!(command.peer_addr, "127.0.0.1:9002");
                assert_eq!(command.network, "wobble-local");
                assert_eq!(command.node_name.as_deref(), Some("observer"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_serve_without_explicit_address_or_network() {
        let cli = Cli::try_parse_from(["wobble", "serve"]).unwrap();

        match cli.command {
            Command::Serve(ServeCommand {
                home,
                listen_addr,
                network,
                node_name,
                peers_path,
                miner_wallet,
                ..
            }) => {
                assert!(home.is_none());
                assert!(listen_addr.is_none());
                assert!(network.is_none());
                assert!(node_name.is_none());
                assert!(peers_path.is_none());
                assert!(miner_wallet.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn resolve_serve_runtime_prefers_cli_over_home_config() {
        let config = NodeConfig {
            listen_addr: "127.0.0.1:9001".to_string(),
            network: "wobble-local".to_string(),
            node_name: Some("saved".to_string()),
        };
        let command = ServeCommand {
            home: Some(PathBuf::from("/tmp/node")),
            listen_addr: Some("127.0.0.1:9010".to_string()),
            network: None,
            node_name: Some("override".to_string()),
            peers_path: None,
            miner_wallet: None,
            mining_interval_ms: None,
            mining_max_transactions: None,
            mining_bits: None,
        };

        let runtime = resolve_serve_runtime(&config, &command);

        assert_eq!(runtime.listen_addr, "127.0.0.1:9010");
        assert_eq!(runtime.network, "wobble-local");
        assert_eq!(runtime.node_name.as_deref(), Some("override"));
    }
}
