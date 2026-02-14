use std::collections::HashSet;

use mill_config::AllocKind;

use super::harness::{Api, TestCluster, poll};

#[tokio::test]
async fn services_spread_across_nodes() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service with replicas=3, expecting spread across all 3 nodes.
    let config = r#"
service "web" {
  image    = "nginx:alpine"
  port     = 80
  replicas = 3
  cpu      = 0.25
  memory   = "128M"
}
"#;
    client.deploy(config).await.unwrap();

    // Wait for allocs to be active.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 3)
    })
    .expect("allocs should be active after deploy")
    .await;

    // Verify allocations are spread across all 3 nodes.
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    let web = &services[0];
    assert_eq!(web.name, "web");
    let active: Vec<_> =
        web.allocations.iter().filter(|a| a.status == "running" || a.status == "healthy").collect();
    assert!(active.len() >= 3, "expected at least 3 active allocations, got {}", active.len());

    let unique_nodes: HashSet<&str> = active.iter().map(|a| a.node.as_str()).collect();
    assert_eq!(unique_nodes.len(), 3, "expected spread across 3 nodes, got {:?}", unique_nodes);

    cluster.shutdown().await;
}

#[tokio::test]
async fn resource_accounting() {
    // 3-node cluster, each with 4 CPU / 8GB. Deploy a 3-CPU service with 3 replicas.
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let config = r#"
service "heavy" {
  image    = "app:1"
  port     = 8080
  replicas = 3
  cpu      = 3.0
  memory   = "1G"
}
"#;
    client.deploy(config).await.unwrap();

    // Wait for all 3 allocs to be active.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().filter(|a| a.status.is_active()).count() >= 3)
    })
    .expect("all 3 allocs should be active")
    .await;

    // Verify 3 allocs spread across 3 distinct nodes.
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    let active: Vec<_> = services[0]
        .allocations
        .iter()
        .filter(|a| a.status == "running" || a.status == "healthy")
        .collect();
    assert_eq!(active.len(), 3, "expected 3 active allocs");
    let unique_nodes: HashSet<&str> = active.iter().map(|a| a.node.as_str()).collect();
    assert_eq!(unique_nodes.len(), 3, "3 allocs should be on 3 distinct nodes");

    // Each node had 4 CPU, used 3 → status should reflect reduced availability.
    let status = client.status().await.unwrap();
    assert_eq!(status.service_count, 1);
    // Total cluster CPU = 12.0 (3 nodes × 4 CPU), consumed 9.0 (3 × 3 CPU), ~3.0 remaining.
    assert!(
        status.cpu_available <= status.cpu_total - 8.0,
        "expected at most {:.1} CPU available, got {:.1}",
        status.cpu_total - 8.0,
        status.cpu_available,
    );

    // A second service requiring 2 CPU per replica × 3 should exceed remaining capacity.
    let config2 = r#"
service "too-big" {
  image    = "app:2"
  port     = 9090
  replicas = 3
  cpu      = 2.0
  memory   = "1G"
}
"#;
    let body = client.deploy(config2).await.unwrap();
    assert!(
        body.contains("failed") || body.contains("error") || body.contains("insufficient"),
        "deploying beyond capacity should fail: {body}"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn task_placed_on_available_node() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let resp = client
        .spawn_task(mill_config::SpawnTaskRequest {
            task: None,
            image: Some("busybox:latest".into()),
            cpu: 0.25,
            memory: "128M".into(),
            env: Default::default(),
            timeout: Some("5m".into()),
        })
        .await
        .unwrap();

    assert!(!resp.id.is_empty(), "task should have an ID");

    // Verify the task appears in the task list.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().any(|a| a.kind == mill_config::AllocKind::Task))
    })
    .expect("task should be scheduled")
    .await;
    let tasks = client.tasks().await.unwrap();
    assert!(!tasks.is_empty(), "expected at least one task");

    cluster.shutdown().await;
}

#[tokio::test]
async fn service_address_in_fsm() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let config = r#"
service "api" {
  image    = "app:1"
  port     = 3000
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    client.deploy(config).await.unwrap();

    // Wait for alloc to have an address (set by RunStarted report).
    poll(|| cluster.raft(0).read_state(|fsm| fsm.allocs.values().any(|a| a.address.is_some())))
        .expect("alloc should have an address")
        .await;

    // Verify the alloc has an address set (from SimulatedSecondary's RunStarted).
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    let alloc = &services[0].allocations[0];
    assert!(alloc.address.is_some(), "alloc should have an address from RunStarted report");

    // Verify the address in the FSM.
    let has_address = cluster.raft(0).read_state(|fsm| {
        fsm.allocs.values().any(|a| a.kind == AllocKind::Service && a.address.is_some())
    });
    assert!(has_address, "FSM should contain alloc with address");

    cluster.shutdown().await;
}

#[tokio::test]
async fn single_node_failure_retries() {
    let mut cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Inject failure on node 0 — the next Run command will fail.
    cluster.secondary(0).fail_next_run("simulated failure");

    let config = r#"
service "api" {
  image    = "app:1"
  port     = 3000
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    client.deploy(config).await.unwrap();

    // Wait for the deploy to succeed (alloc on a non-failed node).
    poll(|| cluster.raft(0).read_state(|fsm| fsm.allocs.values().any(|a| a.status.is_active())))
        .expect("alloc should be active after retry")
        .await;

    // The deploy should succeed by placing the alloc on a different node.
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1, "service should exist");

    let active: Vec<_> = services[0]
        .allocations
        .iter()
        .filter(|a| a.status == "running" || a.status == "healthy")
        .collect();
    assert!(!active.is_empty(), "should have at least one active alloc after retry");

    cluster.shutdown().await;
}
