use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mill_config::{AllocId, AllocStatus, ContainerStats, NodeId, NodeResources};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use mill_node::rpc::{Heartbeat, NodeCommand, NodeReport};

/// A channel-driven fake secondary that handles `NodeCommand` variants and
/// sends `NodeReport` responses. Replaces the real `Secondary` (which needs
/// containerd) for integration testing.
pub struct TestSecondary {
    // NOTE(lifetime): Keep the command loop alive; dropped when TestSecondary drops.
    _cmd_cancel: CancellationToken,
    heartbeat_cancel: CancellationToken,
    state: Arc<TestSecondaryState>,
    // Stored for respawning the heartbeat loop.
    report_tx: mpsc::Sender<NodeReport>,
    node_id: NodeId,
    resources: NodeResources,
}

struct TestSecondaryState {
    inner: Mutex<TestSecondaryInner>,
}

struct TestSecondaryInner {
    /// Whether the next Run should fail.
    fail_next_run: Option<String>,
    /// Currently "running" alloc IDs.
    running: HashSet<AllocId>,
    /// TCP listeners kept alive so health probes can connect.
    listeners: HashMap<AllocId, std::net::TcpListener>,
    /// Port counter for generating host_port values.
    port_counter: u16,
}

impl TestSecondary {
    /// Start a simulated secondary actor on the given channels.
    ///
    /// The other halves of these channels go to `SecondaryHandle::from_parts()`.
    pub fn start(
        cmd_rx: mpsc::Receiver<NodeCommand>,
        report_tx: mpsc::Sender<NodeReport>,
        node_id: NodeId,
        resources: NodeResources,
    ) -> Self {
        let cmd_cancel = CancellationToken::new();
        let heartbeat_cancel = CancellationToken::new();
        let state = Arc::new(TestSecondaryState {
            inner: Mutex::new(TestSecondaryInner {
                fail_next_run: None,
                running: HashSet::new(),
                listeners: HashMap::new(),
                port_counter: 30000,
            }),
        });

        // Spawn command handler.
        let cc = cmd_cancel.clone();
        let cmd_state = Arc::clone(&state);
        let cmd_report_tx = report_tx.clone();
        tokio::spawn(async move {
            Self::command_loop(cmd_rx, cmd_report_tx, cmd_state, cc).await;
        });

        // Spawn heartbeat loop.
        let hb_cancel = heartbeat_cancel.clone();
        let hb_state = Arc::clone(&state);
        let hb_tx = report_tx.clone();
        let hb_node_id = node_id.clone();
        let hb_resources = resources.clone();
        tokio::spawn(async move {
            Self::heartbeat_loop(hb_tx, hb_state, hb_node_id, hb_resources, hb_cancel).await;
        });

        Self { _cmd_cancel: cmd_cancel, heartbeat_cancel, state, report_tx, node_id, resources }
    }

    /// Stop heartbeats only (simulates heartbeat failure / network partition).
    ///
    /// The command loop stays alive so RPCs still work.
    pub fn stop_heartbeats(&mut self) {
        self.heartbeat_cancel.cancel();
    }

    /// Resume heartbeats after a previous `stop_heartbeats`.
    ///
    /// Creates a fresh cancellation token and spawns a new heartbeat loop.
    pub fn resume_heartbeats(&mut self) {
        let cancel = CancellationToken::new();
        self.heartbeat_cancel = cancel.clone();

        let state = Arc::clone(&self.state);
        let report_tx = self.report_tx.clone();
        let node_id = self.node_id.clone();
        let resources = self.resources.clone();
        tokio::spawn(async move {
            Self::heartbeat_loop(report_tx, state, node_id, resources, cancel).await;
        });
    }

    /// Make the next `Run` command respond with `RunFailed`.
    pub fn fail_next_run(&self, reason: impl Into<String>) {
        self.state.inner.lock().fail_next_run = Some(reason.into());
    }

    async fn command_loop(
        mut cmd_rx: mpsc::Receiver<NodeCommand>,
        report_tx: mpsc::Sender<NodeReport>,
        state: Arc<TestSecondaryState>,
        cancel: CancellationToken,
    ) {
        loop {
            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => c,
                    None => break,
                },
                _ = cancel.cancelled() => break,
            };

            match cmd {
                NodeCommand::Run { alloc_id, spec: _, .. } => {
                    // Check if we should fail.
                    let fail = state.inner.lock().fail_next_run.take();
                    if let Some(reason) = fail {
                        let _ = report_tx.send(NodeReport::RunFailed { alloc_id, reason }).await;
                        continue;
                    }

                    // Simulate short startup delay.
                    tokio::time::sleep(Duration::from_millis(50)).await;

                    // Bind a real TCP listener so the primary's health probe
                    // can connect. Using port 0 lets the OS pick a free port.
                    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                    let address = listener.local_addr().unwrap();

                    let host_port = {
                        let mut inner = state.inner.lock();
                        let port = inner.port_counter;
                        inner.port_counter += 1;
                        inner.running.insert(alloc_id.clone());
                        inner.listeners.insert(alloc_id.clone(), listener);
                        port
                    };

                    let _ = report_tx
                        .send(NodeReport::RunStarted {
                            alloc_id,
                            host_port: Some(host_port),
                            address: Some(address),
                        })
                        .await;
                }
                NodeCommand::Stop { alloc_id } => {
                    {
                        let mut inner = state.inner.lock();
                        inner.running.remove(&alloc_id);
                        inner.listeners.remove(&alloc_id);
                    }
                    let _ = report_tx.send(NodeReport::Stopped { alloc_id, exit_code: 0 }).await;
                }
                NodeCommand::Pull { image, .. } => {
                    let _ = report_tx.send(NodeReport::PullComplete { image }).await;
                }
                NodeCommand::LogTail { alloc_id, .. } => {
                    let _ = report_tx.send(NodeReport::LogLines { alloc_id, lines: vec![] }).await;
                }
                NodeCommand::LogFollow { .. } | NodeCommand::LogUnfollow { .. } => {
                    // No-op for simulated secondary.
                }
            }
        }
    }

    async fn heartbeat_loop(
        report_tx: mpsc::Sender<NodeReport>,
        state: Arc<TestSecondaryState>,
        node_id: NodeId,
        resources: NodeResources,
        cancel: CancellationToken,
    ) {
        let mut interval = tokio::time::interval(mill_node::env::heartbeat_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cancel.cancelled() => break,
            }

            let (alloc_statuses, stats) = {
                let inner = state.inner.lock();
                let statuses: HashMap<AllocId, AllocStatus> =
                    inner.running.iter().map(|id| (id.clone(), AllocStatus::Running)).collect();
                let stats: Vec<ContainerStats> = inner
                    .running
                    .iter()
                    .map(|id| ContainerStats {
                        alloc_id: id.clone(),
                        cpu_usage: 0.1,
                        memory_usage: bytesize::ByteSize::mib(64),
                    })
                    .collect();
                (statuses, stats)
            };

            let hb = Heartbeat {
                node_id: node_id.clone(),
                alloc_statuses,
                resources: resources.clone(),
                stats,
                volumes: vec![],
            };

            if report_tx.send(NodeReport::Heartbeat(hb)).await.is_err() {
                break;
            }
        }
    }
}
