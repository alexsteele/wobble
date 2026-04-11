use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    digest.into()
}

pub fn double_sha256(bytes: &[u8]) -> [u8; 32] {
    sha256(&sha256(bytes))
}

pub const PUBLIC_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

pub fn signing_key_from_bytes(bytes: [u8; 32]) -> SigningKey {
    SigningKey::from_bytes(&bytes)
}

pub fn verifying_key_bytes(key: &VerifyingKey) -> [u8; PUBLIC_KEY_LEN] {
    key.to_bytes()
}

pub fn parse_verifying_key(bytes: &[u8]) -> Option<VerifyingKey> {
    let array: [u8; PUBLIC_KEY_LEN] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&array).ok()
}

pub fn parse_signature(bytes: &[u8]) -> Option<Signature> {
    let array: [u8; SIGNATURE_LEN] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&array))
}

pub fn sign_message(signing_key: &SigningKey, message: &[u8]) -> [u8; SIGNATURE_LEN] {
    signing_key.sign(message).to_bytes()
}

pub fn verify_message(signature: &Signature, verifying_key: &VerifyingKey, message: &[u8]) -> bool {
    verifying_key.verify(message, signature).is_ok()
}
