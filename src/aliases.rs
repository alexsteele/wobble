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
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
    InvalidPublicKey,
    UnknownAlias(String),
}

impl From<io::Error> for AliasError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<bincode::error::EncodeError> for AliasError {
    fn from(error: bincode::error::EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<bincode::error::DecodeError> for AliasError {
    fn from(error: bincode::error::DecodeError) -> Self {
        Self::Decode(error)
    }
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
    let bytes = bincode::serde::encode_to_vec(book, bincode::config::standard())?;
    fs::write(path, bytes)?;
    Ok(())
}

pub fn load_alias_book(path: &Path) -> Result<AliasBook, AliasError> {
    let bytes = fs::read(path)?;
    let (book, _): (AliasBook, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
    Ok(book)
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
            "wobble-alias-test-{}-{}.bin",
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
}
