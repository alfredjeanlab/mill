use std::time::Duration;

use containerd_client::{
    services::v1::{
        Container, CreateContainerRequest, CreateTaskRequest, DeleteContainerRequest,
        DeleteTaskRequest, GetContainerRequest, KillRequest, ListContainersRequest, StartRequest,
        WaitRequest,
        containers_client::ContainersClient,
        snapshots::{
            MountsRequest, PrepareSnapshotRequest, RemoveSnapshotRequest,
            snapshots_client::SnapshotsClient,
        },
        tasks_client::TasksClient,
    },
    with_namespace,
};
use mill_config::{AllocId, ContainerSpec, DataDir};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::transport::Channel;

use crate::error::{ContainerdError, Result};
use crate::image::NAMESPACE;
use crate::logs::{self, LogLine};

const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Handle to a running container, providing access to exit status and logs.
pub struct ContainerHandle {
    exit_rx: oneshot::Receiver<i32>,
    log_tx: broadcast::Sender<LogLine>,
    log_cancel: CancellationToken,
}

impl ContainerHandle {
    /// Await the container's exit code. Consumes the handle.
    pub async fn wait(self) -> Result<i32> {
        self.exit_rx.await.map_err(|_| {
            ContainerdError::Grpc(Box::new(tonic::Status::internal("exit channel closed")))
        })
    }

    /// Subscribe to the container's log stream.
    pub fn logs(&self) -> broadcast::Receiver<LogLine> {
        self.log_tx.subscribe()
    }

    /// Cancel the log reader tasks (e.g. on container removal).
    pub fn cancel_logs(&self) {
        self.log_cancel.cancel();
    }
}

/// Format a containerd container ID from an alloc ID.
pub(crate) fn container_id(alloc_id: &AllocId) -> String {
    format!("mill-{}", alloc_id.0)
}

