use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use mill_config::{AllocId, AllocStatus, ContainerSpec};
use mill_containerd::reconnect_fifos;
use tokio_util::sync::CancellationToken;

use super::logs::{LogBuffer, spawn_log_forwarder};
use super::runner::{RunContext, spawn_exit_waiter};
use super::state::LocalAlloc;
use crate::rpc::NodeReport;

/// Reconcile running containers against expected allocations after a restart.
///
/// - Stops orphan containers (running but not expected)
/// - Fully rehydrates expected-and-running containers into AllocationMap:
///   recovers host_port from containerd labels, re-registers port, reconnects
///   log fifos, spawns exit waiter, and reports RunStarted to the primary.
pub async fn reconcile(ctx: &RunContext, expected_alloc_ids: &[AllocId], mesh_ip: Option<IpAddr>) {
    let running = match ctx.runtime.list_managed().await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("reconcile: failed to list managed containers: {e}");
            return;
        }
    };

    let expected: HashSet<&AllocId> = expected_alloc_ids.iter().collect();
    let running_set: HashSet<&AllocId> = running.iter().collect();

    // Stop orphans (running but not expected)
    for alloc_id in &running {
        if !expected.contains(alloc_id) {
            tracing::info!("reconcile: stopping orphan container {}", alloc_id.0);
            if let Err(e) = ctx.runtime.stop(alloc_id).await {
                tracing::warn!("reconcile: failed to stop orphan {}: {e}", alloc_id.0);
            }
            if let Err(e) = ctx.runtime.remove(alloc_id).await {
                tracing::warn!("reconcile: failed to remove orphan {}: {e}", alloc_id.0);
            }
        }
    }

    // Rehydrate expected containers that are still running
    for alloc_id in expected_alloc_ids {
        if !running_set.contains(alloc_id) {
            // Container died while mill was down — report it as stopped so the
            // primary knows the alloc is no longer running and the watchdog
            // can reschedule.
            tracing::info!(
                "reconcile: expected alloc {} not running, reporting stopped",
                alloc_id.0
            );
            let _ = ctx
                .report_tx
                .send(NodeReport::Stopped { alloc_id: alloc_id.clone(), exit_code: -1 })
                .await;
            continue;
        }

        // Recover container info from containerd labels
        let recovered = match ctx.runtime.recover_info(alloc_id).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("reconcile: failed to recover info for {}: {e}", alloc_id.0);
                continue;
            }
        };

        // Re-register the port so it won't be double-allocated
        if let Some(port) = recovered.host_port
            && !ctx.ports.allocate_specific(alloc_id.clone(), port)
        {
            tracing::warn!("reconcile: port {port} for {} already allocated, skipping", alloc_id.0);
        }

        // Reconnect log fifos (or create empty buffer if fifos are gone)
        let log_buffer = Arc::new(LogBuffer::new());
        let log_base_dir = ctx.runtime.data_dir().logs_dir();
        if let Some(log_tx) = reconnect_fifos(&log_base_dir, &alloc_id.0, CancellationToken::new())
        {
            spawn_log_forwarder(log_tx.subscribe(), Arc::clone(&log_buffer));
        }

        // Build a placeholder ContainerSpec with the essential fields we know
        let spec = ContainerSpec {
            alloc_id: alloc_id.clone(),
            kind: recovered.kind,
            timeout: recovered.timeout,
            command: recovered.command,
            image: recovered.image,
            env: Default::default(),
            port: recovered.host_port.map(|_| 0), // we know it exposes a port
            host_port: recovered.host_port,
            cpu: recovered.cpu,
            memory: bytesize::ByteSize::b(recovered.memory),
            volumes: recovered.volumes,
        };

        let local_alloc = LocalAlloc {
            alloc_id: alloc_id.clone(),
            spec,
            status: AllocStatus::Running,
            host_port: recovered.host_port,
            log_buffer,
        };

        if let Err(dup_id) = ctx.allocs.insert(local_alloc).await {
            tracing::warn!("reconcile: alloc {} already in map", dup_id.0);
            continue;
        }

        // Spawn exit waiter for this recovered container
        let rt = ctx.runtime.clone();
        let aid = alloc_id.clone();
        spawn_exit_waiter(
            async move { rt.wait_exit(&aid).await },
            recovered.host_port,
            Arc::clone(&ctx.ports),
            Arc::clone(&ctx.allocs),
            ctx.report_tx.clone(),
            alloc_id.clone(),
        );

        // Report RunStarted so the primary updates its route table
        let address = mesh_ip.zip(recovered.host_port).map(|(ip, port)| SocketAddr::new(ip, port));
        let _ = ctx
            .report_tx
            .send(NodeReport::RunStarted {
                alloc_id: alloc_id.clone(),
                host_port: recovered.host_port,
                address,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::Arc;

    use mill_config::{AllocId, DataDir};
    use mill_containerd::Containers;
    use mill_net::PortAllocator;
    use mill_support::runtime::{TestContainers, test_recovered};
    use tokio::sync::mpsc;

    use super::reconcile;
    use crate::rpc::NodeReport;
    use crate::secondary::runner::RunContext;
    use crate::secondary::state::AllocationMap;

    fn test_ctx(runtime: Arc<dyn Containers>) -> (RunContext, mpsc::Receiver<NodeReport>) {
        let ports = Arc::new(PortAllocator::with_range(50000, 50100).unwrap());
        let allocs = Arc::new(AllocationMap::new());
        let (report_tx, report_rx) = mpsc::channel(64);
        let ctx = RunContext { runtime, ports, allocs, report_tx };
        (ctx, report_rx)
    }

    #[tokio::test]
    async fn stops_orphan_containers() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path());

        let alloc1 = AllocId("alloc-1".into());
        let alloc2 = AllocId("alloc-2".into());
        let alloc3 = AllocId("alloc-3".into());

        let runtime = Arc::new(
            TestContainers::new(data_dir)
                .with_managed(vec![alloc1.clone(), alloc2.clone(), alloc3.clone()])
                .with_recovered(alloc1.clone(), test_recovered("alloc-1", Some(30000))),
        );

        let (ctx, _report_rx) = test_ctx(runtime.clone());

        // Only alloc-1 is expected; alloc-2 and alloc-3 are orphans.
        reconcile(&ctx, &[alloc1.clone()], None).await;

        let stopped = runtime.stopped();
        let removed = runtime.removed();

        // Orphans should be stopped.
        assert!(stopped.contains(&alloc2), "alloc-2 should be stopped");
        assert!(stopped.contains(&alloc3), "alloc-3 should be stopped");
        // Expected alloc should NOT be stopped.
        assert!(!stopped.contains(&alloc1), "alloc-1 should NOT be stopped");

        // Orphans should be removed.
        assert!(removed.contains(&alloc2));
        assert!(removed.contains(&alloc3));
    }

    #[tokio::test]
    async fn rehydrates_expected_running_container() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path());

        let alloc1 = AllocId("alloc-1".into());
        let runtime = Arc::new(
            TestContainers::new(data_dir)
                .with_managed(vec![alloc1.clone()])
                .with_recovered(alloc1.clone(), test_recovered("alloc-1", Some(50050))),
        );

        let (ctx, mut report_rx) = test_ctx(runtime.clone());
        let mesh_ip: IpAddr = "10.99.0.1".parse().unwrap();

        reconcile(&ctx, &[alloc1.clone()], Some(mesh_ip)).await;

        // Alloc should be in the AllocationMap.
        assert!(ctx.allocs.contains(&alloc1).await, "alloc-1 should be in AllocationMap");

        // Port 50050 should be reserved.
        assert!(
            !ctx.ports.allocate_specific(AllocId("other".into()), 50050),
            "port 50050 should already be allocated"
        );

        // A RunStarted report should have been sent.
        let report = report_rx.recv().await.unwrap();
        match report {
            NodeReport::RunStarted { alloc_id, host_port, address } => {
                assert_eq!(alloc_id, alloc1);
                assert_eq!(host_port, Some(50050));
                assert_eq!(address, Some("10.99.0.1:50050".parse().unwrap()),);
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reports_stopped_for_missing_expected() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path());

        let alloc1 = AllocId("alloc-1".into());
        // managed is empty — alloc-1 is NOT running.
        let runtime = Arc::new(TestContainers::new(data_dir));

        let (ctx, mut report_rx) = test_ctx(runtime.clone());

        reconcile(&ctx, &[alloc1.clone()], None).await;

        // Should send Stopped with exit_code -1.
        let report = report_rx.recv().await.unwrap();
        match report {
            NodeReport::Stopped { alloc_id, exit_code } => {
                assert_eq!(alloc_id, alloc1);
                assert_eq!(exit_code, -1);
            }
            other => panic!("expected Stopped, got {other:?}"),
        }

        // No stop/remove calls should have been made.
        assert!(runtime.stopped().is_empty());
        assert!(runtime.removed().is_empty());
    }

    #[tokio::test]
    async fn handles_recover_info_failure() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path());

        let alloc1 = AllocId("alloc-1".into());
        let runtime = Arc::new(
            TestContainers::new(data_dir)
                .with_managed(vec![alloc1.clone()])
                .with_recover_fail(alloc1.clone()),
        );

        let (ctx, mut report_rx) = test_ctx(runtime.clone());

        reconcile(&ctx, &[alloc1.clone()], None).await;

        // Alloc should NOT be in the AllocationMap.
        assert!(
            !ctx.allocs.contains(&alloc1).await,
            "alloc-1 should NOT be in AllocationMap when recover fails"
        );

        // No RunStarted report should be sent.
        // The channel should be empty (try_recv returns Err).
        assert!(report_rx.try_recv().is_err(), "no report expected after recover failure");
    }

    #[tokio::test]
    async fn rehydrates_despite_port_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path());

        let alloc1 = AllocId("alloc-1".into());
        let runtime = Arc::new(
            TestContainers::new(data_dir)
                .with_managed(vec![alloc1.clone()])
                .with_recovered(alloc1.clone(), test_recovered("alloc-1", Some(50060))),
        );

        let (ctx, mut report_rx) = test_ctx(runtime.clone());

        // Pre-allocate port 50060 so it conflicts.
        assert!(ctx.ports.allocate_specific(AllocId("existing".into()), 50060));

        reconcile(&ctx, &[alloc1.clone()], None).await;

        // Alloc should still be rehydrated despite port conflict.
        assert!(
            ctx.allocs.contains(&alloc1).await,
            "alloc should still be rehydrated despite port conflict"
        );

        // RunStarted should still be sent.
        let report = report_rx.recv().await.unwrap();
        assert!(
            matches!(report, NodeReport::RunStarted { .. }),
            "expected RunStarted, got {report:?}"
        );
    }
}
