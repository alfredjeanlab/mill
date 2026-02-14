use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use mill_config::{AllocId, AllocStatus, ContainerSpec};
use mill_containerd::Containers;
use mill_net::PortAllocator;
use tokio::sync::mpsc;

use super::logs::LogBuffer;
use super::state::{AllocationMap, LocalAlloc};
use crate::error::{NodeError, Result};
use crate::rpc::NodeReport;

/// Shared node infrastructure passed to runner and reconcile functions.
pub(crate) struct RunContext {
    pub runtime: Arc<dyn Containers>,
    pub ports: Arc<PortAllocator>,
    pub allocs: Arc<AllocationMap>,
    pub report_tx: mpsc::Sender<NodeReport>,
}

/// Start a container: pull image, allocate port, run, wire up logs and exit waiter.
pub async fn run_container(
    ctx: &RunContext,
    alloc_id: AllocId,
    mut spec: ContainerSpec,
    mesh_ip: Option<IpAddr>,
    auth: Option<&mill_containerd::RegistryAuth>,
) -> Result<()> {
    // Check not already running
    if ctx.allocs.contains(&alloc_id).await {
        return Err(NodeError::AllocAlreadyExists(alloc_id.0.clone()));
    }

    // Report pulling status
    let _ = ctx
        .report_tx
        .send(NodeReport::StatusUpdate { alloc_id: alloc_id.clone(), status: AllocStatus::Pulling })
        .await;

    // Pull image if not cached
    if !ctx.runtime.is_image_cached(&spec.image).await.unwrap_or(false)
        && let Err(e) = ctx.runtime.pull_image(&spec.image, auth).await
    {
        let _ = ctx
            .report_tx
            .send(NodeReport::RunFailed {
                alloc_id: alloc_id.clone(),
                reason: format!("image pull failed: {e}"),
            })
            .await;
        return Err(e.into());
    }

    // Report starting status
    let _ = ctx
        .report_tx
        .send(NodeReport::StatusUpdate {
            alloc_id: alloc_id.clone(),
            status: AllocStatus::Starting,
        })
        .await;

    // Allocate host port if the container exposes one
    let host_port = if spec.port.is_some() {
        let port = ctx.ports.allocate(alloc_id.clone()).ok_or(NodeError::PortExhaustion)?;
        spec.host_port = Some(port);
        Some(port)
    } else {
        None
    };

    // Inject PORT env var so the application knows which port to bind
    if let Some(port) = host_port {
        spec.env.insert("PORT".to_string(), port.to_string());
    }

    // Start the container
    let handle = match ctx.runtime.run(&spec).await {
        Ok(h) => h,
        Err(e) => {
            if let Some(port) = host_port {
                ctx.ports.release(port);
            }
            let _ = ctx
                .report_tx
                .send(NodeReport::RunFailed {
                    alloc_id: alloc_id.clone(),
                    reason: format!("container start failed: {e}"),
                })
                .await;
            return Err(e.into());
        }
    };

    // Set up log buffer and forwarder
    let log_buffer = Arc::new(LogBuffer::new());
    super::logs::spawn_log_forwarder(handle.logs(), Arc::clone(&log_buffer));

    // Insert allocation into map
    let local_alloc = LocalAlloc {
        alloc_id: alloc_id.clone(),
        spec,
        status: AllocStatus::Running,
        host_port,
        log_buffer,
    };
    // insert should succeed since we checked contains above
    let _ = ctx.allocs.insert(local_alloc).await;

    // Send RunStarted report with mesh address if available.
    // The container is expected to listen on host_port (injected as PORT env var).
    let address = mesh_ip.zip(host_port).map(|(ip, port)| SocketAddr::new(ip, port));
    let _ = ctx
        .report_tx
        .send(NodeReport::RunStarted { alloc_id: alloc_id.clone(), host_port, address })
        .await;

    // Spawn exit waiter: waits for container exit, cleans up, sends Stopped
    spawn_exit_waiter(
        handle.wait(),
        host_port,
        Arc::clone(&ctx.ports),
        Arc::clone(&ctx.allocs),
        ctx.report_tx.clone(),
        alloc_id,
    );

    Ok(())
}

/// Stop a running container, clean up resources.
pub async fn stop_container(ctx: &RunContext, alloc_id: &AllocId) -> Result<()> {
    // Get host port before removing
    let host_port = ctx.allocs.host_port(alloc_id).await;

    // Stop the container — capture result but always perform cleanup
    let stop_result = ctx.runtime.stop(alloc_id).await;

    // Always clean up regardless of stop result
    if let Err(e) = ctx.runtime.remove(alloc_id).await {
        tracing::warn!("failed to remove container {}: {e}", alloc_id.0);
    }
    if let Some(port) = host_port {
        ctx.ports.release(port);
    }
    ctx.allocs.remove(alloc_id).await;

    let exit_code = match &stop_result {
        Ok(code) => *code,
        Err(_) => -1,
    };
    let _ = ctx.report_tx.send(NodeReport::Stopped { alloc_id: alloc_id.clone(), exit_code }).await;

    stop_result.map(|_| ()).map_err(Into::into)
}

/// Pull an image, reporting success or failure.
pub async fn pull_image(
    ctx: &RunContext,
    image: &str,
    auth: Option<&mill_containerd::RegistryAuth>,
) {
    match ctx.runtime.pull_image(image, auth).await {
        Ok(()) => {
            let _ = ctx.report_tx.send(NodeReport::PullComplete { image: image.to_string() }).await;
        }
        Err(e) => {
            let _ = ctx
                .report_tx
                .send(NodeReport::PullFailed { image: image.to_string(), reason: e.to_string() })
                .await;
        }
    }
}

/// Spawn a task that waits for a container to exit, then releases the port,
/// removes the allocation from the map, and sends a Stopped report.
pub fn spawn_exit_waiter(
    exit_fut: impl Future<Output = mill_containerd::Result<i32>> + Send + 'static,
    host_port: Option<u16>,
    ports: Arc<PortAllocator>,
    allocs: Arc<AllocationMap>,
    report_tx: mpsc::Sender<NodeReport>,
    alloc_id: AllocId,
) {
    tokio::spawn(async move {
        let exit_code = exit_fut.await.unwrap_or(-1);

        // Set terminal status before cleanup so heartbeats reflect the final state
        let status = if exit_code == 0 {
            AllocStatus::Stopped { exit_code }
        } else {
            AllocStatus::Failed { reason: format!("exit code {exit_code}") }
        };
        allocs.set_status(&alloc_id, status).await;

        if let Some(port) = host_port {
            ports.release(port);
        }
        allocs.remove(&alloc_id).await;

        let _ = report_tx.send(NodeReport::Stopped { alloc_id, exit_code }).await;
    });
}
