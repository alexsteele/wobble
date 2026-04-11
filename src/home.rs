//! Canonical on-disk layout for one local wobble node home.
//!
//! A node home groups the files a single node typically needs under one
//! directory so CLI commands do not have to pass each path separately. The
//! current layout includes the chain state SQLite file, one default wallet, an
//! alias book, a bootstrap peer file, and a small human-readable node config.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    aliases::{self, AliasBook, AliasError},
    node_state::NodeState,
    peers::{self, PeerConfigError},
    server::PeerEndpoint,
    sqlite_store::{SqliteStore, SqliteStoreError},
    wallet::{self, Wallet, WalletError},
};

/// Conventional local home directory for a wobble node when no override is given.
pub const DEFAULT_HOME_DIRNAME: &str = ".wobble";
const STATE_FILENAME: &str = "node.sqlite";
const WALLET_FILENAME: &str = "wallet.bin";
const ALIASES_FILENAME: &str = "aliases.json";
const PEERS_FILENAME: &str = "peers.json";
const CONFIG_FILENAME: &str = "config.json";

/// Default runtime configuration stored in a node home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConfig {
    pub listen_addr: String,
    pub network: String,
    pub node_name: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:9001".to_string(),
            network: "wobble-local".to_string(),
            node_name: None,
        }
    }
}

/// Errors returned while resolving or initializing a node home.
#[derive(Debug)]
pub enum NodeHomeError {
    MissingBaseHomeDir,
    Io(io::Error),
    ConfigParse(serde_json::Error),
    ConfigSerialize(serde_json::Error),
    Sqlite(SqliteStoreError),
    Wallet(WalletError),
    Alias(AliasError),
    Peers(PeerConfigError),
}

impl From<io::Error> for NodeHomeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SqliteStoreError> for NodeHomeError {
    fn from(error: SqliteStoreError) -> Self {
        Self::Sqlite(error)
    }
}

impl From<WalletError> for NodeHomeError {
    fn from(error: WalletError) -> Self {
        Self::Wallet(error)
    }
}

impl From<AliasError> for NodeHomeError {
    fn from(error: AliasError) -> Self {
        Self::Alias(error)
    }
}

impl From<PeerConfigError> for NodeHomeError {
    fn from(error: PeerConfigError) -> Self {
        Self::Peers(error)
    }
}

/// Resolved filesystem layout for one local node home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHome {
    root: PathBuf,
}

impl NodeHome {
    /// Builds a node home rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolves the default node home under the current user's home directory.
    pub fn from_default_dir() -> Result<Self, NodeHomeError> {
        Ok(Self::new(default_home_root()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILENAME)
    }

    pub fn wallet_path(&self) -> PathBuf {
        self.root.join(WALLET_FILENAME)
    }

    pub fn aliases_path(&self) -> PathBuf {
        self.root.join(ALIASES_FILENAME)
    }

    pub fn peers_path(&self) -> PathBuf {
        self.root.join(PEERS_FILENAME)
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILENAME)
    }

    /// Loads the node runtime config from disk.
    pub fn load_config(&self) -> Result<NodeConfig, NodeHomeError> {
        let contents = fs::read_to_string(self.config_path())?;
        serde_json::from_str(&contents).map_err(NodeHomeError::ConfigParse)
    }

    /// Saves a pretty-printed runtime config for this node home.
    pub fn save_config(&self, config: &NodeConfig) -> Result<(), NodeHomeError> {
        let contents =
            serde_json::to_string_pretty(config).map_err(NodeHomeError::ConfigSerialize)?;
        fs::write(self.config_path(), contents)?;
        Ok(())
    }

    /// Creates the home directory and any missing default files.
    ///
    /// Initialization is idempotent: existing files are preserved so rerunning
    /// `init` does not destroy wallet material or local chain state.
    pub fn initialize(&self) -> Result<(), NodeHomeError> {
        fs::create_dir_all(&self.root)?;

        let state_path = self.state_path();
        if !state_path.exists() {
            SqliteStore::open(&state_path)?.save_node_state(&NodeState::new())?;
        }

        let wallet_path = self.wallet_path();
        if !wallet_path.exists() {
            wallet::save_wallet(&wallet_path, &Wallet::generate())?;
        }

        let aliases_path = self.aliases_path();
        if !aliases_path.exists() {
            aliases::save_alias_book(&aliases_path, &AliasBook::new())?;
        }

        let peers_path = self.peers_path();
        if !peers_path.exists() {
            peers::save_peer_endpoints(&peers_path, &Vec::<PeerEndpoint>::new())?;
        }

        let config_path = self.config_path();
        if !config_path.exists() {
            self.save_config(&NodeConfig::default())?;
        }

        Ok(())
    }
}

fn default_home_root() -> Result<PathBuf, NodeHomeError> {
    let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) else {
        return Err(NodeHomeError::MissingBaseHomeDir);
    };
    Ok(PathBuf::from(home).join(DEFAULT_HOME_DIRNAME))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{NodeConfig, NodeHome};

    fn temp_home() -> NodeHome {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!("wobble-home-test-{}-{}", std::process::id(), nanos));
        NodeHome::new(path)
    }

    #[test]
    fn derives_canonical_paths_under_root() {
        let home = NodeHome::new("/tmp/wobble-node");

        assert_eq!(
            home.state_path(),
            std::path::PathBuf::from("/tmp/wobble-node/node.sqlite")
        );
        assert_eq!(
            home.wallet_path(),
            std::path::PathBuf::from("/tmp/wobble-node/wallet.bin")
        );
        assert_eq!(
            home.aliases_path(),
            std::path::PathBuf::from("/tmp/wobble-node/aliases.json")
        );
        assert_eq!(
            home.peers_path(),
            std::path::PathBuf::from("/tmp/wobble-node/peers.json")
        );
        assert_eq!(
            home.config_path(),
            std::path::PathBuf::from("/tmp/wobble-node/config.json")
        );
    }

    #[test]
    fn initialize_creates_missing_default_files() {
        let home = temp_home();

        home.initialize().unwrap();

        assert!(home.root().exists());
        assert!(home.state_path().exists());
        assert!(home.wallet_path().exists());
        assert!(home.aliases_path().exists());
        assert!(home.peers_path().exists());
        assert!(home.config_path().exists());

        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn initialize_creates_default_node_config() {
        let home = temp_home();

        home.initialize().unwrap();

        let config = home.load_config().unwrap();

        assert_eq!(config, NodeConfig::default());

        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn save_and_load_config_round_trip() {
        let home = temp_home();

        fs::create_dir_all(home.root()).unwrap();
        home.save_config(&NodeConfig {
            listen_addr: "127.0.0.1:9010".to_string(),
            network: "custom-net".to_string(),
            node_name: Some("alpha".to_string()),
        })
        .unwrap();

        let config = home.load_config().unwrap();

        assert_eq!(config.listen_addr, "127.0.0.1:9010");
        assert_eq!(config.network, "custom-net");
        assert_eq!(config.node_name.as_deref(), Some("alpha"));

        fs::remove_dir_all(home.root()).unwrap();
    }
}
