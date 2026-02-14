//! Smoke test: Volume Affinity & Migration (docs/workflows/09-volume-affinity.md)
//!
//! Persistent volumes pin services to nodes; on failure the volume migrates first.
//!
//! 1. Deploy service "postgres" with volume "pgdata" (size=50Gi)
//! 2. First deploy: creates volume via cloud API, attaches to a node, schedules service there
//! 3. Subsequent deploys: scheduler places alloc on same node as volume (affinity)
//! 4. Node failure: detach volume, attach to healthy node, reschedule service there
//! 5. Volume deletion: remove from config, redeploy, then DELETE /v1/volumes/:name

use smoke::{Api, MillNode, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_volume_affinity";

/// Config for a postgres service with a persistent volume.
const POSTGRES_CONFIG: &str = r#"
service "postgres" {
  image  = "docker.io/library/postgres:16"
  port   = 5432
  cpu    = 1
  memory = "2G"

  volume "pgdata" {
    path = "/var/lib/postgresql/data"
    size = "50Gi"
  }
}
"#;

/// Config with the volume block removed (for volume deletion workflow).
const POSTGRES_NO_VOLUME_CONFIG: &str = r#"
service "postgres" {
  image  = "docker.io/library/postgres:16"
  port   = 5432
  cpu    = 1
  memory = "2G"
}
"#;

/// Deploy a service with a persistent volume and verify volume creation,
/// volume affinity on redeploy, and volume deletion.
///
/// Note: Volume creation requires a cloud provider driver (e.g. DigitalOcean).
/// This test will only pass in environments with a configured volume driver.
#[tokio::test]
async fn volume_create_affinity_and_delete() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap a single-node cluster.
    let node = MillNode::init(TOKEN).await;
    let api = node.api();

    // Sanity: cluster is up with one node, no volumes yet.
    let status = api.status().await.expect("GET /v1/status");
    assert_eq!(status.node_count, 1, "expected 1 node in fresh cluster");

    let volumes = api.volumes().await.expect("GET /v1/volumes");
    assert!(volumes.is_empty(), "fresh cluster should have no volumes");

    // Step 2: Deploy postgres with a persistent volume.
    let sse_body = api.deploy(POSTGRES_CONFIG).await.expect("deploy request succeeds");
    assert!(sse_body.contains("done"), "SSE stream should contain 'done', got: {sse_body}");

    // Step 3: Verify the volume "pgdata" appears in GET /v1/volumes.
    let volumes = api.volumes().await.expect("GET /v1/volumes after deploy");
    assert_eq!(volumes.len(), 1, "expected 1 volume after deploy, got: {volumes:?}");

    let pgdata = &volumes[0];
    assert_eq!(pgdata.name, "pgdata");
    assert_eq!(pgdata.state, "attached", "volume should be attached after deploy");
    assert!(pgdata.node.is_some(), "volume should be attached to a node");
    assert!(!pgdata.cloud_id.is_empty(), "volume should have a cloud ID from the provider");

    let original_node = pgdata.node.clone().unwrap();

    // Step 4: Verify the postgres service is scheduled.
    let services = api.services().await.expect("GET /v1/services");
    let postgres =
        services.iter().find(|s| s.name == "postgres").expect("postgres service should exist");
    assert_eq!(postgres.replicas.desired, 1);
    assert_eq!(postgres.allocations.len(), 1, "postgres should have 1 allocation");
    assert_eq!(
        postgres.allocations[0].node, original_node,
        "postgres allocation should be on the same node as the volume"
    );

    // Step 5: Redeploy the same config and verify volume affinity.
    // The scheduler should place the new alloc on the same node where
    // "pgdata" is attached (implicit volume affinity).
    let sse_body = api.deploy(POSTGRES_CONFIG).await.expect("redeploy succeeds");
    assert!(
        sse_body.contains("done"),
        "SSE stream should contain 'done' on redeploy, got: {sse_body}"
    );

    // Volume should still be on the same node after redeploy.
    let volumes = api.volumes().await.expect("GET /v1/volumes after redeploy");
    let pgdata = volumes
        .iter()
        .find(|v| v.name == "pgdata")
        .expect("pgdata volume should still exist after redeploy");
    assert_eq!(
        pgdata.node.as_deref(),
        Some(original_node.as_str()),
        "volume should remain on the same node after redeploy"
    );

    // Service allocation should still be on the same node (affinity).
    let services = api.services().await.expect("GET /v1/services after redeploy");
    let postgres = services
        .iter()
        .find(|s| s.name == "postgres")
        .expect("postgres service should exist after redeploy");
    assert_eq!(
        postgres.allocations[0].node, original_node,
        "postgres should still be scheduled on the volume's node (affinity)"
    );

    // Step 6: Volume deletion -- remove volume block from config, redeploy,
    // then delete the volume via API.

    // 6a: Redeploy without the volume block.
    let sse_body =
        api.deploy(POSTGRES_NO_VOLUME_CONFIG).await.expect("redeploy without volume succeeds");
    assert!(
        sse_body.contains("done"),
        "SSE stream should contain 'done' after removing volume, got: {sse_body}"
    );

    // 6b: Delete the volume via DELETE /v1/volumes/pgdata.
    let status_code =
        api.delete_volume("pgdata").await.expect("DELETE /v1/volumes/pgdata succeeds");
    assert!(status_code.is_success(), "volume deletion should return 2xx, got: {status_code}");

    // 6c: Verify volume is gone.
    let volumes = api.volumes().await.expect("GET /v1/volumes after deletion");
    assert!(
        !volumes.iter().any(|v| v.name == "pgdata"),
        "pgdata should no longer appear in volume list after deletion"
    );
}

