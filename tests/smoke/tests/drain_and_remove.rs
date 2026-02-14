//! E2E smoke test: Drain & Remove Node (docs/workflows/06-drain-and-remove.md)
//!
//! Operator decommissions a node by draining its allocations and removing it:
//! 1. Init node-1, join node-2 — form a 2-node cluster
//! 2. Deploy a service with replicas spread across both nodes
//! 3. `mill drain node-2` — mark unschedulable, migrate services, stop tasks
//! 4. Verify: node-2 drained, 0 allocs, service healthy on node-1
//! 5. `mill remove node-2` — delete node from cluster, remove WireGuard peer
//! 6. Verify: node-2 gone from `mill nodes`, service still healthy

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_drain_remove";

/// Service config: 2 replicas spread across both nodes, with an HTTP health
/// check so the deploy coordinator (and watchdog) can promote allocs to Healthy.
const SERVICE_CONFIG: &str = r#"
service "web" {
  image    = "docker.io/library/busybox:latest"
  command  = ["/bin/sh", "-c", "mkdir -p /www && echo ok > /www/index.html && httpd -f -p $PORT -h /www"]
  port     = 8080
  replicas = 2
  cpu      = 0.25
  memory   = "128M"

  health {
    path     = "/"
    interval = "2s"
  }
}
"#;

/// Drain a node and remove it from a 2-node cluster.
#[tokio::test]
async fn drain_and_remove_node() {
    require_harness!();
    smoke::reset_node_counter();

    // -----------------------------------------------------------------------
    // Step 1: Bootstrap a 2-node cluster.
    // -----------------------------------------------------------------------

    let node1 = MillNode::init(TOKEN).await;
    let leader_addr = format!("http://{}", node1.api_addr);

    // Kept alive so Drop doesn't kill the daemon mid-test.
    let _node2 = MillNode::join(&leader_addr, TOKEN).await;

    let api = node1.api();

    // Wait for both nodes to appear as "ready".
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(nodes) = api.nodes().await else { return false };
            nodes.len() == 2 && nodes.iter().all(|n| n.status == "ready")
        }
    })
    .secs(15)
    .expect("cluster should converge to 2 ready nodes")
    .await;

    // -----------------------------------------------------------------------
    // Step 2: Deploy a service with 2 replicas (spread across both nodes).
    // -----------------------------------------------------------------------

    // Deploy once. The RPC connection to node-2 may not yet be established,
    // so the deploy may partially fail. That's fine — the config is committed
    // to Raft regardless (there's no old config to roll back to on first
    // deploy), and the watchdog will reconcile under-replicated services.
    let _ = api.deploy(SERVICE_CONFIG).await.expect("deploy HTTP request");

    // Wait for the service to have 2 healthy replicas. The deploy coordinator
    // and/or the watchdog will keep placing allocs and health-checking them
    // until both replicas are healthy.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(services) = api.services().await else { return false };
            services.iter().any(|s| s.name == "web" && s.replicas.healthy >= 2)
        }
    })
    .secs(45)
    .expect("service 'web' should reach 2 healthy replicas")
    .await;

    // Identify node-2's ID from the cluster for drain/remove.
    let nodes = api.nodes().await.expect("GET /v1/nodes");
    assert_eq!(nodes.len(), 2, "expected 2 nodes before drain");
    let node2_id = nodes
        .iter()
        .find(|n| n.id != nodes[0].id)
        .map(|n| n.id.clone())
        .unwrap_or_else(|| nodes[1].id.clone());

    // -----------------------------------------------------------------------
    // Step 3: Drain node-2 — migrate services away, stop tasks.
    // -----------------------------------------------------------------------

    let drain_output = api.drain(&node2_id).await.expect("POST /v1/nodes/{id}/drain");
    assert!(!drain_output.is_empty(), "drain should return SSE progress");

    // -----------------------------------------------------------------------
    // Step 4: Verify drain completed — node-2 should be "draining" or "drained"
    // with 0 allocations, service still healthy on node-1.
    // -----------------------------------------------------------------------

    // Wait for the drained node to reach 0 allocs.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        let node2_id = node2_id.clone();
        async move {
            let Ok(nodes) = api.nodes().await else { return false };
            nodes.iter().any(|n| n.id == node2_id && n.alloc_count == 0)
        }
    })
    .secs(30)
    .expect("drained node should reach 0 allocations")
    .await;

    // The drained node should have status "draining" or "drained".
    let nodes = api.nodes().await.expect("GET /v1/nodes after drain");
    let drained = nodes.iter().find(|n| n.id == node2_id).expect("node-2 should still be listed");
    assert!(
        drained.status == "draining" || drained.status == "drained",
        "expected node-2 status 'draining' or 'drained', got '{}'",
        drained.status,
    );
    assert_eq!(drained.alloc_count, 0, "drained node should have 0 allocations");

    // Service should still be healthy (replicas migrated to node-1).
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(services) = api.services().await else { return false };
            services.iter().any(|s| s.name == "web" && s.replicas.healthy >= 1)
        }
    })
    .secs(15)
    .expect("service should have at least 1 healthy replica after drain")
    .await;

    // All remaining active allocs for the service should be on node-1 (not node-2).
    let services = api.services().await.expect("GET /v1/services after drain");
    let web = services.iter().find(|s| s.name == "web").expect("service 'web' should exist");
    for alloc in &web.allocations {
        if alloc.status == "running" || alloc.status == "healthy" {
            assert_ne!(
                alloc.node, node2_id,
                "no active allocations should remain on the drained node"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Step 5: Remove node-2 from the cluster.
    // -----------------------------------------------------------------------

    // Api does not expose a `remove` helper — issue DELETE /v1/nodes/{id} directly.
    let remove_resp = reqwest::Client::new()
        .delete(format!("http://{}/v1/nodes/{}", node1.api_addr, node2_id))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("DELETE /v1/nodes/{id}");
    assert_eq!(
        remove_resp.status(),
        reqwest::StatusCode::NO_CONTENT,
        "remove should return 204 No Content for a fully drained node",
    );

    // -----------------------------------------------------------------------
    // Step 6: Verify node-2 is gone, service still healthy.
    // -----------------------------------------------------------------------

    // Wait for the removed node to disappear from the node list.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        let node2_id = node2_id.clone();
        async move {
            let Ok(nodes) = api.nodes().await else { return false };
            !nodes.iter().any(|n| n.id == node2_id)
        }
    })
    .secs(15)
    .expect("removed node should disappear from node list")
    .await;

    let nodes = api.nodes().await.expect("GET /v1/nodes after remove");
    assert_eq!(nodes.len(), 1, "only node-1 should remain in the cluster");
    assert!(!nodes.iter().any(|n| n.id == node2_id), "node-2 should no longer be listed",);

    // Service should still be healthy on the remaining node.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(services) = api.services().await else { return false };
            services.iter().any(|s| s.name == "web" && s.replicas.healthy >= 1)
        }
    })
    .secs(15)
    .expect("service should remain healthy after node removal")
    .await;

    // Overall cluster status should reflect 1 node with the service running.
    let status = api.status().await.expect("GET /v1/status after remove");
    assert_eq!(status.node_count, 1, "cluster should report 1 node");
    assert_eq!(status.service_count, 1, "cluster should still have 1 service");
}
