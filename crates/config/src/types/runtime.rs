use std::fmt;
use std::net::SocketAddr;
use std::time::SystemTime;

use bytesize::ByteSize;
use serde::{Deserialize, Serialize};

use super::resolved::Resources;

/// Unique identifier for a node in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

/// Unique identifier for an allocation (running service replica or task).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AllocId(pub String);

/// Resources available on a node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeResources {
    pub cpu_total: f64,
    pub cpu_available: f64,
    #[serde(with = "super::bytesize_serde")]
    pub memory_total: ByteSize,
    #[serde(with = "super::bytesize_serde")]
    pub memory_available: ByteSize,
}

/// Status of a cluster node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeStatus {
    Ready,
    Draining,
    Down,
}

/// An allocation — a running instance of a service or task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Alloc {
    pub id: AllocId,
    pub node: NodeId,
    pub name: String,
    pub kind: AllocKind,
    pub status: AllocStatus,
    pub address: Option<SocketAddr>,
    pub resources: AllocResources,
    pub started_at: Option<SystemTime>,
}

/// Allocated resources for a running container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllocResources {
    pub cpu: f64,
    #[serde(with = "super::bytesize_serde")]
    pub memory: ByteSize,
}

impl From<Resources> for AllocResources {
    fn from(r: Resources) -> Self {
        AllocResources { cpu: r.cpu, memory: r.memory }
    }
}

/// Whether an allocation is a long-running service or an ephemeral task.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AllocKind {
    #[default]
    Service,
    Task,
}

/// Lifecycle status of an allocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AllocStatus {
    Pulling,
    Starting,
    Running,
    Healthy,
    Stopped { exit_code: i32 },
    Failed { reason: String },
}

impl AllocStatus {
    /// Running or Healthy — actively serving traffic.
    pub fn is_active(&self) -> bool {
        matches!(self, AllocStatus::Running | AllocStatus::Healthy)
    }

    /// Stopped or Failed — will not run again.
    pub fn is_terminal(&self) -> bool {
        matches!(self, AllocStatus::Stopped { .. } | AllocStatus::Failed { .. })
    }
}

/// State of a persistent volume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VolumeState {
    Ready,
    Attached { node: NodeId },
}

impl fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeStatus::Ready => f.write_str("ready"),
            NodeStatus::Draining => f.write_str("draining"),
            NodeStatus::Down => f.write_str("down"),
        }
    }
}

impl fmt::Display for AllocKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocKind::Service => f.write_str("service"),
            AllocKind::Task => f.write_str("task"),
        }
    }
}

impl fmt::Display for AllocStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocStatus::Pulling => f.write_str("pulling"),
            AllocStatus::Starting => f.write_str("starting"),
            AllocStatus::Running => f.write_str("running"),
            AllocStatus::Healthy => f.write_str("healthy"),
            AllocStatus::Stopped { .. } => f.write_str("stopped"),
            AllocStatus::Failed { .. } => f.write_str("failed"),
        }
    }
}

impl fmt::Display for VolumeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumeState::Ready => f.write_str("ready"),
            VolumeState::Attached { node } => write!(f, "attached ({})", node.0),
        }
    }
}
