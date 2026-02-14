use std::sync::Arc;
use std::time::Instant;

use mill_config::{AllocKind, AllocStatus, NodeId, VolumeState};
use mill_raft::MillRaft;
use mill_raft::fsm::command::{Command, Response as RaftResponse};
use tokio::sync::mpsc;

use super::api::sse::SseEvent;
use super::deploy::{DeployCtx, DeployError, send_json_event, start_alloc};
use super::health::HealthPoller;
use crate::error::NodeError;
use crate::rpc::{CommandRouter, NodeCommand};
use crate::storage::Volumes;

/// Drain a node: mark unschedulable, migrate services, stop tasks.
///
/// 1. Proposes `NodeDrain` to Raft (marks the node as Draining).
/// 2. For each active alloc on the node:
///    - Services: schedule a replacement on another node, then stop old.
///    - Tasks: just stop (tasks don't migrate).
/// 3. Reports progress via SSE events.
pub async fn drain_node(
    raft: Arc<MillRaft>,
    router: CommandRouter,
    volumes: Option<Arc<dyn Volumes>>,
    health: Arc<HealthPoller>,
    node_id: u64,
    progress: mpsc::Sender<SseEvent>,
) -> Result<(), DeployError> {
    let started = Instant::now();

    // Step 1: Mark node as draining.
    let resp = raft.propose(Command::NodeDrain { id: node_id }).await.map_err(NodeError::from)?;
    if let RaftResponse::Error(e) = resp {
        return Err(DeployError::Node(NodeError::RaftRejected(e)));
    }
    send_json_event(&progress, "progress", serde_json::json!({"phase": "started"})).await;

    // Step 1b: Detach volumes attached to this node so the scheduler can
    // re-attach them to whichever node is chosen for replacement allocs.
    let mill_id = raft.read_state(|fsm| {
        fsm.nodes.get(&node_id).map(|n| n.mill_id.clone()).unwrap_or(NodeId(String::new()))
    });
    let attached_volumes = raft.read_state(|fsm| {
        fsm.list_volumes()
            .into_iter()
            .filter(
                |(_, vol)| matches!(&vol.state, VolumeState::Attached { node } if *node == mill_id),
            )
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>()
    });
    for vol_name in &attached_volumes {
        if let Some(driver) = &volumes
            && let Err(e) = driver.detach(vol_name).await
        {
            tracing::warn!("drain: failed to detach volume {vol_name}: {e}");
        }
        let _ = raft.propose(Command::VolumeDetached { name: vol_name.clone() }).await;
    }

    // Step 2: Collect active allocs on this node (with their mill_id for routing).
    let (allocs, mill_id) = raft.read_state(|fsm| {
        let node = match fsm.nodes.get(&node_id) {
            Some(n) => n,
            None => return (vec![], NodeId(String::new())),
        };
        let mill_id = node.mill_id.clone();
        let allocs = fsm
            .allocs
            .values()
            .filter(|a| {
                a.node == mill_id
                    && !matches!(a.status, AllocStatus::Stopped { .. } | AllocStatus::Failed { .. })
            })
            .map(|a| (a.id.clone(), a.name.clone(), a.kind.clone()))
            .collect();
        (allocs, mill_id)
    });

    let ctx = DeployCtx { raft, router, volumes, health, progress: progress.clone(), started };

    for (alloc_id, name, kind) in allocs {
        // Services get a replacement scheduled on another node before stopping.
        if kind == AllocKind::Service {
            let def = ctx
                .raft
                .read_state(|fsm| fsm.config.as_ref().and_then(|c| c.services.get(&name).cloned()));
            if let Some(def) = def {
                let new_id = mill_config::AllocId(format!("{name}-{}", crate::util::rand_suffix()));
                match start_alloc(&ctx, &name, &def, &new_id, 0).await {
                    Ok(()) => {
                        send_json_event(
                            &ctx.progress,
                            "progress",
                            serde_json::json!({
                                "service": name,
                                "phase": "migrated",
                            }),
                        )
                        .await;
                    }
                    Err(e) => {
                        send_json_event(
                            &ctx.progress,
                            "failed",
                            serde_json::json!({
                                "service": name,
                                "reason": e.to_string(),
                            }),
                        )
                        .await;
                        continue; // Don't stop the old alloc — it's all we have
                    }
                }
            }
        }

        // Stop the alloc on the draining node (both services and tasks).
        let _ = ctx.router.send(&mill_id, NodeCommand::Stop { alloc_id: alloc_id.clone() }).await;
        send_json_event(
            &ctx.progress,
            "progress",
            serde_json::json!({"service": name, "phase": "stopped"}),
        )
        .await;
    }

    let elapsed = started.elapsed().as_secs_f64();
    send_json_event(&ctx.progress, "done", serde_json::json!({"elapsed": elapsed})).await;
    Ok(())
}

/// Remove a node from the cluster. Raft rejects if active allocs remain.
pub async fn remove_node(raft: &Arc<MillRaft>, node_id: u64) -> Result<(), NodeError> {
    let resp = raft.propose(Command::NodeRemove { id: node_id }).await?;
    match resp {
        RaftResponse::Ok => Ok(()),
        RaftResponse::Error(e) => Err(NodeError::RaftRejected(e)),
    }
}
