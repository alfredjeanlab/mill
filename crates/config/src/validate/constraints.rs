use bytesize::ByteSize;

use crate::error::ConfigError;

/// CPU must be greater than zero.
pub fn check_cpu(path: &str, cpu: f64, errors: &mut Vec<ConfigError>) {
    if cpu <= 0.0 {
        errors.push(ConfigError::Constraint {
            path: path.to_owned(),
            message: "cpu must be greater than 0".to_owned(),
        });
    }
}

/// Port must be greater than zero.
pub fn check_port(path: &str, port: u16, errors: &mut Vec<ConfigError>) {
    if port == 0 {
        errors.push(ConfigError::Constraint {
            path: path.to_owned(),
            message: "port must be greater than 0".to_owned(),
        });
    }
}

/// Health check path must start with `/`.
pub fn check_health_path(path: &str, health_path: &str, errors: &mut Vec<ConfigError>) {
    if !health_path.starts_with('/') {
        errors.push(ConfigError::Constraint {
            path: path.to_owned(),
            message: format!("health path must start with '/', got '{health_path}'"),
        });
    }
}

/// Persistent volumes must have a size.
pub fn check_volume(
    path: &str,
    ephemeral: bool,
    size: &Option<ByteSize>,
    errors: &mut Vec<ConfigError>,
) {
    if !ephemeral && size.is_none() {
        errors.push(ConfigError::Constraint {
            path: path.to_owned(),
            message: "persistent volume must have a size".to_owned(),
        });
    }
}
