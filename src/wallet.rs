//! File-backed wallet storage for a small named Ed25519 keyring.
//!
//! A wallet file stores multiple named signing keys plus one default key name.
//! The default key is used for common single-wallet flows like mining rewards,
//! bootstrapping, and change outputs. Older single-key wallet files still load
//! and are upgraded in memory to a one-entry keyring named `default`.

use std::{fs, io, path::Path};

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::crypto;

const DEFAULT_KEY_NAME: &str = "default";

#[derive(Debug)]
pub enum WalletError {
    Io(io::Error),
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
    DuplicateKeyName(String),
    MissingDefaultKey(String),
    EmptyWallet,
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
struct WalletKeyFile {
    name: String,
    secret_key: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletFileV2 {
    default_key: String,
    keys: Vec<WalletKeyFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletFileV1 {
    secret_key: [u8; 32],
}

/// One named signing key owned by a local wallet.
#[derive(Debug, Clone)]
pub struct WalletKey {
    name: String,
    signing_key: SigningKey,
}

impl WalletKey {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    fn secret_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

/// A local wallet containing multiple named Ed25519 signing keys.
#[derive(Debug, Clone)]
pub struct Wallet {
    default_key: String,
    keys: Vec<WalletKey>,
}

impl Wallet {
    pub fn generate() -> Self {
        Self::from_signing_key(crypto::generate_signing_key())
    }

    pub fn from_signing_key(signing_key: SigningKey) -> Self {
        Self {
            default_key: DEFAULT_KEY_NAME.to_string(),
            keys: vec![WalletKey {
                name: DEFAULT_KEY_NAME.to_string(),
                signing_key,
            }],
        }
    }

    pub fn default_key_name(&self) -> &str {
        &self.default_key
    }

    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    pub fn keys(&self) -> impl Iterator<Item = &WalletKey> {
        self.keys.iter()
    }

    pub fn signing_key(&self) -> &SigningKey {
        self.default_wallet_key().signing_key()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.default_wallet_key().verifying_key()
    }

    pub fn verifying_keys(&self) -> impl Iterator<Item = VerifyingKey> + '_ {
        self.keys.iter().map(|key| key.verifying_key())
    }

    pub fn generate_key(&mut self, name: impl Into<String>) -> Result<VerifyingKey, WalletError> {
        let name = name.into();
        if self.keys.iter().any(|key| key.name == name) {
            return Err(WalletError::DuplicateKeyName(name));
        }
        let signing_key = crypto::generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        self.keys.push(WalletKey { name, signing_key });
        Ok(verifying_key)
    }

    fn default_wallet_key(&self) -> &WalletKey {
        self.keys
            .iter()
            .find(|key| key.name == self.default_key)
            .expect("wallet invariant: default key exists")
    }

    fn validate(&self) -> Result<(), WalletError> {
        if self.keys.is_empty() {
            return Err(WalletError::EmptyWallet);
        }
        for (index, key) in self.keys.iter().enumerate() {
            if self.keys[..index]
                .iter()
                .any(|prior| prior.name == key.name)
            {
                return Err(WalletError::DuplicateKeyName(key.name.clone()));
            }
        }
        if !self.keys.iter().any(|key| key.name == self.default_key) {
            return Err(WalletError::MissingDefaultKey(self.default_key.clone()));
        }
        Ok(())
    }
}

/// Persists a wallet file to disk, replacing any previous file.
pub fn save_wallet(path: &Path, wallet: &Wallet) -> Result<(), WalletError> {
    wallet.validate()?;
    let file = WalletFileV2 {
        default_key: wallet.default_key.clone(),
        keys: wallet
            .keys
            .iter()
            .map(|key| WalletKeyFile {
                name: key.name.clone(),
                secret_key: key.secret_key_bytes(),
            })
            .collect(),
    };
    let bytes = bincode::serde::encode_to_vec(&file, bincode::config::standard())?;
    fs::write(path, bytes)?;
    Ok(())
}

/// Loads a wallet file from disk.
pub fn load_wallet(path: &Path) -> Result<Wallet, WalletError> {
    let bytes = fs::read(path)?;
    if let Ok((file, _)) =
        bincode::serde::decode_from_slice::<WalletFileV2, _>(&bytes, bincode::config::standard())
    {
        let wallet = Wallet {
            default_key: file.default_key,
            keys: file
                .keys
                .into_iter()
                .map(|key| WalletKey {
                    name: key.name,
                    signing_key: crypto::signing_key_from_bytes(key.secret_key),
                })
                .collect(),
        };
        wallet.validate()?;
        return Ok(wallet);
    }

    let (legacy, _): (WalletFileV1, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
    Ok(Wallet::from_signing_key(crypto::signing_key_from_bytes(
        legacy.secret_key,
    )))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::crypto;

    use super::{Wallet, WalletFileV1, load_wallet, save_wallet};

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

        assert_eq!(wallet.key_count(), loaded.key_count());
        assert_eq!(wallet.verifying_key(), loaded.verifying_key());
        assert_eq!(wallet.default_key_name(), loaded.default_key_name());
    }

    #[test]
    fn wallet_round_trips_multiple_named_keys() {
        let mut wallet = Wallet::generate();
        let second = wallet.generate_key("receiving-1").unwrap();
        let path = temp_wallet_path();

        save_wallet(&path, &wallet).unwrap();
        let loaded = load_wallet(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded.key_count(), 2);
        assert_eq!(loaded.default_key_name(), "default");
        assert!(loaded.keys().any(|key| key.name() == "receiving-1"));
        assert!(loaded.verifying_keys().any(|key| key == second));
    }

    #[test]
    fn load_wallet_accepts_legacy_single_key_files() {
        let signing_key = crypto::signing_key_from_bytes([7; 32]);
        let file = WalletFileV1 {
            secret_key: signing_key.to_bytes(),
        };
        let path = temp_wallet_path();
        let bytes = bincode::serde::encode_to_vec(&file, bincode::config::standard()).unwrap();
        fs::write(&path, bytes).unwrap();

        let loaded = load_wallet(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(loaded.key_count(), 1);
        assert_eq!(loaded.default_key_name(), "default");
        assert_eq!(loaded.verifying_key(), signing_key.verifying_key());
    }

    #[test]
    fn save_wallet_rejects_duplicate_key_names() {
        let default = crypto::signing_key_from_bytes([1; 32]);
        let duplicate = crypto::signing_key_from_bytes([2; 32]);
        let wallet = Wallet {
            default_key: "default".to_string(),
            keys: vec![
                super::WalletKey {
                    name: "default".to_string(),
                    signing_key: default,
                },
                super::WalletKey {
                    name: "default".to_string(),
                    signing_key: duplicate,
                },
            ],
        };
        let path = temp_wallet_path();

        let err = save_wallet(&path, &wallet).unwrap_err();
        let _ = fs::remove_file(&path);

        assert!(matches!(err, super::WalletError::DuplicateKeyName(name) if name == "default"));
    }
}
