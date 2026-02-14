use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, RaftError>;

#[derive(Debug, Error)]
pub enum RaftError {
    #[error("raft error: {0}")]
    Raft(String),

    #[error("{0}")]
    Snapshot(#[from] SnapshotError),

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("encode error: {0}")]
    Encode(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum EncryptError {
    #[error("encryption failed: {0}")]
    Encrypt(String),
}

#[derive(Debug, Error)]
pub enum DecryptError {
    #[error("decryption failed: {0}")]
    Decrypt(String),
}
