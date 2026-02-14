use mill_config::VolumeState;
use mill_node::storage::Volumes;
use mill_raft::fsm::command::Command;

use super::harness::{Api, TestCluster, poll};

#[tokio::test]
async fn volume_created_on_deploy() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

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
    client.deploy(config).await.unwrap();

    // Wait for volume to be created and alloc to be active.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            fsm.volumes.contains_key("pgdata") && fsm.allocs.values().any(|a| a.status.is_active())
        })
    })
    .expect("volume should be created and alloc active")
    .await;

    // Verify volume was created in FSM.
    let vol_exists = cluster.raft(0).read_state(|fsm| fsm.volumes.contains_key("pgdata"));
    assert!(vol_exists, "volume 'pgdata' should exist in FSM");

    // Verify volume in API.
    let volumes = client.volumes().await.unwrap();
    assert!(!volumes.is_empty(), "should have at least one volume");
    assert!(
        volumes.iter().any(|v| v.name == "pgdata"),
        "volume 'pgdata' should be in API response"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn scheduler_volume_affinity() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service with a volume — it will be attached to whichever node
    // the scheduler picks.
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
    client.deploy(config).await.unwrap();

    // Wait for volume to be attached and alloc to exist.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            matches!(
                fsm.volumes.get("pgdata").map(|v| &v.state),
                Some(mill_config::VolumeState::Attached { .. })
            ) && !fsm.allocs.is_empty()
        })
    })
    .expect("volume should be attached and alloc should exist")
    .await;

    // Find which node the volume is attached to.
    let vol_node = cluster.raft(0).read_state(|fsm| {
        fsm.volumes.get("pgdata").and_then(|v| match &v.state {
            mill_config::VolumeState::Attached { node } => Some(node.clone()),
            _ => None,
        })
    });
    let vol_node = vol_node.expect("volume should be attached");

    // Verify the alloc is on the same node (volume affinity).
    let alloc_node = cluster
        .raft(0)
        .read_state(|fsm| fsm.allocs.values().find(|a| a.name == "db").map(|a| a.node.clone()));
    let alloc_node = alloc_node.expect("db alloc should exist");
    assert_eq!(alloc_node, vol_node, "alloc should be co-located with volume");

    cluster.shutdown().await;
}

#[tokio::test]
async fn drain_migrates_volumes() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

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
    client.deploy(config).await.unwrap();

    // Wait for alloc to be active.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().any(|a| a.name == "db" && a.status.is_active()))
    })
    .expect("db alloc should be active")
    .await;

    // Find the node the alloc is on.
    let alloc_node_id = cluster
        .raft(0)
        .read_state(|fsm| fsm.allocs.values().find(|a| a.name == "db").map(|a| a.node.0.clone()));
    let alloc_node_id = alloc_node_id.expect("db alloc should exist");

    // Drain that node (compressed timings).
    let _drain_result = client.drain(&alloc_node_id).await;

    // Wait for service to be rescheduled on a different node.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            fsm.allocs
                .values()
                .any(|a| a.name == "db" && a.status.is_active() && a.node.0 != alloc_node_id)
        })
    })
    .expect("service should be rescheduled after drain")
    .await;

    // Verify: service should be rescheduled on a different node.
    let new_alloc_node = cluster.raft(0).read_state(|fsm| {
        fsm.allocs
            .values()
            .find(|a| a.name == "db" && a.status.is_active() && a.node.0 != alloc_node_id)
            .map(|a| a.node.0.clone())
    });

    assert!(
        new_alloc_node.is_some(),
        "service should be rescheduled on a different node after drain"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn volume_attach_on_deploy() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

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
    client.deploy(config).await.unwrap();

    // Wait for volume to be attached.
    poll(|| cluster.volume_driver().attached_to("pgdata").is_some())
        .expect("volume should be attached")
        .await;

    // Verify the TestVolumes received the attach call with the correct instance_id.
    let attached = cluster.volume_driver().attached_to("pgdata");
    assert_eq!(
        attached.as_deref(),
        Some("test-instance-1"),
        "volume should be attached to the node's instance_id"
    );

    // Cross-check FSM state.
    let vol_state =
        cluster.raft(0).read_state(|fsm| fsm.volumes.get("pgdata").map(|v| v.state.clone()));
    assert!(
        matches!(vol_state, Some(VolumeState::Attached { .. })),
        "FSM should show volume as attached"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn volume_destroy_via_api() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service with a volume.
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
    client.deploy(config).await.unwrap();

    // Wait for volume and alloc to exist.
    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.volumes.contains_key("pgdata") && !fsm.allocs.is_empty())
    })
    .expect("volume and alloc should exist")
    .await;

    // Re-deploy without the volume service to free the volume.
    let config_no_vol = r#"
