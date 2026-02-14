use std::collections::HashMap;

use crate::error::ConfigError;
use crate::types::*;

/// Check that all `${service.X.address}` references point to defined services.
pub fn check_service_refs(
    services: &HashMap<String, ServiceDef>,
    tasks: &HashMap<String, TaskDef>,
    defined: &[&str],
    errors: &mut Vec<ConfigError>,
) {
    for (name, svc) in services {
        let path = format!("service.{name}");
        check_env_refs(&path, &svc.env, defined, errors);
    }
    for (name, task) in tasks {
        let path = format!("task.{name}");
        check_env_refs(&path, &task.env, defined, errors);
    }
}

fn check_env_refs(
    parent_path: &str,
    env: &HashMap<String, EnvValue>,
    defined: &[&str],
    errors: &mut Vec<ConfigError>,
) {
    for (key, value) in env {
        if let EnvValue::Interpolated { parts } = value {
            for part in parts {
                if let EnvPart::ServiceAddress { service } = part
                    && !defined.contains(&service.as_str())
                {
                    errors.push(ConfigError::UnknownServiceRef {
                        path: format!("{parent_path}.env.{key}"),
                        service: service.clone(),
                    });
                }
            }
        }
    }
}
