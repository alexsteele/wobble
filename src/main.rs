//! CLI entrypoint for wobble.
//!
//! The CLI centers the normal node workflow around a local node home and a
//! running server. Lower-level chain mutation and remote control commands still
//! exist for development and testing, but they are grouped under explicit
//! subcommands so the main user path stays small and predictable. The `serve`
//! command reads defaults from the node home's config file and treats CLI flags
//! as temporary overrides.

use std::{
    io,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand};
use tracing::info;

use wobble::{
    admin,
    aliases::{self, AliasBook},
    client,
    consensus::BLOCK_SUBSIDY,
    crypto,
    home::{MiningSection, NodeConfig, NodeHome},
    logging, net,
    node_state::{NodeState, build_payment_transaction},
    peer::PeerConfig,
    peers,
    server::{MiningConfig, Server},
    sqlite_store::SqliteStore,
    tx_index::{IndexedTransaction, IndexedTransactionStatus, TransactionKeyRole},
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
    /// Mines initial coinbase-only funding blocks to the local node wallet.
    Bootstrap(BootstrapCommand),
    /// Starts the local node server.
    Serve(ServeCommand),
    /// Alias for `wallet balance`.
    Balance(HomeArg),
    /// Reads or updates the local wallet stored under the node home.
    Wallet {
        #[command(subcommand)]
        command: WalletCommand,
    },
    /// Prints recent wallet transactions from the local sqlite index.
    Transactions(HomeArg),
    /// Prints status from the running local node server.
    Status(HomeArg),
    /// Builds and queues a local payment from one wallet key.
    Pay(PayCommand),
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

/// Mines initial funding blocks to the local node wallet through the admin socket.
#[derive(Debug, Args)]
struct BootstrapCommand {
    /// Number of coinbase-only blocks to mine.
    #[arg(long, default_value_t = 1)]
    blocks: u32,
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
    /// Forces integrated mining on for this run.
    #[arg(long, conflicts_with = "no_mining")]
    mining: bool,
    /// Forces integrated mining off for this run.
    #[arg(long, conflicts_with = "mining")]
    no_mining: bool,
    /// Enables integrated mining using the wallet at this path.
    #[arg(long)]
    miner_wallet: Option<PathBuf>,
    /// Integrated miner polling interval in milliseconds.
    #[arg(long)]
    mining_interval_ms: Option<u64>,
    /// Maximum mempool transactions to include per mined block.
    #[arg(long)]
    mining_max_transactions: Option<usize>,
    /// Compact proof-of-work difficulty target for integrated mining.
    #[arg(long)]
    mining_bits: Option<String>,
}

/// Builds a signed payment from one wallet key and submits it to the local node server.
#[derive(Debug, Args)]
struct PayCommand {
    /// Wallet key name to spend from. Defaults to the wallet's default key.
    #[arg(long = "from")]
    from_key: Option<String>,
    /// Recipient public key hex or `@alias_book:name`.
    to: String,
    /// Number of coins to send.
    amount: u64,
    /// Optional transaction uniqueness override.
    #[arg(long)]
    uniqueness: Option<u32>,
    /// Overrides the default node home directory.
    #[arg(long)]
    home: Option<PathBuf>,
}

/// Offline wallet inspection and key-management commands.
#[derive(Debug, Subcommand)]
enum WalletCommand {
    /// Prints wallet identity and key metadata.
    Info(HomeArg),
    /// Prints total and per-key wallet balances from local state.
    Balance(HomeArg),
    /// Generates a new named keypair in the local wallet.
    NewKey(NewKeyCommand),
}

/// Adds a named keypair to the wallet in the selected node home.
#[derive(Debug, Args)]
struct NewKeyCommand {
    /// Name assigned to the new wallet key.
    name: String,
    /// Overrides the default node home directory.
    #[arg(long)]
    home: Option<PathBuf>,
}

/// Read-only inspection commands for local stores and peers.
#[derive(Debug, Subcommand)]
enum InspectCommand {
    /// Prints persisted chain summary from a local sqlite state file.
    Chain(InfoCommand),
    /// Prints the balance for a public key from a local sqlite state file.
    Balance(BalanceCommand),
    /// Prints the active UTXO set from a local sqlite state file.
    Utxos(UtxosCommand),
    /// Prints configured startup peers from the local peers file.
    Peers(PeersCommand),
    /// Connects to a peer and prints its advertised best tip.
    Tip(GetTipCommand),
}

#[derive(Debug, Args)]
struct InfoCommand {
    sqlite_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct BalanceCommand {
    public_key: String,
    sqlite_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct UtxosCommand {
    sqlite_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct PeersCommand {
    peers_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
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
    state_path: Option<PathBuf>,
    #[arg(long)]
    wallet_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
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
    state_path: Option<PathBuf>,
    #[arg(long)]
    wallet_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
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
    state_path: Option<PathBuf>,
    #[arg(long)]
    wallet_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
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
    sqlite_path: Option<PathBuf>,
    #[arg(long)]
    sender_wallet: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
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
    miner_wallet: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
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
    state_path: Option<PathBuf>,
    #[arg(long)]
    home: Option<PathBuf>,
    #[arg(long)]
    txid: String,
    #[arg(long)]
    vout: u32,
    #[arg(long)]
    amount: u64,
    #[arg(long)]
    sender_wallet: Option<PathBuf>,
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
        Command::Bootstrap(command) => run_bootstrap(command),
        Command::Serve(command) => run_serve(command),
        Command::Balance(command) => run_balance(command),
        Command::Wallet { command } => run_wallet(command),
        Command::Transactions(command) => run_transactions(command),
        Command::Status(command) => run_status(command),
        Command::Pay(command) => run_pay(command),
        Command::Inspect { command } => run_inspect(command),
        Command::Admin { command } => run_admin(command),
        Command::Debug { command } => run_debug(command),
    }
}

/// Dispatches wallet inspection and key-management commands.
fn run_wallet(command: WalletCommand) -> Result<(), String> {
    match command {
        WalletCommand::Info(command) => run_wallet_info(command),
        WalletCommand::Balance(command) => run_balance(command),
        WalletCommand::NewKey(command) => run_wallet_new_key(command),
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
    println!("admin addr: {}", config.admin_addr);
    println!("network: {}", config.network);
    if let Some(name) = config.node_name.as_deref() {
        println!("node name: {name}");
    }
    println!("mining enabled: {}", config.mining.enabled);
    println!(
        "mining reward wallet: {}",
        config
            .mining
            .reward_wallet
            .as_deref()
            .unwrap_or_else(|| Path::new("wallet.bin"))
            .display()
    );
    println!(
        "public: {}",
        encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
    );
    Ok(())
}

/// Mines initial coinbase-only blocks to fund the local node wallet.
fn run_bootstrap(command: BootstrapCommand) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let node_config = home
        .load_config()
        .map_err(|err| format!("config load failed: {err:?}"))?;
    let wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    let summary = admin::bootstrap_funds(
        &node_config.admin_addr,
        crypto::verifying_key_bytes(&wallet.verifying_key()).to_vec(),
        command.blocks,
    )
    .map_err(|err| format_admin_error("bootstrap", &node_config.admin_addr, err))?;

    println!("blocks mined: {}", summary.blocks_mined);
    println!("last block: {}", format_hash(summary.last_block_hash));
    println!("local admin: {}", node_config.admin_addr);
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
    let mining_runtime = resolve_mining_runtime(&home, &config_file.mining, &command)?;
    let mining = build_mining_config(&mining_runtime)?;
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
    println!("admin addr: {}", config_file.admin_addr);
    println!("network: {}", runtime.network);
    if let Some(name) = server.config().node_name.as_deref() {
        println!("node name: {name}");
    }
    println!(
        "best tip: {}",
        format_hash(server.state().chain().best_tip())
    );
    println!("peers file: {}", peer_path.display());
    if let Some(runtime) = &mining_runtime {
        println!("integrated mining: enabled");
        println!("miner wallet: {}", runtime.wallet_path.display());
        println!("mining reward: {}", BLOCK_SUBSIDY);
        println!("mining interval ms: {}", runtime.interval_ms);
        println!("mining max transactions: {}", runtime.max_transactions);
        println!("mining bits: {:#010x}", runtime.bits);
    }
    println!("configured peers: {}", peer_endpoints.len());
    for peer in &peer_endpoints {
        match peer.node_name.as_deref() {
            Some(name) => println!("peer: {} ({name})", peer.addr),
            None => println!("peer: {}", peer.addr),
        }
    }

    server
        .serve_with_admin(&runtime.listen_addr, &config_file.admin_addr)
        .map_err(|err| format!("server failed: {err}"))
}

/// Loads the local wallet and related node config used by wallet-facing commands.
fn load_wallet_context(command: &HomeArg) -> Result<(NodeHome, NodeConfig, Wallet), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let node_config = home
        .load_config()
        .map_err(|err| format!("config load failed: {err:?}"))?;
    let wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    Ok((home, node_config, wallet))
}

/// Collects all signing keys owned by one loaded wallet for payment assembly.
fn wallet_signing_keys(wallet: &Wallet) -> Vec<&ed25519_dalek::SigningKey> {
    wallet.keys().map(|key| key.signing_key()).collect()
}

/// Prints local wallet metadata without requiring a running server.
fn run_wallet_info(command: HomeArg) -> Result<(), String> {
    let (home, node_config, wallet) = load_wallet_context(&command)?;

    println!("node home: {}", home.root().display());
    println!("wallet: {}", home.wallet_path().display());
    println!("state: {}", home.state_path().display());
    println!("admin addr: {}", node_config.admin_addr);
    println!("default key: {}", wallet.default_key_name());
    println!("key count: {}", wallet.key_count());
    println!(
        "public key: {}",
        encode_hex(&crypto::verifying_key_bytes(&wallet.verifying_key()))
    );
    for key in wallet.keys() {
        println!(
            "key {}: {}",
            key.name(),
            encode_hex(&crypto::verifying_key_bytes(&key.verifying_key()))
        );
    }
    Ok(())
}

/// Prints total wallet balance plus one line per named wallet key.
fn run_balance(command: HomeArg) -> Result<(), String> {
    let (home, _, wallet) = load_wallet_context(&command)?;
    let store = SqliteStore::open_read_only(&home.state_path())
        .map_err(|err| format!("sqlite open failed: {err:?}"))?;
    let utxos = store
        .load_active_utxos()
        .map_err(|err| format!("sqlite load failed: {err:?}"))?;
    let mut total = 0_u64;

    println!("wallet: {}", home.wallet_path().display());
    println!("state: {}", home.state_path().display());
    println!("default key: {}", wallet.default_key_name());
    for key in wallet.keys() {
        let owner_bytes = crypto::verifying_key_bytes(&key.verifying_key());
        let balance: u64 = utxos
            .iter()
            .filter(|(_, utxo)| utxo.output.locking_data == owner_bytes)
            .map(|(_, utxo)| utxo.output.value)
            .sum();
        total = total.saturating_add(balance);
        println!("{}: {}", key.name(), balance);
    }
    println!("total: {total}");
    Ok(())
}

/// Generates a new named keypair in the local wallet and persists it.
fn run_wallet_new_key(command: NewKeyCommand) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let wallet_path = home.wallet_path();
    let mut wallet =
        wallet::load_wallet(&wallet_path).map_err(|err| format!("wallet load failed: {err:?}"))?;
    let public_key = wallet
        .generate_key(command.name.clone())
        .map_err(|err| format!("wallet new-key failed: {err:?}"))?;
    wallet::save_wallet(&wallet_path, &wallet)
        .map_err(|err| format!("wallet save failed: {err:?}"))?;

    println!("wallet: {}", wallet_path.display());
    println!("added key: {}", command.name);
    println!(
        "public: {}",
        encode_hex(&crypto::verifying_key_bytes(&public_key))
    );
    Ok(())
}

/// Prints wallet-relative transaction history from the local sqlite index.
fn run_transactions(command: HomeArg) -> Result<(), String> {
    let (home, _, wallet) = load_wallet_context(&command)?;
    let store = SqliteStore::open_read_only(&home.state_path())
        .map_err(|err| format!("sqlite open failed: {err:?}"))?;
    let lines = wallet_transaction_lines(&store, &wallet)
        .map_err(|err| format!("sqlite query failed: {err:?}"))?;

    println!("wallet: {}", home.wallet_path().display());
    println!("state: {}", home.state_path().display());
    println!("default key: {}", wallet.default_key_name());
    println!("transactions: {}", lines.len());
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Prints live status from the running local node server selected by `home`.
fn run_status(command: HomeArg) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let node_config = home
        .load_config()
        .map_err(|err| format!("config load failed: {err:?}"))?;
    let state = load_sqlite_state_read_only(&home.state_path())?;
    let persisted_tip = state.tip_summary();

    println!("listen addr: {}", node_config.listen_addr);
    println!("admin addr: {}", node_config.admin_addr);
    println!("state: {}", home.state_path().display());
    println!("best tip: {}", format_hash(persisted_tip.tip));
    println!(
        "height: {}",
        persisted_tip
            .height
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!("branches: {}", state.chain().branch_count());
    match admin::request_status(&node_config.admin_addr) {
        Ok(status) => {
            println!("server: running");
            println!("live best tip: {}", format_hash(status.tip));
            println!(
                "live height: {}",
                status
                    .height
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            );
            println!("live branches: {}", status.branch_count);
            println!("mempool txs: {}", status.mempool_size);
            println!("connected peers: {}", status.peer_count);
            println!("integrated mining: {}", status.mining_enabled);
        }
        Err(admin::AdminError::Connect(_)) => {
            println!("server: not running");
        }
        Err(err) => {
            println!(
                "server: unavailable ({})",
                format_admin_error("status query", &node_config.admin_addr, err)
            );
        }
    }
    Ok(())
}

/// Returns the signing key selected by `name`, or the wallet default key.
fn payment_signing_key<'a>(
    wallet: &'a Wallet,
    name: Option<&str>,
) -> Result<&'a ed25519_dalek::SigningKey, String> {
    match name {
        Some(name) => wallet
            .key(name)
            .map(|key| key.signing_key())
            .map_err(|err| format!("wallet key lookup failed: {err:?}")),
        None => Ok(wallet.signing_key()),
    }
}

