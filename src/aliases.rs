//! Simple file-backed recipient alias book.
//!
//! This maps human-readable names to public keys so the CLI does not require
//! raw key hex for common recipients.

use std::{collections::BTreeMap, fs, io, path::Path};

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::crypto;

#[derive(Debug)]
pub enum AliasError {
    Io(io::Error),
    Serialize(serde_json::Error),
    Parse(serde_json::Error),
    InvalidPublicKey,
    UnknownAlias(String),
}

impl From<io::Error> for AliasError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AliasError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error)
    }
}

/// Human-readable alias file stored as JSON.
///
/// Keys are alias names and values are hex-encoded 32-byte Ed25519 public keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AliasFile {
    entries: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AliasBook {
    entries: BTreeMap<String, [u8; 32]>,
}

impl AliasBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, alias: String, public_key: VerifyingKey) {
        self.entries
            .insert(alias, crypto::verifying_key_bytes(&public_key));
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, VerifyingKey)> + '_ {
        self.entries.iter().filter_map(|(alias, bytes)| {
            crypto::parse_verifying_key(bytes).map(|key| (alias.as_str(), key))
        })
    }

    pub fn resolve(&self, alias: &str) -> Result<VerifyingKey, AliasError> {
        let bytes = self
            .entries
            .get(alias)
            .ok_or_else(|| AliasError::UnknownAlias(alias.to_string()))?;
        crypto::parse_verifying_key(bytes).ok_or(AliasError::InvalidPublicKey)
    }
}

pub fn save_alias_book(path: &Path, book: &AliasBook) -> Result<(), AliasError> {
    let file = AliasFile {
        entries: book
            .entries
            .iter()
            .map(|(alias, bytes)| (alias.clone(), encode_public_key(*bytes)))
            .collect(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(AliasError::Serialize)?;
    fs::write(path, json)?;
    Ok(())
}

pub fn load_alias_book(path: &Path) -> Result<AliasBook, AliasError> {
    let json = fs::read_to_string(path)?;
    let file: AliasFile = serde_json::from_str(&json).map_err(AliasError::Parse)?;
    let mut entries = BTreeMap::new();
    for (alias, value) in file.entries {
        let bytes = parse_public_key_hex(&value)?;
        entries.insert(alias, bytes);
    }
    Ok(AliasBook { entries })
}

fn encode_public_key(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_public_key_hex(value: &str) -> Result<[u8; 32], AliasError> {
    if value.len() != 64 {
        return Err(AliasError::InvalidPublicKey);
    }
    let mut bytes = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk).map_err(|_| AliasError::InvalidPublicKey)?;
        bytes[index] = u8::from_str_radix(hex, 16).map_err(|_| AliasError::InvalidPublicKey)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::crypto;

    use super::{AliasBook, load_alias_book, save_alias_book};

    fn temp_alias_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-alias-test-{}-{}.json",
            std::process::id(),
            nanos
        ));
        path
    }

    #[test]
    fn alias_book_round_trips_through_disk() {
        let mut book = AliasBook::new();
        let recipient = crypto::generate_signing_key().verifying_key();
        book.insert("recipient".to_string(), recipient);

        let path = temp_alias_path();
        save_alias_book(&path, &book).unwrap();
        let loaded = load_alias_book(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded.resolve("recipient").unwrap(), recipient);
    }

    #[test]
    fn alias_book_is_human_readable_json() {
        let mut book = AliasBook::new();
        let recipient = crypto::generate_signing_key().verifying_key();
        book.insert("recipient".to_string(), recipient);

        let path = temp_alias_path();
        save_alias_book(&path, &book).unwrap();
        let json = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert!(json.contains("\"entries\""));
        assert!(json.contains("\"recipient\""));
    }
}
