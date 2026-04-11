//! Simple file-backed wallet storage for a single Ed25519 keypair.
//!
//! This keeps one signing key per file and is intended for local development.

use std::{fs, io, path::Path};

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::crypto;

#[derive(Debug)]
pub enum WalletError {
    Io(io::Error),
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
}

impl From<io::Error> for WalletError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<bincode::error::EncodeError> for WalletError {
    fn from(error: bincode::error::EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<bincode::error::DecodeError> for WalletError {
    fn from(error: bincode::error::DecodeError) -> Self {
        Self::Decode(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletFile {
    secret_key: [u8; 32],
}

/// A local wallet containing a single Ed25519 signing key.
#[derive(Debug, Clone)]
pub struct Wallet {
    signing_key: SigningKey,
}

impl Wallet {
    pub fn generate() -> Self {
        Self {
            signing_key: crypto::generate_signing_key(),
        }
    }

    pub fn from_signing_key(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

/// Persists a wallet file to disk, replacing any previous file.
pub fn save_wallet(path: &Path, wallet: &Wallet) -> Result<(), WalletError> {
    let file = WalletFile {
        secret_key: wallet.secret_key_bytes(),
    };
    let bytes = bincode::serde::encode_to_vec(&file, bincode::config::standard())?;
    fs::write(path, bytes)?;
    Ok(())
}

/// Loads a wallet file from disk.
pub fn load_wallet(path: &Path) -> Result<Wallet, WalletError> {
    let bytes = fs::read(path)?;
    let (file, _): (WalletFile, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
    Ok(Wallet::from_signing_key(crypto::signing_key_from_bytes(
        file.secret_key,
    )))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Wallet, load_wallet, save_wallet};

    fn temp_wallet_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after unix epoch")
            .as_nanos();
        path.push(format!(
            "wobble-wallet-test-{}-{}.bin",
            std::process::id(),
            nanos
        ));
        path
    }

    #[test]
    fn wallet_round_trips_through_disk() {
        let wallet = Wallet::generate();
        let path = temp_wallet_path();

        save_wallet(&path, &wallet).unwrap();
        let loaded = load_wallet(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(wallet.secret_key_bytes(), loaded.secret_key_bytes());
        assert_eq!(wallet.verifying_key(), loaded.verifying_key());
    }
}
