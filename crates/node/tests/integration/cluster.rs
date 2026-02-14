use bytesize::ByteSize;
use mill_config::NodeResources;
use yare::parameterized;

use super::harness::{Api, TestCluster, poll};

#[tokio::test]
async fn three_node_cluster_forms() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let nodes = client.nodes().await.unwrap();
    assert_eq!(nodes.len(), 3, "expected 3 nodes, got {}", nodes.len());

    // Verify each node has an ID.
    for node in &nodes {
        assert!(!node.id.is_empty(), "node ID should not be empty");
    }

    cluster.shutdown().await;
}

#[tokio::test]
async fn leader_in_status() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let status = client.status().await.unwrap();
    assert_eq!(status.node_count, 3);
    assert_eq!(status.service_count, 0);
    assert_eq!(status.task_count, 0);

    cluster.shutdown().await;
}

#[tokio::test]
async fn dynamic_join() {
    // Start with a 1-node cluster, then dynamically join a second node.
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let nodes = client.nodes().await.unwrap();
    assert_eq!(nodes.len(), 1, "should start with 1 node");

    // Pre-register the joiner's raft node in the TestRouter.
    let joiner_raft = cluster.add_joiner_to_router(2).await;

    let info = client.join_info().await.unwrap();

    let join_req = mill_node::daemon::JoinRequest {
        node_id: "dynamic-node".to_string(),
        raft_id: info.next_raft_id,
        advertise_addr: "10.0.0.2".to_string(),
        tunnel_ip: info.tunnel_ip,
        public_key: "dynamic-pubkey".to_string(),
        resources: NodeResources {
            cpu_total: 4.0,
            cpu_available: 4.0,
            memory_total: ByteSize::gib(8),
            memory_available: ByteSize::gib(8),
        },
        instance_id: None,
        rpc_address: None,
    };

    let status = client.post_join(&join_req).await.unwrap();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    // Wait for FSM to replicate the new node.
    poll(|| cluster.raft(0).read_state(|fsm| fsm.nodes.len()) == 2)
        .expect("FSM should have 2 nodes after join")
        .await;

    // API should now return 2 nodes, including the joined one.
    let nodes = client.nodes().await.unwrap();
    assert_eq!(nodes.len(), 2, "should have 2 nodes after join");
    assert!(nodes.iter().any(|n| n.id == "dynamic-node"), "joined node should appear in node list");

    let status = client.status().await.unwrap();
    assert_eq!(status.node_count, 2);

    let _ = joiner_raft; // keep alive
    cluster.shutdown().await;
}

#[tokio::test]
async fn secrets_via_api() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Set a secret.
    let status = client.secrets_set("db_password", "hunter2").await.unwrap();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    // List secrets — should contain our secret.
    let secrets = client.secrets_list().await.unwrap();
    assert!(
        secrets.iter().any(|s| s.name == "db_password"),
        "secret should appear in list: {secrets:?}"
    );

    // Get the secret back — should be decrypted.
    let secret = client.secret_get("db_password").await.unwrap();
    assert_eq!(secret.name, "db_password");
    assert_eq!(secret.value, "hunter2");

    // Verify in FSM (encrypted — just check existence).
    let exists = cluster.raft(0).read_state(|fsm| fsm.get_secret("db_password").is_some());
    assert!(exists, "secret should exist in FSM");

    // Delete the secret.
    let status = client.delete_secret("db_password").await.unwrap();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    // Verify it's gone.
    let exists = cluster.raft(0).read_state(|fsm| fsm.get_secret("db_password").is_some());
    assert!(!exists, "secret should be deleted from FSM");

    cluster.shutdown().await;
}

#[tokio::test]
async fn join_info_returns_valid_response() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let info = client.join_info().await.unwrap();
    assert_eq!(info.subnet, "10.99.0.0/16");
    assert_eq!(info.prefix_len, 16);
    assert_eq!(info.api_port, 4400);
    assert_eq!(info.next_raft_id, 2, "next raft ID should be 2 for 1-node cluster");
    assert_eq!(info.peers.len(), 1, "1-node cluster should have 1 peer");

    // Tunnel IP should be in 10.99.x.x range
    let ip_str = info.tunnel_ip.to_string();
    assert!(ip_str.starts_with("10.99."), "tunnel IP should be in 10.99.0.0/16: {ip_str}");

    cluster.shutdown().await;
}

