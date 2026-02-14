use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use mill_config::{AllocId, AllocKind, AllocStatus, NodeId, NodeStatus, ServiceDef};
use mill_net::Mesh;
use mill_raft::MillRaft;
use mill_raft::fsm::command::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::deploy::{self, DeployCtx};
use super::health::HealthPoller;
use super::report::LiveMetrics;
use crate::rpc::CommandRouter;
use crate::storage::Volumes;

/// Tracks the last heartbeat timestamp for each node (by raft_id).
///
/// Shared between the report processor (writer) and the watchdog (reader).
/// Uses `parking_lot::Mutex` because operations are fast HashMap lookups/inserts
/// that are never held across await points.
#[derive(Clone)]
pub struct HeartbeatTracker {
    inner: Arc<parking_lot::Mutex<HashMap<u64, Instant>>>,
}

impl HeartbeatTracker {
    pub fn new() -> Self {
        Self { inner: Arc::new(parking_lot::Mutex::new(HashMap::new())) }
    }

    /// Record a heartbeat for the given raft node ID.
    pub fn record(&self, raft_id: u64) {
        self.inner.lock().insert(raft_id, Instant::now());
    }

    /// Get the last heartbeat time for a node.
    pub fn last_seen(&self, raft_id: u64) -> Option<Instant> {
        self.inner.lock().get(&raft_id).copied()
    }

    /// Remove tracking for a node that is no longer in the FSM.
    pub fn remove(&self, raft_id: u64) {
        self.inner.lock().remove(&raft_id);
    }
}

/// Per-service restart backoff state.
struct RestartState {
    attempts: usize,
    last_attempt_at: Instant,
}

/// Configuration for the watchdog background task.
pub struct WatchdogConfig {
    pub raft: Arc<MillRaft>,
    pub router: CommandRouter,
    pub volumes: Option<Arc<dyn Volumes>>,
    pub health: Arc<HealthPoller>,
    pub heartbeat_tracker: HeartbeatTracker,
    pub live_metrics: LiveMetrics,
    pub wireguard: Option<Arc<dyn Mesh>>,
    pub cancel: CancellationToken,
}

/// Spawn the watchdog background task.
///
/// The watchdog runs on a 1-second tick and handles:
/// 1. Heartbeat timeout detection — proposes `NodeDown` for silent nodes.
/// 2. Service restart with backoff — re-dispatches under-replicated services.
pub fn spawn_watchdog(config: WatchdogConfig) {
    let WatchdogConfig {
        raft,
        router,
        volumes,
        health,
        heartbeat_tracker,
        live_metrics,
        wireguard,
        cancel,
    } = config;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(crate::env::watchdog_tick());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // raft_id → when we first discovered this node (for grace period)
        let mut node_discovered: HashMap<u64, Instant> = HashMap::new();
        // service_name → restart backoff state
        let mut restart_backoff: HashMap<String, RestartState> = HashMap::new();
        // raft_id → status from previous tick (for detecting Down → Ready)
        let mut prev_statuses: HashMap<u64, NodeStatus> = HashMap::new();

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cancel.cancelled() => break,
            }

            // === Phase 1: Heartbeat timeout → NodeDown ===
            check_heartbeats(
                &raft,
                &heartbeat_tracker,
                &wireguard,
                &mut node_discovered,
                &mut restart_backoff,
            )
            .await;

            // === Phase 1.5: Re-add WireGuard peers for recovered nodes ===
            reconcile_wireguard_peers(&raft, &wireguard, &mut prev_statuses).await;

            // === Phase 2: Service restart with backoff ===
            check_service_restarts(
                &raft,
                &router,
                &volumes,
                &health,
                &live_metrics,
                &mut restart_backoff,
            )
            .await;
        }

        tracing::info!("watchdog: cancelled, exiting");
    });
}

