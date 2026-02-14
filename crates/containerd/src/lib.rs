#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod cgroups;
mod container;
mod error;
mod image;
pub mod logs;
mod runtime;
mod spec;
mod stats;

pub use container::ContainerHandle;
pub use error::{ContainerdError, Result};
pub use image::RegistryAuth;
pub use logs::{LogLine, LogStream, cleanup_fifos, reconnect_fifos, setup_fifos};
pub use runtime::{BoxFuture, Containers};

use mill_config::{AllocId, AllocKind, ContainerSpec, ContainerStats, ContainerVolume, DataDir};
use std::time::Duration;
use tonic::transport::Channel;

/// Information recovered from containerd labels for a crash-recovered container.
#[derive(Debug, Clone)]
pub struct RecoveredContainer {
    pub alloc_id: AllocId,
    pub kind: AllocKind,
    pub timeout: Option<Duration>,
    pub command: Option<Vec<String>>,
    pub host_port: Option<u16>,
    pub cpu: f64,
    pub memory: u64,
    pub image: String,
    pub volumes: Vec<ContainerVolume>,
}

/// Facade over the containerd gRPC API for mill container management.
///
/// All methods clone the gRPC channel internally — tonic channels are cheap
/// to clone and can be shared across concurrent operations.
#[derive(Clone)]
pub struct Containerd {
    channel: Channel,
    data_dir: DataDir,
}

impl Containerd {
    /// Connect to a containerd socket.
    pub async fn connect(socket_path: &str, data_dir: impl Into<DataDir>) -> Result<Self> {
        let channel = containerd_client::connect(socket_path)
            .await
            .map_err(|e| ContainerdError::Connect { path: socket_path.to_string(), source: e })?;
        Ok(Self { channel, data_dir: data_dir.into() })
    }

    /// The data directory.
    pub fn data_dir(&self) -> &DataDir {
        &self.data_dir
    }

    /// Check if an image is already cached in the local containerd store.
    pub async fn is_image_cached(&self, image: &str) -> Result<bool> {
        image::is_cached(self.channel.clone(), image).await
    }

    /// Pull an image from an OCI registry.
    pub async fn pull_image(&self, image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
        image::pull(self.channel.clone(), image, auth).await
    }

    /// Create and start a container, returning a handle for waiting and log access.
    pub async fn run(&self, spec: &ContainerSpec) -> Result<ContainerHandle> {
        container::create_and_start(self.channel.clone(), &self.data_dir, spec).await
    }

    /// Stop a running container (SIGTERM -> 10s timeout -> SIGKILL).
    /// Returns the exit code.
    pub async fn stop(&self, alloc_id: &AllocId) -> Result<i32> {
        container::stop(self.channel.clone(), alloc_id).await
    }

    /// Remove a container's task, metadata, and log fifos.
    pub async fn remove(&self, alloc_id: &AllocId) -> Result<()> {
        container::remove(self.channel.clone(), &self.data_dir, alloc_id).await
    }

    /// Collect resource usage stats for a single container.
    pub async fn stats(&self, alloc_id: &AllocId) -> Result<ContainerStats> {
        stats::collect(self.channel.clone(), alloc_id).await
    }

    /// Collect resource usage stats for all mill-managed containers.
    pub async fn stats_all(&self) -> Result<Vec<ContainerStats>> {
        stats::collect_all(self.channel.clone()).await
    }

    /// List all mill-managed container alloc IDs (for crash recovery).
    pub async fn list_managed(&self) -> Result<Vec<AllocId>> {
        container::list_managed(self.channel.clone()).await
    }

    /// Recover container info from containerd labels (for crash recovery).
    pub async fn recover_info(&self, alloc_id: &AllocId) -> Result<RecoveredContainer> {
        let container = container::get_container(self.channel.clone(), alloc_id).await?;
        let host_port =
            container.labels.get(spec::LABEL_HOST_PORT).and_then(|s| s.parse::<u16>().ok());
        let cpu = container
            .labels
            .get(spec::LABEL_CPU)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let memory = container
            .labels
            .get(spec::LABEL_MEMORY)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let volumes: Vec<ContainerVolume> = container
            .labels
            .get(spec::LABEL_VOLUMES)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let kind = container
            .labels
            .get(spec::LABEL_KIND)
            .map(|s| match s.as_str() {
                "task" => AllocKind::Task,
                _ => AllocKind::Service,
            })
            .unwrap_or_default();
        let timeout = container
            .labels
            .get(spec::LABEL_TIMEOUT)
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);
        let command: Option<Vec<String>> =
            container.labels.get(spec::LABEL_COMMAND).and_then(|s| serde_json::from_str(s).ok());
        Ok(RecoveredContainer {
            alloc_id: alloc_id.clone(),
            kind,
            timeout,
            command,
            host_port,
            cpu,
            memory,
            image: container.image,
            volumes,
        })
    }

    /// Wait for a crash-recovered container to exit (no existing ContainerHandle).
    pub async fn wait_exit(&self, alloc_id: &AllocId) -> Result<i32> {
        container::wait_exit(self.channel.clone(), alloc_id).await
    }

    /// Stop and remove all mill-managed containers.
    pub async fn cleanup_all(&self) -> Result<()> {
        let alloc_ids = self.list_managed().await?;
        let mut set = tokio::task::JoinSet::new();
        for alloc_id in alloc_ids {
            let rt = self.clone();
            set.spawn(async move {
                if let Err(e) = rt.stop(&alloc_id).await {
                    tracing::warn!("failed to stop mill-{}: {e}", alloc_id.0);
                }
                if let Err(e) = rt.remove(&alloc_id).await {
                    tracing::warn!("failed to remove mill-{}: {e}", alloc_id.0);
                }
            });
        }
        while set.join_next().await.is_some() {}
        Ok(())
    }
}
