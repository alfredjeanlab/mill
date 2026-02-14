#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod types;
mod validate;

#[cfg(feature = "support")]
pub mod support;
#[cfg(feature = "support")]
pub use support::{poll, poll_async};

pub use error::{ConfigError, Result};
pub use types::*;

/// Parse an HCL config string into a fully validated `ClusterConfig`.
pub fn parse(input: &str) -> Result<ClusterConfig> {
    let raw = parse_raw(input)?;
    validate::resolve(raw)
}

/// Parse an HCL config string into raw (unvalidated) structs.
pub fn parse_raw(input: &str) -> Result<RawConfig> {
    hcl::from_str(input).map_err(|e| ConfigError::Parse(e.to_string()))
}
