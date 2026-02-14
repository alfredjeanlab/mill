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

// A real pre-pulled image with a command that listens on PORT.
const GOOD_IMAGE: &str = "docker.io/library/busybox:latest";

// A nonexistent image tag — pull will fail or container will never start.
const BAD_IMAGE: &str = "docker.io/library/busybox:this-tag-does-not-exist-9999";

fn v1_config() -> String {
    format!(
        r#"
service "web" {{
  image    = "{GOOD_IMAGE}"
  command  = ["/bin/sh", "-c", "mkdir -p /www && echo ok > /www/index.html && httpd -f -p $PORT -h /www"]
  port     = 8080
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
  command  = ["/bin/sh", "-c", "mkdir -p /www && echo ok > /www/index.html && httpd -f -p $PORT -h /www"]
  port     = 8080
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
                    && s.allocations.iter().any(|a| a.status == "running" || a.status == "healthy")
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
        .find(|a| a.status == "running" || a.status == "healthy")
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
    // Use poll_async for final assertions since the API may be briefly
    // busy after the rollback raft proposal.
    let v1_id = v1_alloc_id.clone();
    poll_async(move || {
        let api = node.api();
        let v1_id = v1_id.clone();
        async move {
            let Ok(services) = api.services().await else { return false };
            let Some(web) = services.iter().find(|s| s.name == "web") else { return false };
            // Image should be reverted to v1.
            if web.image != GOOD_IMAGE {
                return false;
            }
            // The original v1 alloc should still be active.
            let v1_active = web
                .allocations
                .iter()
                .any(|a| a.id == v1_id && (a.status == "running" || a.status == "healthy"));
            // No OTHER allocs should be active (only the v1 alloc).
            let no_stray_active = !web
                .allocations
                .iter()
                .any(|a| (a.status == "running" || a.status == "healthy") && a.id != v1_id);
            v1_active && no_stray_active
        }
    })
    .secs(15)
    .expect("v1 alloc should still be running after failed v2 deploy")
    .await;
}
