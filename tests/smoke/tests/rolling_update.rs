//! Smoke test: Rolling Update (docs/workflows/03-rolling-update.md)
//!
//! Deploy v1, then v2 of a service and verify zero-downtime rolling update.

use smoke::{MillNode, poll_async, require_harness};

const V1_CONFIG: &str = r#"
service "office" {
  image    = "nginx:1.25-alpine"
  port     = 80
  replicas = 1
  cpu      = 0.5
  memory   = "512M"

  health {
    path     = "/"
    interval = "5s"
  }
}
"#;

const V2_CONFIG: &str = r#"
service "office" {
  image    = "nginx:1.27-alpine"
  port     = 80
  replicas = 1
  cpu      = 0.5
  memory   = "512M"

  health {
    path     = "/"
    interval = "5s"
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

    // Verify v1 is running
    poll_async(|| async {
        let svcs = api.services().await.unwrap_or_default();
        svcs.iter().any(|s| s.name == "office" && s.image.contains("1.25"))
    })
    .secs(15)
    .expect("office v1 should appear in service list")
    .await;

    // Deploy v2 (image change triggers rolling update)
    let v2_output = api.deploy(V2_CONFIG).await.expect("deploy v2");
    assert!(v2_output.contains("done"), "v2 deploy should complete: {v2_output}");

    // Verify only v2 allocations remain
    poll_async(|| async {
        let svcs = api.services().await.unwrap_or_default();
        let office = svcs.iter().find(|s| s.name == "office");
        match office {
            Some(svc) => svc.image.contains("1.27") && svc.replicas.healthy > 0,
            None => false,
        }
    })
    .secs(15)
    .expect("office should be updated to v2 with healthy replicas")
    .await;

    // Confirm no v1 allocations linger
    let svcs = api.services().await.expect("list services after v2 deploy");
    let office = svcs.iter().find(|s| s.name == "office").expect("office service exists");
    assert_eq!(office.image, "nginx:1.27-alpine");
    assert!(
        office.allocations.iter().all(|a| a.status != "stopped"),
        "all allocations should be active, not stopped"
    );
}
