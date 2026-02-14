use mill_config::{AllocStatus, NodeStatus};

use super::harness::{Api, TestCluster, poll, poll_async};

/// Stop heartbeats on a node, wait for the watchdog to detect it and mark
/// allocs as Failed. Uses real wall-clock time since HeartbeatTracker uses
/// `std::time::Instant`.
#[tokio::test]
async fn secondary_heartbeat_timeout() {
    let mut cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service with 1 replica.
    let config = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    client.deploy(config).await.unwrap();

    // Wait for alloc to be scheduled and running.
    poll(|| cluster.raft(0).read_state(|fsm| fsm.allocs.values().any(|a| a.status.is_active())))
        .expect("alloc should be active after deploy")
        .await;

    // Find which node the alloc landed on.
    let alloc_node =
        cluster.raft(0).read_state(|fsm| fsm.allocs.values().next().map(|a| a.node.clone()));
    let alloc_node = alloc_node.expect("should have an alloc");

    // Find the node index for that mill_id.
    let node_idx = (0..cluster.node_count())
        .find(|&i| cluster.mill_id(i) == &alloc_node)
        .expect("node not found");

    // Stop heartbeats on that node.
    cluster.secondary(node_idx).stop_heartbeats();

    // Wait for the watchdog to detect the timeout.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            let raft_id = cluster.raft_id(node_idx);
            let node_down = fsm
                .nodes
                .get(&raft_id)
                .map(|n| n.status == mill_config::NodeStatus::Down)
                .unwrap_or(false);
            let alloc_failed =
                fsm.allocs.values().any(|a| matches!(&a.status, AllocStatus::Failed { .. }));
            node_down && alloc_failed
        })
    })
    .expect("node should be Down and alloc Failed")
    .await;

    // Verify the node is marked Down and allocs are Failed.
    let (node_down, alloc_failed) = cluster.raft(0).read_state(|fsm| {
        let raft_id = cluster.raft_id(node_idx);
        let node_down = fsm
            .nodes
            .get(&raft_id)
            .map(|n| n.status == mill_config::NodeStatus::Down)
            .unwrap_or(false);
        let alloc_failed =
            fsm.allocs.values().any(|a| matches!(&a.status, AllocStatus::Failed { .. }));
        (node_down, alloc_failed)
    });

    assert!(node_down, "node should be marked Down");
    assert!(alloc_failed, "alloc should be marked Failed");

    cluster.shutdown().await;
}

/// Kill a secondary's node, wait for watchdog. Verify the service is
/// rescheduled on a surviving node.
#[tokio::test]
async fn service_rescheduled_after_node_down() {
    let mut cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let config = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    client.deploy(config).await.unwrap();

    // Wait for alloc to be active.
    poll(|| cluster.raft(0).read_state(|fsm| fsm.allocs.values().any(|a| a.status.is_active())))
        .expect("alloc should be active after deploy")
        .await;

    // Find which node the alloc is on.
    let alloc_node =
        cluster.raft(0).read_state(|fsm| fsm.allocs.values().next().map(|a| a.node.clone()));
    let alloc_node = alloc_node.expect("should have an alloc");
    let node_idx = (0..cluster.node_count())
        .find(|&i| cluster.mill_id(i) == &alloc_node)
        .expect("node not found");

    // Stop heartbeats to trigger node down.
    cluster.secondary(node_idx).stop_heartbeats();

    // Wait for watchdog to detect failure and service to be rescheduled.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            fsm.allocs.values().any(|a| a.status.is_active() && a.node != alloc_node)
        })
    })
    .expect("service should be rescheduled on a surviving node")
    .await;

    // Verify: a new alloc exists on a different node (the watchdog restarts).
    let active_allocs = cluster.raft(0).read_state(|fsm| {
        fsm.allocs
            .values()
            .filter(|a| a.status.is_active())
            .map(|a| a.node.clone())
            .collect::<Vec<_>>()
    });

    assert!(!active_allocs.is_empty(), "service should be rescheduled on a surviving node");

    cluster.shutdown().await;
}

