mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mill_config::{AllocId, AllocKind, ClusterConfig, NodeId};
use mill_raft::MillRaft;
use mill_raft::fsm::command::{Command, Response};
use mill_raft::network::TestRouter;
use mill_raft::secrets::ClusterKey;
use openraft::Config;

use mill_config::{poll, poll_async};

use common::{test_alloc_resources, test_resources};

fn test_raft_config() -> Arc<Config> {
    let config = Config {
        heartbeat_interval: 100,
        election_timeout_min: 200,
        election_timeout_max: 400,
        ..Default::default()
    };
    Arc::new(config.validate().unwrap())
}

async fn create_cluster(
    node_ids: &[u64],
    router: &TestRouter,
    cluster_key: &ClusterKey,
) -> Vec<MillRaft> {
    let config = test_raft_config();
    let mut nodes = Vec::new();

    for &id in node_ids {
        let raft =
            MillRaft::new(id, config.clone(), router.clone(), cluster_key.clone()).await.unwrap();
        router.add_node(id, raft.raft().clone());
        nodes.push(raft);
    }

    nodes
}

async fn initialize_cluster(nodes: &[MillRaft], node_ids: &[u64]) {
    // Initialize node 0 as single-node cluster
    nodes[0].initialize().await.unwrap();

    // Add other nodes as learners
    for &id in &node_ids[1..] {
        nodes[0].add_learner(id, "").await.unwrap();
    }

    // Promote all to voters
    nodes[0].change_membership(node_ids.to_vec()).await.unwrap();

    // change_membership waits for quorum commit; callers call find_leader
    // which polls until a leader is confirmed.
}

async fn find_leader(nodes: &[MillRaft]) -> usize {
    for (i, node) in nodes.iter().enumerate() {
        if node.ensure_linearizable().await.is_ok() {
            return i;
        }
    }
    panic!("no leader found");
}

