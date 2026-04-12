//! Bootstrap and persisted peer configuration types.
//!
//! This module keeps peer-file parsing separate from CLI code so a node can
//! load a small static peer list at startup without mixing filesystem and JSON
//! handling into the networking path. It also defines the persisted peer-record
//! types that back the longer-lived sqlite peer store.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    server::PeerEndpoint,
    types::{BlockHash, BlockHeight},
};

/// Describes how a peer first entered the local peer store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerSource {
    Seed,
    Hello,
    PeerExchange,
    Manual,
}

impl PeerSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Hello => "hello",
            Self::PeerExchange => "peer_exchange",
            Self::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "seed" => Some(Self::Seed),
            "hello" => Some(Self::Hello),
            "peer_exchange" => Some(Self::PeerExchange),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// Persisted peer record stored in sqlite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPeer {
    pub addr: String,
    pub node_name: Option<String>,
    pub source: PeerSource,
    pub advertised_tip_hash: Option<BlockHash>,
    pub advertised_height: Option<BlockHeight>,
    pub last_hello_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub last_connect_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub connections: u32,
    pub failed_connections: u32,
    pub behavior_score: i32,
    pub banned_until: Option<String>,
}

impl StoredPeer {
    /// Builds a new persisted peer record from a known endpoint and source.
    pub fn from_endpoint(endpoint: PeerEndpoint, source: PeerSource) -> Self {
        Self {
            addr: endpoint.addr,
            node_name: endpoint.node_name,
            source,
            advertised_tip_hash: None,
            advertised_height: None,
            last_hello_at: None,
            last_seen_at: None,
            last_connect_at: None,
            last_success_at: None,
            last_error: None,
            connections: 0,
            failed_connections: 0,
            behavior_score: 100,
            banned_until: None,
        }
    }

    /// Returns the minimal runtime endpoint view used by the networking layer.
    pub fn endpoint(&self) -> PeerEndpoint {
        PeerEndpoint::new(self.addr.clone(), self.node_name.clone())
    }
}

/// Loads a startup peer list from a JSON file on disk.
///
/// The file must contain a JSON array of `PeerEndpoint` objects. This is used
/// only as bootstrap configuration at process start; the running server does
/// not mutate or rewrite the file.
pub fn load_peer_endpoints(path: &Path) -> Result<Vec<PeerEndpoint>, PeerConfigError> {
    let contents = fs::read_to_string(path).map_err(PeerConfigError::Read)?;
    serde_json::from_str(&contents).map_err(PeerConfigError::Parse)
}

/// Saves a startup peer list to a JSON file on disk.
pub fn save_peer_endpoints(path: &Path, peers: &[PeerEndpoint]) -> Result<(), PeerConfigError> {
    let contents = serde_json::to_string_pretty(peers).map_err(PeerConfigError::Parse)?;
    fs::write(path, contents).map_err(PeerConfigError::Read)
}

#[derive(Debug)]
pub enum PeerConfigError {
    Read(std::io::Error),
    Parse(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        peers::{PeerSource, StoredPeer, load_peer_endpoints, save_peer_endpoints},
        server::PeerEndpoint,
    };

    fn temp_peers_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-peers-test-{}-{}.json",
            std::process::id(),
            nanos
        ));
        path
    }

    #[test]
    fn loads_peer_endpoint_list_from_json() {
        let path = temp_peers_path();
        fs::write(
            &path,
            r#"
[
  { "addr": "127.0.0.1:9001", "node_name": "miner" },
  { "addr": "127.0.0.1:9002", "node_name": null }
]
"#,
        )
        .unwrap();

        let peers = load_peer_endpoints(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(
            peers,
            vec![
                PeerEndpoint::new("127.0.0.1:9001", Some("miner".to_string())),
                PeerEndpoint::new("127.0.0.1:9002", None),
            ]
        );
    }

    #[test]
    fn rejects_invalid_peer_file_json() {
        let path = temp_peers_path();
        fs::write(&path, "{ not json }").unwrap();

        let err = load_peer_endpoints(&path).unwrap_err();
        fs::remove_file(&path).unwrap();

        assert!(matches!(err, crate::peers::PeerConfigError::Parse(_)));
    }

    #[test]
    fn saves_peer_endpoint_list_as_json() {
        let path = temp_peers_path();
        let peers = vec![PeerEndpoint::new(
            "127.0.0.1:9001",
            Some("miner".to_string()),
        )];

        save_peer_endpoints(&path, &peers).unwrap();
        let loaded = load_peer_endpoints(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded, peers);
    }

    #[test]
    fn stored_peer_defaults_to_perfect_behavior() {
        let stored = StoredPeer::from_endpoint(
            PeerEndpoint::new("127.0.0.1:9001", Some("miner".to_string())),
            PeerSource::Seed,
        );

        assert_eq!(stored.behavior_score, 100);
        assert_eq!(stored.connections, 0);
        assert_eq!(stored.failed_connections, 0);
        assert_eq!(stored.advertised_tip_hash, None);
        assert_eq!(stored.advertised_height, None);
        assert_eq!(stored.last_hello_at, None);
        assert_eq!(stored.endpoint().addr, "127.0.0.1:9001");
    }
}
