//! Smoke test: Partial Deploy Failure (docs/workflows/12-partial-deploy-failure.md)
//!
//! Verifies that when a multi-service deploy partially fails:
//!   1. The HTTP response is still 200 OK (deploy is fire-and-forget via SSE)
//!   2. The SSE stream's "done" event carries `"success": false`
//!   3. The good service is running
//!   4. The bad service has no running allocations

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_partial_deploy";

const GOOD_IMAGE: &str = "docker.io/library/busybox:latest";
const BAD_IMAGE: &str = "docker.io/library/busybox:this-tag-does-not-exist-9999";

fn mixed_config() -> String {
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

service "broken" {{
  image    = "{BAD_IMAGE}"
  command  = ["/bin/sh", "-c", "httpd -f -p $PORT"]
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}}
"#
    )
}

#[tokio::test]
async fn partial_deploy_returns_200_with_success_false() {
    require_harness!();
    smoke::reset_node_counter();

    let node = MillNode::init(TOKEN).await;
    let api = node.api();

    // ---------------------------------------------------------------
    // Step 1: Deploy a mixed config — one good service, one bad service
    // ---------------------------------------------------------------
    // The deploy endpoint always returns HTTP 200 (the SSE stream IS the
    // response body), so api.deploy() must not return Err even on failure.
    let body = api
        .deploy(&mixed_config())
        .await
        .expect("deploy returned an HTTP error — should always be 200");

    // ---------------------------------------------------------------
    // Step 2: The SSE done event must carry "success":false
    // ---------------------------------------------------------------
    // The only signal callers have for a partial failure is the "success"
    // field in the terminal "done" event. Callers that only check the HTTP
    // status code will silently miss the failure.
    assert!(
        body.contains("\"success\":false") || body.contains("\"success\": false"),
        "SSE done event must carry success:false for a partially-failed deploy:\n{body}"
    );

    // ---------------------------------------------------------------
    // Step 3: The good service is eventually running
    // ---------------------------------------------------------------
    poll_async(|| {
        let api = Api::new(node.api_addr, TOKEN);
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
    .expect("\"web\" service should be running after partial deploy")
    .await;

    // ---------------------------------------------------------------
    // Step 4: The broken service has no running allocations
    // ---------------------------------------------------------------
    let services = api.services().await.expect("GET /v1/services");

    let broken = services.iter().find(|s| s.name == "broken");
    if let Some(svc) = broken {
        assert!(
            !svc.allocations.iter().any(|a| a.status == "running" || a.status == "healthy"),
            "\"broken\" service must have no running allocations after failed deploy"
        );
    }
    // If "broken" is absent from the service list entirely that is also
    // acceptable — it means the primary never committed the config for it.
}