/// Phase 1: Detect nodes that have missed heartbeats and propose NodeDown.
async fn check_heartbeats(
    raft: &MillRaft,
    tracker: &HeartbeatTracker,
    wireguard: &Option<Arc<dyn Mesh>>,
    node_discovered: &mut HashMap<u64, Instant>,
    restart_backoff: &mut HashMap<String, RestartState>,
) {
    let now = Instant::now();

    // Read current node set from FSM.
    let nodes: Vec<(u64, NodeId, mill_config::NodeStatus)> = raft.read_state(|fsm| {
        fsm.nodes.values().map(|n| (n.raft_id, n.mill_id.clone(), n.status.clone())).collect()
    });

    let current_raft_ids: std::collections::HashSet<u64> =
        nodes.iter().map(|(id, _, _)| *id).collect();

    // Track newly discovered nodes; clean up removed ones.
    for &(raft_id, _, _) in &nodes {
        node_discovered.entry(raft_id).or_insert(now);
    }
    node_discovered.retain(|id, _| {
        let keep = current_raft_ids.contains(id);
        if !keep {
            tracker.remove(*id);
        }
        keep
    });

    for (raft_id, mill_id, status) in &nodes {
        if *status != mill_config::NodeStatus::Ready {
            continue;
        }

        let last_seen = tracker.last_seen(*raft_id);
        let discovered_at = node_discovered.get(raft_id).copied().unwrap_or(now);

        // Effective last-seen: actual heartbeat or discovery time (grace period).
        let effective = last_seen.unwrap_or(discovered_at);

        if now.duration_since(effective) < crate::env::heartbeat_timeout() {
            continue;
        }

        tracing::warn!(
            "watchdog: node {} (raft_id={}) missed heartbeat for {:?}, proposing NodeDown",
            mill_id.0,
            raft_id,
            now.duration_since(effective),
        );

        // Read WireGuard pubkey before proposing NodeDown (FSM may not change
        // the node entry, but we want the key for peer removal).
        let wg_pubkey =
            raft.read_state(|fsm| fsm.nodes.get(raft_id).and_then(|n| n.wireguard_pubkey.clone()));

        if let Err(e) = raft.propose(Command::NodeDown { id: *raft_id }).await {
            tracing::warn!("watchdog: failed to propose NodeDown for raft_id={}: {e}", raft_id);
            continue;
        }

        // Remove WireGuard peer if configured.
        if let (Some(wg), Some(pubkey)) = (wireguard, &wg_pubkey)
            && let Err(e) = wg.remove_peer(pubkey).await
        {
            tracing::warn!(
                "watchdog: failed to remove WireGuard peer for raft_id={}: {e}",
                raft_id
            );
        }

        // Phase 1b: Mark allocs on the downed node as Failed.
        mark_node_allocs_failed(raft, mill_id, restart_backoff).await;
    }
}

/// Re-add WireGuard peers for nodes that recovered (Down → Ready).
async fn reconcile_wireguard_peers(
    raft: &MillRaft,
    wireguard: &Option<Arc<dyn Mesh>>,
    prev_statuses: &mut HashMap<u64, NodeStatus>,
) {
    let wg = match wireguard {
        Some(wg) => wg,
        None => {
            // Still snapshot statuses so transitions are tracked even without WireGuard.
            snapshot_statuses(raft, prev_statuses);
            return;
        }
    };

    let recovered: Vec<mill_net::PeerInfo> = raft.read_state(|fsm| {
        fsm.nodes
            .values()
            .filter_map(|n| {
                if prev_statuses.get(&n.raft_id) == Some(&NodeStatus::Down)
                    && n.status == NodeStatus::Ready
                {
                    n.wireguard_pubkey.as_ref().map(|pubkey| mill_net::PeerInfo {
                        node_id: n.mill_id.clone(),
                        public_key: pubkey.clone(),
                        endpoint: None,
                        tunnel_ip: n.address.ip(),
                    })
                } else {
                    None
                }
            })
            .collect()
    });

    for peer in recovered {
        if let Err(e) = wg.add_peer(&peer).await {
            tracing::warn!("watchdog: failed to re-add WireGuard peer: {e}");
        }
    }

    snapshot_statuses(raft, prev_statuses);
}

