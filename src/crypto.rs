use sha2::{Digest, Sha256};

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    digest.into()
}

pub fn double_sha256(bytes: &[u8]) -> [u8; 32] {
    sha256(&sha256(bytes))
}
