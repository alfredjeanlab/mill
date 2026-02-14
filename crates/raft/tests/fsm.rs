mod common;

use std::collections::HashMap;
use std::net::SocketAddr;

use bytesize::ByteSize;
use mill_config::{
    AllocId, AllocKind, AllocStatus, ClusterConfig, NodeId, NodeStatus, VolumeState,
};
use mill_raft::fsm::{Command, FsmState, Response, apply_command};
use yare::parameterized;

use common::{test_alloc_resources, test_resources};

fn test_addr() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}

fn test_config() -> ClusterConfig {
    ClusterConfig { services: HashMap::new(), tasks: HashMap::new() }
}

fn register_node(state: &mut FsmState, id: u64, mill_id: &str) {
    let resp = apply_command(
        state,
        Command::NodeRegister {
            id,
            mill_id: NodeId(mill_id.to_string()),
            address: test_addr(),
            resources: test_resources(),
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        },
    );
    assert!(matches!(resp, Response::Ok));
}

#[test]
fn test_deploy() {
    let mut state = FsmState::default();
    assert!(state.config.is_none());

    let resp = apply_command(&mut state, Command::Deploy(test_config()));
    assert!(matches!(resp, Response::Ok));
    assert!(state.config.is_some());

    // Deploy again replaces config
    let resp = apply_command(&mut state, Command::Deploy(test_config()));
    assert!(matches!(resp, Response::Ok));
}

#[test]
fn test_node_register() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let nodes = state.list_nodes();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].raft_id, 1);
    assert_eq!(nodes[0].mill_id, NodeId("node-1".to_string()));
    assert_eq!(nodes[0].status, NodeStatus::Ready);
}

#[test]
fn test_node_heartbeat() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    // Schedule an alloc first
    let alloc_id = AllocId("alloc-1".to_string());
    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: alloc_id.clone(),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: Some(test_addr()),
            resources: test_alloc_resources(),
        },
    );

    // Heartbeat updates alloc status
    let mut statuses = HashMap::new();
    statuses.insert(alloc_id.clone(), AllocStatus::Running);
    let resp =
        apply_command(&mut state, Command::NodeHeartbeat { id: 1, alloc_statuses: statuses });
    assert!(matches!(resp, Response::Ok));
    assert_eq!(state.allocs[&alloc_id].status, AllocStatus::Running);
}

#[parameterized(
    heartbeat_unknown_node = {
        Command::NodeHeartbeat { id: 99, alloc_statuses: HashMap::new() },
    },
    node_down_unknown = {
        Command::NodeDown { id: 99 },
    },
    node_drain_unknown = {
        Command::NodeDrain { id: 99 },
    },
    node_remove_unknown = {
        Command::NodeRemove { id: 99 },
    },
    alloc_scheduled_unknown_node = {
        Command::AllocScheduled {
            alloc_id: AllocId("alloc-1".to_string()),
            node_id: 99,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    },
    alloc_status_unknown = {
        Command::AllocStatus {
            alloc_id: AllocId("nope".to_string()),
            status: AllocStatus::Running,
        },
    },
    alloc_running_unknown = {
        Command::AllocRunning {
            alloc_id: AllocId("nope".to_string()),
            address: None,
            started_at: None,
        },
    },
    secret_delete_unknown = {
        Command::SecretDelete { name: "nope".to_string() },
    },
    volume_attach_unknown = {
        Command::VolumeAttached { name: "nope".to_string(), node: 1 },
    },
    volume_detach_unknown = {
        Command::VolumeDetached { name: "nope".to_string() },
    },
)]
fn unknown_entity_returns_error(command: Command) {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");
    let resp = apply_command(&mut state, command);
    assert!(matches!(resp, Response::Error(_)), "expected Error, got: {resp:?}");
}

#[test]
fn test_node_down_and_heartbeat_recovery() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    // Mark down
    let resp = apply_command(&mut state, Command::NodeDown { id: 1 });
    assert!(matches!(resp, Response::Ok));
    assert_eq!(state.nodes[&1].status, NodeStatus::Down);

    // Heartbeat brings it back
    let resp =
        apply_command(&mut state, Command::NodeHeartbeat { id: 1, alloc_statuses: HashMap::new() });
    assert!(matches!(resp, Response::Ok));
    assert_eq!(state.nodes[&1].status, NodeStatus::Ready);
}

