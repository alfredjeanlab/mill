pub mod diff;
mod validate;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mill_config::{
    AllocId, AllocKind, AllocStatus, ClusterConfig, ContainerSpec, ContainerVolume, EnvPart,
    EnvValue, NodeId, NodeResources, NodeStatus, ServiceDef,
};
use mill_raft::MillRaft;
use mill_raft::fsm::command::Command;
use tokio::sync::mpsc;

use super::health::HealthPoller;
use super::scheduler::{self, ScheduleRequest};
use crate::error::NodeError;
use crate::primary::api::sse::SseEvent;
use crate::rpc::{CommandRouter, NodeCommand};
use crate::storage::Volumes;

use self::diff::diff_config;
use self::validate::{validate_resource_capacity, validate_secret_refs};

/// Errors specific to the deploy process.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("scheduling failed for {service}, replica {replica}: no nodes available")]
    SchedulingFailed { service: String, replica: u32 },

    #[error("health check timeout for {service}")]
    HealthCheckTimeout { service: String },

    #[error("failed to resolve env var: {0}")]
    EnvResolution(String),

    #[error("deploy validation failed: {0}")]
    ValidationFailed(String),

    #[error(transparent)]
    Node(#[from] NodeError),

    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
}

/// Shared context for deploy operations, grouping the handles needed by the
/// deploy coordinator so that individual functions take fewer arguments.
pub(crate) struct DeployCtx {
    pub(crate) raft: Arc<MillRaft>,
    pub(crate) router: CommandRouter,
    pub(crate) volumes: Option<Arc<dyn Volumes>>,
    pub(crate) health: Arc<HealthPoller>,
    pub(crate) progress: mpsc::Sender<SseEvent>,
    pub(crate) started: Instant,
}

/// Run a full deploy: diff configs, create volumes, schedule + start allocs,
/// health-check, and stop old allocs.
pub async fn run_deploy(
    raft: Arc<MillRaft>,
    router: CommandRouter,
    volumes: Option<Arc<dyn Volumes>>,
    health: Arc<HealthPoller>,
    new_config: ClusterConfig,
    progress: mpsc::Sender<SseEvent>,
) -> Result<(), DeployError> {
    let ctx = DeployCtx { raft, router, volumes, health, progress, started: Instant::now() };

    // Step 1: Read current config from FSM, compute diff
    let old_config = ctx.raft.read_state(|fsm| fsm.config.clone());
    let plan = diff_config(old_config.as_ref(), &new_config);

    send_json_event(&ctx.progress, "progress", serde_json::json!({"phase": "started"})).await;

    // Step 1b: Pre-deploy validation
    let known_secrets = ctx.raft.read_state(|fsm| {
        fsm.list_secret_names().into_iter().map(|s| s.to_owned()).collect::<HashSet<String>>()
    });
    validate_secret_refs(&new_config, &known_secrets)?;

    let node_capacities: Vec<NodeResources> = ctx.raft.read_state(|fsm| {
        fsm.list_nodes()
            .into_iter()
            .filter(|n| n.status == NodeStatus::Ready)
            .map(|n| n.resources.clone())
            .collect()
    });
    validate_resource_capacity(&new_config, &node_capacities)?;

    // Step 1c: Eager image pull — broadcast Pull for unique images from added/changed services
    eager_pull(&ctx, &plan).await;

    // Step 2: Propose new config to raft
    ctx.raft.propose(Command::Deploy(new_config.clone())).await.map_err(NodeError::from)?;

    // Step 3: Removed services — stop all allocs
    for name in &plan.removed {
        stop_service(&ctx, name).await?;
    }

    // Step 4: Added services — schedule and start each replica
    let mut has_failures = false;
    for (name, def) in &plan.added {
        if let Err(e) = deploy_service(&ctx, name, def).await {
            has_failures = true;
            send_json_event(
                &ctx.progress,
                "failed",
                serde_json::json!({"service": name, "reason": e.to_string()}),
            )
            .await;
        }
    }

    // Step 5: Changed services — rolling update
    for (name, def) in &plan.changed {
        if let Err(e) = rolling_update(&ctx, name, def).await {
            has_failures = true;
            send_json_event(
                &ctx.progress,
                "failed",
                serde_json::json!({"service": name, "reason": e.to_string()}),
            )
            .await;
        }
    }

    // Rollback config if any service deployments failed.
    if has_failures && let Some(old) = old_config {
        let _ = ctx.raft.propose(Command::Deploy(old)).await;
    }

    let elapsed = ctx.started.elapsed().as_secs_f64();
    send_json_event(
        &ctx.progress,
        "done",
        serde_json::json!({"elapsed": elapsed, "success": !has_failures}),
    )
    .await;
    Ok(())
}