/// Snapshot current node statuses for the next watchdog tick.
fn snapshot_statuses(raft: &MillRaft, prev_statuses: &mut HashMap<u64, NodeStatus>) {
    let nodes = raft.read_state(|fsm| {
        fsm.nodes.values().map(|n| (n.raft_id, n.status.clone())).collect::<Vec<_>>()
    });
    prev_statuses.clear();
    for (id, status) in nodes {
        prev_statuses.insert(id, status);
    }
}

/// After a NodeDown, mark all active allocs on that node as Failed.
async fn mark_node_allocs_failed(
    raft: &MillRaft,
    mill_id: &NodeId,
    restart_backoff: &mut HashMap<String, RestartState>,
) {
    let allocs: Vec<(AllocId, AllocKind, String)> = raft.read_state(|fsm| {
        fsm.allocs
            .values()
            .filter(|a| a.node == *mill_id && !a.status.is_terminal())
            .map(|a| (a.id.clone(), a.kind.clone(), a.name.clone()))
            .collect()
    });

    for (alloc_id, kind, name) in allocs {
        let reason = "node down".to_string();
        if let Err(e) = raft
            .propose(Command::AllocStatus {
                alloc_id: alloc_id.clone(),
                status: AllocStatus::Failed { reason },
            })
            .await
        {
            tracing::warn!(
                "watchdog: failed to propose AllocStatus::Failed for {}: {e}",
                alloc_id.0
            );
        }

        // For services, reset backoff so restart picks them up quickly.
        if kind == AllocKind::Service {
            restart_backoff.remove(&name);
        }
    }
}