#[test]
fn test_alloc_scheduled() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let alloc_id = AllocId("alloc-1".to_string());
    let resp = apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: alloc_id.clone(),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: Some(test_addr()),
            resources: test_alloc_resources(),
        },
    );
    assert!(matches!(resp, Response::Ok));

    let allocs = state.list_allocs();
    assert_eq!(allocs.len(), 1);
    assert_eq!(allocs[0].id, alloc_id);
    assert_eq!(allocs[0].node, NodeId("node-1".to_string()));
    assert_eq!(allocs[0].name, "web");
    assert!(matches!(allocs[0].status, AllocStatus::Pulling));
}

#[test]
fn test_alloc_status_update() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let alloc_id = AllocId("alloc-1".to_string());
    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: alloc_id.clone(),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );

    let resp = apply_command(
        &mut state,
        Command::AllocStatus { alloc_id: alloc_id.clone(), status: AllocStatus::Healthy },
    );
    assert!(matches!(resp, Response::Ok));
    assert_eq!(state.allocs[&alloc_id].status, AllocStatus::Healthy);
}

#[test]
fn test_list_service_allocs() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: AllocId("web-1".to_string()),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );
    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: AllocId("api-1".to_string()),
            node_id: 1,
            name: "api".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );

    let web_allocs = state.list_service_allocs("web");
    assert_eq!(web_allocs.len(), 1);
    assert_eq!(web_allocs[0].name, "web");
}

#[test]
fn test_secret_set_and_get() {
    let mut state = FsmState::default();
    let resp = apply_command(
        &mut state,
        Command::SecretSet {
            name: "db_password".to_string(),
            encrypted_value: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
        },
    );
    assert!(matches!(resp, Response::Ok));

    let secret = state.get_secret("db_password");
    assert!(secret.is_some());
    assert_eq!(secret.unwrap().encrypted_value, vec![1, 2, 3]);
    assert_eq!(secret.unwrap().nonce, vec![4, 5, 6]);
}

#[test]
fn test_secret_delete() {
    let mut state = FsmState::default();
    apply_command(
        &mut state,
        Command::SecretSet {
            name: "db_password".to_string(),
            encrypted_value: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
        },
    );

    let resp = apply_command(&mut state, Command::SecretDelete { name: "db_password".to_string() });
    assert!(matches!(resp, Response::Ok));
    assert!(state.get_secret("db_password").is_none());
}

#[test]
fn test_list_secret_names() {
    let mut state = FsmState::default();
    apply_command(
        &mut state,
        Command::SecretSet { name: "key_a".to_string(), encrypted_value: vec![], nonce: vec![] },
    );
    apply_command(
        &mut state,
        Command::SecretSet { name: "key_b".to_string(), encrypted_value: vec![], nonce: vec![] },
    );

    let names = state.list_secret_names();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"key_a"));
    assert!(names.contains(&"key_b"));
}

#[test]
fn test_volume_lifecycle() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");
    register_node(&mut state, 2, "node-2");

    // Create — volume starts in Ready state
    let resp = apply_command(
        &mut state,
        Command::VolumeCreated { name: "data".to_string(), cloud_id: "vol-123".to_string() },
    );
    assert!(matches!(resp, Response::Ok));

    let vols = state.list_volumes();
    assert_eq!(vols.len(), 1);
    assert!(matches!(vols[0].1.state, VolumeState::Ready));

    // Attach to node-1
    let resp =
        apply_command(&mut state, Command::VolumeAttached { name: "data".to_string(), node: 1 });
    assert!(matches!(resp, Response::Ok));
    assert!(matches!(
        state.volumes["data"].state,
        VolumeState::Attached { ref node } if *node == NodeId("node-1".to_string())
    ));

    // Detach
    let resp = apply_command(&mut state, Command::VolumeDetached { name: "data".to_string() });
    assert!(matches!(resp, Response::Ok));
    assert!(matches!(state.volumes["data"].state, VolumeState::Ready));

    // Attach to different node
    let resp =
        apply_command(&mut state, Command::VolumeAttached { name: "data".to_string(), node: 2 });
    assert!(matches!(resp, Response::Ok));
    assert!(matches!(
        state.volumes["data"].state,
        VolumeState::Attached { ref node } if *node == NodeId("node-2".to_string())
    ));

    // Destroy
    let resp = apply_command(&mut state, Command::VolumeDestroyed { name: "data".to_string() });
    assert!(matches!(resp, Response::Ok));
    assert!(state.list_volumes().is_empty());
}