/// Restart a single service: same image, new containers, rolling update.
pub async fn run_restart(
    raft: Arc<MillRaft>,
    router: CommandRouter,
    volumes: Option<Arc<dyn Volumes>>,
    health: Arc<HealthPoller>,
    service_name: String,
    progress: mpsc::Sender<SseEvent>,
) -> Result<(), DeployError> {
    let ctx = DeployCtx { raft, router, volumes, health, progress, started: Instant::now() };

    let def = ctx
        .raft
        .read_state(|fsm| fsm.config.as_ref().and_then(|c| c.services.get(&service_name).cloned()));
    let def = def.ok_or_else(|| NodeError::ServiceNotFound(service_name.clone()))?;

    send_json_event(
        &ctx.progress,
        "progress",
        serde_json::json!({"phase": "started", "service": service_name}),
    )
    .await;

    rolling_update(&ctx, &service_name, &def).await?;

    let elapsed = ctx.started.elapsed().as_secs_f64();
    send_json_event(&ctx.progress, "done", serde_json::json!({"elapsed": elapsed})).await;
    Ok(())
}

/// Stop all allocations for a service.
async fn stop_service(ctx: &DeployCtx, name: &str) -> Result<(), DeployError> {
    let allocs = ctx.raft.read_state(|fsm| {
        fsm.list_service_allocs(name)
            .into_iter()
            .map(|a| (a.id.clone(), a.node.clone()))
            .collect::<Vec<_>>()
    });
    for (alloc_id, node_id) in allocs {
        ctx.router.send(&node_id, NodeCommand::Stop { alloc_id: alloc_id.clone() }).await?;
    }
    send_json_event(
        &ctx.progress,
        "progress",
        serde_json::json!({"service": name, "phase": "stopped"}),
    )
    .await;
    Ok(())
}

/// Deploy a brand-new service: create volumes, schedule, start, health-check.
async fn deploy_service(ctx: &DeployCtx, name: &str, def: &ServiceDef) -> Result<(), DeployError> {
    // Create persistent volumes if needed. Use the first Ready node; the
    // scheduler will co-locate workloads via volume affinity after creation.
    let raft_node_id = ctx
        .raft
        .read_state(|fsm| {
            fsm.nodes.values().find(|n| n.status == NodeStatus::Ready).map(|n| n.raft_id)
        })
        .ok_or(DeployError::SchedulingFailed { service: name.into(), replica: 0 })?;
    create_volumes_if_needed(ctx, def, raft_node_id).await?;

    for replica in 0..def.replicas {
        let alloc_id = AllocId(format!("{name}-{replica}"));
        start_alloc(ctx, name, def, &alloc_id, replica).await?;
    }

    Ok(())
}