/// Phase 2: Restart under-replicated services with exponential backoff.
async fn check_service_restarts(
    raft: &Arc<MillRaft>,
    router: &CommandRouter,
    volumes: &Option<Arc<dyn Volumes>>,
    health: &Arc<HealthPoller>,
    live_metrics: &LiveMetrics,
    restart_backoff: &mut HashMap<String, RestartState>,
) {
    let now = Instant::now();

    // Read desired services from config.
    let services: Vec<(String, ServiceDef)> = raft.read_state(|fsm| {
        fsm.config
            .as_ref()
            .map(|c| c.services.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    });

    // Clean up backoff for services no longer in config.
    let service_names: std::collections::HashSet<&str> =
        services.iter().map(|(n, _)| n.as_str()).collect();
    restart_backoff.retain(|name, _| service_names.contains(name.as_str()));

    for (name, def) in &services {
        // Count allocs by status.
        let (active, non_terminal) = raft.read_state(|fsm| {
            let allocs = fsm.list_service_allocs(name);
            let active = allocs.iter().filter(|a| a.status.is_active()).count() as u32;
            let non_terminal = allocs.iter().filter(|a| !a.status.is_terminal()).count() as u32;
            (active, non_terminal)
        });

        if active >= def.replicas {
            // All replicas healthy — reset backoff.
            restart_backoff.remove(name);
            continue;
        }

        if non_terminal >= def.replicas {
            // Enough allocs in the pipeline (Pulling/Starting/Running) — don't
            // double-dispatch while a deploy or previous restart is in progress.
            continue;
        }

        // Check backoff. The first time we see a service under-replicated we
        // only record the timestamp — this gives an in-progress deploy time to
        // finish placing its remaining replicas before the watchdog interferes.
        let state = restart_backoff.get(name);
        match state {
            None => {
                restart_backoff
                    .insert(name.clone(), RestartState { attempts: 0, last_attempt_at: now });
                continue;
            }
            Some(s) => {
                let backoff = crate::env::restart_backoff();
                let idx = s.attempts.min(backoff.len() - 1);
                let delay = backoff[idx];
                if now.duration_since(s.last_attempt_at) < delay {
                    continue;
                }
            }
        }

        // Update backoff state.
        let attempts = state.map(|s| s.attempts + 1).unwrap_or(0);
        restart_backoff.insert(name.clone(), RestartState { attempts, last_attempt_at: now });

        let needed = def.replicas - active;
        tracing::info!(
            "watchdog: service '{}' under-replicated ({}/{} active), restarting {} replica(s) (attempt {})",
            name,
            active,
            def.replicas,
            needed,
            attempts,
        );

        // Record the restart for metrics.
        live_metrics.record_restart(name);

        // Spawn a non-blocking restart task for each needed replica.
        for _ in 0..needed {
            let raft = Arc::clone(raft);
            let router = router.clone();
            let volumes = volumes.clone();
            let health = Arc::clone(health);
            let name = name.clone();
            let def = def.clone();

            tokio::spawn(async move {
                let alloc_id = AllocId(format!("{}-{}", name, crate::util::rand_suffix()));
                let (progress_tx, _) = mpsc::channel(1);
                let ctx = DeployCtx {
                    raft,
                    router,
                    volumes,
                    health,
                    progress: progress_tx,
                    started: Instant::now(),
                };
                if let Err(e) = deploy::start_alloc(&ctx, &name, &def, &alloc_id, 0).await {
                    tracing::warn!("watchdog: failed to restart service '{}': {e}", name);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn heartbeat_tracker_record_and_read() {
        let tracker = HeartbeatTracker::new();

        assert!(tracker.last_seen(1).is_none());

        tracker.record(1);
        let t1 = tracker.last_seen(1);
        assert!(t1.is_some());

        // Second record updates the timestamp.
        std::thread::sleep(Duration::from_millis(5));
        tracker.record(1);
        let t2 = tracker.last_seen(1).unwrap();
        assert!(t2 > t1.unwrap());
    }

    #[test]
    fn heartbeat_tracker_remove() {
        let tracker = HeartbeatTracker::new();
        tracker.record(1);
        assert!(tracker.last_seen(1).is_some());

        tracker.remove(1);
        assert!(tracker.last_seen(1).is_none());
    }

    #[test]
    fn heartbeat_tracker_multiple_nodes() {
        let tracker = HeartbeatTracker::new();
        tracker.record(1);
        tracker.record(2);
        tracker.record(3);

        assert!(tracker.last_seen(1).is_some());
        assert!(tracker.last_seen(2).is_some());
        assert!(tracker.last_seen(3).is_some());
        assert!(tracker.last_seen(4).is_none());

        tracker.remove(2);
        assert!(tracker.last_seen(1).is_some());
        assert!(tracker.last_seen(2).is_none());
        assert!(tracker.last_seen(3).is_some());
    }

    #[test]
    fn backoff_schedule() {
        let backoff = crate::env::restart_backoff();
        // Verify the backoff constants match the plan.
        assert_eq!(backoff[0], Duration::from_secs(1));
        assert_eq!(backoff[1], Duration::from_secs(5));
        assert_eq!(backoff[2], Duration::from_secs(30));
        assert_eq!(backoff[3], Duration::from_secs(60));

        // Index clamped to len-1 for attempts beyond the array.
        for attempts in 4..10 {
            let idx = attempts.min(backoff.len() - 1);
            assert_eq!(backoff[idx], Duration::from_secs(60));
        }
    }

    #[test]
    fn restart_state_backoff_not_elapsed() {
        let backoff = crate::env::restart_backoff();
        let now = Instant::now();
        let state = RestartState { attempts: 0, last_attempt_at: now };

        // With 0 attempts, backoff is 1s — should not be elapsed immediately.
        let idx = state.attempts.min(backoff.len() - 1);
        let delay = backoff[idx];
        assert!(now.duration_since(state.last_attempt_at) < delay);
    }

    #[test]
    fn heartbeat_tracker_clone_shares_state() {
        let tracker = HeartbeatTracker::new();
        let clone = tracker.clone();

        tracker.record(1);
        assert!(clone.last_seen(1).is_some());

        clone.record(2);
        assert!(tracker.last_seen(2).is_some());
    }
}
