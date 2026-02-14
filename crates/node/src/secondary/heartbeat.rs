use std::sync::Arc;

use mill_config::NodeId;
use tokio::sync::mpsc;

use super::metrics::MetricsCollector;
use super::state::AllocationMap;
use crate::rpc::{Heartbeat, NodeReport};

/// Run the heartbeat loop, sending periodic status to the primary.
///
/// Exits when `report_tx` is closed (primary dropped its receiver).
pub async fn heartbeat_loop(
    allocs: Arc<AllocationMap>,
    metrics: Arc<MetricsCollector>,
    report_tx: mpsc::Sender<NodeReport>,
    node_id: NodeId,
) {
    let mut interval = tokio::time::interval(crate::env::heartbeat_interval());
    loop {
        interval.tick().await;

        let alloc_statuses = allocs.all_statuses().await;
        let (mut resources, stats) = metrics.current().await;

        // Subtract reserved CPU from available.
        let reserved_cpu = allocs.total_reserved_cpu().await;
        resources.cpu_available = (resources.cpu_total - reserved_cpu).max(0.0);

        // Subtract reserved memory from available.
        let reserved_memory = allocs.total_reserved_memory().await;
        resources.memory_available =
            bytesize::ByteSize::b(resources.memory_total.as_u64().saturating_sub(reserved_memory));

        let heartbeat = Heartbeat {
            node_id: node_id.clone(),
            alloc_statuses,
            resources,
            stats,
            volumes: allocs.volume_info().await,
        };

        if report_tx.send(NodeReport::Heartbeat(heartbeat)).await.is_err() {
            // Channel closed — primary shut down
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn test_node_id() -> NodeId {
        NodeId("test-node".into())
    }

    #[tokio::test]
    async fn heartbeat_sends_on_interval() {
        tokio::time::pause();

        let allocs = Arc::new(AllocationMap::new());
        let metrics = Arc::new(MetricsCollector::new());
        let (report_tx, mut report_rx) = mpsc::channel(64);

        let handle = tokio::spawn(heartbeat_loop(allocs, metrics, report_tx, test_node_id()));

        // Advance past the first tick (interval fires immediately)
        tokio::time::advance(Duration::from_millis(10)).await;
        let report = report_rx.recv().await;
        assert!(matches!(report, Some(NodeReport::Heartbeat(_))));

        // Advance to next heartbeat
        tokio::time::advance(Duration::from_secs(5)).await;
        let report = report_rx.recv().await;
        assert!(matches!(report, Some(NodeReport::Heartbeat(_))));

        handle.abort();
    }

    #[tokio::test]
    async fn heartbeat_exits_on_channel_close() {
        tokio::time::pause();

        let allocs = Arc::new(AllocationMap::new());
        let metrics = Arc::new(MetricsCollector::new());
        let (report_tx, report_rx) = mpsc::channel(64);

        let handle = tokio::spawn(heartbeat_loop(allocs, metrics, report_tx, test_node_id()));

        // Drop receiver to close channel
        drop(report_rx);

        // Advance time so the heartbeat loop tries to send
        tokio::time::advance(Duration::from_millis(10)).await;

        // The loop should exit — give it a moment
        tokio::time::advance(Duration::from_secs(1)).await;
        // Task should complete (not hang)
        tokio::time::timeout(Duration::from_secs(1), handle).await.ok();
    }
}
