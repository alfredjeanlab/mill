use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::runtime::Alloc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub node_count: usize,
    pub service_count: usize,
    pub task_count: usize,
    pub cpu_total: f64,
    pub cpu_available: f64,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResponse {
    pub id: String,
    pub address: String,
    pub status: String,
    pub cpu_total: f64,
    pub cpu_available: f64,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub alloc_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceResponse {
    pub name: String,
    pub image: String,
    pub replicas: ReplicaCount,
    pub allocations: Vec<AllocResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaCount {
    pub desired: u32,
    pub healthy: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllocResponse {
    pub id: String,
    pub node: String,
    pub status: String,
    pub address: Option<String>,
    pub cpu: f64,
    pub memory: u64,
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResponse {
    pub id: String,
    pub name: String,
    pub node: String,
    pub status: String,
    pub address: Option<String>,
    pub cpu: f64,
    pub memory: u64,
}

impl From<&Alloc> for TaskResponse {
    fn from(a: &Alloc) -> Self {
        TaskResponse {
            id: a.id.0.clone(),
            name: a.name.clone(),
            node: a.node.0.clone(),
            status: a.status.to_string(),
            address: a.address.map(|addr| addr.to_string()),
            cpu: a.resources.cpu,
            memory: a.resources.memory.as_u64(),
        }
    }
}

/// Request body for spawning a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnTaskRequest {
    /// Task template name — "task" per api.md, "template" accepted for compat.
    #[serde(alias = "template")]
    pub task: Option<String>,
    /// Inline image (top-level per api.md).
    pub image: Option<String>,
    #[serde(default = "crate::default_cpu")]
    pub cpu: f64,
    #[serde(default = "crate::default_memory")]
    pub memory: String,
    /// Optional environment variable overrides.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional timeout (e.g. "30m", "1h"). Task is killed after this duration.
    pub timeout: Option<String>,
}

/// Response from spawning a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnTaskResponse {
    pub id: String,
    pub node: String,
    pub status: String,
    pub address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeResponse {
    pub name: String,
    pub state: String,
    pub node: Option<String>,
    pub cloud_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretListItem {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretResponse {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretSetRequest {
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
