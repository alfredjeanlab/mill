/// Errors that can occur when parsing or validating a Mill config.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigError {
    /// HCL parse error.
    #[error("parse error: {0}")]
    Parse(String),
    /// A field has an invalid value (e.g. bad duration or byte size).
    #[error("{path}: {message}")]
    InvalidField { path: String, message: String },
    /// An `${service.X.address}` reference points to an undefined service.
    #[error("{path}: unknown service reference '{service}'")]
    UnknownServiceRef { path: String, service: String },
    /// A constraint violation (e.g. cpu <= 0, health path not starting with `/`).
    #[error("{path}: {message}")]
    Constraint { path: String, message: String },
    /// Multiple errors collected during validation.
    #[error("{}", display_multiple(.0))]
    Multiple(Vec<ConfigError>),
}

fn display_multiple(errors: &[ConfigError]) -> String {
    errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")
}

pub type Result<T> = std::result::Result<T, ConfigError>;