/// Rolling update: for each replica, start new → health check → stop old.
async fn rolling_update(ctx: &DeployCtx, name: &str, def: &ServiceDef) -> Result<(), DeployError> {
    let raft_node_id = ctx
        .raft
        .read_state(|fsm| {
            fsm.nodes.values().find(|n| n.status == NodeStatus::Ready).map(|n| n.raft_id)
        })
        .ok_or(DeployError::SchedulingFailed { service: name.into(), replica: 0 })?;
    create_volumes_if_needed(ctx, def, raft_node_id).await?;

    // Get existing allocs for this service (id + node for targeted stops).
    let old_allocs = ctx.raft.read_state(|fsm| {
        fsm.list_service_allocs(name)
            .into_iter()
            .map(|a| (a.id.clone(), a.node.clone()))
            .collect::<Vec<_>>()
    });

    // Start new replicas.
    let mut new_allocs: Vec<(AllocId, NodeId)> = Vec::new();
    for replica in 0..def.replicas {
        let alloc_id = AllocId(format!("{name}-{}", crate::util::rand_suffix()));
        match start_alloc(ctx, name, def, &alloc_id, replica).await {
            Ok(()) => {
                // Look up the node this alloc was scheduled on.
                let node_id =
                    ctx.raft.read_state(|fsm| fsm.allocs.get(&alloc_id).map(|a| a.node.clone()));
                if let Some(nid) = node_id {
                    new_allocs.push((alloc_id, nid));
                }
            }
            Err(e) => {
                // Rollback: stop any new allocs we already started, keep old ones.
                for (id, nid) in &new_allocs {
                    let _ = ctx.router.send(nid, NodeCommand::Stop { alloc_id: id.clone() }).await;
                }
                return Err(e);
            }
        }
    }

    // New replicas are healthy — stop old ones. Continue on failure so we
    // don't leave remaining old allocs running alongside the new ones.
    let mut stop_err: Option<NodeError> = None;
    for (old_id, node_id) in old_allocs {
        if let Err(e) =
            ctx.router.send(&node_id, NodeCommand::Stop { alloc_id: old_id.clone() }).await
        {
            tracing::warn!(alloc = %old_id.0, node = %node_id.0, error = %e, "failed to stop old alloc");
            stop_err.get_or_insert(e);
        }
    }

    send_json_event(
        &ctx.progress,
        "progress",
        serde_json::json!({"service": name, "phase": "stopped"}),
    )
    .await;

    if let Some(e) = stop_err {
        return Err(e.into());
    }
    Ok(())
}

/// Schedule, resolve env, and start a single allocation. Waits for it to reach
/// Running status and optionally pass health checks.
pub(crate) async fn start_alloc(
    ctx: &DeployCtx,
    name: &str,
    def: &ServiceDef,
    alloc_id: &AllocId,
    replica: u32,
) -> Result<(), DeployError> {
    // Schedule: pick a node.
    let volume_name = def.volumes.first().map(|v| v.name.as_str());
    let node_id = ctx.raft.read_state(|fsm| {
        let nodes: Vec<_> = fsm.list_nodes().into_iter().collect();
        let allocs: Vec<_> = fsm
            .list_allocs()
            .into_iter()
            .map(|a| (a.node.clone(), a.name.as_str(), &a.kind))
            .collect();
        let vols: Vec<_> = fsm.list_volumes();

        scheduler::schedule(
            &ScheduleRequest {
                need: &def.resources,
                name,
                kind: &AllocKind::Service,
                volume: volume_name,
            },
            &nodes,
            &allocs,
            &vols,
            |id| fsm.node_available_resources(id),
        )
    });

    let node_id = node_id.ok_or(DeployError::SchedulingFailed { service: name.into(), replica })?;

    send_json_event(
        &ctx.progress,
        "progress",
        serde_json::json!({"service": name, "phase": "scheduled", "node": node_id.0}),
    )
    .await;

    // Find the raft_id for this node so we can record the alloc.
    let raft_node_id = ctx.raft.read_state(|fsm| fsm.raft_id_for(&node_id));
    let raft_node_id =
        raft_node_id.ok_or(DeployError::SchedulingFailed { service: name.into(), replica })?;

    // Propose AllocScheduled to raft.
    ctx.raft
        .propose(Command::AllocScheduled {
            alloc_id: alloc_id.clone(),
            node_id: raft_node_id,
            name: name.to_owned(),
            kind: AllocKind::Service,
            address: None, // Will be set by RunStarted report.
            resources: def.resources.clone().into(),
        })
        .await
        .map_err(NodeError::from)?;

    // After scheduling, any failure must mark the alloc as Failed so the
    // watchdog doesn't count it as non-terminal and block replacement allocs.
    let result = run_scheduled_alloc(ctx, name, def, alloc_id, &node_id).await;
    if let Err(ref e) = result {
        tracing::warn!("alloc {} failed after scheduling: {e}", alloc_id.0);
        let _ = ctx
            .raft
            .propose(Command::AllocStatus {
                alloc_id: alloc_id.clone(),
                status: AllocStatus::Failed { reason: e.to_string() },
            })
            .await;
    }
    result
}

