use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroize;

use crate::error::{DecryptError, EncryptError, RaftError, Result};

/// A cluster-wide encryption key for secrets.
///
/// Generated at `mill init` and stored locally on primary nodes.
/// Never enters the Raft log.
#[derive(Clone)]
pub struct ClusterKey {
    key: Key,
}

impl Drop for ClusterKey {
    fn drop(&mut self) {
        self.key.as_mut_slice().zeroize();
    }
}

impl ClusterKey {
    /// Generate a new random cluster key.
    pub fn generate() -> Self {
        Self { key: ChaCha20Poly1305::generate_key(&mut OsRng) }
    }

    /// Create a ClusterKey from raw bytes (32 bytes).
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self { key: *Key::from_slice(bytes) }
    }

    /// Return the raw 32-byte key for persistence.
    pub fn as_bytes(&self) -> &[u8] {
        self.key.as_slice()
    }

    /// Encrypt plaintext, returning (ciphertext, nonce).
    pub fn encrypt(
        &self,
        plaintext: &[u8],
    ) -> std::result::Result<(Vec<u8>, Vec<u8>), EncryptError> {
        let cipher = ChaCha20Poly1305::new(&self.key);
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext =
            cipher.encrypt(&nonce, plaintext).map_err(|e| EncryptError::Encrypt(e.to_string()))?;
        Ok((ciphertext, nonce.to_vec()))
    }

    /// Decrypt ciphertext using the provided nonce.
    pub fn decrypt(
        &self,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> std::result::Result<Vec<u8>, DecryptError> {
        let cipher = ChaCha20Poly1305::new(&self.key);
        let nonce = Nonce::from_slice(nonce);
        cipher.decrypt(nonce, ciphertext).map_err(|e| DecryptError::Decrypt(e.to_string()))
    }

    /// Save the raw 32-byte key to `<dir>/cluster.key`.
    pub async fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join("cluster.key");
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(&path, self.key.as_slice()).await?;
        Ok(())
    }

    /// Load a cluster key from `<dir>/cluster.key`.
    pub async fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("cluster.key");
        let bytes = tokio::fs::read(&path).await?;
        if bytes.len() != 32 {
            return Err(RaftError::Raft(format!(
                "cluster.key: expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self::from_bytes(&arr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let original_bytes = key.as_bytes().to_vec();

        key.save(dir.path()).await.expect("save");
        let loaded = ClusterKey::load(dir.path()).await.expect("load");
        assert_eq!(loaded.as_bytes(), &original_bytes[..]);
    }

    #[tokio::test]
    async fn load_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = ClusterKey::load(dir.path()).await;
        assert!(result.is_err());
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = ClusterKey::generate();
        let plaintext = b"hello cluster";
        let (ciphertext, nonce) = key.encrypt(plaintext).expect("encrypt");
        let decrypted = key.decrypt(&ciphertext, &nonce).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }
}
