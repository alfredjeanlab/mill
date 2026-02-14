pub mod api;
pub mod deploy;
pub mod dns;
pub mod health;
pub mod logs;
pub mod nodes;
pub mod proxy;
pub(crate) mod report;
pub mod scheduler;
pub mod tasks;
pub(crate) mod watchdog;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use mill_config::NodeId;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use mill_net::DnsServer;
use mill_proxy::ProxyServer;
use mill_raft::MillRaft;
use mill_raft::fsm::FsmState;

use crate::error::NodeError;
use crate::rpc::{CommandRouter, NodeReport};
use crate::secondary::SecondaryHandle;
use crate::storage::Volumes;

/// Configuration for the primary node.
pub struct PrimaryConfig {
    pub api_addr: SocketAddr,
    pub auth_token: String,
    /// Token for authenticating RPC connections to remote secondaries.
    pub rpc_token: String,
    /// This node's mill ID (if running a co-located secondary).
    pub local_node_id: Option<NodeId>,
    /// Optional volume driver for persistent storage.
    pub volume_driver: Option<Arc<dyn Volumes>>,
    /// Optional DNS server for service discovery.
    pub dns: Option<Arc<DnsServer>>,
    /// Optional reverse proxy for route management.
    pub proxy: Option<Arc<ProxyServer>>,
    /// Optional WireGuard peer manager for peer removal/re-add on node failure/recovery.
    pub wireguard: Option<Arc<dyn mill_net::Mesh>>,
    /// Optional HTTP Raft network for updating peer maps on join.
    pub raft_network: Option<mill_raft::network::http::HttpNetwork>,
}

/// The primary node runtime: API server backed by Raft.
pub struct Primary {
    api_addr: SocketAddr,
    router: CommandRouter,
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Primary {
    /// Start the primary node, binding the API server.
    ///
    /// Accepts an optional `SecondaryHandle` for in-process command/report
    /// channels (single-node mode). When `None`, the primary runs without a
    /// local secondary (pure primary).
    ///
    /// All secondaries (local and remote) communicate through the
    /// `CommandRouter` and a fan-in report channel.
    pub async fn start(
        config: PrimaryConfig,
        raft: Arc<MillRaft>,
        local_secondary: Option<SecondaryHandle>,
        cancel: CancellationToken,
    ) -> Result<Self, NodeError> {
        // Fan-in report channel: all secondaries feed into this.
        let (report_tx, report_rx) = mpsc::channel::<NodeReport>(256);

        // Create command router.
        let cmd_router = CommandRouter::new();

        // Register local secondary if present.
        if let Some(handle) = local_secondary {
            if let Some(ref node_id) = config.local_node_id {
                cmd_router.register(node_id.clone(), handle.cmd_tx().clone()).await;
            }

            // Spawn task to drain local secondary reports into the fan-in channel.
            let fan_in_tx = report_tx.clone();
            tokio::spawn(async move {
                let mut handle = handle;
                while let Some(report) = handle.recv().await {
                    if fan_in_tx.send(report).await.is_err() {
                        break;
                    }
                }
            });
        }

        // Spawn connection watcher to auto-connect/disconnect remote nodes.
        spawn_connection_watcher(
            Arc::clone(&raft),
            cmd_router.clone(),
            report_tx.clone(),
            config.rpc_token.clone(),
            config.local_node_id.clone(),
            cancel.clone(),
        );

        // Create log subscribers shared between report processor and API.
        let log_subscribers = report::new_log_subscribers();

        // Create heartbeat tracker shared between report processor and watchdog.
        let heartbeat_tracker = watchdog::HeartbeatTracker::new();

        // Create live metrics store shared between report processor and API.
        let live_metrics = report::LiveMetrics::new();

        // Spawn report processor to drain the fan-in channel → FSM proposals.
        report::spawn_report_processor(
            Arc::clone(&raft),
            report_rx,
            log_subscribers.clone(),
            heartbeat_tracker.clone(),
            live_metrics.clone(),
        );

        // Spawn DNS watcher if configured.
        if let Some(dns) = &config.dns {
            dns::spawn_dns_watcher(Arc::clone(&raft), Arc::clone(dns), cancel.clone());
        }

        // Spawn proxy watcher if configured.
        if let Some(proxy) = &config.proxy {
            proxy::spawn_proxy_watcher(Arc::clone(&raft), Arc::clone(proxy), cancel.clone());
        }

        let health = Arc::new(health::HealthPoller::new());

        // Spawn watchdog for heartbeat timeout detection and service restart.
        watchdog::spawn_watchdog(watchdog::WatchdogConfig {
            raft: Arc::clone(&raft),
            router: cmd_router.clone(),
            volumes: config.volume_driver.clone(),
            health: Arc::clone(&health),
            heartbeat_tracker,
            live_metrics: live_metrics.clone(),
            wireguard: config.wireguard.clone(),
            cancel: cancel.clone(),
        });

        let proxy_metrics = config.proxy.as_ref().map(|p| p.metrics().clone());

        let state = api::AppState {
            raft,
            auth_token: config.auth_token,
            router: cmd_router.clone(),
            health,
            volumes: config.volume_driver,
            log_subscribers,
            live_metrics,
            proxy_metrics,
            wireguard: config.wireguard.clone(),
            raft_network: config.raft_network,
        };
        let api_router = api::router(state);

        let listener = TcpListener::bind(config.api_addr)
            .await
            .map_err(|source| NodeError::Bind { address: config.api_addr.to_string(), source })?;
        let api_addr = listener
            .local_addr()
            .map_err(|source| NodeError::Bind { address: config.api_addr.to_string(), source })?;

        let shutdown = cancel.clone();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, api_router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await;
        });

