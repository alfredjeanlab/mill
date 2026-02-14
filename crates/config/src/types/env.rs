use serde::{Deserialize, Serialize};

/// A classified environment variable value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EnvValue {
    /// Plain string with no interpolation.
    Literal(String),
    /// Contains one or more `${...}` expressions.
    Interpolated { parts: Vec<EnvPart> },
}

/// A segment of an interpolated environment variable value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EnvPart {
    /// Literal text between interpolation expressions.
    Literal(String),
    /// `${service.NAME.address}` — resolved at deploy time.
    ServiceAddress { service: String },
    /// `${secret(NAME)}` — resolved from the secret store.
    Secret { name: String },
}
