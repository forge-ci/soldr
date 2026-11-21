//! One Ed25519 seed for enrollment and the dist iroh node identity.
use crate::{proto, Result};
use ed25519_dalek::{Signer, SigningKey};
use std::{
    io::{Read, Write},
    path::Path,
};

// Deliberately no Debug: this value holds private key material.
pub struct HostKey(SigningKey);
impl HostKey {
    pub fn public_key(&self) -> String {
        proto::encode(self.0.verifying_key().as_bytes())
    }
    pub fn sign_challenge(&self, nonce: &str, name: &str) -> String {
        proto::encode(&self.0.sign(&proto::signing_bytes(nonce, name)).to_bytes())
    }
    /// Pass these exact seed bytes to iroh::SecretKey::from_bytes in dist serve.
    /// The caller must never log or persist a second copy of them.
    pub fn iroh_secret_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

/// Publish a fully written 0600 seed atomically, without replacing any key.
/// The parent must exist and be controlled by the service user. Concurrent
/// creators load the winning seed; interrupted writers never publish partial keys.
pub fn load_or_create(path: &Path) -> Result<HostKey> {
    match load(path) {
        Ok(key) => return Ok(key),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).map_err(|_| "host key randomness unavailable")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    soldr_platform::platform::fs::permissions::restore_mode(staged.path(), Some(0o600))?;
    staged.write_all(&seed)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.error.into()),
    }
    load(path)
}
fn load(path: &Path) -> Result<HostKey> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() != 32 {
        return Err("host key must be a regular 32-byte seed file".into());
    }
    soldr_platform::platform::fs::permissions::restore_mode(path, Some(0o600))?;
    let mut seed = [0; 32];
    std::fs::File::open(path)?.read_exact(&mut seed)?;
    Ok(HostKey(SigningKey::from_bytes(&seed)))
}
