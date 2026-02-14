//! E2E smoke test: Cluster Formation (docs/workflows/01-cluster-formation.md)
//!
//! Standing up a 3-node Mill cluster from scratch:
//! 1. `mill init` on node-1 — bootstraps single-node Raft, generates token
//! 2. `mill join` on node-2 and node-3 — authenticates, registers in FSM, mesh, Raft voter
//! 3. Verify: `mill status` shows 3 nodes, all healthy; heartbeats flowing

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_cluster_formation";

/// Bootstrap a single-node cluster and verify it reports healthy.
#[tokio::test]
async fn single_node_init_and_status() {
    require_harness!();
    smoke::reset_node_counter();

    let node = MillNode::init(TOKEN).await;

    let api = node.api();

    // Status endpoint should report exactly 1 node, 0 services, 0 tasks.
    let status = api.status().await.expect("GET /v1/status");
    assert_eq!(status.node_count, 1, "expected 1 node in fresh cluster");
    assert_eq!(status.service_count, 0);
    assert_eq!(status.task_count, 0);

    // Nodes endpoint should list the single node as "ready".
    let nodes = api.nodes().await.expect("GET /v1/nodes");
    assert_eq!(nodes.len(), 1, "expected exactly 1 node");
    assert_eq!(nodes[0].status, "ready");
    assert_eq!(nodes[0].alloc_count, 0);
}

/// Stand up a 3-node cluster: init node-1, join node-2 and node-3.
#[tokio::test]
async fn three_node_cluster_formation() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap the leader.
    let node1 = MillNode::init(TOKEN).await;
    let leader_addr = format!("http://{}", node1.api_addr);

    // Step 2: Join node-2.
    let node2 = MillNode::join(&leader_addr, TOKEN).await;

    // Step 3: Join node-3.
    let node3 = MillNode::join(&leader_addr, TOKEN).await;

    // Step 4: Verify cluster status from the leader's perspective.
    let api = node1.api();

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

    let status = api.status().await.expect("GET /v1/status");
    assert_eq!(status.node_count, 3, "expected 3 nodes in cluster");
    assert_eq!(status.service_count, 0);
    assert_eq!(status.task_count, 0);

    // All three nodes should be "ready".
    let nodes = api.nodes().await.expect("GET /v1/nodes");
    assert_eq!(nodes.len(), 3, "expected 3 nodes listed");
    for node in &nodes {
        assert_eq!(node.status, "ready", "node {} should be ready", node.id);
    }

    // Verify each node can serve its own status (heartbeats flowing).
    for n in [&node2, &node3] {
        let s = n.api().status().await.expect("node should serve status");
        assert_eq!(s.node_count, 3, "every node should see the full cluster");
    }
}