/// Inner body of `start_alloc` after the alloc has been proposed as Scheduled.
/// Resolves env, builds the container spec, sends Run, waits for Running,
/// polls for address, runs health checks, and proposes Healthy.
async fn run_scheduled_alloc(
    ctx: &DeployCtx,
    name: &str,
    def: &ServiceDef,
    alloc_id: &AllocId,
    node_id: &NodeId,
) -> Result<(), DeployError> {
    // Resolve environment variables.
    let env = resolve_env(&ctx.raft, &def.env)?;

    // Build container spec.
    let spec = ContainerSpec {
        alloc_id: alloc_id.clone(),
        kind: AllocKind::Service,
        timeout: None,
        command: def.command.clone(),
        image: def.image.clone(),
        env,
        port: Some(def.port),
        host_port: None, // Assigned by secondary.
        cpu: def.resources.cpu,
        memory: def.resources.memory,
        volumes: def
            .volumes
            .iter()
            .map(|v| ContainerVolume {
                name: v.name.clone(),
                path: v.path.clone(),
                ephemeral: v.ephemeral,
            })
            .collect(),
    };

    // Send Run command to the target node.
    ctx.router
        .send(node_id, NodeCommand::Run { alloc_id: alloc_id.clone(), spec, auth: None })
        .await?;

    send_json_event(
        &ctx.progress,
        "progress",
        serde_json::json!({"service": name, "phase": "starting"}),
    )
    .await;

    // Wait for Running status in FSM (set by report processor).
    wait_for_status(&ctx.raft, alloc_id, crate::env::deploy_timeout()).await?;

    // Wait for address to be set (RunStarted report from secondary).
    let addr = poll_for_address(&ctx.raft, alloc_id, crate::env::poll_address_timeout()).await;
    match addr {
        Some(addr) => {
            // Health check: HTTP probe if configured, TCP readiness otherwise.
            let ready = if let Some(hc) = &def.health {
                ctx.health.wait_healthy(addr, hc, crate::env::health_timeout()).await
            } else {
                ctx.health.wait_tcp_ready(addr, crate::env::health_timeout()).await
            };
            if !ready {
                return Err(DeployError::HealthCheckTimeout { service: name.into() });
            }
            ctx.raft
                .propose(Command::AllocStatus {
                    alloc_id: alloc_id.clone(),
                    status: AllocStatus::Healthy,
                })
                .await
                .map_err(NodeError::from)?;
            let elapsed = ctx.started.elapsed().as_secs_f64();
            send_json_event(
                &ctx.progress,
                "progress",
                serde_json::json!({"service": name, "phase": "healthy", "elapsed": elapsed}),
            )
            .await;
        }
        None => {
            return Err(DeployError::HealthCheckTimeout { service: name.into() });
        }
    }

    Ok(())
}

