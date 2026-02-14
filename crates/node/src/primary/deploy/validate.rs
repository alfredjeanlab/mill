use std::collections::{HashMap, HashSet};

use mill_config::{ClusterConfig, EnvPart, EnvValue, NodeResources};

use super::DeployError;

/// Validate that all secret references in the config exist in the FSM.
pub(crate) fn validate_secret_refs(
    config: &ClusterConfig,
    known_secrets: &HashSet<String>,
) -> Result<(), DeployError> {
    let mut missing = Vec::new();
    for (name, def) in &config.services {
        collect_missing_secrets(&def.env, known_secrets, name, &mut missing);
    }
    for (name, def) in &config.tasks {
        collect_missing_secrets(&def.env, known_secrets, name, &mut missing);
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(DeployError::ValidationFailed(format!("unknown secrets: {}", missing.join(", "))))
    }
}

fn collect_missing_secrets(
    env: &HashMap<String, EnvValue>,
    known: &HashSet<String>,
    context: &str,
    missing: &mut Vec<String>,
) {
    for value in env.values() {
        if let EnvValue::Interpolated { parts } = value {
            for part in parts {
                if let EnvPart::Secret { name } = part
                    && !known.contains(name)
                {
                    missing.push(format!("{context}/{name}"));
                }
            }
        }
    }
}

/// Validate that every service/task can fit on at least one Ready node.
pub(crate) fn validate_resource_capacity(
    config: &ClusterConfig,
    nodes: &[NodeResources],
) -> Result<(), DeployError> {
    if nodes.is_empty() {
        return Err(DeployError::ValidationFailed("no ready nodes in cluster".into()));
    }
    let max_cpu = nodes.iter().map(|n| n.cpu_total).fold(0.0_f64, f64::max);
    let max_mem = nodes.iter().map(|n| n.memory_total.as_u64()).max().unwrap_or(0);

    let mut oversized = Vec::new();
    for (name, def) in &config.services {
        if def.resources.cpu > max_cpu || def.resources.memory.as_u64() > max_mem {
            oversized.push(name.as_str());
        }
    }
    for (name, def) in &config.tasks {
        if def.resources.cpu > max_cpu || def.resources.memory.as_u64() > max_mem {
            oversized.push(name.as_str());
        }
    }
    if oversized.is_empty() {
        Ok(())
    } else {
        Err(DeployError::ValidationFailed(format!(
            "resources exceed largest node capacity: {}",
            oversized.join(", ")
        )))
    }
}
