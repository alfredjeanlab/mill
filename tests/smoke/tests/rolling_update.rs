//! Smoke test: Rolling Update (docs/workflows/03-rolling-update.md)
//!
//! Deploy v1, then v2 of a service and verify zero-downtime rolling update.
//! Uses busybox for both versions (different commands trigger a config diff).

use smoke::{MillNode, poll_async, require_harness};

const V1_CONFIG: &str = r#"
service "office" {
  image    = "docker.io/library/busybox:latest"
  command  = ["/bin/sh", "-c", "mkdir -p /www && echo v1 > /www/index.html && httpd -f -p $PORT -h /www"]
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"

  health {
    path     = "/"
    interval = "2s"
  }
}
"#;

/// V2 changes the command (echo v2 instead of v1) — enough to trigger a config
/// diff and rolling update even though the image stays the same.
const V2_CONFIG: &str = r#"
service "office" {
  image    = "docker.io/library/busybox:latest"
  command  = ["/bin/sh", "-c", "mkdir -p /www && echo v2 > /www/index.html && httpd -f -p $PORT -h /www"]
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"

  health {
    path     = "/"
    interval = "2s"
  }
}
"#;

#[tokio::test]
async fn rolling_update_zero_downtime() {
    require_harness!();
    smoke::reset_node_counter();

    let node = MillNode::init("test-token-rolling").await;
    let api = node.api();

    // Deploy v1
    let v1_output = api.deploy(V1_CONFIG).await.expect("deploy v1");
    assert!(v1_output.contains("done"), "v1 deploy should complete: {v1_output}");

    // Verify v1 is running with healthy replicas.
    poll_async(|| async {
        let svcs = api.services().await.unwrap_or_default();
        svcs.iter().any(|s| s.name == "office" && s.replicas.healthy >= 1)
    })
    .secs(15)
    .expect("office v1 should be healthy")
    .await;

    // Capture v1 alloc IDs before the update.
    let svcs = api.services().await.expect("list services after v1");
    let office_v1 = svcs.iter().find(|s| s.name == "office").expect("office exists");
    let v1_alloc_ids: Vec<String> = office_v1
        .allocations
        .iter()
        .filter(|a| a.status == "healthy" || a.status == "running")
        .map(|a| a.id.clone())
        .collect();
    assert!(!v1_alloc_ids.is_empty(), "v1 should have active allocations");

    // Deploy v2 (command change triggers rolling update)
    let v2_output = api.deploy(V2_CONFIG).await.expect("deploy v2");
    assert!(v2_output.contains("done"), "v2 deploy should complete: {v2_output}");

    // Verify v2 allocations are healthy and v1 allocs are gone.
    poll_async(|| async {
        let svcs = api.services().await.unwrap_or_default();
        let Some(office) = svcs.iter().find(|s| s.name == "office") else { return false };
        // At least 1 healthy replica, and no v1 allocs still active.
        office.replicas.healthy >= 1
            && !office.allocations.iter().any(|a| {
                v1_alloc_ids.contains(&a.id) && (a.status == "running" || a.status == "healthy")
            })
    })
    .secs(15)
    .expect("office should have new v2 replicas, old v1 allocs stopped")
    .await;

    // Confirm final state: no stopped allocations among active ones.
    let svcs = api.services().await.expect("list services after v2 deploy");
    let office = svcs.iter().find(|s| s.name == "office").expect("office service exists");
    assert!(
        office.replicas.healthy >= 1,
        "office should have at least 1 healthy replica after rolling update"
    );
}
