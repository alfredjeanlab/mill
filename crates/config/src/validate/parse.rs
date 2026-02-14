use std::collections::HashMap;
use std::time::Duration;

use bytesize::ByteSize;

use crate::error::ConfigError;
use crate::types::{EnvPart, EnvValue};

/// Parse a byte size string like "512M" or "5Gi".
/// On failure, pushes an error and returns a zero-size default.
pub fn parse_byte_size(path: &str, value: &str, errors: &mut Vec<ConfigError>) -> ByteSize {
    match value.parse::<ByteSize>() {
        Ok(size) => size,
        Err(e) => {
            errors.push(ConfigError::InvalidField {
                path: path.to_owned(),
                message: format!("invalid byte size '{value}': {e}"),
            });
            ByteSize(0)
        }
    }
}

/// Parse a duration string like "10s" or "2h".
/// On failure, pushes an error and returns a zero duration.
pub fn parse_duration(path: &str, value: &str, errors: &mut Vec<ConfigError>) -> Duration {
    match value.parse::<humantime::Duration>() {
        Ok(d) => d.into(),
        Err(e) => {
            errors.push(ConfigError::InvalidField {
                path: path.to_owned(),
                message: format!("invalid duration '{value}': {e}"),
            });
            Duration::ZERO
        }
    }
}

/// Parse a raw env map into classified `EnvValue`s.
pub fn parse_env_map(
    parent_path: &str,
    raw: &HashMap<String, String>,
    errors: &mut Vec<ConfigError>,
) -> HashMap<String, EnvValue> {
    let mut result = HashMap::new();
    for (key, value) in raw {
        let path = format!("{parent_path}.env.{key}");
        result.insert(key.clone(), parse_env_value(&path, value, errors));
    }
    result
}

/// Classify a single env value string.
///
/// - No `${...}` → `Literal`
/// - Contains `${...}` → `Interpolated` with parts:
///   - `${service.NAME.address}` → `ServiceAddress`
///   - `${secret(NAME)}` → `Secret`
pub fn parse_env_value(path: &str, value: &str, errors: &mut Vec<ConfigError>) -> EnvValue {
    let mut parts = Vec::new();
    let mut rest = value;

    loop {
        match rest.find("${") {
            None => {
                // No more interpolations — capture trailing literal.
                if !rest.is_empty() {
                    if parts.is_empty() {
                        // No interpolations at all — plain literal.
                        return EnvValue::Literal(rest.to_owned());
                    }
                    parts.push(EnvPart::Literal(rest.to_owned()));
                }
                break;
            }
            Some(start) => {
                // Capture literal before `${`.
                if start > 0 {
                    parts.push(EnvPart::Literal(rest[..start].to_owned()));
                }

                let after_dollar_brace = &rest[start + 2..];
                match after_dollar_brace.find('}') {
                    None => {
                        errors.push(ConfigError::InvalidField {
                            path: path.to_owned(),
                            message: format!("unclosed interpolation in '{value}'"),
                        });
                        // Treat remainder as literal.
                        parts.push(EnvPart::Literal(rest[start..].to_owned()));
                        break;
                    }
                    Some(end) => {
                        let expr = &after_dollar_brace[..end];
                        let part = parse_interpolation_expr(path, expr, value, errors);
                        parts.push(part);
                        rest = &after_dollar_brace[end + 1..];
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        // Empty string.
        EnvValue::Literal(String::new())
    } else {
        EnvValue::Interpolated { parts }
    }
}

/// Parse the expression inside `${...}`.
fn parse_interpolation_expr(
    path: &str,
    expr: &str,
    full_value: &str,
    errors: &mut Vec<ConfigError>,
) -> EnvPart {
    // ${secret(NAME)}
    if let Some(inner) = expr.strip_prefix("secret(").and_then(|s| s.strip_suffix(')')) {
        let name = inner.trim();
        if name.is_empty() {
            errors.push(ConfigError::InvalidField {
                path: path.to_owned(),
                message: format!("empty secret name in '{full_value}'"),
            });
        }
        return EnvPart::Secret { name: name.to_owned() };
    }

    // ${service.NAME.address}
    if let Some(rest) = expr.strip_prefix("service.")
        && let Some(name) = rest.strip_suffix(".address")
    {
        if name.is_empty() {
            errors.push(ConfigError::InvalidField {
                path: path.to_owned(),
                message: format!("empty service name in '{full_value}'"),
            });
        }
        return EnvPart::ServiceAddress { service: name.to_owned() };
    }

    errors.push(ConfigError::InvalidField {
        path: path.to_owned(),
        message: format!("unknown interpolation '${{{}}}' in '{}'", expr, full_value),
    });
    EnvPart::Literal(format!("${{{expr}}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_literal() {
        let mut errors = Vec::new();
        let result = parse_env_value("test", "hello", &mut errors);
        assert!(errors.is_empty());
        assert_eq!(result, EnvValue::Literal("hello".to_owned()));
    }

    #[test]
    fn test_secret_ref() {
        let mut errors = Vec::new();
        let result = parse_env_value("test", "${secret(db.password)}", &mut errors);
        assert!(errors.is_empty());
        assert_eq!(
            result,
            EnvValue::Interpolated {
                parts: vec![EnvPart::Secret { name: "db.password".to_owned() }]
            }
        );
    }

    #[test]
    fn test_service_address() {
        let mut errors = Vec::new();
        let result = parse_env_value("test", "tcp://${service.redis.address}", &mut errors);
        assert!(errors.is_empty());
        assert_eq!(
            result,
            EnvValue::Interpolated {
                parts: vec![
                    EnvPart::Literal("tcp://".to_owned()),
                    EnvPart::ServiceAddress { service: "redis".to_owned() },
                ]
            }
        );
    }

    #[test]
    fn test_multiple_interpolations() {
        let mut errors = Vec::new();
        let result =
            parse_env_value("test", "${service.a.address}:${service.b.address}", &mut errors);
        assert!(errors.is_empty());
        assert_eq!(
            result,
            EnvValue::Interpolated {
                parts: vec![
                    EnvPart::ServiceAddress { service: "a".to_owned() },
                    EnvPart::Literal(":".to_owned()),
                    EnvPart::ServiceAddress { service: "b".to_owned() },
                ]
            }
        );
    }

    #[test]
    fn test_empty_string() {
        let mut errors = Vec::new();
        let result = parse_env_value("test", "", &mut errors);
        assert!(errors.is_empty());
        assert_eq!(result, EnvValue::Literal(String::new()));
    }

    #[test]
    fn test_unclosed_interpolation() {
        let mut errors = Vec::new();
        let _result = parse_env_value("test", "${service.x.address", &mut errors);
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            ConfigError::InvalidField { message, .. } => {
                assert!(message.contains("unclosed"));
            }
            other => unreachable!("unexpected error: {other:?}"),
        }
    }
}