/// Builds a signed payment from one wallet key and submits it to the local server.
fn run_pay(command: PayCommand) -> Result<(), String> {
    let home = resolve_node_home(command.home.as_deref())?;
    let node_config = home
        .load_config()
        .map_err(|err| format!("config load failed: {err:?}"))?;
    let recipient_verifying_key = parse_public_key_or_alias(&command.to)?;
    let sender_wallet = wallet::load_wallet(&home.wallet_path())
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    let sender_signing_key = payment_signing_key(&sender_wallet, command.from_key.as_deref())?;
    let store = SqliteStore::open_read_only(&home.state_path())
        .map_err(|err| format!("sqlite open failed: {err:?}"))?;
    let utxos = store
        .load_active_utxos()
        .map_err(|err| format!("sqlite load failed: {err:?}"))?;
    let transaction = build_payment_transaction(
        &utxos,
        &[sender_signing_key],
        &sender_wallet.verifying_key(),
        &recipient_verifying_key,
        command.amount,
        command.uniqueness.unwrap_or_else(default_uniqueness),
    )
    .map_err(|err| format!("build failed: {err:?}"))?;
    let submitted = transaction.txid();
    admin::submit_transaction(&node_config.admin_addr, transaction).map_err(|err| {
        format_admin_error("submit to local server", &node_config.admin_addr, err)
    })?;

    println!("submitted payment {}", submitted);
    println!("local admin: {}", node_config.admin_addr);
    Ok(())
}

