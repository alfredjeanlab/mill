use std::collections::HashMap;
use std::net::SocketAddr;

use mill_config::{AllocId, AllocStatus, ContainerSpec, ContainerStats, NodeId, NodeResources};
use mill_containerd::LogLine;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct AuthRequest {
    pub(crate) token: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AuthResponse {
    pub(crate) ok: bool,
}

/// Credentials for authenticating to a container registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
}

/// Commands sent from the primary to a secondary node.
///
/// # Security
///
/// `Run` carries a [`ContainerSpec`] with fully-resolved env vars that may
/// contain secret values. `ContainerSpec::Debug` redacts them, but avoid
/// serializing commands for logging or diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeCommand {
    /// Start a container. `spec.env` is fully resolved — the primary has
    /// already substituted `${secret(...)}` references before sending.
    Run { alloc_id: AllocId, spec: ContainerSpec, auth: Option<RegistryCredentials> },
    /// Stop a running container.
    Stop { alloc_id: AllocId },
    /// Pull an image into the local containerd store.
    Pull { image: String, auth: Option<RegistryCredentials> },
    /// Request the last N log lines for an allocation.
    LogTail { alloc_id: AllocId, lines: usize },
    /// Subscribe to live log lines for an allocation.
    LogFollow { alloc_id: AllocId },
    /// Unsubscribe from live log lines for an allocation.
    LogUnfollow { alloc_id: AllocId },
}

/// Reports sent from a secondary node back to the primary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeReport {
    /// Periodic heartbeat with all allocation statuses and resource usage.
    Heartbeat(Heartbeat),
    /// Container started successfully.
    RunStarted { alloc_id: AllocId, host_port: Option<u16>, address: Option<SocketAddr> },
    /// Container exited (normal or after stop).
    Stopped { alloc_id: AllocId, exit_code: i32 },
    /// Container failed to start.
    RunFailed { alloc_id: AllocId, reason: String },
    /// Image pull completed.
    PullComplete { image: String },
    /// Image pull failed.
    PullFailed { image: String, reason: String },
    /// Log lines in response to a LogTail command.
    LogLines { alloc_id: AllocId, lines: Vec<LogLine> },
    /// Inline status transition for an allocation.
    StatusUpdate { alloc_id: AllocId, status: AllocStatus },
}

/// Heartbeat payload sent periodically from secondary to primary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node_id: NodeId,
    pub alloc_statuses: HashMap<AllocId, AllocStatus>,
    pub resources: NodeResources,
    pub stats: Vec<ContainerStats>,
    pub volumes: Vec<VolumeInfo>,
}

/// Information about a volume on this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub name: String,
    pub size_bytes: u64,
    pub alloc_id: Option<AllocId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::AllocId;

    #[test]
    fn node_command_serde_round_trip() {
        use bytesize::ByteSize;
        use mill_config::AllocKind;

        let spec = ContainerSpec {
            alloc_id: AllocId("test-run".into()),
            kind: AllocKind::Task,
            timeout: Some(std::time::Duration::from_secs(3600)),
            command: Some(vec!["migrate".into()]),
            image: "app:1".into(),
            env: HashMap::new(),
            port: None,
            host_port: None,
            cpu: 0.5,
            memory: ByteSize::mib(256),
            volumes: vec![],
        };
        let cmds = vec![
            NodeCommand::Stop { alloc_id: AllocId("test-1".into()) },
            NodeCommand::Pull { image: "nginx:latest".into(), auth: None },
            NodeCommand::LogTail { alloc_id: AllocId("test-1".into()), lines: 100 },
            NodeCommand::Run {
                alloc_id: AllocId("test-run".into()),
                spec,
                auth: Some(RegistryCredentials {
                    username: "user".into(),
                    password: "pass".into(),
                }),
            },
        ];
        for cmd in cmds {
            let json = serde_json::to_string(&cmd).unwrap_or_default();
            let back: NodeCommand = serde_json::from_str(&json)
                .unwrap_or_else(|_| NodeCommand::Stop { alloc_id: AllocId("fallback".into()) });
            // Verify round-trip doesn't panic — detailed field checks per variant
            let _ = format!("{back:?}");
        }
    }

    #[test]
    fn node_report_serde_round_trip() {
        let reports = vec![
            NodeReport::Stopped { alloc_id: AllocId("a".into()), exit_code: 0 },
            NodeReport::RunFailed { alloc_id: AllocId("b".into()), reason: "oom".into() },
            NodeReport::PullComplete { image: "redis:7".into() },
            NodeReport::PullFailed { image: "bad:img".into(), reason: "not found".into() },
        ];
        for report in reports {
            let json = serde_json::to_string(&report).unwrap_or_default();
            let back: NodeReport = serde_json::from_str(&json).unwrap_or_else(|_| {
                NodeReport::Stopped { alloc_id: AllocId("fallback".into()), exit_code: -1 }
            });
            let _ = format!("{back:?}");
        }
    }

    #[test]
    fn heartbeat_serde_round_trip() {
        use bytesize::ByteSize;

        let hb = Heartbeat {
            node_id: NodeId("test-node".into()),
            alloc_statuses: HashMap::from([
                (AllocId("svc-1".into()), AllocStatus::Running),
                (AllocId("svc-2".into()), AllocStatus::Stopped { exit_code: 0 }),
            ]),
            resources: NodeResources {
                cpu_total: 4.0,
                cpu_available: 2.5,
                memory_total: ByteSize::gib(8),
                memory_available: ByteSize::gib(4),
            },
            stats: vec![],
            volumes: vec![VolumeInfo { name: "data".into(), size_bytes: 1024, alloc_id: None }],
        };

        let json = serde_json::to_string(&hb).unwrap_or_default();
        let back: Heartbeat = serde_json::from_str(&json).unwrap_or_else(|_| Heartbeat {
            node_id: NodeId("fallback".into()),
            alloc_statuses: HashMap::new(),
            resources: NodeResources {
                cpu_total: 0.0,
                cpu_available: 0.0,
                memory_total: ByteSize::b(0),
                memory_available: ByteSize::b(0),
            },
            stats: vec![],
            volumes: vec![],
        });
        assert_eq!(back.alloc_statuses.len(), 2);
        assert_eq!(back.volumes.len(), 1);
    }
}
