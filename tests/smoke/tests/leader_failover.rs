//! E2E smoke test: Leader Failover (docs/workflows/10-leader-failover.md)
//!
//! Raft leader crashes; surviving nodes elect a new leader and resume the API:
//!   1. 3-node cluster running
//!   2. Kill node-1 (leader) -- Raft heartbeats stop
//!   3. Election timeout (~5s) -- node-2 sends RequestVote, node-3 grants,
//!      node-2 becomes leader
//!   4. New leader rebuilds Primary: API server, scheduler, deploy coordinator
//!   5. NodeDown(node-1) committed, services on node-1 rescheduled
//!   6. Clients retry against node-2 or node-3 -- API works
//!   7. Old leader recovers: steps down, catches up via log replication

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_leader_failover";

/// Kill the Raft leader in a 3-node cluster and verify the surviving nodes
/// elect a new leader and resume serving the API.
#[tokio::test]
async fn leader_failover_new_leader_serves_api() {
    require_harness!();
    smoke::reset_node_counter();

    // ---------------------------------------------------------------
    // Step 1: Bootstrap a 3-node cluster
    // ---------------------------------------------------------------

    // Init node-1 as the initial leader.
    let node1 = MillNode::init(TOKEN).await;
    let leader_addr = format!("http://{}", node1.api_addr);

    // Join node-2 to the cluster.
    let node2 = MillNode::join(&leader_addr, TOKEN).await;

    // Join node-3 to the cluster.
    let node3 = MillNode::join(&leader_addr, TOKEN).await;

    // Verify the cluster has converged to 3 healthy nodes.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(status) = api.status().await else { return false };
            status.node_count == 3
        }
    })
    .secs(15)
    .expect("cluster should converge to 3 nodes")
    .await;

    let nodes_before = node1.api().nodes().await.expect("GET /v1/nodes on leader");
    assert_eq!(nodes_before.len(), 3, "expected 3 nodes before failover");
    for node in &nodes_before {
        assert_eq!(node.status, "ready", "node {} should be ready", node.id);
    }

    // Record node-2's and node-3's API addresses so we can query them after
    // node-1 is killed.
    let node2_addr = node2.api_addr;
    let node3_addr = node3.api_addr;

    // ---------------------------------------------------------------
    // Step 2: Kill the leader (node-1)
    // ---------------------------------------------------------------

    // Dropping the MillNode kills the child process, simulating a crash.
    // The Raft heartbeats from node-1 stop immediately.
    drop(node1);

    // ---------------------------------------------------------------
    // Steps 3-4: Election and Primary rebuild
    // ---------------------------------------------------------------
    // After the election timeout (~5s with fast env: heartbeat_timeout=2s),
    // one of the surviving nodes should win an election and rebuild the
    // Primary (API server, scheduler, deploy coordinator).

    // ---------------------------------------------------------------
    // Step 5-6: Verify the new leader serves the API
    // ---------------------------------------------------------------
    // Poll node-2's API for a status showing the cluster has detected node-1
    // as down. Either node-2 or node-3 may become the new leader; we try
    // both endpoints.

    poll_async(|| async move {
        // Try node-2 first, fall back to node-3.
        let api2 = Api::new(node2_addr, TOKEN);
        let api3 = Api::new(node3_addr, TOKEN);

        if let Ok(nodes) = api2.nodes().await {
            let healthy = nodes.iter().filter(|n| n.status == "ready").count();
            let down = nodes.iter().filter(|n| n.status == "down").count();
            if healthy == 2 && down == 1 {
                return true;
            }
        }

        if let Ok(nodes) = api3.nodes().await {
            let healthy = nodes.iter().filter(|n| n.status == "ready").count();
            let down = nodes.iter().filter(|n| n.status == "down").count();
            if healthy == 2 && down == 1 {
                return true;
            }
        }

        false
    })
    .secs(30)
    .expect("surviving nodes should elect a new leader and report 2 healthy + 1 down")
    .await;

    // Once a leader is available, verify the full cluster state via whichever
    // node is responding.
    let api = {
        let api2 = Api::new(node2_addr, TOKEN);
        if api2.status().await.is_ok() { api2 } else { Api::new(node3_addr, TOKEN) }
    };

    let status = api.status().await.expect("new leader should serve /v1/status");
    // The cluster still knows about 3 nodes (one is down, not removed).
    assert_eq!(status.node_count, 3, "cluster should still report 3 nodes total");

    let nodes_after = api.nodes().await.expect("new leader should serve /v1/nodes");
    assert_eq!(nodes_after.len(), 3, "all 3 nodes should still be listed");

    let healthy_count = nodes_after.iter().filter(|n| n.status == "ready").count();
    let down_count = nodes_after.iter().filter(|n| n.status == "down").count();
    assert_eq!(healthy_count, 2, "2 nodes should be healthy after leader failover");
    assert_eq!(down_count, 1, "1 node (old leader) should be marked down");

    // The down node should have zero allocations (its work was rescheduled).
    let down_node = nodes_after.iter().find(|n| n.status == "down").expect("a node should be down");
    assert_eq!(down_node.alloc_count, 0, "down node should have 0 allocations after reschedule");

    // Verify that node-2 and node-3 are both still accessible.
    let s2 = node2.api().status().await;
    let s3 = node3.api().status().await;
    assert!(s2.is_ok() || s3.is_ok(), "at least one surviving node should respond to API requests");
}
