use std::collections::HashSet;

use super::harness::{Api, TestCluster, poll};

#[tokio::test]
async fn multi_node_with_local_leader() {
    let cluster = TestCluster::new_with_local_leader(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service with replicas=3 — one per node.
    let config = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 3
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(config).await.unwrap();
    assert!(body.contains("done"), "deploy should complete: {body}");

    // Wait for all 3 allocs to be active.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 3)
    })
    .expect("allocs should be active")
    .await;

    // Verify allocs are spread across all 3 nodes (including the local leader).
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    let active: Vec<_> = services[0]
        .allocations
        .iter()
        .filter(|a| a.status == "running" || a.status == "healthy")
        .collect();
    assert!(active.len() >= 3, "expected 3 active allocs, got {}", active.len());

    let unique_nodes: HashSet<&str> = active.iter().map(|a| a.node.as_str()).collect();
    assert_eq!(unique_nodes.len(), 3, "expected spread across 3 nodes, got {:?}", unique_nodes);

    cluster.shutdown().await;
}

#[tokio::test]
async fn rolling_update_with_local_leader() {
    let cluster = TestCluster::new_with_local_leader(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy v1 with replicas=2.
    let v1 = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 2
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(v1).await.unwrap();
    assert!(body.contains("done"), "v1 deploy should complete: {body}");

    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 2)
    })
    .expect("v1 allocs should be active")
    .await;

    // Deploy v2 (rolling update).
    let v2 = r#"
service "web" {
  image    = "app:v2"
  port     = 8080
  replicas = 2
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(v2).await.unwrap();
    assert!(body.contains("done"), "v2 deploy should complete: {body}");

    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 2)
    })
    .expect("v2 allocs should be active")
    .await;

    // Only v2 allocs should remain.
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].image, "app:v2");

    // Verify allocs are not all on the local leader — at least 2 distinct nodes.
    let active: Vec<_> = services[0]
        .allocations
        .iter()
        .filter(|a| a.status == "running" || a.status == "healthy")
        .collect();
    let unique_nodes: HashSet<&str> = active.iter().map(|a| a.node.as_str()).collect();
    assert!(unique_nodes.len() >= 2, "expected spread across nodes, got {:?}", unique_nodes);

    cluster.shutdown().await;
}

#[tokio::test]
async fn rolling_update_completes() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy v1.
    let v1 = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 2
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(v1).await.unwrap();
    assert!(body.contains("done"), "v1 deploy should complete: {body}");

    // Wait for v1 allocs to be active.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 2)
    })
    .expect("v1 allocs should be active")
    .await;

    // Verify v1 allocs exist.
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].image, "app:v1");
    let v1_active: Vec<_> = services[0]
        .allocations
        .iter()
        .filter(|a| a.status == "running" || a.status == "healthy")
        .collect();
    assert!(v1_active.len() >= 2, "expected at least 2 active v1 allocs, got {}", v1_active.len());

    // Deploy v2 (rolling update).
    let v2 = r#"
service "web" {
  image    = "app:v2"
  port     = 8080
  replicas = 2
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(v2).await.unwrap();
    assert!(body.contains("done"), "v2 deploy should complete: {body}");

    // Wait for v2 allocs to be active.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 2)
    })
    .expect("v2 allocs should be active")
    .await;

    // After deploy: only v2 allocs should exist (old ones stopped).
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].image, "app:v2");

    cluster.shutdown().await;
}

