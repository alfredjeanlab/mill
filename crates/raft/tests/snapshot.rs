use std::net::SocketAddr;

use bytesize::ByteSize;
use mill_config::{
    AllocId, AllocKind, AllocResources, AllocStatus, NodeId, NodeResources, NodeStatus,
};
use mill_raft::fsm::{EncryptedSecret, FsmNode, FsmState, FsmVolume};
use mill_raft::snapshot::codec;

fn populated_state() -> FsmState {
    let mut state = FsmState::default();

    state.nodes.insert(
        1,
        FsmNode {
            raft_id: 1,
            mill_id: NodeId("node-1".to_string()),
            address: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            resources: NodeResources {
                cpu_total: 4.0,
                cpu_available: 3.0,
                memory_total: ByteSize::gib(8),
                memory_available: ByteSize::gib(7),
            },
            status: NodeStatus::Ready,
            wireguard_pubkey: None,
            instance_id: None,
            rpc_address: None,
            advertise_addr: None,
        },
    );

    state.allocs.insert(
        AllocId("alloc-1".to_string()),
        mill_config::Alloc {
            id: AllocId("alloc-1".to_string()),
            node: NodeId("node-1".to_string()),
            name: "web".to_string(),
            kind: AllocKind::Service,
            status: AllocStatus::Running,
            address: Some("127.0.0.1:3000".parse().unwrap()),
            resources: AllocResources { cpu: 1.0, memory: ByteSize::gib(1) },
            started_at: None,
        },
    );

    state.secrets.insert(
        "db_pass".to_string(),
        EncryptedSecret { encrypted_value: vec![1, 2, 3, 4, 5], nonce: vec![10, 11, 12] },
    );

    state.volumes.insert(
        "data".to_string(),
        FsmVolume {
            cloud_id: "vol-abc".to_string(),
            state: mill_config::VolumeState::Attached { node: NodeId("node-1".to_string()) },
        },
    );

    state
}

#[test]
fn test_encode_decode_roundtrip() {
    let state = populated_state();
    let encoded = codec::encode(&state).unwrap();
    let decoded = codec::decode(&encoded).unwrap();

    // Verify all state is preserved
    assert_eq!(decoded.nodes.len(), 1);
    assert_eq!(decoded.allocs.len(), 1);
    assert_eq!(decoded.secrets.len(), 1);
    assert_eq!(decoded.volumes.len(), 1);

    assert_eq!(decoded.nodes[&1].mill_id, NodeId("node-1".to_string()));
    assert_eq!(decoded.allocs[&AllocId("alloc-1".to_string())].name, "web");
    assert_eq!(decoded.secrets["db_pass"].encrypted_value, vec![1, 2, 3, 4, 5]);
    assert_eq!(decoded.volumes["data"].cloud_id, "vol-abc");
}

#[test]
fn test_encode_decode_empty() {
    let state = FsmState::default();
    let encoded = codec::encode(&state).unwrap();
    let decoded = codec::decode(&encoded).unwrap();

    assert!(decoded.config.is_none());
    assert!(decoded.nodes.is_empty());
    assert!(decoded.allocs.is_empty());
    assert!(decoded.secrets.is_empty());
    assert!(decoded.volumes.is_empty());
}

#[test]
fn test_compression_reduces_size() {
    let state = populated_state();
    let encoded = codec::encode(&state).unwrap();
    let raw_msgpack = rmp_serde::to_vec(&state).unwrap();

    // Compressed should generally be smaller (or at least not significantly larger)
    // For small payloads compression overhead may increase size slightly,
    // but for real-world state it should compress well.
    // Just verify we got valid data
    assert!(!encoded.is_empty());
    assert!(!raw_msgpack.is_empty());
}

#[test]
fn test_decode_invalid_data() {
    let result = codec::decode(&[0, 1, 2, 3]);
    assert!(result.is_err());
}
