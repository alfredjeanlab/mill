use std::collections::HashMap;
use std::net::SocketAddr;

use bytesize::ByteSize;
use mill_config::{
    Alloc, AllocId, AllocKind, AllocResources, AllocStatus, ClusterConfig, NodeId, NodeResources,
    NodeStatus, VolumeState,
};
use serde::{Deserialize, Serialize};

/// The complete FSM state — all cluster state lives here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsmState {
    pub config: Option<ClusterConfig>,
    pub nodes: HashMap<u64, FsmNode>,
    pub allocs: HashMap<AllocId, Alloc>,
    pub secrets: HashMap<String, EncryptedSecret>,
    pub volumes: HashMap<String, FsmVolume>,
}

/// A node as tracked by the FSM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsmNode {
    pub raft_id: u64,
    pub mill_id: NodeId,
    pub address: SocketAddr,
    pub resources: NodeResources,
    pub status: NodeStatus,
    #[serde(default)]
    pub wireguard_pubkey: Option<String>,
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub rpc_address: Option<SocketAddr>,
    /// The external/advertise address of the node (used as WireGuard endpoint).
    #[serde(default)]
    pub advertise_addr: Option<String>,
}

/// An encrypted secret stored in the FSM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedSecret {
    pub encrypted_value: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// A persistent volume tracked by the FSM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsmVolume {
    pub cloud_id: String,
    pub state: VolumeState,
}

impl FsmState {
    /// List all registered nodes.
    pub fn list_nodes(&self) -> Vec<&FsmNode> {
        self.nodes.values().collect()
    }

    /// List all allocations.
    pub fn list_allocs(&self) -> Vec<&Alloc> {
        self.allocs.values().collect()
    }

    /// List allocations for a specific service or task by name.
    pub fn list_service_allocs(&self, name: &str) -> Vec<&Alloc> {
        self.allocs.values().filter(|a| a.name == name).collect()
    }

    /// Get an encrypted secret by name.
    pub fn get_secret(&self, name: &str) -> Option<&EncryptedSecret> {
        self.secrets.get(name)
    }

    /// List all secret names (without values).
    pub fn list_secret_names(&self) -> Vec<&str> {
        self.secrets.keys().map(|s| s.as_str()).collect()
    }

    /// List all volumes.
    pub fn list_volumes(&self) -> Vec<(&str, &FsmVolume)> {
        self.volumes.iter().map(|(name, vol)| (name.as_str(), vol)).collect()
    }

    /// Compute available resources for a given node by subtracting allocated resources.
    pub fn node_available_resources(&self, node_id: u64) -> Option<NodeResources> {
        let node = self.nodes.get(&node_id)?;

        let mut cpu_used = 0.0;
        let mut mem_used: u64 = 0;

        for alloc in self.allocs.values() {
            // Match alloc to node via mill_id
            if alloc.node == node.mill_id {
                // Only count active allocations
                if alloc.status.is_terminal() {
                    continue;
                }
                cpu_used += alloc.resources.cpu;
                mem_used += alloc.resources.memory.as_u64();
            }
        }

        Some(NodeResources {
            cpu_total: node.resources.cpu_total,
            cpu_available: node.resources.cpu_total - cpu_used,
            memory_total: node.resources.memory_total,
            memory_available: ByteSize::b(
                node.resources.memory_total.as_u64().saturating_sub(mem_used),
            ),
        })
    }

    /// Look up a node's cloud instance ID by raft node ID.
    pub fn instance_id_for(&self, raft_id: u64) -> Option<&str> {
        self.nodes.get(&raft_id).and_then(|n| n.instance_id.as_deref())
    }

    /// Look up the raft node ID for a given mill NodeId.
    pub fn raft_id_for(&self, mill_id: &NodeId) -> Option<u64> {
        self.nodes.values().find(|n| &n.mill_id == mill_id).map(|n| n.raft_id)
    }

    /// Build an Alloc from FSM fields. Used internally during command application.
    pub(crate) fn make_alloc(
        alloc_id: AllocId,
        node_mill_id: NodeId,
        name: String,
        kind: AllocKind,
        address: Option<SocketAddr>,
        resources: AllocResources,
    ) -> Alloc {
        Alloc {
            id: alloc_id,
            node: node_mill_id,
            name,
            kind,
            status: AllocStatus::Pulling,
            address,
            resources,
            started_at: None,
        }
    }
}
