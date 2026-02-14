// Smoke test: Node Failure & Reschedule (docs/workflows/05-node-failure.md)
//
// Verifies that when a secondary stops heartbeating, the leader marks it down
// and reschedules its service allocations to healthy nodes.
//
// Workflow:
//   1. Deploy a service with replicas=2 to a 3-node cluster
//   2. Kill one secondary (drop the MillNode)
//   3. Watchdog detects heartbeat timeout (2s in test env), proposes NodeDown
//   4. Service allocs on dead node marked Failed, tasks marked Failed
//   5. Services rescheduled to remaining healthy nodes
//   6. Verify: dead node status=down, service replicas back to desired count

use smoke::{Api, MillNode, poll_async, require_harness};

/// Service config: a simple HTTP service with 2 replicas, suitable for
/// testing placement across a 3-node cluster.
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

/// Init a 3-node cluster, deploy a service, kill one node, and verify that
/// the leader detects the failure and reschedules the service.
#[tokio::test]
async fn node_failure_reschedules_service() {
    require_harness!();
    smoke::reset_node_counter();

    let token = "test-token-node-failure";

    // -----------------------------------------------------------------------
    // Step 1: Bootstrap a 3-node cluster
    // -----------------------------------------------------------------------

    let node1 = MillNode::init(token).await;
    let leader_api = node1.api();
    let leader_addr = format!("http://{}", node1.api_addr);

    let node2 = MillNode::join(&leader_addr, token).await;
    let node3 = MillNode::join(&leader_addr, token).await;

    // Wait for all 3 nodes to register as ready.
    poll_async(|| async {
        let nodes = leader_api.nodes().await.unwrap_or_default();
        nodes.len() == 3 && nodes.iter().all(|n| n.status == "ready")
    })
    .secs(10)
    .expect("all 3 nodes should be ready")
    .await;

    // -----------------------------------------------------------------------
    // Step 2: Deploy a service with replicas=2
    // -----------------------------------------------------------------------

    leader_api.deploy(SERVICE_CONFIG).await.expect("deploy service");

    // Wait until the service has 2 healthy replicas.
    poll_async(|| async {
        let services = leader_api.services().await.unwrap_or_default();
        services.iter().any(|s| s.name == "web" && s.replicas.healthy == 2)
    })
    .secs(30)
    .expect("service 'web' should have 2 healthy replicas")
    .await;

    // Record which node hosts an alloc so we can identify the one to kill.
    let services = leader_api.services().await.unwrap();
    let web = services.iter().find(|s| s.name == "web").unwrap();
    assert_eq!(web.replicas.desired, 2);
    assert_eq!(web.replicas.healthy, 2);

    // Pick the node ID of the third node — we will kill this one.
    let nodes = leader_api.nodes().await.unwrap();
    let node3_id = nodes
        .iter()
        .find(|n| n.address.contains(&node3.api_addr.ip().to_string()))
        .map(|n| n.id.clone())
        .expect("should find node3 in cluster");

    // -----------------------------------------------------------------------
    // Step 3: Kill one node (stop heartbeats)
    // -----------------------------------------------------------------------
    //
    // Dropping the MillNode kills the child process, which stops heartbeats.
    // The watchdog (ticking every 500ms in test env) should detect the
    // heartbeat timeout (2s in test env) and propose NodeDown.
    drop(node3);

    // -----------------------------------------------------------------------
    // Step 4: Watchdog detects timeout, marks node down
    // -----------------------------------------------------------------------

    // Wait for the killed node to be marked "down". The heartbeat timeout is
    // 2s and the watchdog tick is 500ms, so this should happen within ~3s.
    poll_async(|| {
        let api = Api::new(node1.api_addr, token);
        let node3_id = node3_id.clone();
        async move {
            let nodes = api.nodes().await.unwrap_or_default();
            nodes.iter().any(|n| n.id == node3_id && n.status == "down")
        }
    })
    .secs(10)
    .expect("killed node should be marked 'down'")
    .await;

    // Verify the down node has zero allocs.
    let nodes = leader_api.nodes().await.unwrap();
    let dead_node = nodes.iter().find(|n| n.id == node3_id).unwrap();
    assert_eq!(dead_node.status, "down");
    assert_eq!(dead_node.alloc_count, 0, "dead node should have no allocs");

    // -----------------------------------------------------------------------
    // Step 5: Service allocs rescheduled to healthy nodes
    // -----------------------------------------------------------------------

    // The restart phase should detect the under-replicated service and place
    // new allocs on the two remaining healthy nodes. Backoff starts at 1s.
    poll_async(|| async {
        let services = leader_api.services().await.unwrap_or_default();
        services.iter().any(|s| s.name == "web" && s.replicas.healthy == 2)
    })
    .secs(30)
    .expect("service 'web' should be rescheduled to 2 healthy replicas")
    .await;

    // -----------------------------------------------------------------------
    // Step 6: Final verification
    // -----------------------------------------------------------------------

    // All healthy allocs should be on live nodes, not on the dead node.
    let services = leader_api.services().await.unwrap();
    let web = services.iter().find(|s| s.name == "web").unwrap();
    assert_eq!(web.replicas.desired, 2);
    assert_eq!(web.replicas.healthy, 2);

    for alloc in &web.allocations {
        if alloc.status == "healthy" || alloc.status == "running" {
            assert_ne!(alloc.node, node3_id, "healthy alloc should not be on the dead node");
        }
    }

    // Verify overall cluster health: 2 ready nodes, 1 down.
    let nodes = leader_api.nodes().await.unwrap();
    let ready_count = nodes.iter().filter(|n| n.status == "ready").count();
    let down_count = nodes.iter().filter(|n| n.status == "down").count();
    assert_eq!(ready_count, 2, "should have 2 ready nodes");
    assert_eq!(down_count, 1, "should have 1 down node");

    // Keep node1 and node2 alive until assertions complete.
    drop(node2);
    drop(node1);
}