/// Spawn a task, kill its node. Verify: task marked Failed, NOT restarted.
#[tokio::test]
async fn task_not_rescheduled() {
    let mut cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let resp = client
        .spawn_task(mill_config::SpawnTaskRequest {
            task: None,
            image: Some("busybox:latest".into()),
            cpu: 0.25,
            memory: "128M".into(),
            env: Default::default(),
            timeout: Some("10m".into()),
        })
        .await
        .unwrap();

    // Wait for task to be active.
    let task_id = mill_config::AllocId(resp.id.clone());
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.get(&task_id).map(|a| a.status.is_active()) == Some(true))
    })
    .expect("task should be active")
    .await;

    // Find which node the task landed on.
    let task_node =
        cluster.raft(0).read_state(|fsm| fsm.allocs.get(&task_id).map(|a| a.node.clone()));
    let task_node = task_node.expect("task should exist");
    let node_idx = (0..cluster.node_count())
        .find(|&i| cluster.mill_id(i) == &task_node)
        .expect("node not found");

    // Stop heartbeats.
    cluster.secondary(node_idx).stop_heartbeats();

    // Wait for watchdog to mark task as failed.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            matches!(fsm.allocs.get(&task_id).map(|a| &a.status), Some(AllocStatus::Failed { .. }))
        })
    })
    .expect("task should be marked Failed")
    .await;

    // Verify: task is Failed, not Running or restarted.
    let task_status =
        cluster.raft(0).read_state(|fsm| fsm.allocs.get(&task_id).map(|a| a.status.clone()));
    assert!(
        matches!(task_status, Some(AllocStatus::Failed { .. })),
        "task should be marked Failed, got: {task_status:?}"
    );

    // Verify: no new Running tasks.
    let running_tasks = cluster.raft(0).read_state(|fsm| {
        fsm.allocs
            .values()
            .filter(|a| a.kind == mill_config::AllocKind::Task && a.status.is_active())
            .count()
    });
    assert_eq!(running_tasks, 0, "task should NOT be rescheduled");

    cluster.shutdown().await;
}

/// Remove the leader from the TestRouter, trigger election. Verify: new leader
/// within 5s, FSM state preserved.
#[tokio::test]
async fn leader_failover() {
    let cluster = TestCluster::new(3).await;

    // Find the current leader.
    let leader_idx = cluster.find_leader().await;
    let leader_raft_id = cluster.raft_id(leader_idx);

    // Apply some state before failover.
    cluster
        .raft(leader_idx)
        .propose(mill_raft::fsm::command::Command::SecretSet {
            name: "test-key".into(),
            encrypted_value: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
        })
        .await
        .unwrap();

    // Wait for replication.
    poll(|| cluster.raft(leader_idx).read_state(|fsm| fsm.get_secret("test-key").is_some()))
        .expect("secret should be committed")
        .await;

    // Remove leader from Raft router (simulates network partition/crash).
    cluster.raft_router().remove_node(leader_raft_id);

    // Trigger election on surviving nodes.
    for i in 0..cluster.node_count() {
        if i != leader_idx {
            let _ = cluster.raft(i).raft().trigger().elect().await;
        }
    }

    // Wait for new leader to emerge.
    poll_async(|| async {
        for i in 0..cluster.node_count() {
            if i == leader_idx {
                continue;
            }
            if let Some(lid) = cluster.raft(i).raft().current_leader().await {
                if lid != leader_raft_id && cluster.raft(i).ensure_linearizable().await.is_ok() {
                    return true;
                }
            }
        }
        false
    })
    .expect("new leader should be elected after failover")
    .await;

    let mut new_leader_idx = 0;
    for i in 0..cluster.node_count() {
        if i != leader_idx && cluster.raft(i).ensure_linearizable().await.is_ok() {
            new_leader_idx = i;
            break;
        }
    }
    assert_ne!(new_leader_idx, leader_idx);

    // Verify FSM state preserved.
    let has_secret =
        cluster.raft(new_leader_idx).read_state(|fsm| fsm.get_secret("test-key").is_some());
    assert!(has_secret, "FSM state should be preserved after failover");

    cluster.shutdown().await;
}

