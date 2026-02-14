use std::io;

/// Errors from the mill-proxy reverse proxy.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Failed to bind a listener to an address.
    #[error("bind {address}: {source}")]
    Bind { address: String, source: io::Error },

    /// TLS configuration or handshake error.
    #[error("tls error: {0}")]
    Tls(String),

    /// Failed to generate a certificate.
    #[error("cert generation: {0}")]
    CertGeneration(String),

    /// HTTP protocol error during proxying.
    #[error("http error: {0}")]
    Http(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Failed to connect to a backend service.
    #[error("backend {backend}: {reason}")]
    BackendConnect { backend: String, reason: String },

    /// ACME certificate acquisition or renewal error.
    #[error("acme error: {0}")]
    Acme(String),

    /// WebSocket upgrade or relay error.
    #[error("websocket error: {0}")]
    WebSocket(String),

    /// General I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, ProxyError>;