#[tokio::test]
async fn join_info_tunnel_ip_not_in_peer_list() {
    let cluster = TestCluster::new(3).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let info = client.join_info().await.unwrap();
    let peer_ips: Vec<_> = info.peers.iter().map(|p| p.tunnel_ip).collect();
    assert!(
        !peer_ips.contains(&info.tunnel_ip),
        "allocated tunnel IP {ip} should not be in peer list {peer_ips:?}",
        ip = info.tunnel_ip
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn cluster_key_endpoint_returns_32_bytes() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    let key = client.cluster_key_bytes().await.unwrap();
    assert_eq!(key.len(), 32, "cluster key should be exactly 32 bytes");

    cluster.shutdown().await;
}

#[tokio::test]
async fn post_join_registers_new_node() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Pre-register the joiner's raft node in the TestRouter so add_learner works
    let joiner_raft = cluster.add_joiner_to_router(2).await;

    let info = client.join_info().await.unwrap();

    let join_req = mill_node::daemon::JoinRequest {
        node_id: "joining-node".to_string(),
        raft_id: info.next_raft_id,
        advertise_addr: "5.6.7.8".to_string(),
        tunnel_ip: info.tunnel_ip,
        public_key: "fake-pubkey".to_string(),
        resources: NodeResources {
            cpu_total: 2.0,
            cpu_available: 2.0,
            memory_total: ByteSize::gib(4),
            memory_available: ByteSize::gib(4),
        },
        instance_id: None,
        rpc_address: None,
    };

    let status = client.post_join(&join_req).await.unwrap();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

    // Wait for replication.
    poll(|| cluster.raft(0).read_state(|fsm| fsm.nodes.len()) == 2)
        .expect("FSM should have 2 nodes after join")
        .await;

    // FSM should now have 2 nodes
    let node_count = cluster.raft(0).read_state(|fsm| fsm.nodes.len());
    assert_eq!(node_count, 2, "FSM should have 2 nodes after join");

    // API should return 2 nodes
    let nodes = client.nodes().await.unwrap();
    assert_eq!(nodes.len(), 2, "API should return 2 nodes");
    assert!(nodes.iter().any(|n| n.id == "joining-node"), "joined node should appear in node list");

    let _ = joiner_raft; // keep alive
    cluster.shutdown().await;
}

#[parameterized(
    get_status      = { "GET",    "/v1/status" },
    get_nodes       = { "GET",    "/v1/nodes" },
    get_services    = { "GET",    "/v1/services" },
    get_tasks       = { "GET",    "/v1/tasks" },
    get_secrets     = { "GET",    "/v1/secrets" },
    get_volumes     = { "GET",    "/v1/volumes" },
    get_join_info   = { "GET",    "/v1/join-info" },
    get_cluster_key = { "GET",    "/v1/cluster-key" },
    post_deploy     = { "POST",   "/v1/deploy" },
    post_join       = { "POST",   "/v1/join" },
    put_secret      = { "PUT",    "/v1/secrets/test" },
    delete_secret   = { "DELETE",  "/v1/secrets/test" },
)]
#[test_macro(tokio::test)]
async fn endpoint_requires_auth(method: &str, path: &str) {
    let cluster = TestCluster::new(1).await;
    let base = format!("http://{}", cluster.leader_api_addr());
    let client = reqwest::Client::new();

    let resp = client
        .request(method.parse().expect("valid HTTP method"), format!("{base}{path}"))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("request should reach server");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "{method} {path} should return 401 with bad token"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn join_info_next_raft_id_increments() {
    let cluster = TestCluster::new(1).await;
    let client = Api::new(cluster.leader_api_addr(), cluster.api_token());

    // Before join: next_raft_id should be 2
    let info1 = client.join_info().await.unwrap();
    assert_eq!(info1.next_raft_id, 2);

    // Register joiner in router and perform a join
    let joiner_raft = cluster.add_joiner_to_router(2).await;

    let join_req = mill_node::daemon::JoinRequest {
        node_id: "node-2".to_string(),
        raft_id: 2,
        advertise_addr: "5.6.7.8".to_string(),
        tunnel_ip: info1.tunnel_ip,
        public_key: "pubkey-2".to_string(),
        resources: NodeResources {
            cpu_total: 2.0,
            cpu_available: 2.0,
            memory_total: ByteSize::gib(4),
            memory_available: ByteSize::gib(4),
        },
        instance_id: None,
        rpc_address: None,
    };
    client.post_join(&join_req).await.unwrap();

    // Wait for replication.
    poll(|| cluster.raft(0).read_state(|fsm| fsm.nodes.len()) == 2)
        .expect("FSM should have 2 nodes after join")
        .await;

    // After join: next_raft_id should be 3
    let info2 = client.join_info().await.unwrap();
    assert_eq!(info2.next_raft_id, 3, "next_raft_id should increment after join");

    let _ = joiner_raft;
    cluster.shutdown().await;
}
