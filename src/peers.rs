//! Bootstrap peer configuration loaded from disk.
//!
//! This module keeps peer-file parsing separate from CLI code so a node can
//! load a small static peer list at startup without mixing filesystem and JSON
//! handling into the networking path. The file format is intentionally simple:
//! a JSON array of `PeerEndpoint` records.

use std::{fs, path::Path};

use crate::server::PeerEndpoint;

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
        peers::{load_peer_endpoints, save_peer_endpoints},
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
}
