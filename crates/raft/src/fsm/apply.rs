use mill_config::{AllocStatus, NodeStatus, VolumeState};

use super::command::{Command, Response};
use super::state::{EncryptedSecret, FsmState, FsmVolume};

/// Apply a command to the FSM state, returning a response.
///
/// This is a pure function — no I/O. Deterministic: applying the same
/// command sequence on any node produces identical state.
pub fn apply_command(state: &mut FsmState, cmd: Command) -> Response {
    match cmd {
        Command::Deploy(config) => {
            state.config = Some(config);
            Response::Ok
        }

        Command::NodeRegister {
            id,
            mill_id,
            address,
            resources,
            wireguard_pubkey,
            instance_id,
            rpc_address,
            advertise_addr,
        } => {
            use super::state::FsmNode;
            state.nodes.insert(
                id,
                FsmNode {
                    raft_id: id,
                    mill_id,
                    address,
                    resources,
                    status: NodeStatus::Ready,
                    wireguard_pubkey,
                    instance_id,
                    rpc_address,
                    advertise_addr,
                },
            );
            Response::Ok
        }

        Command::NodeHeartbeat { id, alloc_statuses } => {
            // Update node status to Ready (it's alive)
            if let Some(node) = state.nodes.get_mut(&id) {
                if node.status == NodeStatus::Down {
                    node.status = NodeStatus::Ready;
                }
            } else {
                return Response::Error(format!("unknown node {id}"));
            }

            // Update allocation statuses from the secondary's heartbeat.
            // Guard: never downgrade a Healthy alloc to Running. The deploy
            // coordinator marks an alloc Healthy after the TCP/HTTP probe, but
            // the secondary only ever reports Running (it doesn't perform health
            // checks). A heartbeat must not silently undo the Healthy promotion.
            // Terminal states (Stopped/Failed) are always accepted so the FSM
            // reflects container exits even for previously-healthy allocs.
            for (alloc_id, status) in alloc_statuses {
                if let Some(alloc) = state.allocs.get_mut(&alloc_id) {
                    let is_downgrade = matches!(alloc.status, AllocStatus::Healthy)
                        && matches!(status, AllocStatus::Running);
                    if !is_downgrade {
                        alloc.status = status;
                    }
                }
            }

            Response::Ok
        }

        Command::NodeDown { id } => {
            if let Some(node) = state.nodes.get_mut(&id) {
                node.status = NodeStatus::Down;
                Response::Ok
            } else {
                Response::Error(format!("unknown node {id}"))
            }
        }

        Command::AllocScheduled { alloc_id, node_id, name, kind, address, resources } => {
            let node = match state.nodes.get(&node_id) {
                Some(n) => n,
                None => return Response::Error(format!("unknown node {node_id}")),
            };
            let alloc = FsmState::make_alloc(
                alloc_id,
                node.mill_id.clone(),
                name,
                kind,
                address,
                resources,
            );
            state.allocs.insert(alloc.id.clone(), alloc);
            Response::Ok
        }

        Command::AllocStatus { alloc_id, status } => {
            if let Some(alloc) = state.allocs.get_mut(&alloc_id) {
                alloc.status = status;
                Response::Ok
            } else {
                Response::Error(format!("unknown alloc {}", alloc_id.0))
            }
        }

        Command::AllocRunning { alloc_id, address, started_at } => {
            if let Some(alloc) = state.allocs.get_mut(&alloc_id) {
                // Guard: only transition from Pulling/Starting. Duplicate RunStarted
                // reports (e.g. from reconcile after restart) are safe no-ops.
                if matches!(alloc.status, AllocStatus::Pulling | AllocStatus::Starting) {
                    alloc.status = AllocStatus::Running;
                    alloc.address = address;
                    alloc.started_at = started_at;
                }
                Response::Ok
            } else {
                Response::Error(format!("unknown alloc {}", alloc_id.0))
            }
        }

        Command::SecretSet { name, encrypted_value, nonce } => {
            state.secrets.insert(name, EncryptedSecret { encrypted_value, nonce });
            Response::Ok
        }

        Command::SecretDelete { name } => {
            if state.secrets.remove(&name).is_some() {
                Response::Ok
            } else {
                Response::Error(format!("unknown secret {name}"))
            }
        }

        Command::VolumeCreated { name, cloud_id } => {
            state.volumes.insert(name, FsmVolume { cloud_id, state: VolumeState::Ready });
            Response::Ok
        }

        Command::VolumeAttached { name, node } => {
            let node_mill_id = match state.nodes.get(&node) {
                Some(n) => n.mill_id.clone(),
                None => return Response::Error(format!("unknown node {node}")),
            };
            if let Some(vol) = state.volumes.get_mut(&name) {
                vol.state = VolumeState::Attached { node: node_mill_id };
                Response::Ok
            } else {
                Response::Error(format!("unknown volume {name}"))
            }
        }

        Command::VolumeDetached { name } => {
            if let Some(vol) = state.volumes.get_mut(&name) {
                vol.state = VolumeState::Ready;
                Response::Ok
            } else {
                Response::Error(format!("unknown volume {name}"))
            }
        }

        Command::VolumeDestroyed { name } => {
            state.volumes.remove(&name);
            Response::Ok
        }

        Command::NodeDrain { id } => {
            if let Some(node) = state.nodes.get_mut(&id) {
                node.status = NodeStatus::Draining;
                Response::Ok
            } else {
                Response::Error(format!("unknown node {id}"))
            }
        }

        Command::NodeRemove { id } => {
            // Reject if the node has any active (non-stopped, non-failed) allocs.
            if let Some(node) = state.nodes.get(&id) {
                let has_active = state.allocs.values().any(|a| {
                    a.node == node.mill_id
                        && !matches!(
                            a.status,
                            AllocStatus::Stopped { .. } | AllocStatus::Failed { .. }
                        )
                });
                if has_active {
                    return Response::Error(format!("node {id} still has active allocations"));
                }
                // Remove stopped/failed allocs belonging to this node.
                let mill_id = node.mill_id.clone();
                state.allocs.retain(|_, a| a.node != mill_id);
                state.nodes.remove(&id);
                Response::Ok
            } else {
                Response::Error(format!("unknown node {id}"))
            }
        }
    }
}