#[tokio::test]
async fn deploy_rollback_on_failure() {
    let mut cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy v1 successfully.
    let v1 = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(v1).await.unwrap();
    assert!(body.contains("done"), "v1 deploy should complete");

    // Wait for v1 allocs to be active.
    poll(|| cluster.raft(0).read_state(|fsm| fsm.allocs.values().any(|a| a.status.is_active())))
        .expect("v1 alloc should be active")
        .await;

    // Inject failure on ALL nodes so the v2 alloc can't start.
    for i in 0..cluster.node_count() {
        cluster.secondary(i).fail_next_run("simulated crash");
    }

    // Deploy v2 — should fail and roll back.
    let v2 = r#"
service "web" {
  image    = "app:v2"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(v2).await.unwrap();

    assert!(
        body.contains("failed") || body.contains("error"),
        "deploy should report failure: {body}"
    );

    // v1 allocs should still be present and active (rollback preserved them).
    let v1_survived = cluster
        .raft(0)
        .read_state(|fsm| fsm.allocs.values().any(|a| a.name == "web" && a.status.is_active()));
    assert!(v1_survived, "v1 alloc should still be active after failed v2 deploy");

    // NOTE: services API currently returns v2 image after failed deploy — the config
    // is written to FSM before allocs are created, and rollback doesn't revert it.
    // This assertion documents the desired behavior; fix the deploy rollback to revert
    // the config entry when all new allocs fail.
    // API should still show the service with the v1 image.
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1, "service should still exist");
    assert_eq!(services[0].image, "app:v1", "service image should still be v1 after rollback");

    cluster.shutdown().await;
}

#[tokio::test]
async fn deploy_env_service_address_unresolvable() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // "web" references its own address via ${service.web.address}. Config
    // validation passes (web IS defined), but at deploy time no running alloc
    // for "web" exists yet, so resolve_env_value returns EnvResolution error.
    let config = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
  env {
    SELF_URL = "http://${service.web.address}"
  }
}
"#;
    let body = client.deploy(config).await.unwrap();
    assert!(body.contains("failed"), "deploy should report env resolution failure: {body}");

    cluster.shutdown().await;
}

#[tokio::test]
async fn deploy_env_missing_secret() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Reference a secret that was never created. The validate_secret_refs gate
    // catches this before allocs start — run_deploy returns Err and the SSE
    // stream closes without a "done" event.
    let config = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
  env {
    DB_PASS = "${secret(missing)}"
  }
}
"#;
    let body = client.deploy(config).await.unwrap();
    assert!(
        !body.contains("\"success\":true"),
        "deploy with missing secret should not succeed: {body}"
    );

    // No services should have been deployed (config is proposed after validation).
    let services = client.services().await.unwrap();
    assert!(services.is_empty(), "no services should exist after secret validation failure");

    cluster.shutdown().await;
}

#[tokio::test]
async fn deploy_volume_create_failure() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Inject failure on the volume driver so create() fails.
    cluster.volume_driver().fail_next_create("simulated cloud error");

    let config = r#"
service "db" {
  image    = "postgres:16"
  port     = 5432
  replicas = 1
  cpu      = 0.5
  memory   = "512M"

  volume "pgdata" {
    path = "/var/lib/postgresql/data"
    size = "10G"
  }
}
"#;
    let body = client.deploy(config).await.unwrap();
    assert!(body.contains("failed"), "deploy should report volume create failure: {body}");

    cluster.shutdown().await;
}

#[tokio::test]
async fn concurrent_deploys_no_crash() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let config_a = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let config_b = r#"
service "web" {
  image    = "app:v2"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;

    // Fire two deploys concurrently.
    let (result_a, result_b) = tokio::join!(client.deploy(config_a), client.deploy(config_b));

    // Both should return without panicking. At least one should complete.
    let body_a = result_a.unwrap();
    let body_b = result_b.unwrap();
    assert!(
        body_a.contains("done") || body_b.contains("done"),
        "at least one deploy should complete: a={body_a}, b={body_b}"
    );

    // Cluster should still be healthy.
    let status = client.status().await.unwrap();
    assert!(status.node_count > 0, "cluster should still have nodes");

    // FSM config should match one of the two deployed configs.
    let image = cluster.raft(0).read_state(|fsm| {
        fsm.config.as_ref().and_then(|c| c.services.get("web").map(|s| s.image.clone()))
    });
    let image = image.expect("web service should exist in FSM");
    assert!(
        image == "app:v1" || image == "app:v2",
        "FSM config should match one of the deployed configs, got: {image}"
    );

    cluster.shutdown().await;
}
