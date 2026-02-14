mod constraints;
mod parse;
mod refs;

use std::collections::HashMap;

use crate::error::{ConfigError, Result};
use crate::types::*;

/// Resolve a raw HCL config into a fully validated `ClusterConfig`.
pub fn resolve(raw: RawConfig) -> Result<ClusterConfig> {
    let mut errors = Vec::new();

    let mut services = HashMap::new();
    for (name, raw_svc) in &raw.service {
        match resolve_service(name, raw_svc) {
            Ok(svc) => {
                services.insert(name.clone(), svc);
            }
            Err(ConfigError::Multiple(errs)) => errors.extend(errs),
            Err(e) => errors.push(e),
        }
    }

    let mut tasks = HashMap::new();
    for (name, raw_task) in &raw.task {
        match resolve_task(name, raw_task) {
            Ok(task) => {
                tasks.insert(name.clone(), task);
            }
            Err(ConfigError::Multiple(errs)) => errors.extend(errs),
            Err(e) => errors.push(e),
        }
    }

    // Check service address references after all services are resolved.
    let service_names: Vec<&str> = raw.service.keys().map(|s| s.as_str()).collect();
    refs::check_service_refs(&services, &tasks, &service_names, &mut errors);

    if errors.is_empty() {
        Ok(ClusterConfig { services, tasks })
    } else if errors.len() == 1 {
        Err(errors.remove(0))
    } else {
        Err(ConfigError::Multiple(errors))
    }
}

fn resolve_service(name: &str, raw: &RawServiceDef) -> Result<ServiceDef> {
    let mut errors = Vec::new();
    let path = format!("service.{name}");

    let memory = parse::parse_byte_size(&format!("{path}.memory"), &raw.memory, &mut errors);
    constraints::check_cpu(&format!("{path}.cpu"), raw.cpu, &mut errors);
    constraints::check_port(&format!("{path}.port"), raw.port, &mut errors);

    let env = parse::parse_env_map(&path, &raw.env, &mut errors);

    let health = match &raw.health {
        Some(h) => {
            let interval =
                parse::parse_duration(&format!("{path}.health.interval"), &h.interval, &mut errors);
            constraints::check_health_path(&format!("{path}.health.path"), &h.path, &mut errors);
            Some(HealthCheck {
                path: h.path.clone(),
                interval,
                failure_threshold: h.failure_threshold.unwrap_or(3),
            })
        }
        None => None,
    };

    let routes: Vec<RouteDef> = raw
        .route
        .iter()
        .map(|(hostname, r)| RouteDef {
            hostname: hostname.clone(),
            path: r.path.clone(),
            websocket: r.websocket,
        })
        .collect();

    let mut volumes = Vec::new();
    for (vol_name, raw_vol) in &raw.volume {
        let vol_path = format!("{path}.volume.{vol_name}");
        let size = raw_vol
            .size
            .as_ref()
            .map(|s| parse::parse_byte_size(&format!("{vol_path}.size"), s, &mut errors));
        constraints::check_volume(&vol_path, raw_vol.ephemeral, &size, &mut errors);
        volumes.push(VolumeDef {
            name: vol_name.clone(),
            path: raw_vol.path.clone(),
            size,
            ephemeral: raw_vol.ephemeral,
        });
    }

    if errors.is_empty() {
        Ok(ServiceDef {
            image: raw.image.clone(),
            port: raw.port,
            replicas: raw.replicas,
            resources: Resources { cpu: raw.cpu, memory },
            env,
            command: raw.command.clone(),
            health,
            routes,
            volumes,
        })
    } else if errors.len() == 1 {
        Err(errors.remove(0))
    } else {
        Err(ConfigError::Multiple(errors))
    }
}

fn resolve_task(name: &str, raw: &RawTaskDef) -> Result<TaskDef> {
    let mut errors = Vec::new();
    let path = format!("task.{name}");

    let memory = parse::parse_byte_size(&format!("{path}.memory"), &raw.memory, &mut errors);
    let timeout = parse::parse_duration(&format!("{path}.timeout"), &raw.timeout, &mut errors);
    constraints::check_cpu(&format!("{path}.cpu"), raw.cpu, &mut errors);

    let env = parse::parse_env_map(&path, &raw.env, &mut errors);

    if errors.is_empty() {
        Ok(TaskDef {
            image: raw.image.clone(),
            resources: Resources { cpu: raw.cpu, memory },
            timeout,
            env,
            command: raw.command.clone(),
        })
    } else if errors.len() == 1 {
        Err(errors.remove(0))
    } else {
        Err(ConfigError::Multiple(errors))
    }
}
