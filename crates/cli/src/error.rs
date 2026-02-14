use mill_config::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// API returned a structured error response.
    #[error("{}", .0.error)]
    Api(ErrorResponse),

    /// HTTP error with non-JSON body.
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// Connection refused — cluster probably not running.
    #[error("connection refused — is the cluster running? (address: {address})")]
    ConnectionRefused { address: String },

    /// Other request errors (DNS, timeout, TLS, etc.).
    #[error("request failed: {0}")]
    Request(reqwest::Error),

    /// I/O errors (reading config files, etc.).
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// Argument validation errors.
    #[error("{0}")]
    Arg(String),
}

impl CliError {
    /// Map a reqwest error, checking for connection refused.
    pub fn from_reqwest(err: reqwest::Error, address: &str) -> Self {
        if err.is_connect() {
            CliError::ConnectionRefused { address: address.to_string() }
        } else {
            CliError::Request(err)
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Api(_) | CliError::Http { .. } => 1,
            CliError::ConnectionRefused { .. } => 2,
            CliError::Request(_) => 3,
            CliError::Io(_) => 4,
            CliError::Arg(_) => 64, // EX_USAGE
        }
    }
}
