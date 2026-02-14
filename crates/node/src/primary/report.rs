use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mill_config::{AllocId, AllocKind, AllocStatus, ContainerStats, NodeId, NodeResources};
use mill_containerd::LogLine;
use mill_raft::MillRaft;
use mill_raft::fsm::command::Command;
use tokio::sync::{Mutex, mpsc};

use super::watchdog::HeartbeatTracker;
use crate::rpc::NodeReport;

/// Subscribers for log lines, keyed by allocation ID.
///
/// When the report processor receives `LogLines` for an alloc, it
/// forwards them to all registered subscriber channels.
pub type LogSubscribers = Arc<Mutex<HashMap<AllocId, Vec<mpsc::Sender<Vec<LogLine>>>>>>;

/// Create a new empty log subscribers map.
pub fn new_log_subscribers() -> LogSubscribers {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Snapshot of live metrics for a single node, updated on each heartbeat.
pub struct NodeMetricsSnapshot {
    pub resources: NodeResources,
    pub container_stats: Vec<ContainerStats>,
}

/// Histogram bucket upper bounds for task durations (seconds).
const TASK_DURATION_BUCKETS: [f64; 9] =
    [1.0, 5.0, 30.0, 60.0, 300.0, 600.0, 1800.0, 3600.0, f64::INFINITY];

/// Per-task duration histogram.
pub struct DurationHistogram {
    count: u64,
    sum_seconds: f64,
    /// Bucket counts (non-cumulative); cumulated at snapshot time.
    buckets: [u64; 9],
}

impl DurationHistogram {
    fn new() -> Self {
        Self { count: 0, sum_seconds: 0.0, buckets: [0; 9] }
    }

    fn record(&mut self, dur: Duration) {
        let secs = dur.as_secs_f64();
        self.count += 1;
        self.sum_seconds += secs;
        for (i, &bound) in TASK_DURATION_BUCKETS.iter().enumerate() {
            if secs <= bound {
                self.buckets[i] += 1;
                break;
            }
        }
    }
}

/// Snapshot of a task duration histogram for the metrics handler.
pub struct DurationHistogramSnapshot {
    pub name: String,
    pub count: u64,
    pub sum_seconds: f64,
    /// Cumulative bucket counts matching `TASK_DURATION_BUCKETS`.
    pub buckets: [(f64, u64); 9],
}

/// Inner state protected by the mutex.
struct LiveMetricsInner {
    node_metrics: HashMap<NodeId, NodeMetricsSnapshot>,
    restart_counts: HashMap<String, u64>,
    task_spawn_counts: HashMap<String, u64>,
    task_durations: HashMap<String, DurationHistogram>,
}

/// Volatile per-heartbeat metrics stored outside Raft.
///
/// Follows the `HeartbeatTracker` pattern: `Arc<parking_lot::Mutex<...>>`
/// shared between the report processor (writer) and the API handler (reader).
#[derive(Clone)]
pub struct LiveMetrics {
    inner: Arc<parking_lot::Mutex<LiveMetricsInner>>,
}

impl LiveMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(parking_lot::Mutex::new(LiveMetricsInner {
                node_metrics: HashMap::new(),
                restart_counts: HashMap::new(),
                task_spawn_counts: HashMap::new(),
                task_durations: HashMap::new(),
            })),
        }
    }

    /// Update metrics for a node from its latest heartbeat.
    pub fn update(&self, node_id: NodeId, resources: NodeResources, stats: Vec<ContainerStats>) {
        self.inner
            .lock()
            .node_metrics
            .insert(node_id, NodeMetricsSnapshot { resources, container_stats: stats });
    }

    /// Snapshot all node metrics for reading.
    pub fn snapshot_all(&self) -> HashMap<NodeId, NodeMetricsSnapshot> {
        let guard = self.inner.lock();
        guard
            .node_metrics
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    NodeMetricsSnapshot {
                        resources: v.resources.clone(),
                        container_stats: v.container_stats.clone(),
                    },
                )
            })
            .collect()
    }

    /// Record a service restart.
    pub fn record_restart(&self, service_name: &str) {
        *self.inner.lock().restart_counts.entry(service_name.to_string()).or_insert(0) += 1;
    }

    /// Snapshot restart counts for all services.
    pub fn restart_counts(&self) -> HashMap<String, u64> {
        self.inner.lock().restart_counts.clone()
    }

    /// Record a task spawn.
    pub fn record_task_spawn(&self, task_name: &str) {
        *self.inner.lock().task_spawn_counts.entry(task_name.to_string()).or_insert(0) += 1;
    }

    /// Snapshot task spawn counts.
    pub fn task_spawn_counts(&self) -> HashMap<String, u64> {
        self.inner.lock().task_spawn_counts.clone()
    }

    /// Record a completed task duration.
    pub fn record_task_duration(&self, task_name: &str, dur: Duration) {
        self.inner
            .lock()
            .task_durations
            .entry(task_name.to_string())
            .or_insert_with(DurationHistogram::new)
            .record(dur);
    }

    /// Snapshot task duration histograms.
    pub fn task_duration_snapshots(&self) -> Vec<DurationHistogramSnapshot> {
        let guard = self.inner.lock();
        guard
            .task_durations
            .iter()
            .map(|(name, h)| {
                let mut cumulative = 0u64;
                let mut buckets = [(0.0, 0u64); 9];
                for (i, &bound) in TASK_DURATION_BUCKETS.iter().enumerate() {
                    cumulative += h.buckets[i];
                    buckets[i] = (bound, cumulative);
                }
                DurationHistogramSnapshot {
                    name: name.clone(),
                    count: h.count,
                    sum_seconds: h.sum_seconds,
                    buckets,
                }
            })
            .collect()
    }
}