/// Poll FSM until the given alloc has an address set.
async fn poll_for_address(
    raft: &MillRaft,
    alloc_id: &AllocId,
    timeout: Duration,
) -> Option<SocketAddr> {
    let mut changes = raft.subscribe_changes();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let addr = raft.read_state(|fsm| fsm.allocs.get(alloc_id).and_then(|a| a.address));
        if addr.is_some() {
            return addr;
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        let remaining = deadline - tokio::time::Instant::now();
        if tokio::time::timeout(remaining, changes.changed()).await.is_err() {
            return None;
        }
    }
}

/// Poll FSM until the given alloc reaches Running (or Healthy) status.
pub(crate) async fn wait_for_status(
    raft: &MillRaft,
    alloc_id: &AllocId,
    timeout: Duration,
) -> Result<(), DeployError> {
    let mut changes = raft.subscribe_changes();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let status = raft.read_state(|fsm| fsm.allocs.get(alloc_id).map(|a| a.status.clone()));

        match status {
            Some(s) if s.is_active() => return Ok(()),
            Some(AllocStatus::Stopped { .. }) => return Ok(()),
            Some(AllocStatus::Failed { reason }) => {
                return Err(DeployError::Node(NodeError::DeployFailed {
                    service: alloc_id.0.clone(),
                    reason,
                }));
            }
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(DeployError::HealthCheckTimeout { service: alloc_id.0.clone() });
        }

        let remaining = deadline - tokio::time::Instant::now();
        if tokio::time::timeout(remaining, changes.changed()).await.is_err() {
            return Err(DeployError::HealthCheckTimeout { service: alloc_id.0.clone() });
        }
    }
}

/// Resolve all `EnvValue`s to plain strings.
pub(crate) fn resolve_env(
    raft: &MillRaft,
    env: &HashMap<String, EnvValue>,
) -> Result<HashMap<String, String>, DeployError> {
    let mut resolved = HashMap::new();
    for (key, value) in env {
        let val = resolve_env_value(raft, value)?;
        resolved.insert(key.clone(), val);
    }
    Ok(resolved)
}

fn resolve_env_value(raft: &MillRaft, value: &EnvValue) -> Result<String, DeployError> {
    match value {
        EnvValue::Literal(s) => Ok(s.clone()),
        EnvValue::Interpolated { parts } => {
            let mut result = String::new();
            for part in parts {
                match part {
                    EnvPart::Literal(s) => result.push_str(s),
                    EnvPart::ServiceAddress { service } => {
                        let addr = raft.read_state(|fsm| {
                            fsm.list_service_allocs(service).into_iter().find_map(|a| a.address)
                        });
                        let addr = addr.ok_or_else(|| {
                            DeployError::EnvResolution(format!(
                                "no address for service '{service}'"
                            ))
                        })?;
                        result.push_str(&addr.to_string());
                    }
                    EnvPart::Secret { name } => {
                        let encrypted = raft.read_state(|fsm| fsm.get_secret(name).cloned());
                        let secret = encrypted.ok_or_else(|| {
                            DeployError::EnvResolution(format!("secret '{name}' not found"))
                        })?;
                        let plaintext = raft
                            .decrypt_secret(&secret.encrypted_value, &secret.nonce)
                            .map_err(|e| DeployError::EnvResolution(e.to_string()))?;
                        let val = String::from_utf8(plaintext).map_err(|e| {
                            DeployError::EnvResolution(format!("secret '{name}' not utf8: {e}"))
                        })?;
                        result.push_str(&val);
                    }
                }
            }
            Ok(result)
        }
    }
}

