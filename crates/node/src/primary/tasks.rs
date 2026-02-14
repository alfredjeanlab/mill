use std::sync::Arc;

use mill_config::{AllocId, AllocKind, ContainerSpec, SpawnTaskRequest, SpawnTaskResponse};
use mill_raft::MillRaft;
use mill_raft::fsm::command::Command;

use super::deploy::{DeployError, resolve_env, wait_for_status};
use super::report::LiveMetrics;
use super::scheduler::{self, ScheduleRequest};
use crate::error::NodeError;
use crate::rpc::{CommandRouter, NodeCommand};

/// Spawn a task: resolve def, schedule, propose, run, wait for started.
pub async fn spawn_task(
    raft: &Arc<MillRaft>,
    router: &CommandRouter,
    live_metrics: &LiveMetrics,
    request: SpawnTaskRequest,
) -> Result<SpawnTaskResponse, DeployError> {
    // Resolve task definition.
    let (name, image, resources, mut env_defs, def_timeout, def_command) =
        match (&request.task, &request.image) {
            (Some(template), None) => {
                let def = raft.read_state(|fsm| {
                    fsm.config.as_ref().and_then(|c| c.tasks.get(template).cloned())
                });
                let def = def.ok_or_else(|| {
                    DeployError::Node(NodeError::ServiceNotFound(template.clone()))
                })?;
                let timeout = if def.timeout.is_zero() { None } else { Some(def.timeout) };
                (template.clone(), def.image, def.resources, def.env, timeout, def.command)
            }
            (None, Some(img)) => {
                let memory = request
                    .memory
                    .parse::<bytesize::ByteSize>()
                    .map_err(|e| DeployError::EnvResolution(format!("invalid memory: {e}")))?;
                let resources = mill_config::Resources { cpu: request.cpu, memory };
                let env = request
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), mill_config::EnvValue::Literal(v.clone())))
                    .collect();
                let name = format!("task-{}", rand_suffix());
                (name, img.clone(), resources, env, None, None)
            }
            _ => {
                return Err(DeployError::Node(NodeError::BadRequest(
                    "must specify exactly one of 'task' or 'image'".into(),
                )));
            }
        };

    // Compute effective timeout: request overrides template default.
    let timeout_dur = match &request.timeout {
        Some(s) => Some(
            humantime::parse_duration(s)
                .map_err(|e| DeployError::EnvResolution(format!("invalid timeout: {e}")))?,
        ),
        None => def_timeout,
    };

    // Apply env overrides from the request.
    for (k, v) in &request.env {
        env_defs.insert(k.clone(), mill_config::EnvValue::Literal(v.clone()));
    }

    let alloc_id = AllocId(format!("{name}-{}", rand_suffix()));

    // Schedule on a node.
    let node_id = raft.read_state(|fsm| {
        let nodes: Vec<_> = fsm.list_nodes().into_iter().collect();
        let allocs: Vec<_> = fsm
            .list_allocs()
            .into_iter()
            .map(|a| (a.node.clone(), a.name.as_str(), &a.kind))
            .collect();

        scheduler::schedule(
            &ScheduleRequest {
                need: &resources,
                name: &name,
                kind: &AllocKind::Task,
                volume: None,
            },
            &nodes,
            &allocs,
            &[],
            |id| fsm.node_available_resources(id),
        )
    });

    let node_id =
        node_id.ok_or(DeployError::SchedulingFailed { service: name.clone(), replica: 0 })?;

    // Find raft_id for this node.
    let raft_node_id = raft.read_state(|fsm| fsm.raft_id_for(&node_id));
    let raft_node_id =
        raft_node_id.ok_or(DeployError::SchedulingFailed { service: name.clone(), replica: 0 })?;

    // Propose AllocScheduled.
    raft.propose(Command::AllocScheduled {
        alloc_id: alloc_id.clone(),
        node_id: raft_node_id,
        name: name.clone(),
        kind: AllocKind::Task,
        address: None,
        resources: resources.clone().into(),
    })
    .await
    .map_err(NodeError::from)?;

    live_metrics.record_task_spawn(&name);

    // Resolve env.
    let env = resolve_env(raft, &env_defs)?;

    // Build container spec (tasks have no port, no volumes).
    let spec = ContainerSpec {
        alloc_id: alloc_id.clone(),
        kind: AllocKind::Task,
        timeout: timeout_dur,
        command: def_command,
        image,
        env,
        port: None,
        host_port: None,
        cpu: resources.cpu,
        memory: resources.memory,
        volumes: vec![],
    };

    // Send Run command to the target node.
    router
        .send(&node_id, NodeCommand::Run { alloc_id: alloc_id.clone(), spec, auth: None })
        .await?;

    // Wait for Running status.
    wait_for_status(raft, &alloc_id, crate::env::deploy_timeout()).await?;

    // Spawn background kill timer if timeout is set.
    if let Some(dur) = timeout_dur {
        let kill_raft = Arc::clone(raft);
        let kill_router = router.clone();
        let kill_id = alloc_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(dur).await;
            let _ = kill_task(&kill_raft, &kill_router, &kill_id).await;
        });
    }

    // Read back the address.
    let address = raft
        .read_state(|fsm| fsm.allocs.get(&alloc_id).and_then(|a| a.address.map(|a| a.to_string())));

    Ok(SpawnTaskResponse { id: alloc_id.0, node: node_id.0, status: "running".into(), address })
}

/// Kill a running task.
pub async fn kill_task(
    raft: &Arc<MillRaft>,
    router: &CommandRouter,
    alloc_id_str: &str,
) -> Result<(), DeployError> {
    let alloc_id = AllocId(alloc_id_str.to_owned());

    // Verify alloc exists and is a task, and get its node.
    let alloc_info = raft.read_state(|fsm| {
        fsm.allocs.get(&alloc_id).map(|a| (a.kind == AllocKind::Task, a.node.clone()))
    });

    match alloc_info {
        Some((true, node_id)) => {
            // Send stop command to the correct node.
            router.send(&node_id, NodeCommand::Stop { alloc_id: alloc_id.clone() }).await?;
        }
        Some((false, _)) => {
            return Err(DeployError::Node(NodeError::NotATask {
                alloc_id: alloc_id_str.to_owned(),
            }));
        }
        None => {
            return Err(DeployError::Node(NodeError::AllocNotFound(alloc_id_str.to_owned())));
        }
    }

    Ok(())
}

use crate::util::rand_suffix;
