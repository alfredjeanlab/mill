mod heartbeat;
pub(crate) mod logs;
mod metrics;
mod reconcile;
mod runner;
pub(crate) mod state;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use mill_config::{AllocId, DataDir, NodeId};
use mill_containerd::{Containerd, Containers};
use mill_net::PortAllocator;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::metrics::MetricsCollector;
use self::runner::RunContext;
use self::state::AllocationMap;
use crate::error::Result;
use crate::rpc::{NodeCommand, NodeReport};

const CHANNEL_BUFFER: usize = 256;

/// Configuration for the secondary runtime.
pub struct SecondaryConfig {
    /// Containerd socket path.
    pub containerd_socket: String,
    /// Mill data directory root (e.g. `/var/lib/mill`).
    pub data_dir: DataDir,
    /// Allocation IDs expected to be running (for crash recovery reconciliation).
    pub expected_allocs: Vec<AllocId>,
    /// Identity of this node in the cluster.
    pub node_id: NodeId,
    /// WireGuard mesh IP for this node (used to construct allocation addresses).
    pub mesh_ip: Option<IpAddr>,
}

/// The secondary runtime — manages containers on a single node, receives
/// commands from the primary, and reports status back via heartbeats.
pub struct Secondary {
    ctx: RunContext,
    metrics: Arc<MetricsCollector>,
    cmd_rx: mpsc::Receiver<NodeCommand>,
    expected_allocs: Vec<AllocId>,
    log_follows: HashMap<AllocId, CancellationToken>,
    node_id: NodeId,
    mesh_ip: Option<IpAddr>,
}

/// Handle held by the primary to communicate with a secondary.
pub struct SecondaryHandle {
    cmd_tx: mpsc::Sender<NodeCommand>,
    report_rx: mpsc::Receiver<NodeReport>,
}

impl SecondaryHandle {
    /// Get a reference to the command sender (for cloning before consuming the handle).
    pub fn cmd_tx(&self) -> &mpsc::Sender<NodeCommand> {
        &self.cmd_tx
    }

    /// Receive the next report from the secondary, or None if the channel is closed.
    pub async fn recv(&mut self) -> Option<NodeReport> {
        self.report_rx.recv().await
    }

    /// Consume the handle, returning the report receiver.
    /// The command sender can be obtained via `cmd_tx()` before calling this.
    pub fn into_report_rx(self) -> mpsc::Receiver<NodeReport> {
        self.report_rx
    }

    /// Construct a handle from raw channel parts.
    pub fn from_parts(
        cmd_tx: mpsc::Sender<NodeCommand>,
        report_rx: mpsc::Receiver<NodeReport>,
    ) -> Self {
        Self { cmd_tx, report_rx }
    }
}

impl Secondary {
    /// Create a new secondary runtime, returning it alongside a handle for the primary.
    pub async fn new(
        config: SecondaryConfig,
        ports: Arc<PortAllocator>,
    ) -> Result<(Self, SecondaryHandle)> {
        let runtime: Arc<dyn Containers> =
            Containerd::connect(&config.containerd_socket, config.data_dir.clone())
                .await?
                .into_runtime();

        let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_BUFFER);
        let (report_tx, report_rx) = mpsc::channel(CHANNEL_BUFFER);

        let secondary = Self {
            ctx: RunContext { runtime, ports, allocs: Arc::new(AllocationMap::new()), report_tx },
            metrics: Arc::new(MetricsCollector::new()),
            cmd_rx,
            expected_allocs: config.expected_allocs,
            log_follows: HashMap::new(),
            node_id: config.node_id,
            mesh_ip: config.mesh_ip,
        };

        let handle = SecondaryHandle { cmd_tx, report_rx };
        Ok((secondary, handle))
    }

    /// Run the secondary event loop. Blocks until the command channel closes.
    ///
    /// On channel close, exits without stopping containers so that a restart
    /// can reconcile against still-running containers.
    pub async fn run(mut self) {
        // Step 1: Reconcile against expected allocs from crash recovery
        reconcile::reconcile(&self.ctx, &self.expected_allocs, self.mesh_ip).await;

        // Step 2: Spawn heartbeat loop
        let heartbeat_handle = tokio::spawn(heartbeat::heartbeat_loop(
            Arc::clone(&self.ctx.allocs),
            Arc::clone(&self.metrics),
            self.ctx.report_tx.clone(),
            self.node_id.clone(),
        ));

        // Step 3: Spawn metrics collector
        let cancel = tokio_util::sync::CancellationToken::new();
        let metrics_cancel = cancel.clone();
        let metrics_runtime = self.ctx.runtime.clone();
        let metrics_ref = Arc::clone(&self.metrics);
        let metrics_handle =
            tokio::spawn(async move { metrics_ref.run(metrics_runtime, metrics_cancel).await });

        // Step 4: Command loop
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                NodeCommand::Run { alloc_id, spec, auth } => {
                    let auth = auth.map(|c| mill_containerd::RegistryAuth {
                        username: c.username,
                        password: c.password,
                    });
                    if let Err(e) = runner::run_container(
                        &self.ctx,
                        alloc_id.clone(),
                        spec,
                        self.mesh_ip,
                        auth.as_ref(),
                    )
                    .await
                    {
                        tracing::error!("failed to run container {}: {e}", alloc_id.0);
                    }
                }
                NodeCommand::Stop { alloc_id } => {
                    if let Err(e) = runner::stop_container(&self.ctx, &alloc_id).await {
                        tracing::error!("failed to stop container {}: {e}", alloc_id.0);
                    }
                }
                NodeCommand::Pull { image, auth } => {
                    let auth = auth.map(|c| mill_containerd::RegistryAuth {
                        username: c.username,
                        password: c.password,
                    });
                    runner::pull_image(&self.ctx, &image, auth.as_ref()).await;
                }
                NodeCommand::LogTail { alloc_id, lines } => {
                    let log_lines = if let Some(buf) = self.ctx.allocs.log_buffer(&alloc_id).await {
                        buf.tail(lines)
                    } else {
                        vec![]
                    };
                    let _ = self
                        .ctx
                        .report_tx
                        .send(NodeReport::LogLines { alloc_id, lines: log_lines })
                        .await;
                }
                NodeCommand::LogFollow { alloc_id } => {
                    // Cancel any existing follow for this alloc.
                    if let Some(token) = self.log_follows.remove(&alloc_id) {
                        token.cancel();
                    }

                    if let Some(buf) = self.ctx.allocs.log_buffer(&alloc_id).await {
                        let token = CancellationToken::new();
                        self.log_follows.insert(alloc_id.clone(), token.clone());

                        let mut rx = buf.follow();
                        let report_tx = self.ctx.report_tx.clone();
                        let aid = alloc_id.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::select! {
                                    _ = token.cancelled() => break,
                                    result = rx.recv() => {
                                        match result {
                                            Ok(line) => {
                                                if report_tx
                                                    .send(NodeReport::LogLines {
                                                        alloc_id: aid.clone(),
                                                        lines: vec![line],
                                                    })
                                                    .await
                                                    .is_err()
                                                {
                                                    break; // report channel closed
                                                }
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                                break; // container exited
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                                continue; // skip lagged lines
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
                NodeCommand::LogUnfollow { alloc_id } => {
                    if let Some(token) = self.log_follows.remove(&alloc_id) {
                        token.cancel();
                    }
                }
            }
        }

        // Channel closed — shut down background tasks
        for (_, token) in self.log_follows.drain() {
            token.cancel();
        }
        cancel.cancel();
        heartbeat_handle.abort();
        metrics_handle.abort();
    }
}