#[test]
fn test_node_drain() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let resp = apply_command(&mut state, Command::NodeDrain { id: 1 });
    assert!(matches!(resp, Response::Ok));
    assert_eq!(state.nodes[&1].status, NodeStatus::Draining);
}

#[test]
fn test_node_remove_empty() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let resp = apply_command(&mut state, Command::NodeRemove { id: 1 });
    assert!(matches!(resp, Response::Ok));
    assert!(state.nodes.is_empty());
}

#[test]
fn test_node_remove_with_stopped_allocs() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: AllocId("alloc-1".to_string()),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );
    apply_command(
        &mut state,
        Command::AllocStatus {
            alloc_id: AllocId("alloc-1".to_string()),
            status: AllocStatus::Stopped { exit_code: 0 },
        },
    );

    // Should succeed — only stopped allocs.
    let resp = apply_command(&mut state, Command::NodeRemove { id: 1 });
    assert!(matches!(resp, Response::Ok));
    assert!(state.nodes.is_empty());
    assert!(state.allocs.is_empty()); // Stopped allocs cleaned up.
}

#[test]
fn test_node_remove_rejected_with_active_allocs() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: AllocId("alloc-1".to_string()),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );

    // Alloc is in Pulling status (active) — remove should be rejected.
    let resp = apply_command(&mut state, Command::NodeRemove { id: 1 });
    assert!(matches!(resp, Response::Error(_)));
    assert!(!state.nodes.is_empty()); // Node still present.
}

#[test]
fn test_alloc_running_sets_address() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let alloc_id = AllocId("alloc-1".to_string());
    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: alloc_id.clone(),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );

    // Address should be None initially.
    assert!(state.allocs[&alloc_id].address.is_none());
    assert!(matches!(state.allocs[&alloc_id].status, AllocStatus::Pulling));

    // Apply AllocRunning with an address.
    let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let resp = apply_command(
        &mut state,
        Command::AllocRunning { alloc_id: alloc_id.clone(), address: Some(addr), started_at: None },
    );
    assert!(matches!(resp, Response::Ok));
    assert_eq!(state.allocs[&alloc_id].status, AllocStatus::Running);
    assert_eq!(state.allocs[&alloc_id].address, Some(addr));
}

#[test]
fn test_alloc_running_no_resurrect() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    let alloc_id = AllocId("alloc-1".to_string());
    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: alloc_id.clone(),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );

    // Advance to Stopped.
    apply_command(
        &mut state,
        Command::AllocStatus {
            alloc_id: alloc_id.clone(),
            status: AllocStatus::Stopped { exit_code: 0 },
        },
    );
    assert!(matches!(state.allocs[&alloc_id].status, AllocStatus::Stopped { .. }));

    // A delayed AllocRunning must not resurrect the alloc.
    let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let resp = apply_command(
        &mut state,
        Command::AllocRunning { alloc_id: alloc_id.clone(), address: Some(addr), started_at: None },
    );
    assert!(matches!(resp, Response::Ok)); // Valid command, just a no-op.
    assert!(matches!(state.allocs[&alloc_id].status, AllocStatus::Stopped { .. }));
    assert!(state.allocs[&alloc_id].address.is_none());
}

#[test]
fn test_node_available_resources() {
    let mut state = FsmState::default();
    register_node(&mut state, 1, "node-1");

    // Before any allocs, full resources available
    let avail = state.node_available_resources(1).unwrap();
    assert_eq!(avail.cpu_available, 4.0);
    assert_eq!(avail.memory_available, ByteSize::gib(8));

    // Schedule an alloc
    apply_command(
        &mut state,
        Command::AllocScheduled {
            alloc_id: AllocId("alloc-1".to_string()),
            node_id: 1,
            name: "web".to_string(),
            kind: AllocKind::Service,
            address: None,
            resources: test_alloc_resources(),
        },
    );

    let avail = state.node_available_resources(1).unwrap();
    assert_eq!(avail.cpu_available, 3.0);
    assert_eq!(avail.memory_available, ByteSize::gib(7));

    // Stopped allocs don't count
    apply_command(
        &mut state,
        Command::AllocStatus {
            alloc_id: AllocId("alloc-1".to_string()),
            status: AllocStatus::Stopped { exit_code: 0 },
        },
    );
    let avail = state.node_available_resources(1).unwrap();
    assert_eq!(avail.cpu_available, 4.0);
    assert_eq!(avail.memory_available, ByteSize::gib(8));
}

#[test]
fn test_node_available_resources_unknown() {
    let state = FsmState::default();
    assert!(state.node_available_resources(99).is_none());
}