/// Spawn a background task that drains reports from the fan-in channel
/// and proposes corresponding FSM updates.
///
/// All secondaries (local and remote) feed into this single receiver.
/// Log line reports are forwarded to any registered subscribers.
pub fn spawn_report_processor(
    raft: Arc<MillRaft>,
    mut report_rx: mpsc::Receiver<NodeReport>,
    log_subscribers: LogSubscribers,
    heartbeat_tracker: HeartbeatTracker,
    live_metrics: LiveMetrics,
) {
    tokio::spawn(async move {
        while let Some(report) = report_rx.recv().await {
            if let Err(e) =
                process_report(&raft, report, &log_subscribers, &heartbeat_tracker, &live_metrics)
                    .await
            {
                tracing::warn!("failed to propose report update: {e}");
            }
        }
        tracing::info!("report processor: report channel closed, exiting");
    });
}

async fn process_report(
    raft: &MillRaft,
    report: NodeReport,
    log_subscribers: &LogSubscribers,
    heartbeat_tracker: &HeartbeatTracker,
    live_metrics: &LiveMetrics,
) -> Result<(), crate::error::NodeError> {
    let cmd = match report {
        NodeReport::RunStarted { alloc_id, host_port, address } => {
            // Use the address reported by the secondary if available, otherwise
            // reconstruct from the FSM node address + host_port.
            let address = address.or_else(|| {
                host_port.and_then(|port| {
                    raft.read_state(|fsm| {
                        let node_mill_id = fsm.allocs.get(&alloc_id).map(|a| &a.node);
                        node_mill_id.and_then(|mill_id| {
                            fsm.nodes
                                .values()
                                .find(|n| &n.mill_id == mill_id)
                                .map(|n| SocketAddr::new(n.address.ip(), port))
                        })
                    })
                })
            });
            Command::AllocRunning { alloc_id, address, started_at: Some(SystemTime::now()) }
        }
        NodeReport::RunFailed { alloc_id, reason } => {
            Command::AllocStatus { alloc_id, status: AllocStatus::Failed { reason } }
        }
        NodeReport::Stopped { alloc_id, exit_code } => {
            // Record task duration if this is a completed task with a start time.
            if let Some((name, elapsed)) = raft.read_state(|fsm| {
                fsm.allocs.get(&alloc_id).and_then(|a| {
                    if a.kind == AllocKind::Task {
                        a.started_at.and_then(|started| {
                            SystemTime::now()
                                .duration_since(started)
                                .ok()
                                .map(|d| (a.name.clone(), d))
                        })
                    } else {
                        None
                    }
                })
            }) {
                live_metrics.record_task_duration(&name, elapsed);
            }
            Command::AllocStatus { alloc_id, status: AllocStatus::Stopped { exit_code } }
        }
        NodeReport::StatusUpdate { alloc_id, status } => Command::AllocStatus { alloc_id, status },
        NodeReport::Heartbeat(hb) => {
            let raft_id = raft.read_state(|fsm| {
                fsm.nodes.iter().find(|(_, n)| n.mill_id == hb.node_id).map(|(id, _)| *id)
            });
            match raft_id {
                Some(id) => {
                    heartbeat_tracker.record(id);
                    live_metrics.update(hb.node_id.clone(), hb.resources, hb.stats);
                    Command::NodeHeartbeat { id, alloc_statuses: hb.alloc_statuses }
                }
                None => {
                    tracing::warn!("heartbeat from unknown node {}", hb.node_id.0);
                    return Ok(());
                }
            }
        }
        NodeReport::LogLines { alloc_id, lines } => {
            // Forward to subscribers; don't propose to FSM.
            let mut subs = log_subscribers.lock().await;
            if let Some(subscribers) = subs.get_mut(&alloc_id) {
                // Remove closed channels.
                subscribers.retain(|tx| !tx.is_closed());
                for tx in subscribers.iter() {
                    let _ = tx.try_send(lines.clone());
                }
            }
            return Ok(());
        }
        NodeReport::PullComplete { image } => {
            tracing::debug!(image = %image, "pull complete (no FSM update)");
            return Ok(());
        }
        NodeReport::PullFailed { image, reason } => {
            tracing::debug!(image = %image, reason = %reason, "pull failed (no FSM update)");
            return Ok(());
        }
    };

    raft.propose(cmd).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytesize::ByteSize;

    #[test]
    fn live_metrics_update_and_snapshot() {
        let lm = LiveMetrics::new();
        let node = NodeId("node-1".into());
        let resources = NodeResources {
            cpu_total: 4.0,
            cpu_available: 3.0,
            memory_total: ByteSize::gib(8),
            memory_available: ByteSize::gib(6),
        };
        let stats = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 0.5,
            memory_usage: ByteSize::mib(256),
        }];

        lm.update(node.clone(), resources.clone(), stats.clone());

        let snap = lm.snapshot_all();
        assert_eq!(snap.len(), 1);
        let ns = snap.get(&node).unwrap();
        assert_eq!(ns.resources.cpu_total, 4.0);
        assert_eq!(ns.container_stats.len(), 1);
        assert_eq!(ns.container_stats[0].cpu_usage, 0.5);
    }

    #[test]
    fn live_metrics_clone_shares_state() {
        let lm = LiveMetrics::new();
        let clone = lm.clone();

        lm.update(
            NodeId("n1".into()),
            NodeResources {
                cpu_total: 2.0,
                cpu_available: 1.0,
                memory_total: ByteSize::gib(4),
                memory_available: ByteSize::gib(2),
            },
            vec![],
        );

        let snap = clone.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key(&NodeId("n1".into())));
    }

    #[test]
    fn live_metrics_overwrite_on_update() {
        let lm = LiveMetrics::new();
        let node = NodeId("node-1".into());
        let res = NodeResources {
            cpu_total: 4.0,
            cpu_available: 3.0,
            memory_total: ByteSize::gib(8),
            memory_available: ByteSize::gib(6),
        };

        lm.update(node.clone(), res.clone(), vec![]);

        // Update with new stats
        let stats = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 1.5,
            memory_usage: ByteSize::mib(512),
        }];
        lm.update(node.clone(), res, stats);

        let snap = lm.snapshot_all();
        assert_eq!(snap[&node].container_stats.len(), 1);
        assert_eq!(snap[&node].container_stats[0].cpu_usage, 1.5);
    }

    #[test]
    fn restart_count_tracking() {
        let lm = LiveMetrics::new();
        assert!(lm.restart_counts().is_empty());

        lm.record_restart("web");
        lm.record_restart("web");
        lm.record_restart("api");

        let counts = lm.restart_counts();
        assert_eq!(counts["web"], 2);
        assert_eq!(counts["api"], 1);
    }

    #[test]
    fn task_spawn_count_tracking() {
        let lm = LiveMetrics::new();
        assert!(lm.task_spawn_counts().is_empty());

        lm.record_task_spawn("migrate");
        lm.record_task_spawn("migrate");
        lm.record_task_spawn("backup");

        let counts = lm.task_spawn_counts();
        assert_eq!(counts["migrate"], 2);
        assert_eq!(counts["backup"], 1);
    }

    #[test]
    fn task_duration_histogram() {
        let lm = LiveMetrics::new();

        // Record a 2-second task and a 45-second task.
        lm.record_task_duration("migrate", Duration::from_secs(2));
        lm.record_task_duration("migrate", Duration::from_secs(45));

        let snaps = lm.task_duration_snapshots();
        assert_eq!(snaps.len(), 1);
        let s = &snaps[0];
        assert_eq!(s.name, "migrate");
        assert_eq!(s.count, 2);
        assert!((s.sum_seconds - 47.0).abs() < 0.001);

        // 2s → bucket[1] (5.0), 45s → bucket[3] (60.0)
        // Cumulative: bucket[0](1.0)=0, bucket[1](5.0)=1, bucket[2](30.0)=1, bucket[3](60.0)=2
        assert_eq!(s.buckets[0].1, 0); // <=1s
        assert_eq!(s.buckets[1].1, 1); // <=5s
        assert_eq!(s.buckets[2].1, 1); // <=30s
        assert_eq!(s.buckets[3].1, 2); // <=60s
        assert_eq!(s.buckets[8].1, 2); // +Inf
    }
}
