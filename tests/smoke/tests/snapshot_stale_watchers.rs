// Smoke test: Snapshot Stale Watchers (docs/workflows/11-snapshot-stale-watchers.md)
//
// Tests the fix for: install_snapshot not calling change_tx.send(), causing
// DNS/proxy watchers to never be notified after a snapshot install.
// Fix: crates/raft/src/fsm/mod.rs - install_snapshot now bumps version and
// sends change notification, matching the apply() path.

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_snapshot_stale_watchers";

/// Minimal service config: one service with replicas=1, just enough for
/// an alloc with an address that produces a DNS record.
///
/// Uses busybox httpd on the dynamically-allocated PORT (injected as env var)
/// so it binds a non-conflicting port inside the node's network namespace.
const CONFIG: &str = r#"
service "web" {
  image    = "docker.io/library/busybox:latest"
  command  = ["/bin/sh", "-c", "httpd -f -p $PORT"]
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "64M"
}
"#;

/// Augment fast_env() with snapshot settings so the leader compacts its log
/// into a snapshot after every committed entry, guaranteeing the joiner calls
/// install_snapshot() rather than replaying individual log entries.
///
/// Not added to fast_env() globally to keep other smoke tests unaffected.
fn snapshot_env() -> Vec<(&'static str, &'static str)> {
    let mut env = smoke::fast_env();
    env.push(("MILL_SNAPSHOT_THRESHOLD", "1"));
    env.push(("MILL_SNAPSHOT_LOG_KEEP", "0"));
    env
}

/// Verify that DNS resolves on a node that received state via snapshot install.
///
/// Previously, `install_snapshot()` replaced the FSM state but never called
/// `self.change_tx.send(version)`. DNS/proxy watchers subscribe via
/// `raft.subscribe_changes()` and only recompute when the watch channel fires.
/// A node joining via snapshot would have a populated FSM but empty DNS
/// indefinitely in a quiescent cluster.
///
/// This test verifies the fix: `install_snapshot` now bumps `inner.version`
/// and sends the change notification, matching the `apply()` path.
#[tokio::test]
async fn snapshot_install_notifies_dns_watcher() {
    require_harness!();
    smoke::reset_node_counter();

    // 1. Bootstrap a single-node cluster with aggressive snapshot compaction.
    let leader = MillNode::init_with(TOKEN, &snapshot_env()).await;
    let leader_addr = format!("http://{}", leader.api_addr);

    // 2. Deploy a service so the FSM has an active alloc with an address.
    let sse = leader.api().deploy(CONFIG).await.expect("deploy");
    assert!(sse.contains("done"), "deploy SSE: {sse}");

    // 3. Wait until the service is healthy (alloc has an address in the FSM).
    poll_async(|| async {
        let svcs = Api::new(leader.api_addr, TOKEN).services().await.unwrap_or_default();
        svcs.iter().any(|s| s.name == "web" && s.replicas.healthy >= 1)
    })
    .secs(60)
    .expect("service healthy on leader")
    .await;

    // 4. Join a second node. Because MILL_SNAPSHOT_THRESHOLD=1 and the
    //    leader has at least one committed log entry, the leader has already
    //    compacted its log into a snapshot. The joiner's openraft will call
    //    install_snapshot() instead of replaying individual log entries.
    let joiner = MillNode::join_with(&leader_addr, TOKEN, &snapshot_env()).await;

    // 5. Confirm the joiner has fully processed the snapshot: its FSM
    //    reports 2 nodes, proving install_snapshot() completed.
    poll_async(|| async {
        let Ok(status) = Api::new(joiner.api_addr, TOKEN).status().await else {
            return false;
        };
        status.node_count == 2
    })
    .secs(15)
    .expect("joiner FSM populated via snapshot (node_count == 2)")
    .await;

    // 6. The cluster is now quiescent -- no new log entries will arrive.
    //    The joiner's DNS watcher must be triggered by install_snapshot()
    //    calling change_tx.send(). Without the fix, the DNS watcher would
    //    never fire and "web.mill." would remain unresolvable.
    poll_async(|| async { joiner.dns_resolve("web.mill.").await })
        .secs(5)
        .expect("joiner DNS should resolve 'web.mill.' after snapshot install")
        .await;
}