/// Create and start a container, returning a handle for waiting and log access.
pub async fn create_and_start(
    channel: Channel,
    data_dir: &DataDir,
    spec: &ContainerSpec,
) -> Result<ContainerHandle> {
    let cid = container_id(&spec.alloc_id);

    // Read image config to get layer chain ID and default command.
    let image_info = crate::image::image_info(channel.clone(), &spec.image).await?;

    // Build OCI spec and labels
    let oci_spec = crate::spec::build_oci_spec(data_dir, spec, &image_info)?;
    let labels = crate::spec::build_labels(spec);
    let oci_spec_json =
        serde_json::to_vec(&oci_spec).map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let oci_spec_any = prost_types::Any {
        type_url: "types.containerd.io/opencontainers/runtime-spec/1/Spec".to_string(),
        value: oci_spec_json,
    };

    // Prepare a writable overlayfs snapshot on top of the image's layer chain.
    // The snapshot key matches the container ID so containerd can find the rootfs.
    let chain_id = image_info.chain_id;
    let mut snapshots_client = SnapshotsClient::new(channel.clone());
    let snap_req = PrepareSnapshotRequest {
        snapshotter: "overlayfs".to_string(),
        key: cid.clone(),
        parent: chain_id,
        ..Default::default()
    };
    let snap_req = with_namespace!(snap_req, NAMESPACE);
    let rootfs_mounts = match snapshots_client.prepare(snap_req).await {
        Ok(resp) => resp.into_inner().mounts,
        Err(status) if status.code() == tonic::Code::AlreadyExists => {
            // Snapshot from a previous attempt; fetch its current mounts.
            let req = MountsRequest { snapshotter: "overlayfs".to_string(), key: cid.clone() };
            let req = with_namespace!(req, NAMESPACE);
            snapshots_client
                .mounts(req)
                .await
                .map_err(|s| ContainerdError::Grpc(Box::new(s)))?
                .into_inner()
                .mounts
        }
        Err(status) => return Err(ContainerdError::Grpc(Box::new(status))),
    };

    // Create container metadata
    let container = Container {
        id: cid.clone(),
        image: spec.image.clone(),
        runtime: Some(containerd_client::services::v1::container::Runtime {
            name: "io.containerd.runc.v2".to_string(),
            options: None,
        }),
        spec: Some(oci_spec_any),
        snapshotter: "overlayfs".to_string(),
        snapshot_key: cid.clone(),
        labels,
        ..Default::default()
    };

    let mut containers_client = ContainersClient::new(channel.clone());
    let req = CreateContainerRequest { container: Some(container) };
    let req = with_namespace!(req, NAMESPACE);
    containers_client.create(req).await.map_err(|status| {
        if status.code() == tonic::Code::AlreadyExists {
            ContainerdError::AlreadyExists(spec.alloc_id.0.clone())
        } else {
            ContainerdError::Grpc(Box::new(status))
        }
    })?;

    // Setup log fifos
    let log_cancel = CancellationToken::new();
    let log_base_dir = data_dir.logs_dir();
    let (log_tx, stdout_path, stderr_path) =
        match logs::setup_fifos(&log_base_dir, &spec.alloc_id.0, log_cancel.clone()) {
            Ok(result) => result,
            Err(e) => {
                let _ = delete_container(channel.clone(), &cid).await;
                return Err(e);
            }
        };

    // Create task with fifo paths for stdout/stderr.
    // rootfs_mounts are the overlayfs mount options from the prepared snapshot;
    // containerd uses them to mount the container's rootfs before running runc.
    let mut tasks_client = TasksClient::new(channel.clone());
    let req = CreateTaskRequest {
        container_id: cid.clone(),
        rootfs: rootfs_mounts,
        stdout: stdout_path.to_string_lossy().to_string(),
        stderr: stderr_path.to_string_lossy().to_string(),
        ..Default::default()
    };
    let req = with_namespace!(req, NAMESPACE);
    if let Err(status) = tasks_client.create(req).await {
        let _ = delete_container(channel.clone(), &cid).await;
        let _ = delete_snapshot(channel.clone(), &cid).await;
        logs::cleanup_fifos(&log_base_dir, &spec.alloc_id.0);
        return Err(ContainerdError::Grpc(Box::new(status)));
    }

    // Start task
    let req = StartRequest { container_id: cid.clone(), ..Default::default() };
    let req = with_namespace!(req, NAMESPACE);
    if let Err(status) = tasks_client.start(req).await {
        let _ = delete_task(channel.clone(), &cid).await;
        let _ = delete_container(channel.clone(), &cid).await;
        let _ = delete_snapshot(channel.clone(), &cid).await;
        logs::cleanup_fifos(&log_base_dir, &spec.alloc_id.0);
        return Err(ContainerdError::Grpc(Box::new(status)));
    }

    // Spawn background waiter for exit code
    let (exit_tx, exit_rx) = oneshot::channel();
    let wait_channel = channel.clone();
    let wait_cid = cid.clone();
    tokio::spawn(async move {
        let mut tc = TasksClient::new(wait_channel);
        let req = WaitRequest { container_id: wait_cid, ..Default::default() };
        let req = with_namespace!(req, NAMESPACE);
        match tc.wait(req).await {
            Ok(resp) => {
                let exit_code = resp.into_inner().exit_status;
                let _ = exit_tx.send(exit_code as i32);
            }
            Err(_) => {
                let _ = exit_tx.send(-1);
            }
        }
    });

    Ok(ContainerHandle { exit_rx, log_tx, log_cancel })
}

/// Stop a container: SIGTERM, wait up to 10s, then SIGKILL. Returns exit code.
pub async fn stop(channel: Channel, alloc_id: &AllocId) -> Result<i32> {
    let cid = container_id(alloc_id);
    let mut tasks_client = TasksClient::new(channel.clone());

    // Send SIGTERM
    let req = KillRequest {
        container_id: cid.clone(),
        signal: 15, // SIGTERM
        ..Default::default()
    };
    let req = with_namespace!(req, NAMESPACE);
    if let Err(status) = tasks_client.kill(req).await {
        if status.code() == tonic::Code::NotFound {
            return Err(ContainerdError::NotFound(alloc_id.0.clone()));
        }
        return Err(ContainerdError::Grpc(Box::new(status)));
    }

    // Wait for exit with timeout
    let wait_fut = async {
        let mut tc = TasksClient::new(channel.clone());
        let req = WaitRequest { container_id: cid.clone(), ..Default::default() };
        let req = with_namespace!(req, NAMESPACE);
        tc.wait(req).await
    };

    match tokio::time::timeout(STOP_TIMEOUT, wait_fut).await {
        Ok(Ok(resp)) => Ok(resp.into_inner().exit_status as i32),
        Ok(Err(status)) => Err(ContainerdError::Grpc(Box::new(status))),
        Err(_) => {
            // Timeout — escalate to SIGKILL
            tracing::warn!(
                "container mill-{} did not exit after SIGTERM, sending SIGKILL",
                alloc_id.0
            );
            let req = KillRequest {
                container_id: cid.clone(),
                signal: 9, // SIGKILL
                ..Default::default()
            };
            let req = with_namespace!(req, NAMESPACE);
            tasks_client.kill(req).await?;

            // Wait for exit after SIGKILL
            let req = WaitRequest { container_id: cid, ..Default::default() };
            let req = with_namespace!(req, NAMESPACE);
            let resp = tasks_client.wait(req).await?;
            Ok(resp.into_inner().exit_status as i32)
        }
    }
}

