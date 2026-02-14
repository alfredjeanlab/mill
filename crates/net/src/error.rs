use std::io;

pub type Result<T> = std::result::Result<T, NetError>;

/// Errors that can occur in mill-net operations.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// An external command (wg, ip, ping, ss) failed.
    #[error("{program}: {message}")]
    Command { program: String, message: String },

    /// A required external binary was not found.
    #[error("{program}: command not found")]
    CommandNotFound { program: String },

    /// Failed to bind a socket to an address.
    #[error("bind {address}: {source}")]
    Bind { address: String, source: io::Error },

    /// General I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// Malformed DNS message.
    #[error("dns protocol error: {0}")]
    DnsProtocol(String),

    /// Invalid IP address or subnet string.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}
