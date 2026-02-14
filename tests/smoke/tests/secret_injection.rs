//! Smoke test: Secret Injection (docs/workflows/08-secret-injection.md)
//!
//! Store encrypted secrets, reference them in config, inject as env vars.
//!
//! Two tests:
//! 1. Secret CRUD — set, list, get, update, delete a secret; verify round-trip.
//! 2. Secret in deploy — deploy config referencing a secret, verify success;
//!    delete the secret and redeploy, verify validation failure.

use reqwest::StatusCode;
use smoke::{Api, MillNode, require_harness};

const TOKEN: &str = "mill_t_smoke_secret_injection";

/// Config that references a secret `DB_URL` via `${secret(DB_URL)}`.
const CONFIG_WITH_SECRET: &str = r#"
service "myapp" {
  image   = "docker.io/library/busybox:latest"
  command = ["/bin/sh", "-c", "mkdir -p /www && echo ok > /www/index.html && httpd -f -p $PORT -h /www"]
  port    = 8080
  cpu     = 0.25
  memory  = "128M"

  env {
    DATABASE_URL = "${secret(DB_URL)}"
  }

  health {
    path     = "/"
    interval = "2s"
  }
}
"#;

// ---------------------------------------------------------------------------
// Test 1: Secret CRUD
// ---------------------------------------------------------------------------

/// Set, list, get, update, and delete a secret — verify round-trip at each step.
#[tokio::test]
async fn secret_crud() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap a single-node cluster.
    let node = MillNode::init(TOKEN).await;
    let api: Api = node.api();

    // Step 2: Set a secret via PUT /v1/secrets/DB_URL.
    let status = api
        .secrets_set("DB_URL", "postgres://user:pass@db.example.com:5432/myapp")
        .await
        .expect("PUT /v1/secrets/DB_URL should succeed");
    assert!(
        status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::NO_CONTENT,
        "expected 200/201/204 for secret set, got {status}"
    );

    // Step 3: List secrets — should contain DB_URL.
    let secrets = api.secrets_list().await.expect("GET /v1/secrets should succeed");
    assert!(
        secrets.iter().any(|s| s.name == "DB_URL"),
        "secret list should contain DB_URL, got: {secrets:?}"
    );

    // Step 4: Get the secret — value should match what we set.
    let secret = api.secret_get("DB_URL").await.expect("GET /v1/secrets/DB_URL should succeed");
    assert_eq!(secret.name, "DB_URL");
    assert_eq!(secret.value, "postgres://user:pass@db.example.com:5432/myapp");

    // Step 5: Update the secret with a new value.
    let status = api
        .secrets_set("DB_URL", "postgres://user:newpass@db.example.com:5432/myapp")
        .await
        .expect("PUT /v1/secrets/DB_URL (update) should succeed");
    assert!(
        status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::NO_CONTENT,
        "expected 200/201/204 for secret update, got {status}"
    );

    // Step 6: Verify the updated value.
    let secret =
        api.secret_get("DB_URL").await.expect("GET /v1/secrets/DB_URL after update should succeed");
    assert_eq!(secret.value, "postgres://user:newpass@db.example.com:5432/myapp");

    // Step 7: Delete the secret.
    let status =
        api.delete_secret("DB_URL").await.expect("DELETE /v1/secrets/DB_URL should succeed");
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "expected 200/204 for secret delete, got {status}"
    );

    // Step 8: Verify the secret is gone — list should not contain it.
    let secrets = api.secrets_list().await.expect("GET /v1/secrets after delete should succeed");
    assert!(
        !secrets.iter().any(|s| s.name == "DB_URL"),
        "secret list should not contain DB_URL after deletion, got: {secrets:?}"
    );

    // Step 9: Getting a deleted secret should fail.
    let result = api.secret_get("DB_URL").await;
    assert!(result.is_err(), "GET /v1/secrets/DB_URL after delete should return an error");
}

// ---------------------------------------------------------------------------
// Test 2: Secret reference in deploy
// ---------------------------------------------------------------------------

/// Set a secret, deploy config referencing it, verify success. Then delete the
/// secret and redeploy — the deploy should fail validation because the
/// referenced secret no longer exists.
#[tokio::test]
async fn deploy_with_secret_reference() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap a single-node cluster.
    let node = MillNode::init(TOKEN).await;
    let api: Api = node.api();

    // Step 2: Store the secret that the config references.
    let status = api
        .secrets_set("DB_URL", "postgres://user:pass@db.example.com:5432/myapp")
        .await
        .expect("PUT /v1/secrets/DB_URL should succeed");
    assert!(
        status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::NO_CONTENT,
        "expected 200/201/204 for secret set, got {status}"
    );

    // Step 3: Deploy config that references ${secret(DB_URL)}.
    // The deploy should pass validation (the secret exists).
    let sse_body = api
        .deploy(CONFIG_WITH_SECRET)
        .await
        .expect("deploy with existing secret should pass validation");
    assert!(
        sse_body.contains("done"),
        "SSE stream should contain 'done' indicating successful deploy, got: {sse_body}"
    );

    // Step 4: Delete the secret so the reference becomes dangling.
    let status =
        api.delete_secret("DB_URL").await.expect("DELETE /v1/secrets/DB_URL should succeed");
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "expected 200/204 for secret delete, got {status}"
    );

    // Step 5: Redeploy the same config — should fail validation because
    // ${secret(DB_URL)} references a secret that no longer exists.
    let result = api.deploy(CONFIG_WITH_SECRET).await;
    assert!(
        result.is_err() || result.as_ref().is_ok_and(|body| body.contains("failed")),
        "deploy with missing secret should fail validation, got: {result:?}"
    );
}