/// Remove a container's task and metadata, plus clean up fifos.
pub async fn remove(channel: Channel, data_dir: &DataDir, alloc_id: &AllocId) -> Result<()> {
    let cid = container_id(alloc_id);

    // Delete task (ignore not-found)
    let _ = delete_task(channel.clone(), &cid).await;

    // Delete container
    delete_container(channel.clone(), &cid).await.map_err(|status| {
        if status.code() == tonic::Code::NotFound {
            ContainerdError::NotFound(alloc_id.0.clone())
        } else {
            ContainerdError::Grpc(Box::new(status))
        }
    })?;

    // Delete overlayfs snapshot
    let _ = delete_snapshot(channel.clone(), &cid).await;

    // Clean up fifos
    logs::cleanup_fifos(&data_dir.logs_dir(), &alloc_id.0);

    Ok(())
}

/// List all mill-managed container alloc IDs (for crash recovery).
pub async fn list_managed(channel: Channel) -> Result<Vec<AllocId>> {
    let mut client = ContainersClient::new(channel);
    let filter = format!("labels.\"{}\"==true", crate::spec::LABEL_MANAGED);
    let req = ListContainersRequest { filters: vec![filter] };
    let req = with_namespace!(req, NAMESPACE);
    let resp = client.list(req).await?;
    let containers = resp.into_inner().containers;

    let alloc_ids = containers
        .into_iter()
        .filter_map(|c| c.labels.get(crate::spec::LABEL_ALLOC_ID).map(|id| AllocId(id.clone())))
        .collect();

    Ok(alloc_ids)
}

/// Fetch a container by alloc ID, returning its metadata (for crash recovery).
pub async fn get_container(channel: Channel, alloc_id: &AllocId) -> Result<Container> {
    let cid = container_id(alloc_id);
    let mut client = ContainersClient::new(channel);
    let req = GetContainerRequest { id: cid };
    let req = with_namespace!(req, NAMESPACE);
    let resp = client.get(req).await.map_err(|status| {
        if status.code() == tonic::Code::NotFound {
            ContainerdError::NotFound(alloc_id.0.clone())
        } else {
            ContainerdError::Grpc(Box::new(status))
        }
    })?;
    resp.into_inner().container.ok_or_else(|| ContainerdError::NotFound(alloc_id.0.clone()))
}

/// Issue a fresh wait request for a crash-recovered container (no existing ContainerHandle).
/// Returns the exit code when the container exits.
pub async fn wait_exit(channel: Channel, alloc_id: &AllocId) -> Result<i32> {
    let cid = container_id(alloc_id);
    let mut tc = TasksClient::new(channel);
    let req = WaitRequest { container_id: cid, ..Default::default() };
    let req = with_namespace!(req, NAMESPACE);
    let resp = tc.wait(req).await?;
    Ok(resp.into_inner().exit_status as i32)
}

async fn delete_task(
    channel: Channel,
    container_id: &str,
) -> std::result::Result<(), tonic::Status> {
    let mut client = TasksClient::new(channel);
    let req = DeleteTaskRequest { container_id: container_id.to_string() };
    let req = with_namespace!(req, NAMESPACE);
    match client.delete(req).await {
        Ok(_) => Ok(()),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(()),
        Err(status) => Err(status),
    }
}

async fn delete_snapshot(
    channel: Channel,
    container_id: &str,
) -> std::result::Result<(), tonic::Status> {
    let mut client = SnapshotsClient::new(channel);
    let req = RemoveSnapshotRequest {
        snapshotter: "overlayfs".to_string(),
        key: container_id.to_string(),
    };
    let req = with_namespace!(req, NAMESPACE);
    match client.remove(req).await {
        Ok(_) => Ok(()),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(()),
        Err(status) => Err(status),
    }
}

async fn delete_container(
    channel: Channel,
    container_id: &str,
) -> std::result::Result<(), tonic::Status> {
    let mut client = ContainersClient::new(channel);
    let req = DeleteContainerRequest { id: container_id.to_string() };
    let req = with_namespace!(req, NAMESPACE);
    match client.delete(req).await {
        Ok(_) => Ok(()),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(()),
        Err(status) => Err(status),
    }
}
