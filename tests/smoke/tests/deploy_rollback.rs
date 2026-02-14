//! Smoke test: Deploy Rollback (docs/workflows/04-deploy-rollback.md)
//!
//! Verifies automatic rollback when a new version fails health checks:
//!   1. Deploy v1 with a good image
//!   2. Deploy v2 with a bad (nonexistent) image
//!   3. New alloc starts but never becomes healthy
//!   4. Primary kills new alloc, aborts deploy, old alloc preserved
//!   5. SSE stream emits "failed", never "healthy" or "stopped"
//!   6. Old v1 alloc still running, service image still v1

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "rollback-test-token";

// A real public image that starts and responds to health checks.
const GOOD_IMAGE: &str = "nginx:1.25-alpine";

// A nonexistent image tag — pull will fail or container will never start.
const BAD_IMAGE: &str = "nginx:this-tag-does-not-exist-9999";

fn v1_config() -> String {
    format!(
        r#"
service "web" {{
  image    = "{GOOD_IMAGE}"
  port     = 80
  replicas = 1
  cpu      = 0.25
  memory   = "128M"

  health {{
    path     = "/"
    interval = "2s"
  }}
}}
"#
    )
}

fn v2_config() -> String {
    format!(
        r#"
service "web" {{
  image    = "{BAD_IMAGE}"
  port     = 80
  replicas = 1
  cpu      = 0.25
  memory   = "128M"

  health {{
    path     = "/"
    interval = "2s"
  }}
}}
"#
    )
}

#[tokio::test]
async fn deploy_rollback_on_bad_image() {
    require_harness!();
    smoke::reset_node_counter();

    let node = MillNode::init(TOKEN).await;
    let api: Api = node.api();

    // ---------------------------------------------------------------
    // Step 1: Deploy v1 (good image) — should succeed
    // ---------------------------------------------------------------
    let v1_response = api.deploy(&v1_config()).await.expect("v1 deploy request failed");

    // The SSE stream for v1 should eventually contain a success indicator.
    assert!(
        !v1_response.contains("event: failed"),
        "v1 deploy should succeed but got failure:\n{v1_response}"
    );

    // Wait for the service to appear with a healthy allocation.
    poll_async(|| {
        let api = node.api();
        async move {
            let Ok(services) = api.services().await else { return false };
            services.iter().any(|s| {
                s.name == "web"
                    && s.image == GOOD_IMAGE
                    && s.allocations.iter().any(|a| a.status == "running")
            })
        }
    })
    .secs(30)
    .expect("v1 service should become running with good image")
    .await;

    // Capture the v1 allocation id so we can verify it survives rollback.
    let services_before = api.services().await.expect("list services after v1");
    let web_before = services_before.iter().find(|s| s.name == "web").expect("web service exists");
    assert_eq!(web_before.image, GOOD_IMAGE);
    let v1_alloc_id = web_before
        .allocations
        .iter()
        .find(|a| a.status == "running")
        .expect("v1 has a running alloc")
        .id
        .clone();

    // ---------------------------------------------------------------
    // Step 2: Deploy v2 (bad image) — should fail and trigger rollback
    // ---------------------------------------------------------------
    let v2_response = api.deploy(&v2_config()).await.expect("v2 deploy request failed");

    // ---------------------------------------------------------------
    // Step 3–4: Verify SSE stream reports failure
    // ---------------------------------------------------------------
    // The deploy response (SSE body) should contain "failed" and must NOT
    // contain "healthy" or "stopped" for the web service.
    assert!(
        v2_response.contains("failed"),
        "v2 deploy should report failure in SSE stream:\n{v2_response}"
    );
    assert!(
        !v2_response.contains("\"phase\": \"healthy\"")
            && !v2_response.contains("\"phase\":\"healthy\""),
        "v2 deploy must never emit 'healthy':\n{v2_response}"
    );
    assert!(
        !v2_response.contains("\"phase\": \"stopped\"")
            && !v2_response.contains("\"phase\":\"stopped\""),
        "v2 deploy must never emit 'stopped' for old alloc:\n{v2_response}"
    );

    // ---------------------------------------------------------------
    // Step 5–6: Verify old v1 alloc is still running, image reverted
    // ---------------------------------------------------------------
    // After a failed deploy, the service listing should still show v1.
    let v1_id = v1_alloc_id.clone();
    poll_async(move || {
        let api = node.api();
        let v1_id = v1_id.clone();
        async move {
            let Ok(services) = api.services().await else { return false };
            services.iter().any(|s| {
                s.name == "web"
                    && s.image == GOOD_IMAGE
                    && s.allocations.iter().any(|a| a.id == v1_id && a.status == "running")
            })
        }
    })
    .secs(15)
    .expect("v1 alloc should still be running after failed v2 deploy")
    .await;

    // Final explicit assertions on the service state.
    let services_after = api.services().await.expect("list services after rollback");
    let web_after =
        services_after.iter().find(|s| s.name == "web").expect("web service still exists");
    assert_eq!(web_after.image, GOOD_IMAGE, "service image should still be v1 after rollback");
    assert!(
        web_after.allocations.iter().any(|a| a.id == v1_alloc_id && a.status == "running"),
        "original v1 allocation should still be running"
    );

    // The bad v2 alloc should have been killed — no running allocs with the bad image.
    assert!(
        !web_after.allocations.iter().any(|a| a.status == "running" && a.id != v1_alloc_id),
        "no other running allocations should remain after rollback"
    );
}