#[tokio::test]
async fn test_leader_election() {
    let router = TestRouter::new();
    let cluster_key = ClusterKey::generate();
    let node_ids = [1, 2, 3];

    let nodes = create_cluster(&node_ids, &router, &cluster_key).await;
    initialize_cluster(&nodes, &node_ids).await;

    // Verify a leader was elected within 2 seconds.
    poll_async(|| async {
        for node in &nodes {
            if node.ensure_linearizable().await.is_ok() {
                return true;
            }
        }
        false
    })
    .expect("leader not elected within 2 seconds")
    .await;

    for node in &nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn test_command_replication() {
    let router = TestRouter::new();
    let cluster_key = ClusterKey::generate();
    let node_ids = [1, 2, 3];

    let nodes = create_cluster(&node_ids, &router, &cluster_key).await;
    initialize_cluster(&nodes, &node_ids).await;

    let leader_idx = find_leader(&nodes).await;

    // Deploy config
    let config = ClusterConfig { services: HashMap::new(), tasks: HashMap::new() };
    let resp = nodes[leader_idx].propose(Command::Deploy(config)).await.unwrap();
    assert!(matches!(resp, Response::Ok));

    // Register a node
    let resp = nodes[leader_idx]
        .propose(Command::NodeRegister {
            id: 1,
            mill_id: NodeId("mill-node-1".to_string()),
            address: "127.0.0.1:8080".parse().unwrap(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Ok));

    // Schedule an allocation
    let resp = nodes[leader_idx]
        .propose(Command::AllocScheduled {
            alloc_id: AllocId("web-1".to_string()),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: Some("127.0.0.1:3000".parse().unwrap()),
            resources: test_alloc_resources(),
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Ok));

    // Wait for replication.
    poll(|| nodes.iter().all(|n| n.read_state(|s| s.list_allocs().len()) == 1))
        .expect("alloc not replicated to all nodes")
        .await;

    // Verify state on ALL nodes (not just leader)
    for node in &nodes {
        let has_config = node.read_state(|s| s.config.is_some());
        assert!(has_config, "config should be replicated");

        let node_count = node.read_state(|s| s.list_nodes().len());
        assert_eq!(node_count, 1, "node should be replicated");

        let alloc_count = node.read_state(|s| s.list_allocs().len());
        assert_eq!(alloc_count, 1, "alloc should be replicated");

        let alloc_name =
            node.read_state(|s| s.list_service_allocs("web").first().map(|a| a.name.clone()));
        assert_eq!(alloc_name, Some("web".to_string()));
    }

    for node in &nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn test_leader_failover() {
    let router = TestRouter::new();
    let cluster_key = ClusterKey::generate();
    let node_ids = [1, 2, 3];

    let nodes = create_cluster(&node_ids, &router, &cluster_key).await;
    initialize_cluster(&nodes, &node_ids).await;

    // Find initial leader
    let leader_idx = find_leader(&nodes).await;

    // Submit a command before failover
    let resp = nodes[leader_idx]
        .propose(Command::NodeRegister {
            id: 1,
            mill_id: NodeId("node-1".to_string()),
            address: "127.0.0.1:8080".parse().unwrap(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Ok));

    // Simulate leader crash: remove from router (network partition)
    let leader_raft_id = node_ids[leader_idx];
    router.remove_node(leader_raft_id);

    // Trigger election on surviving nodes
    for (i, node) in nodes.iter().enumerate() {
        if i != leader_idx {
            let _ = node.raft().trigger().elect().await;
        }
    }

    // Wait for new leader to emerge
    poll_async(|| async {
        for (i, node) in nodes.iter().enumerate() {
            if i == leader_idx {
                continue;
            }
            if let Some(lid) = node.raft().current_leader().await
                && lid != leader_raft_id
                && node.ensure_linearizable().await.is_ok()
            {
                return true;
            }
        }
        false
    })
    .expect("new leader should be elected after failover")
    .await;

    // Identify the new leader.
    let mut new_leader_idx = 0;
    for (i, node) in nodes.iter().enumerate() {
        if i != leader_idx && node.ensure_linearizable().await.is_ok() {
            new_leader_idx = i;
            break;
        }
    }
    assert_ne!(new_leader_idx, leader_idx);

    // Submit a command to the new leader
    let resp = nodes[new_leader_idx]
        .propose(Command::NodeRegister {
            id: 2,
            mill_id: NodeId("node-2".to_string()),
            address: "127.0.0.1:8081".parse().unwrap(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Ok));

    // Verify state includes both commands
    let node_count = nodes[new_leader_idx].read_state(|s| s.list_nodes().len());
    assert_eq!(node_count, 2, "both nodes should be in state");

    // Shut down surviving nodes
    for (i, node) in nodes.iter().enumerate() {
        if i != leader_idx {
            node.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn test_secret_roundtrip() {
    let router = TestRouter::new();
    let cluster_key = ClusterKey::generate();
    let node_ids = [1, 2, 3];

    let nodes = create_cluster(&node_ids, &router, &cluster_key).await;
    initialize_cluster(&nodes, &node_ids).await;

    let leader_idx = find_leader(&nodes).await;
    let leader = &nodes[leader_idx];

    // Encrypt, store through Raft, retrieve, decrypt
    let plaintext = b"super-secret-db-password";
    let (ciphertext, nonce) = leader.encrypt_secret(plaintext).unwrap();

    let resp = leader
        .propose(Command::SecretSet {
            name: "db_password".to_string(),
            encrypted_value: ciphertext.clone(),
            nonce: nonce.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Ok));

    // Wait for replication.
    poll(|| nodes.iter().all(|n| n.read_state(|s| s.get_secret("db_password").is_some())))
        .expect("secret not replicated to all nodes")
        .await;

    // Retrieve and decrypt from each node
    for node in &nodes {
        let secret = node.read_state(|s| s.get_secret("db_password").cloned());
        let secret = secret.expect("secret should exist");

        let decrypted = node.decrypt_secret(&secret.encrypted_value, &secret.nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    // Verify secret names list
    let names = leader
        .read_state(|s| s.list_secret_names().into_iter().map(String::from).collect::<Vec<_>>());
    assert_eq!(names, vec!["db_password"]);

    for node in &nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn test_snapshot_restore() {
    let router = TestRouter::new();
    let cluster_key = ClusterKey::generate();
    let node_ids = [1, 2, 3];

    let nodes = create_cluster(&node_ids, &router, &cluster_key).await;
    initialize_cluster(&nodes, &node_ids).await;

    let leader_idx = find_leader(&nodes).await;
    let leader = &nodes[leader_idx];

    // Apply some state
    leader
        .propose(Command::NodeRegister {
            id: 1,
            mill_id: NodeId("node-1".to_string()),
            address: "127.0.0.1:8080".parse().unwrap(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        })
        .await
        .unwrap();

    leader
        .propose(Command::SecretSet {
            name: "key".to_string(),
            encrypted_value: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
        })
        .await
        .unwrap();

    // Build a snapshot on the leader's state machine
    let original_state = leader.read_state(|s| s.clone());

    // Encode and decode the state (simulates snapshot round-trip)
    let encoded = mill_raft::snapshot::codec::encode(&original_state).unwrap();
    let restored_state = mill_raft::snapshot::codec::decode(&encoded).unwrap();

    // Verify restored state matches
    assert_eq!(restored_state.nodes.len(), original_state.nodes.len());
    assert_eq!(restored_state.allocs.len(), original_state.allocs.len());
    assert_eq!(restored_state.secrets.len(), original_state.secrets.len());

    assert!(restored_state.nodes.contains_key(&1));
    assert_eq!(restored_state.nodes[&1].mill_id, NodeId("node-1".to_string()));
    assert_eq!(restored_state.secrets["key"].encrypted_value, vec![1, 2, 3]);

    for node in &nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn test_snapshot_install() {
    let router = TestRouter::new();
    let cluster_key = ClusterKey::generate();
    let node_ids = [1, 2, 3];

    let nodes = create_cluster(&node_ids, &router, &cluster_key).await;
    initialize_cluster(&nodes, &node_ids).await;

    let leader_idx = find_leader(&nodes).await;
    let leader = &nodes[leader_idx];

    // Apply a variety of state: config, node, alloc, secret, volume
    leader
        .propose(Command::Deploy(ClusterConfig { services: HashMap::new(), tasks: HashMap::new() }))
        .await
        .unwrap();

    leader
        .propose(Command::NodeRegister {
            id: 1,
            mill_id: NodeId("node-1".into()),
            address: "127.0.0.1:8080".parse().unwrap(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        })
        .await
        .unwrap();

    leader
        .propose(Command::SecretSet {
            name: "my_secret".into(),
            encrypted_value: vec![10, 20, 30],
            nonce: vec![40, 50, 60],
        })
        .await
        .unwrap();

    leader
        .propose(Command::VolumeCreated { name: "data-vol".into(), cloud_id: "vol-abc".into() })
        .await
        .unwrap();

    leader
        .propose(Command::AllocScheduled {
            alloc_id: AllocId("web-0".into()),
            node_id: 1,
            name: "web".into(),
            kind: AllocKind::Service,
            address: Some("10.0.0.1:8080".parse().unwrap()),
            resources: test_alloc_resources(),
        })
        .await
        .unwrap();

    // Wait for replication before snapshot.
    poll(|| nodes.iter().all(|n| n.read_state(|s| !s.volumes.is_empty())))
        .expect("state not replicated before snapshot")
        .await;

    // Force a snapshot on the leader.
    leader.raft().trigger().snapshot().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create node 4 and add it to the cluster
    let config = test_raft_config();
    let node4 = MillRaft::new(4, config, router.clone(), cluster_key).await.unwrap();
    router.add_node(4, node4.raft().clone());

    leader.add_learner(4, "").await.unwrap();
    leader.change_membership(vec![1, 2, 3, 4]).await.unwrap();

    // Poll node 4 until state converges.
    poll(|| {
        node4.read_state(|fsm| {
            fsm.config.is_some()
                && !fsm.nodes.is_empty()
                && !fsm.allocs.is_empty()
                && !fsm.secrets.is_empty()
                && !fsm.volumes.is_empty()
        })
    })
    .expect("node 4 state did not converge")
    .await;

    // Verify full state matches leader
    let leader_state = leader.read_state(|s| s.clone());
    let node4_state = node4.read_state(|s| s.clone());

    assert!(node4_state.config.is_some());
    assert_eq!(node4_state.nodes.len(), leader_state.nodes.len());
    assert_eq!(node4_state.allocs.len(), leader_state.allocs.len());
    assert_eq!(node4_state.secrets.len(), leader_state.secrets.len());
    assert_eq!(node4_state.volumes.len(), leader_state.volumes.len());

    assert!(node4_state.nodes.contains_key(&1));
    assert!(node4_state.allocs.contains_key(&AllocId("web-0".into())));
    assert!(node4_state.secrets.contains_key("my_secret"));
    assert!(node4_state.volumes.contains_key("data-vol"));

    for node in &nodes {
        node.shutdown().await.unwrap();
    }
    node4.shutdown().await.unwrap();
}

#[tokio::test]
async fn open_restores_from_snapshot() {
    use mill_raft::network::http::HttpNetwork;

    let dir = tempfile::tempdir().unwrap();
    let raft_dir = dir.path().join("raft");
    let cluster_key = ClusterKey::generate();
    let config = test_raft_config();

    // Phase 1: open, initialize, propose, snapshot, shutdown
    {
        let network = HttpNetwork::new();
        let raft = MillRaft::open(1, config.clone(), network, cluster_key.clone(), &raft_dir)
            .await
            .unwrap();
        raft.initialize().await.unwrap();

        raft.propose(Command::NodeRegister {
            id: 1,
            mill_id: NodeId("persisted-node".into()),
            address: "127.0.0.1:8080".parse().unwrap(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        })
        .await
        .unwrap();

        // Trigger snapshot to flush to disk.
        raft.raft().trigger().snapshot().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        raft.shutdown().await.unwrap();
    }

    // Phase 2: re-open from same dir and verify state
    {
        let network = HttpNetwork::new();
        let raft = MillRaft::open(1, config, network, cluster_key, &raft_dir).await.unwrap();

        let node_count = raft.read_state(|fsm| fsm.nodes.len());
        assert_eq!(node_count, 1, "restored state should have 1 node");

        let has_node = raft.read_state(|fsm| {
            fsm.nodes.values().any(|n| n.mill_id == NodeId("persisted-node".into()))
        });
        assert!(has_node, "restored state should contain the persisted node");

        raft.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn open_starts_empty_without_snapshot() {
    use mill_raft::network::http::HttpNetwork;

    let dir = tempfile::tempdir().unwrap();
    let raft_dir = dir.path().join("raft");
    let cluster_key = ClusterKey::generate();
    let config = test_raft_config();

    let network = HttpNetwork::new();
    let raft = MillRaft::open(1, config, network, cluster_key, &raft_dir).await.unwrap();

    let node_count = raft.read_state(|fsm| fsm.nodes.len());
    assert_eq!(node_count, 0, "fresh raft should have 0 nodes");

    raft.shutdown().await.unwrap();
}