        Ok(Self { api_addr, router: cmd_router, handle, cancel })
    }

    /// The address the API server is listening on.
    pub fn api_addr(&self) -> SocketAddr {
        self.api_addr
    }

    /// The command router for sending commands to nodes.
    pub fn router(&self) -> &CommandRouter {
        &self.router
    }

    /// Stop the primary node.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

/// Spawn a background task that watches FSM changes and applies a derived value.
///
/// On startup and after every FSM change, calls `compute` to derive a value
/// from the current state, then passes it to `apply`.
fn spawn_fsm_watcher<T, F, Fut>(
    raft: Arc<MillRaft>,
    compute: fn(&FsmState) -> T,
    mut apply: F,
    label: &'static str,
    cancel: CancellationToken,
) where
    T: Send + 'static,
    F: FnMut(T) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let mut rx = raft.subscribe_changes();

    tokio::spawn(async move {
        let value = raft.read_state(compute);
        apply(value).await;

        loop {
            tokio::select! {
                result = rx.changed() => {
                    if result.is_err() {
                        break;
                    }
                    let value = raft.read_state(compute);
                    apply(value).await;
                }
                _ = cancel.cancelled() => break,
            }
        }
        tracing::debug!("{label} watcher exiting");
    });
}

/// Spawns a background task that watches FSM state changes and automatically
/// connects to new remote nodes and disconnects from removed ones.
fn spawn_connection_watcher(
    raft: Arc<MillRaft>,
    router: CommandRouter,
    report_tx: mpsc::Sender<NodeReport>,
    rpc_token: String,
    local_node_id: Option<NodeId>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut changes = raft.subscribe_changes();
        let mut connected: HashSet<NodeId> = HashSet::new();

        // Closure to read current node set and reconcile connections.
        let reconcile =
            |raft: &Arc<MillRaft>, local_node_id: &Option<NodeId>| -> Vec<(NodeId, SocketAddr)> {
                raft.read_state(|fsm| {
                    fsm.nodes
                        .values()
                        .filter(|n| {
                            // Skip local node — it's registered directly via SecondaryHandle.
                            if let Some(local) = local_node_id
                                && &n.mill_id == local
                            {
                                return false;
                            }
                            true
                        })
                        .map(|n| (n.mill_id.clone(), n.rpc_address.unwrap_or(n.address)))
                        .collect()
                })
            };

        // Push current state immediately so connections are established from
        // startup, not just after the first FSM change (crash recovery).
        let current_nodes = reconcile(&raft, &local_node_id);
        for (node_id, addr) in &current_nodes {
            let conn_cancel = cancel.child_token();
            if let Err(e) = router
                .connect(node_id.clone(), *addr, rpc_token.clone(), report_tx.clone(), conn_cancel)
                .await
            {
                tracing::warn!("failed to connect to node {}: {e}", node_id.0);
                continue;
            }
            connected.insert(node_id.clone());
            tracing::info!("connected to remote node {}", node_id.0);
        }

        loop {
            tokio::select! {
                result = changes.changed() => {
                    if result.is_err() {
                        break; // watch sender dropped
                    }
                }
                _ = cancel.cancelled() => break,
            }

            let current_nodes = reconcile(&raft, &local_node_id);
            let current_ids: HashSet<NodeId> =
                current_nodes.iter().map(|(id, _)| id.clone()).collect();

            // Connect to new nodes.
            for (node_id, addr) in &current_nodes {
                if !connected.contains(node_id) {
                    let conn_cancel = cancel.child_token();
                    if let Err(e) = router
                        .connect(
                            node_id.clone(),
                            *addr,
                            rpc_token.clone(),
                            report_tx.clone(),
                            conn_cancel,
                        )
                        .await
                    {
                        tracing::warn!("failed to connect to node {}: {e}", node_id.0);
                        continue;
                    }
                    connected.insert(node_id.clone());
                    tracing::info!("connected to remote node {}", node_id.0);
                }
            }

            // Disconnect removed nodes.
            let removed: Vec<NodeId> = connected.difference(&current_ids).cloned().collect();
            for node_id in removed {
                router.disconnect(&node_id).await;
                connected.remove(&node_id);
                tracing::info!("disconnected from removed node {}", node_id.0);
            }
        }
    });
}

impl Drop for Primary {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
