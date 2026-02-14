//! Smoke test: First Deploy (docs/workflows/02-first-deploy.md)
//!
//! Deploy two services ("web" replicas=2, "api" replicas=1) to a fresh
//! single-node cluster and verify via API that both are registered and
//! the SSE deploy stream contains "done".

use smoke::{Api, MillNode, poll_async, require_harness};

const CONFIG: &str = r#"
service "web" {
  image = "registry.example.com/web:v1"
  port  = 3000
  replicas = 2
  cpu   = 0.5
  memory = "512M"

  health {
    path     = "/health"
    interval = "5s"
  }

  route "app.example.com" {
    path = "/"
  }
}

service "api" {
  image = "registry.example.com/api:v1"
  port  = 8080
  cpu   = 1
  memory = "1G"

  health {
    path     = "/healthz"
    interval = "10s"
  }
}
"#;

#[tokio::test]
async fn first_deploy() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap a single-node cluster.
    let node = MillNode::init("test-token").await;

    let api: Api = node.api();

    // Sanity: cluster is up with one node.
    let status = api.status().await.expect("status endpoint reachable");
    assert_eq!(status.node_count, 1);

    // Step 2: POST /v1/deploy with the two-service config.
    let sse_body = api.deploy(CONFIG).await.expect("deploy request succeeds");

    // Step 3a: SSE stream should end with a "done" message.
    assert!(sse_body.contains("done"), "SSE stream should contain 'done', got: {sse_body}");

    // Step 3b: Verify both services exist via GET /v1/services.
    let services = api.services().await.expect("services endpoint reachable");
    assert_eq!(services.len(), 2, "expected 2 services, got: {services:?}");

    let web = services.iter().find(|s| s.name == "web").expect("web service exists");
    let api_svc = services.iter().find(|s| s.name == "api").expect("api service exists");

    // Verify desired replica counts match the config.
    assert_eq!(web.replicas.desired, 2);
    assert_eq!(api_svc.replicas.desired, 1);

    // Step 3c: Wait for services to become healthy.
    poll_async(|| async {
        let svcs = api.services().await.unwrap();
        let w = svcs.iter().find(|s| s.name == "web").unwrap();
        let a = svcs.iter().find(|s| s.name == "api").unwrap();
        w.replicas.healthy == 2 && a.replicas.healthy == 1
    })
    .secs(15)
    .expect("services become healthy")
    .await;

    // Step 3d: Verify allocations are placed on nodes.
    let services = api.services().await.unwrap();
    let web = services.iter().find(|s| s.name == "web").unwrap();
    let api_svc = services.iter().find(|s| s.name == "api").unwrap();

    assert_eq!(web.allocations.len(), 2, "web should have 2 allocations");
    assert_eq!(api_svc.allocations.len(), 1, "api should have 1 allocation");

    for alloc in &web.allocations {
        assert!(!alloc.node.is_empty(), "allocation must be placed on a node");
    }
    for alloc in &api_svc.allocations {
        assert!(!alloc.node.is_empty(), "allocation must be placed on a node");
    }
}