service "web" {
  image    = "nginx:alpine"
  port     = 80
  replicas = 1
  cpu      = 0.25
  memory   = "128M"
}
"#;
    client.deploy(config_no_vol).await.unwrap();

    // Wait for the new config to be committed.
    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            fsm.config.as_ref().map(|c| c.services.contains_key("web")).unwrap_or(false)
        })
    })
    .expect("new config should be committed")
    .await;

    // The volume is still in FSM (not destroyed yet) but the service is gone,
    // so the volume may still be Attached. Detach via raft if needed — the
    // deploy path doesn't auto-detach volumes when a service is removed.
    let vol_state =
        cluster.raft(0).read_state(|fsm| fsm.volumes.get("pgdata").map(|v| v.state.clone()));
    if matches!(vol_state, Some(VolumeState::Attached { .. })) {
        let _ = cluster.volume_driver().detach("pgdata").await;
        cluster
            .raft(0)
            .propose(mill_raft::fsm::command::Command::VolumeDetached { name: "pgdata".into() })
            .await
            .unwrap();
        poll(|| {
            cluster.raft(0).read_state(|fsm| {
                !matches!(
                    fsm.volumes.get("pgdata").map(|v| &v.state),
                    Some(VolumeState::Attached { .. })
                )
            })
        })
        .expect("volume should be detached")
        .await;
    }

    // Now delete the volume via API.
    let status = client.delete_volume("pgdata").await.unwrap();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    // Verify the volume is gone from FSM.
    poll(|| !cluster.raft(0).read_state(|fsm| fsm.volumes.contains_key("pgdata")))
        .expect("volume should be removed from FSM after DELETE")
        .await;
    let vol_exists = cluster.raft(0).read_state(|fsm| fsm.volumes.contains_key("pgdata"));
    assert!(!vol_exists, "volume should be removed from FSM after DELETE");

    // Verify it was destroyed in the driver.
    assert!(!cluster.volume_driver().exists("pgdata"), "volume should be destroyed in driver");

    cluster.shutdown().await;
}

#[tokio::test]
async fn delete_volume_attached_returns_conflict() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service with a volume so it becomes Attached.
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
    client.deploy(config).await.unwrap();

    poll(|| {
        cluster.raft(0).read_state(|fsm| {
            matches!(
                fsm.volumes.get("pgdata").map(|v| &v.state),
                Some(VolumeState::Attached { .. })
            )
        })
    })
    .expect("volume should be attached")
    .await;

    let status = client.delete_volume_raw("pgdata").await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "DELETE attached volume should return CONFLICT"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn delete_volume_not_found_returns_404() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let status = client.delete_volume_raw("nonexistent").await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "DELETE nonexistent volume should return NOT_FOUND"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn delete_volume_driver_failure_returns_500() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Propose VolumeCreated directly to Raft (bypassing the driver) so FSM has
    // a Ready volume that the TestVolumes doesn't know about.
    cluster
        .raft(0)
        .propose(Command::VolumeCreated { name: "orphan".into(), cloud_id: "fake-123".into() })
        .await
        .unwrap();

    poll(|| cluster.raft(0).read_state(|fsm| fsm.volumes.contains_key("orphan")))
        .expect("volume should exist in FSM")
        .await;

    // DELETE → driver.destroy() returns NotFound → handler maps to 500.
    let status = client.delete_volume_raw("orphan").await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "driver destroy failure should return 500"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn drain_migration_failure_preserves_alloc() {
    let mut cluster = TestCluster::new(2).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Deploy a service (lands on some node).
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
    assert!(body.contains("done"), "deploy should complete: {body}");

    poll(|| {
        cluster
            .raft(0)
            .read_state(|fsm| fsm.allocs.values().any(|a| a.name == "web" && a.status.is_active()))
    })
    .expect("web alloc should be active")
    .await;

    // Find which node the alloc landed on.
    let alloc_node_id = cluster
        .raft(0)
        .read_state(|fsm| fsm.allocs.values().find(|a| a.name == "web").map(|a| a.node.0.clone()));
    let alloc_node_id = alloc_node_id.expect("web alloc should exist");

    // Inject failure on the OTHER node so migration fails.
    for i in 0..cluster.node_count() {
        if cluster.mill_id(i).0 != alloc_node_id {
            cluster.secondary(i).fail_next_run("simulated migration failure");
        }
    }

    // Drain the node hosting the alloc.
    let drain_body = client.drain(&alloc_node_id).await.unwrap();
    assert!(drain_body.contains("failed"), "drain should report migration failure: {drain_body}");

    // The old alloc should still be active (not stopped, since migration failed).
    let old_still_active = cluster.raft(0).read_state(|fsm| {
        fsm.allocs
            .values()
            .any(|a| a.name == "web" && a.node.0 == alloc_node_id && a.status.is_active())
    });
    assert!(old_still_active, "old alloc should be preserved when migration fails");

    cluster.shutdown().await;
}