/// Node failure triggers volume migration: detach from failed node, attach
/// to a healthy node, reschedule the service there.
///
/// Note: Volume operations require a cloud provider driver.
/// Multi-node test requires the harness for network namespace isolation.
#[tokio::test]
async fn volume_migration_on_node_failure() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap a 3-node cluster.
    let node1 = MillNode::init(TOKEN).await;
    let leader_addr = format!("http://{}", node1.api_addr);

    let _node2 = MillNode::join(&leader_addr, TOKEN).await;
    let _node3 = MillNode::join(&leader_addr, TOKEN).await;

    let api = node1.api();

    // Wait for all 3 nodes to be visible.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(status) = api.status().await else { return false };
            status.node_count == 3
        }
    })
    .secs(15)
    .expect("cluster should converge to 3 nodes")
    .await;

    // Step 2: Deploy postgres with a persistent volume.
    let sse_body = api.deploy(POSTGRES_CONFIG).await.expect("deploy succeeds");
    assert!(sse_body.contains("done"), "SSE stream should contain 'done', got: {sse_body}");

    // Record which node the volume was initially attached to.
    let volumes = api.volumes().await.expect("GET /v1/volumes");
    let pgdata = volumes.iter().find(|v| v.name == "pgdata").expect("pgdata volume should exist");
    let original_node = pgdata.node.clone().expect("volume should be attached to a node");
    let cloud_id = pgdata.cloud_id.clone();

    // Step 3: Drain the node that holds the volume to simulate failure.
    // This should trigger: detach volume -> attach to healthy node -> reschedule.
    api.drain(&original_node).await.expect("drain node should succeed");

    // Step 4: Wait for the volume to migrate to a different node.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        let original_node = original_node.clone();
        async move {
            let Ok(volumes) = api.volumes().await else { return false };
            let Some(v) = volumes.iter().find(|v| v.name == "pgdata") else {
                return false;
            };
            // Volume should be attached to a *different* node.
            v.state == "attached" && v.node.as_deref() != Some(&original_node)
        }
    })
    .secs(30)
    .expect("volume should migrate to a healthy node")
    .await;

    // Step 5: Verify the volume migrated but retained its cloud ID.
    let volumes = api.volumes().await.expect("GET /v1/volumes after migration");
    let pgdata = volumes
        .iter()
        .find(|v| v.name == "pgdata")
        .expect("pgdata volume should still exist after migration");
    assert_eq!(pgdata.state, "attached");
    assert_ne!(
        pgdata.node.as_deref(),
        Some(original_node.as_str()),
        "volume should have moved to a different node"
    );
    assert_eq!(pgdata.cloud_id, cloud_id, "cloud ID should be preserved across migration");

    let new_node = pgdata.node.clone().unwrap();

    // Step 6: Verify the postgres service followed the volume.
    let services = api.services().await.expect("GET /v1/services after migration");
    let postgres = services
        .iter()
        .find(|s| s.name == "postgres")
        .expect("postgres service should exist after migration");
    assert_eq!(postgres.allocations.len(), 1, "postgres should still have 1 allocation");
    assert_eq!(
        postgres.allocations[0].node, new_node,
        "postgres should follow the volume to the new node"
    );

    // Wait for the service to become healthy on the new node.
    poll_async(|| {
        let api = Api::new(node1.api_addr, TOKEN);
        async move {
            let Ok(svcs) = api.services().await else { return false };
            let Some(pg) = svcs.iter().find(|s| s.name == "postgres") else {
                return false;
            };
            pg.replicas.healthy == 1
        }
    })
    .secs(15)
    .expect("postgres should become healthy on new node")
    .await;
}
