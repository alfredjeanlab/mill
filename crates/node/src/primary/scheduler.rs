use bytesize::ByteSize;
use mill_config::{AllocKind, NodeId, NodeResources, NodeStatus};
use mill_raft::fsm::{FsmNode, FsmVolume};

/// Input to the placement scheduler.
pub struct ScheduleRequest<'a> {
    /// Resources needed by this allocation.
    pub need: &'a mill_config::Resources,
    /// Name of the service or task.
    pub name: &'a str,
    /// Whether this is a service or task.
    pub kind: &'a AllocKind,
    /// Volume name, if the allocation needs a persistent volume.
    pub volume: Option<&'a str>,
}

/// Pure placement function. No I/O.
///
/// Algorithm:
/// 1. Volume affinity — if a volume is attached to a node, prefer that node.
/// 2. Filter — only nodes that are `Ready` with enough available resources.
/// 3. Spread — for services, prefer nodes without existing allocs of the same name.
/// 4. Bin-pack — among remaining candidates, pick the node with least available resources.
pub fn schedule<F>(
    request: &ScheduleRequest<'_>,
    nodes: &[&FsmNode],
    allocs: &[(NodeId, &str, &AllocKind)],
    volumes: &[(&str, &FsmVolume)],
    available_fn: F,
) -> Option<NodeId>
where
    F: Fn(u64) -> Option<NodeResources>,
{
    // Step 1: Volume affinity
    if let Some(vol_name) = request.volume
        && let Some((_, vol)) = volumes.iter().find(|(name, _)| *name == vol_name)
        && let mill_config::VolumeState::Attached { node } = &vol.state
    {
        // Volume is attached — must schedule here if resources fit
        let fits = nodes.iter().any(|n| {
            n.mill_id == *node
                && n.status == NodeStatus::Ready
                && has_capacity(&available_fn(n.raft_id), request.need)
        });
        return if fits { Some(node.clone()) } else { None };
    }

    // Step 2: Filter to ready nodes with sufficient capacity
    let mut candidates: Vec<(&FsmNode, NodeResources)> = nodes
        .iter()
        .filter(|n| n.status == NodeStatus::Ready)
        .filter_map(|n| {
            let avail = available_fn(n.raft_id)?;
            if has_capacity(&Some(avail.clone()), request.need) { Some((*n, avail)) } else { None }
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Step 3: Spread — for services, prefer nodes without existing allocs of the same name
    if *request.kind == AllocKind::Service {
        let nodes_with_same: std::collections::HashSet<&NodeId> = allocs
            .iter()
            .filter(|(_, name, kind)| *name == request.name && **kind == AllocKind::Service)
            .map(|(node_id, _, _)| node_id)
            .collect();

        let spread: Vec<(&FsmNode, NodeResources)> = candidates
            .iter()
            .filter(|(n, _)| !nodes_with_same.contains(&n.mill_id))
            .cloned()
            .collect();

        if !spread.is_empty() {
            candidates = spread;
        }
    }

    // Step 4: Bin-pack — pick node with least available resources
    candidates
        .into_iter()
        .min_by(|(_, a), (_, b)| {
            let score_a = bin_pack_score(a);
            let score_b = bin_pack_score(b);
            score_a.partial_cmp(&score_b).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(n, _)| n.mill_id.clone())
}

fn has_capacity(available: &Option<NodeResources>, need: &mill_config::Resources) -> bool {
    match available {
        Some(avail) => avail.cpu_available >= need.cpu && avail.memory_available >= need.memory,
        None => false,
    }
}

/// Lower score = more packed = preferred for bin-packing.
fn bin_pack_score(avail: &NodeResources) -> f64 {
    avail.cpu_available
        + (avail.memory_available.as_u64() as f64 / ByteSize::gib(1).as_u64() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::{Resources, VolumeState};
    use std::net::SocketAddr;

    fn node(raft_id: u64, mill_id: &str, cpu: f64, mem_gb: u64) -> FsmNode {
        FsmNode {
            raft_id,
            mill_id: NodeId(mill_id.into()),
            address: SocketAddr::from(([10, 0, 0, raft_id as u8], 4400)),
            resources: NodeResources {
                cpu_total: cpu,
                cpu_available: cpu,
                memory_total: ByteSize::gib(mem_gb),
                memory_available: ByteSize::gib(mem_gb),
            },
            status: NodeStatus::Ready,
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        }
    }

    fn avail(cpu: f64, mem_gb: u64) -> NodeResources {
        NodeResources {
            cpu_total: cpu,
            cpu_available: cpu,
            memory_total: ByteSize::gib(mem_gb),
            memory_available: ByteSize::gib(mem_gb),
        }
    }

    fn need(cpu: f64, mem_gb: u64) -> Resources {
        Resources { cpu, memory: ByteSize::gib(mem_gb) }
    }

    #[test]
    fn volume_affinity_places_on_attached_node() {
        let n1 = node(1, "node-1", 4.0, 8);
        let n2 = node(2, "node-2", 4.0, 8);
        let nodes = vec![&n1, &n2];
        let need = need(1.0, 1);
        let vol = FsmVolume {
            cloud_id: "vol-123".into(),
            state: VolumeState::Attached { node: NodeId("node-2".into()) },
        };
        let volumes = vec![("data", &vol)];

        let result = schedule(
            &ScheduleRequest {
                need: &need,
                name: "web",
                kind: &AllocKind::Service,
                volume: Some("data"),
            },
            &nodes,
            &[],
            &volumes,
            |id| match id {
                1 => Some(avail(4.0, 8)),
                2 => Some(avail(4.0, 8)),
                _ => None,
            },
        );

        assert_eq!(result, Some(NodeId("node-2".into())));
    }

    #[test]
    fn volume_affinity_returns_none_if_node_full() {
        let n1 = node(1, "node-1", 4.0, 8);
        let n2 = node(2, "node-2", 4.0, 8);
        let nodes = vec![&n1, &n2];
        let need = need(8.0, 16); // More than available
        let vol = FsmVolume {
            cloud_id: "vol-123".into(),
            state: VolumeState::Attached { node: NodeId("node-2".into()) },
        };
        let volumes = vec![("data", &vol)];

        let result = schedule(
            &ScheduleRequest {
                need: &need,
                name: "db",
                kind: &AllocKind::Service,
                volume: Some("data"),
            },
            &nodes,
            &[],
            &volumes,
            |id| match id {
                1 => Some(avail(4.0, 8)),
                2 => Some(avail(4.0, 8)),
                _ => None,
            },
        );

        assert_eq!(result, None);
    }

    #[test]
    fn spread_prefers_node_without_same_service() {
        let n1 = node(1, "node-1", 4.0, 8);
        let n2 = node(2, "node-2", 4.0, 8);
        let nodes = vec![&n1, &n2];
        let need = need(1.0, 1);
        // node-1 already has a "web" service alloc
        let allocs = vec![(NodeId("node-1".into()), "web", &AllocKind::Service)];

        let result = schedule(
            &ScheduleRequest { need: &need, name: "web", kind: &AllocKind::Service, volume: None },
            &nodes,
            &allocs,
            &[],
            |id| match id {
                1 => Some(avail(3.0, 7)),
                2 => Some(avail(3.0, 7)),
                _ => None,
            },
        );

        assert_eq!(result, Some(NodeId("node-2".into())));
    }

    #[test]
    fn bin_pack_picks_most_packed_node() {
        let n1 = node(1, "node-1", 8.0, 16);
        let n2 = node(2, "node-2", 8.0, 16);
        let nodes = vec![&n1, &n2];
        let need = need(1.0, 1);

        let result = schedule(
            &ScheduleRequest { need: &need, name: "api", kind: &AllocKind::Service, volume: None },
            &nodes,
            &[],
            &[],
            |id| match id {
                // node-1 has less available (more packed)
                1 => Some(avail(2.0, 2)),
                // node-2 has more available
                2 => Some(avail(6.0, 12)),
                _ => None,
            },
        );

        assert_eq!(result, Some(NodeId("node-1".into())));
    }

    #[test]
    fn no_fit_returns_none() {
        let n1 = node(1, "node-1", 2.0, 4);
        let nodes = vec![&n1];
        let need = need(4.0, 8); // More than available

        let result = schedule(
            &ScheduleRequest { need: &need, name: "big", kind: &AllocKind::Service, volume: None },
            &nodes,
            &[],
            &[],
            |_| Some(avail(2.0, 4)),
        );

        assert_eq!(result, None);
    }

    #[test]
    fn tasks_skip_spread() {
        let n1 = node(1, "node-1", 4.0, 8);
        let n2 = node(2, "node-2", 4.0, 8);
        let nodes = vec![&n1, &n2];
        let need = need(1.0, 1);
        // node-1 already has a "migrate" task, but tasks skip spread
        let allocs = vec![(NodeId("node-1".into()), "migrate", &AllocKind::Task)];

        let result = schedule(
            &ScheduleRequest { need: &need, name: "migrate", kind: &AllocKind::Task, volume: None },
            &nodes,
            &allocs,
            &[],
            |id| match id {
                // node-1 is more packed — should be preferred via bin-pack
                1 => Some(avail(2.0, 4)),
                2 => Some(avail(4.0, 8)),
                _ => None,
            },
        );

        // Should pick node-1 (more packed) since spread is skipped for tasks
        assert_eq!(result, Some(NodeId("node-1".into())));
    }
}
