use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::SystemTime;

use mill_config::{
    AllocId, AllocKind, AllocResources, AllocStatus, ClusterConfig, NodeId, NodeResources,
};
use serde::{Deserialize, Serialize};

/// A command to be applied to the FSM through Raft consensus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Replace the desired cluster configuration.
    Deploy(ClusterConfig),

    /// Register a new node in the cluster.
    NodeRegister {
        id: u64,
        mill_id: NodeId,
        address: SocketAddr,
        resources: NodeResources,
        wireguard_pubkey: Option<String>,
        instance_id: Option<String>,
        #[serde(default)]
        rpc_address: Option<SocketAddr>,
        #[serde(default)]
        advertise_addr: Option<String>,
    },

    /// Update node liveness and allocation statuses.
    NodeHeartbeat { id: u64, alloc_statuses: HashMap<AllocId, AllocStatus> },

    /// Mark a node as down.
    NodeDown { id: u64 },

    /// Record an allocation placement decision.
    AllocScheduled {
        alloc_id: AllocId,
        node_id: u64,
        name: String,
        kind: AllocKind,
        address: Option<SocketAddr>,
        resources: AllocResources,
    },

    /// Update allocation lifecycle status.
    AllocStatus { alloc_id: AllocId, status: AllocStatus },

    /// Mark an allocation as running with its resolved address and start time.
    AllocRunning { alloc_id: AllocId, address: Option<SocketAddr>, started_at: Option<SystemTime> },

    /// Store an encrypted secret.
    SecretSet { name: String, encrypted_value: Vec<u8>, nonce: Vec<u8> },

    /// Remove a secret.
    SecretDelete { name: String },

    /// Record a newly created persistent volume.
    VolumeCreated { name: String, cloud_id: String },

    /// Record that a volume has been attached to a node.
    VolumeAttached { name: String, node: u64 },

    /// Mark a volume as detached.
    VolumeDetached { name: String },

    /// Remove a volume from state.
    VolumeDestroyed { name: String },

    /// Mark a node as draining (unschedulable, allocs being migrated).
    NodeDrain { id: u64 },

    /// Remove a node from the cluster. Rejected if active allocs remain.
    NodeRemove { id: u64 },
}

/// Response from applying a command to the FSM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error(String),
}