/// Ensure persistent volumes exist and are attached to the target node.
async fn create_volumes_if_needed(
    ctx: &DeployCtx,
    def: &ServiceDef,
    raft_node_id: u64,
) -> Result<(), DeployError> {
    for vol_def in &def.volumes {
        if vol_def.ephemeral {
            continue;
        }

        let vol_state =
            ctx.raft.read_state(|fsm| fsm.volumes.get(&vol_def.name).map(|v| v.state.clone()));
        let target_mill_id =
            ctx.raft.read_state(|fsm| fsm.nodes.get(&raft_node_id).map(|n| n.mill_id.clone()));

        match vol_state {
            None => {
                let driver = match &ctx.volumes {
                    Some(d) => d,
                    None => continue,
                };
                let size = vol_def.size.unwrap_or(bytesize::ByteSize::gib(10));
                let cloud_id = driver.create(&vol_def.name, size).await?;
                ctx.raft
                    .propose(Command::VolumeCreated { name: vol_def.name.clone(), cloud_id })
                    .await
                    .map_err(NodeError::from)?;
            }
            Some(mill_config::VolumeState::Attached { ref node })
                if Some(node) == target_mill_id.as_ref() =>
            {
                continue;
            }
            Some(mill_config::VolumeState::Attached { .. }) => {
                if let Some(driver) = &ctx.volumes {
                    driver.detach(&vol_def.name).await?;
                }
                ctx.raft
                    .propose(Command::VolumeDetached { name: vol_def.name.clone() })
                    .await
                    .map_err(NodeError::from)?;
            }
            Some(mill_config::VolumeState::Ready) => {}
        }

        // Attach to target node
        if let Some(driver) = &ctx.volumes {
            let iid =
                ctx.raft.read_state(|fsm| fsm.instance_id_for(raft_node_id).map(|s| s.to_owned()));
            match iid {
                Some(id) => driver.attach(&vol_def.name, &id).await?,
                None => tracing::warn!(
                    "no instance_id for node {raft_node_id}, skipping cloud attach for {}",
                    vol_def.name
                ),
            }
        }
        ctx.raft
            .propose(Command::VolumeAttached { name: vol_def.name.clone(), node: raft_node_id })
            .await
            .map_err(NodeError::from)?;
    }
    Ok(())
}

/// Broadcast `Pull` commands for all unique images in added/changed services.
///
/// Fire-and-forget: we don't wait for `PullComplete`. The `run_container()`
/// path will skip the inline pull if the image is already cached, so this
/// is purely an optimization to pre-warm containerd image stores.
async fn eager_pull(ctx: &DeployCtx, plan: &diff::DeployPlan) {
    let images: HashSet<&str> =
        plan.added.iter().chain(plan.changed.iter()).map(|(_, def)| def.image.as_str()).collect();

    for image in &images {
        let _ =
            ctx.router.broadcast(NodeCommand::Pull { image: image.to_string(), auth: None }).await;
    }

    if !images.is_empty() {
        send_json_event(&ctx.progress, "progress", serde_json::json!({"phase": "pulling"})).await;
    }
}