fn run_inspect(command: InspectCommand) -> Result<(), String> {
    match command {
        InspectCommand::Chain(command) => {
            let sqlite_path =
                resolve_state_path(command.home.as_deref(), command.sqlite_path.as_deref())?;
            let store = SqliteStore::open_read_only(&sqlite_path)
                .map_err(|err| format!("sqlite open failed: {err:?}"))?;
            let indexed_blocks = store
                .count_chain_entries()
                .map_err(|err| format!("sqlite load failed: {err:?}"))?;
            let best_tip = store
                .load_best_tip()
                .map_err(|err| format!("sqlite load failed: {err:?}"))?;
            let active_utxos = store
                .load_active_utxos()
                .map_err(|err| format!("sqlite load failed: {err:?}"))?;
            let mempool = store
                .load_mempool()
                .map_err(|err| format!("sqlite load failed: {err:?}"))?;
            println!("sqlite: {}", sqlite_path.display());
            println!("indexed blocks: {indexed_blocks}");
            println!("best tip: {}", format_hash(best_tip));
            println!("active utxos: {}", active_utxos.len());
            println!("mempool txs: {}", mempool.len());
            Ok(())
        }
        InspectCommand::Balance(command) => {
            let owner = parse_public_key(&command.public_key)?;
            let sqlite_path =
                resolve_state_path(command.home.as_deref(), command.sqlite_path.as_deref())?;
            let store = SqliteStore::open_read_only(&sqlite_path)
                .map_err(|err| format!("sqlite open failed: {err:?}"))?;
            let owner_bytes = crypto::verifying_key_bytes(&owner);
            let balance: u64 = store
                .load_active_utxos()
                .map_err(|err| format!("sqlite load failed: {err:?}"))?
                .iter()
                .filter(|(_, utxo)| utxo.output.locking_data == owner_bytes)
                .map(|(_, utxo)| utxo.output.value)
                .sum();
            println!("{balance}");
            Ok(())
        }
        InspectCommand::Utxos(command) => {
            let sqlite_path =
                resolve_state_path(command.home.as_deref(), command.sqlite_path.as_deref())?;
            let store = SqliteStore::open_read_only(&sqlite_path)
                .map_err(|err| format!("sqlite open failed: {err:?}"))?;
            let utxos = store
                .load_active_utxos()
                .map_err(|err| format!("sqlite load failed: {err:?}"))?;
            let mut outpoints: Vec<OutPoint> =
                utxos.iter().map(|(outpoint, _)| *outpoint).collect();
            outpoints.sort_by_key(|outpoint| (outpoint.txid.to_string(), outpoint.vout));
            for outpoint in outpoints {
                let utxo = utxos
                    .get(&outpoint)
                    .expect("sorted outpoint should still exist in loaded UTXO set");
                println!(
                    "{}:{} value={} coinbase={} owner={}",
                    outpoint.txid,
                    outpoint.vout,
                    utxo.output.value,
                    utxo.is_coinbase,
                    encode_hex(&utxo.output.locking_data)
                );
            }
            Ok(())
        }
        InspectCommand::Peers(command) => {
            let peers_path = resolve_peers_path(command.home.as_deref(), command.peers_path.as_deref())?;
            let peers = peers::load_peer_endpoints(&peers_path)
                .map_err(|err| format!("peer config load failed: {err:?}"))?;
            println!("peers file: {}", peers_path.display());
            println!("configured peers: {}", peers.len());
            for peer in peers {
                match peer.node_name {
                    Some(node_name) => println!("{} ({node_name})", peer.addr),
                    None => println!("{}", peer.addr),
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
            let (state_path, wallet_path) = resolve_state_and_wallet_paths(
                command.home.as_deref(),
                command.state_path.as_deref(),
                command.wallet_path.as_deref(),
            )?;
            let miner_wallet = wallet::load_wallet(&wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut state = load_sqlite_state(&state_path)?;
            let block_hash = state
                .mine_block(
                    command.reward,
                    &miner_wallet.verifying_key(),
                    command.uniqueness,
                    parse_bits(&command.bits)?,
                    0,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(&state_path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            if let Some(outpoint) = state.active_outpoints().last() {
                println!("latest outpoint: {}:{}", outpoint.txid, outpoint.vout);
            }
            Ok(())
        }
        DebugCommand::MinePending(command) => {
            let (state_path, wallet_path) = resolve_state_and_wallet_paths(
                command.home.as_deref(),
                command.state_path.as_deref(),
                command.wallet_path.as_deref(),
            )?;
            let miner_wallet = wallet::load_wallet(&wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut state = load_sqlite_state(&state_path)?;
            let block_hash = state
                .mine_block(
                    command.reward,
                    &miner_wallet.verifying_key(),
                    command.uniqueness,
                    parse_bits(&command.bits)?,
                    command.max_transactions,
                )
                .map_err(|err| format!("block rejected: {err:?}"))?;
            save_sqlite_state(&state_path, &state)?;

            println!("mined block {}", block_hash);
            println!("new best tip: {}", format_hash(state.chain().best_tip()));
            println!("active utxos: {}", state.active_utxos().len());
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        DebugCommand::SubmitPayment(command) => {
            let (state_path, wallet_path) = resolve_state_and_wallet_paths(
                command.home.as_deref(),
                command.state_path.as_deref(),
                command.wallet_path.as_deref(),
            )?;
            let recipient_verifying_key = parse_public_key_or_alias(&command.recipient)?;
            let sender_wallet = wallet::load_wallet(&wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut state = load_sqlite_state(&state_path)?;
            let submitted = state
                .submit_payment(
                    &wallet_signing_keys(&sender_wallet),
                    &sender_wallet.verifying_key(),
                    &recipient_verifying_key,
                    command.amount,
                    command.uniqueness.unwrap_or_else(default_uniqueness),
                )
                .map_err(|err| format!("submit failed: {err:?}"))?;
            save_sqlite_state(&state_path, &state)?;

            println!("queued payment {}", submitted);
            println!("mempool txs: {}", state.mempool().len());
            Ok(())
        }
        DebugCommand::SubmitPaymentRemote(command) => {
            let (sqlite_path, sender_wallet_path) = resolve_state_and_wallet_paths(
                command.home.as_deref(),
                command.sqlite_path.as_deref(),
                command.sender_wallet.as_deref(),
            )?;
            let recipient_verifying_key = parse_public_key_or_alias(&command.recipient)?;
            let config = PeerConfig::new(command.network, command.node_name);
            let sender_wallet = wallet::load_wallet(&sender_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let mut local_state = load_sqlite_state(&sqlite_path)?;
            let txid = local_state
                .submit_payment(
                    &wallet_signing_keys(&sender_wallet),
                    &sender_wallet.verifying_key(),
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
            let miner_wallet_path =
                resolve_wallet_path(command.home.as_deref(), command.miner_wallet.as_deref())?;
            let config = PeerConfig::new(command.network, command.node_name);
            let miner_wallet = wallet::load_wallet(&miner_wallet_path)
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
            let (state_path, sender_wallet_path) = resolve_state_and_wallet_paths(
                command.home.as_deref(),
                command.state_path.as_deref(),
                command.sender_wallet.as_deref(),
            )?;
            let txid = parse_txid(&command.txid)?;
            let sender_wallet = wallet::load_wallet(&sender_wallet_path)
                .map_err(|err| format!("wallet load failed: {err:?}"))?;
            let recipient_public_key = parse_public_key(&command.recipient_public_key)?;

            let mut state = load_sqlite_state(&state_path)?;
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
            save_sqlite_state(&state_path, &state)?;

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

/// Loads persisted node state through a read-only SQLite handle for inspection.
fn load_sqlite_state_read_only(path: &Path) -> Result<NodeState, String> {
    SqliteStore::open_read_only(path)
        .and_then(|store| store.load_node_state())
        .map_err(|err| format!("sqlite load failed: {err:?}"))
}

fn resolve_node_home(path: Option<&Path>) -> Result<NodeHome, String> {
    match path {
        Some(path) => Ok(NodeHome::new(path)),
        None => NodeHome::from_default_dir().map_err(|err| format!("home resolve failed: {err:?}")),
    }
}

/// Resolves the local sqlite state path, defaulting to the selected node home.
fn resolve_state_path(home: Option<&Path>, state_path: Option<&Path>) -> Result<PathBuf, String> {
    match state_path {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(resolve_node_home(home)?.state_path()),
    }
}

/// Resolves the local wallet path, defaulting to the selected node home.
fn resolve_wallet_path(home: Option<&Path>, wallet_path: Option<&Path>) -> Result<PathBuf, String> {
    match wallet_path {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(resolve_node_home(home)?.wallet_path()),
    }
}

/// Resolves the local peers file path, defaulting to the selected node home.
fn resolve_peers_path(home: Option<&Path>, peers_path: Option<&Path>) -> Result<PathBuf, String> {
    match peers_path {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(resolve_node_home(home)?.peers_path()),
    }
}

/// Resolves both local state and wallet paths, preferring explicit overrides.
fn resolve_state_and_wallet_paths(
    home: Option<&Path>,
    state_path: Option<&Path>,
    wallet_path: Option<&Path>,
) -> Result<(PathBuf, PathBuf), String> {
    Ok((
        resolve_state_path(home, state_path)?,
        resolve_wallet_path(home, wallet_path)?,
    ))
}

/// Effective serve settings after merging home config defaults with CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeRuntime {
    listen_addr: String,
    network: String,
    node_name: Option<String>,
}

/// Effective mining settings after merging home config defaults with CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MiningRuntime {
    wallet_path: PathBuf,
    interval_ms: u64,
    max_transactions: usize,
    bits: u32,
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

/// Resolves integrated mining settings from node-home config plus CLI overrides.
fn resolve_mining_runtime(
    home: &NodeHome,
    config: &MiningSection,
    command: &ServeCommand,
) -> Result<Option<MiningRuntime>, String> {
    let enabled = if command.no_mining {
        false
    } else if command.mining || command.miner_wallet.is_some() {
        true
    } else {
        config.enabled
    };
    if !enabled {
        return Ok(None);
    }

    let wallet_path = command
        .miner_wallet
        .clone()
        .or_else(|| {
            config.reward_wallet.clone().map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    home.root().join(path)
                }
            })
        })
        .unwrap_or_else(|| home.wallet_path());
    let interval_ms = command.mining_interval_ms.unwrap_or(config.interval_ms);
    let max_transactions = command
        .mining_max_transactions
        .unwrap_or(config.max_transactions);
    let bits = command
        .mining_bits
        .as_deref()
        .map(parse_bits)
        .transpose()?
        .unwrap_or(parse_bits(&config.bits)?);

    Ok(Some(MiningRuntime {
        wallet_path,
        interval_ms,
        max_transactions,
        bits,
    }))
}

/// Loads the configured miner wallet and builds the server mining configuration.
fn build_mining_config(runtime: &Option<MiningRuntime>) -> Result<Option<MiningConfig>, String> {
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    let wallet = wallet::load_wallet(&runtime.wallet_path)
        .map_err(|err| format!("wallet load failed: {err:?}"))?;
    let mining = MiningConfig::new(wallet.verifying_key())
        .with_interval(std::time::Duration::from_millis(runtime.interval_ms))
        .with_max_transactions(runtime.max_transactions)
        .with_bits(runtime.bits);
    Ok(Some(mining))
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

/// Builds wallet-history display lines from the persisted transaction index.
///
/// Each line summarizes one transaction in terms of wallet-owned keys: how much
/// value those keys spent, how much they received, and the wallet-relative net
/// effect. Pending mempool transactions sort after confirmed history.
fn wallet_transaction_lines(store: &SqliteStore, wallet: &Wallet) -> Result<Vec<String>, String> {
    let wallet_keys: Vec<Vec<u8>> = wallet
        .verifying_keys()
        .map(|key| crypto::verifying_key_bytes(&key).to_vec())
        .collect();
    let transactions = store
        .load_transactions_for_keys(&wallet_keys)
        .map_err(|err| format!("{err:?}"))?;
    let mut lines = Vec::with_capacity(transactions.len());
    for transaction in transactions {
        let edges = store
            .load_transaction_key_edges(transaction.txid)
            .map_err(|err| format!("{err:?}"))?;
        lines.push(format_wallet_transaction_line(
            &transaction,
            &wallet_keys,
            &edges,
        ));
    }
    Ok(lines)
}

/// Formats one wallet-facing transaction summary line.
fn format_wallet_transaction_line(
    transaction: &IndexedTransaction,
    wallet_keys: &[Vec<u8>],
    edges: &[wobble::tx_index::TransactionKeyEdge],
) -> String {
    let mut sent = 0_u64;
    let mut received = 0_u64;
    for edge in edges {
        if !wallet_keys.iter().any(|key| *key == edge.key_data) {
            continue;
        }
        match edge.key_role {
            TransactionKeyRole::Input => {
                sent = sent.saturating_add(edge.value.unwrap_or(0));
            }
            TransactionKeyRole::Output => {
                received = received.saturating_add(edge.value.unwrap_or(0));
            }
        }
    }
    let net = received as i128 - sent as i128;
    let position = match transaction.status {
        IndexedTransactionStatus::Confirmed => format!(
            "confirmed height={} pos={}",
            transaction.block_height.unwrap_or_default(),
            transaction.position_in_block.unwrap_or_default()
        ),
        IndexedTransactionStatus::Mempool => "mempool".to_string(),
    };

    format!(
        "{} txid={} sent={} received={} net={:+}",
        position, transaction.txid, sent, received, net
    )
}

/// Formats local admin client failures into operator-facing CLI errors.
///
/// Connection failures usually mean the local node server is not running yet,
/// so those cases get an explicit `wobble serve` hint. Other protocol or
/// server-side failures keep their original detail for debugging.
fn format_admin_error(action: &str, admin_addr: &str, err: admin::AdminError) -> String {
    match err {
        admin::AdminError::Connect(source)
            if matches!(
                source.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            format!(
                "{action} failed: local admin server is not running at {admin_addr}; start `wobble serve` first ({source})"
            )
        }
        admin::AdminError::Connect(source) => {
            format!(
                "{action} failed: could not reach local admin server at {admin_addr} ({source})"
            )
        }
        admin::AdminError::Send(source) => {
            format!("{action} failed: could not send admin request to {admin_addr} ({source})")
        }
        admin::AdminError::Receive(source) => {
            format!(
                "{action} failed: did not receive a valid admin response from {admin_addr} ({source})"
            )
        }
        admin::AdminError::Server(message) => {
            format!("{action} failed: server rejected request: {message}")
        }
        admin::AdminError::UnexpectedResponse(response) => {
            format!("{action} failed: unexpected admin response: {response:?}")
        }
    }
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
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use clap::Parser;
    use wobble::{
        admin::{AdminError, AdminResponse},
        home::{MiningSection, NodeConfig, NodeHome},
        sqlite_store::SqliteStore,
        types::{Block, BlockHash, BlockHeader, OutPoint, Transaction, TxIn, TxOut, Txid},
        wallet,
    };

    use super::{
        BootstrapCommand, Cli, Command, DebugCommand, HomeArg, InspectCommand, MinePendingCommand,
        NewKeyCommand, PayCommand, ServeCommand, WalletCommand, format_admin_error,
        resolve_mining_runtime, resolve_serve_runtime, resolve_state_and_wallet_paths,
        resolve_peers_path, resolve_state_path, resolve_wallet_path, run_wallet_new_key,
        wallet_transaction_lines,
    };

    static NEXT_TEMP_HOME_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_home_root() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        let unique = NEXT_TEMP_HOME_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!(
            "wobble-main-test-{}-{}-{}",
            std::process::id(),
            nanos,
            unique
        ));
        path
    }

    fn mine_block_with_transactions(
        prev_blockhash: BlockHash,
        bits: u32,
        miner: &ed25519_dalek::VerifyingKey,
        uniqueness: u32,
        mut transactions: Vec<Transaction>,
    ) -> Block {
        let mut full_transactions = Vec::with_capacity(transactions.len() + 1);
        full_transactions.push(Transaction {
            version: 1,
            inputs: Vec::new(),
            outputs: vec![TxOut {
                value: 50,
                locking_data: wobble::crypto::verifying_key_bytes(miner).to_vec(),
            }],
            lock_time: uniqueness,
        });
        full_transactions.append(&mut transactions);

        let mut block = Block {
            header: BlockHeader {
                version: 1,
                prev_blockhash,
                merkle_root: [0; 32],
                time: 1,
                bits,
                nonce: 0,
            },
            transactions: full_transactions,
        };
        block.header.merkle_root = block.merkle_root();

        loop {
            if wobble::consensus::validate_block(&block).is_ok() {
                return block;
            }
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
    }

    #[test]
    fn parses_pay_with_optional_from_and_uniqueness() {
        let cli = Cli::try_parse_from([
            "wobble",
            "pay",
            "@/tmp/book.json:alice",
            "25",
            "--from",
            "savings",
            "--uniqueness",
            "7",
            "--home",
            "/tmp/node",
        ])
        .unwrap();

        match cli.command {
            Command::Pay(PayCommand {
                from_key,
                to,
                amount,
                uniqueness,
                home,
            }) => {
                assert_eq!(from_key.as_deref(), Some("savings"));
                assert_eq!(to, "@/tmp/book.json:alice");
                assert_eq!(amount, 25);
                assert_eq!(uniqueness, Some(7));
                assert_eq!(home.unwrap(), PathBuf::from("/tmp/node"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_top_level_status_with_home_override() {
        let cli = Cli::try_parse_from(["wobble", "status", "--home", "/tmp/node"]).unwrap();

        match cli.command {
            Command::Status(HomeArg { home }) => {
                assert_eq!(home.unwrap(), PathBuf::from("/tmp/node"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_wallet_info_with_home_override() {
        let cli = Cli::try_parse_from(["wobble", "wallet", "info", "--home", "/tmp/node"]).unwrap();

        match cli.command {
            Command::Wallet {
                command: WalletCommand::Info(HomeArg { home }),
            } => {
                assert_eq!(home, Some(PathBuf::from("/tmp/node")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_balance_with_home_override() {
        let cli = Cli::try_parse_from(["wobble", "balance", "--home", "/tmp/node"]).unwrap();

        match cli.command {
            Command::Balance(HomeArg { home }) => {
                assert_eq!(home, Some(PathBuf::from("/tmp/node")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_wallet_balance_with_home_override() {
        let cli =
            Cli::try_parse_from(["wobble", "wallet", "balance", "--home", "/tmp/node"]).unwrap();

        match cli.command {
            Command::Wallet {
                command: WalletCommand::Balance(HomeArg { home }),
            } => {
                assert_eq!(home, Some(PathBuf::from("/tmp/node")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_wallet_new_key_with_home_override() {
        let cli = Cli::try_parse_from([
            "wobble",
            "wallet",
            "new-key",
            "miner",
            "--home",
            "/tmp/node",
        ])
        .unwrap();

        match cli.command {
            Command::Wallet {
                command: WalletCommand::NewKey(NewKeyCommand { name, home }),
            } => {
                assert_eq!(name, "miner");
                assert_eq!(home, Some(PathBuf::from("/tmp/node")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_transactions_with_home_override() {
        let cli = Cli::try_parse_from(["wobble", "transactions", "--home", "/tmp/node"]).unwrap();

        match cli.command {
            Command::Transactions(HomeArg { home }) => {
                assert_eq!(home.unwrap(), PathBuf::from("/tmp/node"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_bootstrap_with_block_count() {
        let cli = Cli::try_parse_from([
            "wobble",
            "bootstrap",
            "--blocks",
            "3",
            "--home",
            "/tmp/node",
        ])
        .unwrap();

        match cli.command {
            Command::Bootstrap(BootstrapCommand { blocks, home }) => {
                assert_eq!(blocks, 3);
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
                mining,
                no_mining,
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
                assert!(!mining);
                assert!(!no_mining);
                assert_eq!(miner_wallet.unwrap(), PathBuf::from("/tmp/miner.wallet"));
                assert_eq!(mining_interval_ms, Some(500));
                assert_eq!(mining_max_transactions, Some(25));
                assert_eq!(mining_bits.as_deref(), Some("0x207fffff"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn formats_connection_refused_admin_errors_with_serve_hint() {
        let err = AdminError::Connect(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Connection refused",
        ));

        let message = format_admin_error("status query", "127.0.0.1:9000", err);

        assert!(message.contains("local admin server is not running at 127.0.0.1:9000"));
        assert!(message.contains("start `wobble serve` first"));
    }

    #[test]
    fn formats_server_side_admin_errors_without_debug_wrappers() {
        let message = format_admin_error(
            "submit to local server",
            "127.0.0.1:9000",
            AdminError::Server("insufficient fee".to_string()),
        );

        assert_eq!(
            message,
            "submit to local server failed: server rejected request: insufficient fee"
        );
    }

    #[test]
    fn formats_unexpected_admin_responses_with_context() {
        let message = format_admin_error(
            "bootstrap",
            "127.0.0.1:9000",
            AdminError::UnexpectedResponse(AdminResponse::Submitted {
                txid: Txid::new([0x11; 32]),
            }),
        );

        assert!(message.starts_with("bootstrap failed: unexpected admin response:"));
        assert!(message.contains("Submitted"));
    }

    #[test]
    fn parses_debug_mine_pending_named_args() {
        let cli = Cli::try_parse_from([
            "wobble",
            "debug",
            "mine-pending",
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
                        home,
                        reward,
                        uniqueness,
                        max_transactions,
                        bits,
                    }),
            } => {
                assert!(state_path.is_none());
                assert!(wallet_path.is_none());
                assert!(home.is_none());
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
                mining,
                no_mining,
                miner_wallet,
                ..
            }) => {
                assert!(home.is_none());
                assert!(listen_addr.is_none());
                assert!(network.is_none());
                assert!(node_name.is_none());
                assert!(peers_path.is_none());
                assert!(!mining);
                assert!(!no_mining);
                assert!(miner_wallet.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_inspect_chain_without_sqlite_path() {
        let cli = Cli::try_parse_from(["wobble", "inspect", "chain"]).unwrap();

        match cli.command {
            Command::Inspect {
                command: InspectCommand::Chain(command),
            } => {
                assert!(command.sqlite_path.is_none());
                assert!(command.home.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_inspect_balance_with_public_key_and_optional_sqlite_path() {
        let cli = Cli::try_parse_from([
            "wobble",
            "inspect",
            "balance",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "/tmp/node.sqlite",
            "--home",
            "/tmp/node",
        ])
        .unwrap();

        match cli.command {
            Command::Inspect {
                command: InspectCommand::Balance(command),
            } => {
                assert_eq!(
                    command.public_key,
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                );
                assert_eq!(command.sqlite_path, Some(PathBuf::from("/tmp/node.sqlite")));
                assert_eq!(command.home, Some(PathBuf::from("/tmp/node")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_inspect_peers_with_optional_path_and_home() {
        let cli = Cli::try_parse_from([
            "wobble",
            "inspect",
            "peers",
            "/tmp/peers.json",
            "--home",
            "/tmp/node",
        ])
        .unwrap();

        match cli.command {
            Command::Inspect {
                command: InspectCommand::Peers(command),
            } => {
                assert_eq!(command.peers_path, Some(PathBuf::from("/tmp/peers.json")));
                assert_eq!(command.home, Some(PathBuf::from("/tmp/node")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn resolve_serve_runtime_prefers_cli_over_home_config() {
        let config = NodeConfig {
            listen_addr: "127.0.0.1:9001".to_string(),
            admin_addr: "127.0.0.1:9000".to_string(),
            network: "wobble-local".to_string(),
            node_name: Some("saved".to_string()),
            mining: MiningSection::default(),
        };
        let command = ServeCommand {
            home: Some(PathBuf::from("/tmp/node")),
            listen_addr: Some("127.0.0.1:9010".to_string()),
            network: None,
            node_name: Some("override".to_string()),
            peers_path: None,
            mining: false,
            no_mining: false,
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

    #[test]
    fn resolve_mining_runtime_uses_home_wallet_when_config_enables_mining() {
        let home = NodeHome::new("/tmp/node");
        let config = MiningSection {
            enabled: true,
            reward_wallet: None,
            interval_ms: 400,
            max_transactions: 12,
            bits: "0x1f00ffff".to_string(),
        };
        let command = ServeCommand {
            home: None,
            listen_addr: None,
            network: None,
            node_name: None,
            peers_path: None,
            mining: false,
            no_mining: false,
            miner_wallet: None,
            mining_interval_ms: None,
            mining_max_transactions: None,
            mining_bits: None,
        };

        let runtime = resolve_mining_runtime(&home, &config, &command)
            .unwrap()
            .expect("mining should be enabled");

        assert_eq!(runtime.wallet_path, home.wallet_path());
        assert_eq!(runtime.interval_ms, 400);
        assert_eq!(runtime.max_transactions, 12);
        assert_eq!(runtime.bits, 0x1f00ffff);
    }

    #[test]
    fn resolve_mining_runtime_applies_cli_overrides() {
        let home = NodeHome::new("/tmp/node");
        let config = MiningSection {
            enabled: false,
            reward_wallet: Some(PathBuf::from("miner-wallet.bin")),
            interval_ms: 250,
            max_transactions: 100,
            bits: "0x207fffff".to_string(),
        };
        let command = ServeCommand {
            home: None,
            listen_addr: None,
            network: None,
            node_name: None,
            peers_path: None,
            mining: true,
            no_mining: false,
            miner_wallet: Some(PathBuf::from("/tmp/override.wallet")),
            mining_interval_ms: Some(500),
            mining_max_transactions: Some(21),
            mining_bits: Some("0x1f00ffff".to_string()),
        };

        let runtime = resolve_mining_runtime(&home, &config, &command)
            .unwrap()
            .expect("mining should be enabled");

        assert_eq!(runtime.wallet_path, PathBuf::from("/tmp/override.wallet"));
        assert_eq!(runtime.interval_ms, 500);
        assert_eq!(runtime.max_transactions, 21);
        assert_eq!(runtime.bits, 0x1f00ffff);
    }

    #[test]
    fn resolve_mining_runtime_respects_no_mining_override() {
        let home = NodeHome::new("/tmp/node");
        let config = MiningSection {
            enabled: true,
            reward_wallet: Some(PathBuf::from("miner-wallet.bin")),
            interval_ms: 250,
            max_transactions: 100,
            bits: "0x207fffff".to_string(),
        };
        let command = ServeCommand {
            home: None,
            listen_addr: None,
            network: None,
            node_name: None,
            peers_path: None,
            mining: false,
            no_mining: true,
            miner_wallet: None,
            mining_interval_ms: None,
            mining_max_transactions: None,
            mining_bits: None,
        };

        let runtime = resolve_mining_runtime(&home, &config, &command).unwrap();

        assert!(runtime.is_none());
    }

    #[test]
    fn resolve_state_path_defaults_to_home_state() {
        let path = resolve_state_path(Some(Path::new("/tmp/node")), None).unwrap();

        assert_eq!(path, PathBuf::from("/tmp/node/node.sqlite"));
    }

    #[test]
    fn resolve_wallet_path_defaults_to_home_wallet() {
        let path = resolve_wallet_path(Some(Path::new("/tmp/node")), None).unwrap();

        assert_eq!(path, PathBuf::from("/tmp/node/wallet.bin"));
    }

    #[test]
    fn resolve_peers_path_defaults_to_home_peers() {
        let path = resolve_peers_path(Some(Path::new("/tmp/node")), None).unwrap();

        assert_eq!(path, PathBuf::from("/tmp/node/peers.json"));
    }

    #[test]
    fn resolve_state_and_wallet_paths_prefers_explicit_overrides() {
        let (state_path, wallet_path) = resolve_state_and_wallet_paths(
            Some(Path::new("/tmp/node")),
            Some(Path::new("/tmp/custom.sqlite")),
            Some(Path::new("/tmp/custom.wallet")),
        )
        .unwrap();

        assert_eq!(state_path, PathBuf::from("/tmp/custom.sqlite"));
        assert_eq!(wallet_path, PathBuf::from("/tmp/custom.wallet"));
    }

    #[test]
    fn wallet_transaction_lines_report_wallet_relative_history() {
        let root = temp_home_root();
        let home = NodeHome::new(&root);
        home.initialize().unwrap();

        let mut wallet = wallet::load_wallet(&home.wallet_path()).unwrap();
        let sender_public = wallet.generate_key("sender").unwrap();
        wallet::save_wallet(&home.wallet_path(), &wallet).unwrap();
        let sender = wallet
            .keys()
            .find(|key| key.name() == "sender")
            .unwrap()
            .signing_key()
            .clone();
        let recipient = wobble::crypto::signing_key_from_bytes([0x44; 32]);
        let miner = wobble::crypto::signing_key_from_bytes([0x55; 32]);

        let store = SqliteStore::open(&home.state_path()).unwrap();
        let mut state = wobble::node_state::NodeState::new();
        let genesis = mine_block_with_transactions(
            BlockHash::default(),
            0x207f_ffff,
            &sender_public,
            1,
            Vec::new(),
        );
        let genesis_txid = genesis.transactions[0].txid();
        state.accept_block(genesis).unwrap();

        let mut confirmed = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: genesis_txid,
                    vout: 0,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![
                TxOut {
                    value: 30,
                    locking_data: wobble::crypto::verifying_key_bytes(&recipient.verifying_key())
                        .to_vec(),
                },
                TxOut {
                    value: 20,
                    locking_data: wobble::crypto::verifying_key_bytes(&wallet.verifying_key())
                        .to_vec(),
                },
            ],
            lock_time: 2,
        };
        confirmed.inputs[0].unlocking_data =
            wobble::crypto::sign_message(&sender, &confirmed.signing_digest()).to_vec();
        let confirmed_block = mine_block_with_transactions(
            state.chain().best_tip().unwrap(),
            0x207f_ffff,
            &miner.verifying_key(),
            2,
            vec![confirmed.clone()],
        );
        state.accept_block(confirmed_block).unwrap();

        let mut pending = Transaction {
            version: 1,
            inputs: vec![TxIn {
                previous_output: OutPoint {
                    txid: confirmed.txid(),
                    vout: 1,
                },
                unlocking_data: Vec::new(),
            }],
            outputs: vec![TxOut {
                value: 20,
                locking_data: wobble::crypto::verifying_key_bytes(&recipient.verifying_key())
                    .to_vec(),
            }],
            lock_time: 3,
        };
        pending.inputs[0].unlocking_data =
            wobble::crypto::sign_message(wallet.signing_key(), &pending.signing_digest()).to_vec();
        state.submit_transaction(pending.clone()).unwrap();
        store.save_node_state(&state).unwrap();

        let read_only = SqliteStore::open_read_only(&home.state_path()).unwrap();
        let lines = wallet_transaction_lines(&read_only, &wallet).unwrap();
        drop(read_only);
        drop(store);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            format!(
                "confirmed height=0 pos=0 txid={} sent=0 received=50 net=+50",
                genesis_txid
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "confirmed height=1 pos=1 txid={} sent=50 received=20 net=-30",
                confirmed.txid()
            )
        );
        assert_eq!(
            lines[2],
            format!("mempool txid={} sent=20 received=0 net=-20", pending.txid())
        );
    }

    #[test]
    fn wallet_new_key_adds_named_key_to_home_wallet() {
        let root = temp_home_root();
        let home = NodeHome::new(&root);
        home.initialize().unwrap();

        run_wallet_new_key(NewKeyCommand {
            name: "miner".to_string(),
            home: Some(root.clone()),
        })
        .unwrap();

        let wallet = wallet::load_wallet(&home.wallet_path()).unwrap();
        let key_names: Vec<&str> = wallet.keys().map(|key| key.name()).collect();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(wallet.key_count(), 2);
        assert!(key_names.contains(&"default"));
        assert!(key_names.contains(&"miner"));
    }

    #[test]
    fn payment_signing_key_uses_named_key_or_default() {
        let mut wallet = wallet::Wallet::generate();
        wallet.generate_key("savings").unwrap();

        let default_public = wallet.verifying_key();
        let named_public = wallet.key("savings").unwrap().verifying_key();

        let selected_default = super::payment_signing_key(&wallet, None).unwrap();
        let selected_named = super::payment_signing_key(&wallet, Some("savings")).unwrap();

        assert_eq!(selected_default.verifying_key(), default_public);
        assert_eq!(selected_named.verifying_key(), named_public);
    }
}