/// After failover, start a new Primary on the new leader and deploy through
/// its API.
#[tokio::test]
async fn new_leader_accepts_deploy() {
    let mut cluster = TestCluster::new(3).await;

    let leader_idx = cluster.find_leader().await;
    let leader_raft_id = cluster.raft_id(leader_idx);

    // Partition the old leader.
    cluster.raft_router().remove_node(leader_raft_id);

    // Trigger election on surviving nodes.
    for i in 0..cluster.node_count() {
        if i != leader_idx {
            let _ = cluster.raft(i).raft().trigger().elect().await;
        }
    }

    // Wait for new leader.
    poll_async(|| async {
        for i in 0..cluster.node_count() {
            if i == leader_idx {
                continue;
            }
            if let Some(lid) = cluster.raft(i).raft().current_leader().await {
                if lid != leader_raft_id && cluster.raft(i).ensure_linearizable().await.is_ok() {
                    return true;
                }
            }
        }
        false
    })
    .expect("new leader should emerge")
    .await;

    let mut new_leader_idx = 0;
    for i in 0..cluster.node_count() {
        if i != leader_idx && cluster.raft(i).ensure_linearizable().await.is_ok() {
            new_leader_idx = i;
            break;
        }
    }

    // Start a new Primary on the new leader.
    cluster.restart_primary_on(new_leader_idx).await;

    // Deploy through the new Primary's API.
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());
    let config = r#"
service "web" {
  image    = "app:v1"
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    let body = client.deploy(config).await.unwrap();
    assert!(body.contains("done"), "deploy on new leader should succeed: {body}");

    // Verify the service exists.
    poll(|| cluster.raft(new_leader_idx).read_state(|fsm| fsm.config.is_some()))
        .expect("deploy should be committed on new leader")
        .await;
    let services = client.services().await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].name, "web");

    cluster.shutdown().await;
}

/// When a node goes down, the watchdog should remove its WireGuard peer.
#[tokio::test]
async fn wireguard_peer_removed_on_node_down() {
    let mut cluster = TestCluster::new(3).await;
    let wg = cluster.mesh().clone();

    assert_eq!(wg.peer_count(), 3, "all peers should be present initially");

    // Pick a non-leader node.
    let leader_idx = cluster.find_leader().await;
    let target_idx = (0..cluster.node_count()).find(|&i| i != leader_idx).unwrap();
    let target_raft_id = cluster.raft_id(target_idx);
    let target_pubkey = format!("test-pubkey-{target_raft_id}");

    // Stop heartbeats to simulate node failure.
    cluster.secondary(target_idx).stop_heartbeats();

    // Wait for watchdog to mark node Down and remove the WireGuard peer.
    poll(|| {
        let node_down = cluster
            .raft(leader_idx)
            .read_state(|fsm| fsm.nodes.get(&target_raft_id).map(|n| n.status == NodeStatus::Down))
            .unwrap_or(false);
        node_down && !wg.has_peer(&target_pubkey)
    })
    .expect("node should be Down and WireGuard peer removed")
    .await;

    assert!(!wg.has_peer(&target_pubkey), "peer should be removed");
    assert_eq!(wg.peer_count(), 2, "only 2 peers should remain");

    cluster.shutdown().await;
}

/// When a downed node recovers (heartbeats resume), the watchdog should
/// re-add its WireGuard peer.
#[tokio::test]
async fn wireguard_peer_readded_on_recovery() {
    let mut cluster = TestCluster::new(3).await;
    let wg = cluster.mesh().clone();

    // Pick a non-leader node.
    let leader_idx = cluster.find_leader().await;
    let target_idx = (0..cluster.node_count()).find(|&i| i != leader_idx).unwrap();
    let target_raft_id = cluster.raft_id(target_idx);
    let target_pubkey = format!("test-pubkey-{target_raft_id}");

    // Stop heartbeats → wait for Down + peer removal.
    cluster.secondary(target_idx).stop_heartbeats();

    poll(|| {
        let node_down = cluster
            .raft(leader_idx)
            .read_state(|fsm| fsm.nodes.get(&target_raft_id).map(|n| n.status == NodeStatus::Down))
            .unwrap_or(false);
        node_down && !wg.has_peer(&target_pubkey)
    })
    .expect("node should be Down and WireGuard peer removed")
    .await;

    // Resume heartbeats → node should recover and peer should be re-added.
    cluster.secondary(target_idx).resume_heartbeats();

    poll(|| {
        let node_ready = cluster
            .raft(leader_idx)
            .read_state(|fsm| fsm.nodes.get(&target_raft_id).map(|n| n.status == NodeStatus::Ready))
            .unwrap_or(false);
        node_ready && wg.has_peer(&target_pubkey)
    })
    .expect("node should be Ready and WireGuard peer re-added")
    .await;

    assert_eq!(wg.peer_count(), 3, "all 3 peers should be present after recovery");

    cluster.shutdown().await;
}