pub(crate) async fn send_json_event(
    tx: &mpsc::Sender<SseEvent>,
    event: &str,
    data: serde_json::Value,
) {
    let _ = tx.send(SseEvent { event: event.to_owned(), data: data.to_string() }).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primary::api::sse::SseEvent;
    use diff::DeployPlan;

    fn service(image: &str) -> ServiceDef {
        ServiceDef {
            image: image.into(),
            port: 8080,
            replicas: 1,
            resources: mill_config::Resources { cpu: 0.25, memory: bytesize::ByteSize::mib(256) },
            env: HashMap::new(),
            command: None,
            health: None,
            routes: vec![],
            volumes: vec![],
        }
    }

    fn config_with_secret(service_name: &str, secret_name: &str) -> ClusterConfig {
        let mut env = HashMap::new();
        env.insert(
            "DB_PASS".into(),
            EnvValue::Interpolated { parts: vec![EnvPart::Secret { name: secret_name.into() }] },
        );
        let mut services = HashMap::new();
        services.insert(service_name.into(), ServiceDef { env, ..service("img:1") });
        ClusterConfig { services, tasks: HashMap::new() }
    }

    fn config_with_resources(name: &str, cpu: f64, memory_mib: u64) -> ClusterConfig {
        let mut services = HashMap::new();
        services.insert(
            name.into(),
            ServiceDef {
                resources: mill_config::Resources {
                    cpu,
                    memory: bytesize::ByteSize::mib(memory_mib),
                },
                ..service("img:1")
            },
        );
        ClusterConfig { services, tasks: HashMap::new() }
    }

    fn node_resources(cpu: f64, memory_mib: u64) -> NodeResources {
        NodeResources {
            cpu_total: cpu,
            cpu_available: cpu,
            memory_total: bytesize::ByteSize::mib(memory_mib),
            memory_available: bytesize::ByteSize::mib(memory_mib),
        }
    }

    #[test]
    fn validate_secret_refs_missing() {
        let config = config_with_secret("web", "db_pass");
        let known = HashSet::new();
        let err = validate_secret_refs(&config, &known).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("db_pass"), "expected 'db_pass' in: {msg}");
    }

    #[test]
    fn validate_secret_refs_all_present() {
        let config = config_with_secret("web", "db_pass");
        let known = HashSet::from(["db_pass".to_owned()]);
        validate_secret_refs(&config, &known).unwrap();
    }

    #[test]
    fn validate_resource_capacity_exceeds() {
        let config = config_with_resources("big", 8.0, 256);
        let nodes = vec![node_resources(4.0, 8192)];
        let err = validate_resource_capacity(&config, &nodes).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("big"), "expected 'big' in: {msg}");
    }

    #[test]
    fn validate_resource_capacity_fits() {
        let config = config_with_resources("web", 2.0, 1024);
        let nodes = vec![node_resources(4.0, 8192)];
        validate_resource_capacity(&config, &nodes).unwrap();
    }

    #[test]
    fn validate_resource_capacity_no_nodes() {
        let config = config_with_resources("web", 0.25, 256);
        let nodes: Vec<NodeResources> = vec![];
        let err = validate_resource_capacity(&config, &nodes).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no ready nodes"), "expected 'no ready nodes' in: {msg}");
    }

    #[tokio::test]
    async fn eager_pull_broadcasts_unique_images() {
        let router = CommandRouter::new();
        let (tx1, mut rx1) = mpsc::channel(16);
        let (tx2, mut rx2) = mpsc::channel(16);
        router.register(NodeId("n1".into()), tx1).await;
        router.register(NodeId("n2".into()), tx2).await;

        let (_progress_tx, _progress_rx) = mpsc::channel::<SseEvent>(16);
        let plan = DeployPlan {
            added: vec![("web".into(), service("nginx:1")), ("api".into(), service("api:2"))],
            changed: vec![("cache".into(), service("nginx:1"))], // duplicate image
            removed: vec![],
            unchanged: vec![],
        };

        // We can't easily construct a full DeployCtx (needs raft), so test the
        // image collection logic directly.
        let images: HashSet<&str> = plan
            .added
            .iter()
            .chain(plan.changed.iter())
            .map(|(_, def)| def.image.as_str())
            .collect();

        // Duplicates are collapsed.
        assert_eq!(images.len(), 2);
        assert!(images.contains("nginx:1"));
        assert!(images.contains("api:2"));

        // Broadcast to router and verify all nodes receive.
        for image in &images {
            router
                .broadcast(NodeCommand::Pull { image: image.to_string(), auth: None })
                .await
                .unwrap();
        }

        // Each node should receive 2 Pull commands.
        let mut n1_images = Vec::new();
        while let Ok(cmd) = rx1.try_recv() {
            if let NodeCommand::Pull { image, .. } = cmd {
                n1_images.push(image);
            }
        }
        assert_eq!(n1_images.len(), 2);

        let mut n2_images = Vec::new();
        while let Ok(cmd) = rx2.try_recv() {
            if let NodeCommand::Pull { image, .. } = cmd {
                n2_images.push(image);
            }
        }
        assert_eq!(n2_images.len(), 2);
    }
}
