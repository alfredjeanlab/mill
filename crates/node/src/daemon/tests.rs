use mill_config::DataDir;
use mill_raft::secrets::ClusterKey;

use super::*;

#[test]
fn detect_resources_returns_nonzero() {
    let res = detect_resources();
    assert!(res.cpu_total > 0.0);
    assert!(res.memory_total.as_u64() > 0);
}

#[tokio::test]
async fn node_identity_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.json");

    let identity = NodeIdentity {
        node_id: "test-node".to_string(),
        raft_id: 1,
        cluster_token: "tok123".to_string(),
        rpc_port: 4400,
        advertise_addr: "1.2.3.4".to_string(),
        provider: None,
        provider_token: None,
        region: None,
        wireguard_subnet: "10.99.0.0/16".to_string(),
        peers: vec![PeerEntry {
            raft_id: 2,
            advertise_addr: "5.6.7.8".to_string(),
            tunnel_ip: "10.99.0.2".parse().expect("parse"),
            public_key: "abc123".to_string(),
        }],
        acme_email: Some("admin@example.com".to_string()),
        acme_staging: Some(true),
    };

    identity.save(&path).await.expect("save");
    let loaded = NodeIdentity::load(&path).await.expect("load");
    assert_eq!(loaded.node_id, "test-node");
    assert_eq!(loaded.raft_id, 1);
    assert_eq!(loaded.peers.len(), 1);
    assert_eq!(loaded.peers[0].raft_id, 2);
    assert_eq!(loaded.acme_email.as_deref(), Some("admin@example.com"));
    assert_eq!(loaded.acme_staging, Some(true));
}

#[test]
fn generate_id_is_unique() {
    let a = generate_id();
    let b = generate_id();
    assert_ne!(a, b);
    assert_eq!(a.len(), 16, "generate_id should produce 16 hex chars");
}

#[test]
fn generate_token_is_unique() {
    let a = generate_token();
    let b = generate_token();
    assert_ne!(a, b);
    assert_eq!(a.len(), 32, "generate_token should produce 32 hex chars");
}

#[tokio::test]
async fn node_identity_load_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nonexistent.json");
    let result = NodeIdentity::load(&path).await;
    assert!(result.is_err(), "loading nonexistent node.json should fail");
}

#[tokio::test]
async fn boot_without_node_json_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = DataDir::new(dir.path());
    let result = boot(BootOpts { data_dir }).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("node.json"), "error should mention node.json: {err}");
}

#[tokio::test]
async fn leave_without_node_json_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = DataDir::new(dir.path());
    let result = leave(&data_dir).await;
    assert!(result.is_err(), "leave with missing node.json should fail");
}

#[tokio::test]
async fn identity_and_cluster_key_survive_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = DataDir::new(dir.path());

    // Save identity
    let identity = NodeIdentity {
        node_id: "restart-node".to_string(),
        raft_id: 5,
        cluster_token: "tok-restart".to_string(),
        rpc_port: 4400,
        advertise_addr: "9.8.7.6".to_string(),
        provider: Some("aws".to_string()),
        provider_token: Some("pt-123".to_string()),
        region: None,
        wireguard_subnet: "10.99.0.0/16".to_string(),
        peers: vec![],
        acme_email: Some("ops@example.com".to_string()),
        acme_staging: None,
    };
    identity.save(&data_dir.node_json_path()).await.expect("save identity");

    // Save cluster key
    let cluster_key = ClusterKey::generate();
    let raft_dir = data_dir.raft_dir();
    cluster_key.save(&raft_dir).await.expect("save cluster key");

    // Load both back
    let loaded_id = NodeIdentity::load(&data_dir.node_json_path()).await.expect("load identity");
    let loaded_key = ClusterKey::load(&raft_dir).await.expect("load cluster key");

    assert_eq!(loaded_id.node_id, "restart-node");
    assert_eq!(loaded_id.raft_id, 5);
    assert_eq!(loaded_id.cluster_token, "tok-restart");
    assert_eq!(loaded_id.provider, Some("aws".to_string()));
    assert_eq!(loaded_id.acme_email.as_deref(), Some("ops@example.com"));
    assert_eq!(loaded_id.acme_staging, None);
    assert_eq!(loaded_key.as_bytes(), cluster_key.as_bytes());
}

#[tokio::test]
async fn node_identity_backward_compat_no_acme_fields() {
    // Old node.json without acme_email / acme_staging should deserialize fine
    let json = r#"{
        "node_id": "old-node",
        "raft_id": 1,
        "cluster_token": "tok",
        "rpc_port": 4400,
        "advertise_addr": "0.0.0.0",
        "wireguard_subnet": "10.99.0.0/16",
        "peers": []
    }"#;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.json");
    tokio::fs::write(&path, json).await.expect("write");
    let loaded = NodeIdentity::load(&path).await.expect("load");
    assert_eq!(loaded.node_id, "old-node");
    assert!(loaded.acme_email.is_none());
    assert!(loaded.acme_staging.is_none());
    assert!(loaded.acme_config().is_none());
}

#[test]
fn acme_config_from_identity() {
    let identity = NodeIdentity {
        node_id: "n".to_string(),
        raft_id: 1,
        cluster_token: "t".to_string(),
        rpc_port: 4400,
        advertise_addr: "0.0.0.0".to_string(),
        provider: None,
        provider_token: None,
        region: None,
        wireguard_subnet: "10.99.0.0/16".to_string(),
        peers: vec![],
        acme_email: Some("admin@example.com".to_string()),
        acme_staging: Some(true),
    };
    let cfg = identity.acme_config().expect("should produce AcmeConfig");
    assert_eq!(cfg.contact, vec!["mailto:admin@example.com"]);
    assert!(cfg.directory_url.contains("staging"), "should use staging URL");
}

#[test]
fn acme_config_none_without_email() {
    let identity = NodeIdentity {
        node_id: "n".to_string(),
        raft_id: 1,
        cluster_token: "t".to_string(),
        rpc_port: 4400,
        advertise_addr: "0.0.0.0".to_string(),
        provider: None,
        provider_token: None,
        region: None,
        wireguard_subnet: "10.99.0.0/16".to_string(),
        peers: vec![],
        acme_email: None,
        acme_staging: None,
    };
    assert!(identity.acme_config().is_none());
}

#[test]
fn acme_config_production_url_without_staging_flag() {
    let identity = NodeIdentity {
        node_id: "n".to_string(),
        raft_id: 1,
        cluster_token: "t".to_string(),
        rpc_port: 4400,
        advertise_addr: "0.0.0.0".to_string(),
        provider: None,
        provider_token: None,
        region: None,
        wireguard_subnet: "10.99.0.0/16".to_string(),
        peers: vec![],
        acme_email: Some("admin@example.com".to_string()),
        acme_staging: None,
    };
    let cfg = identity.acme_config().expect("should produce AcmeConfig");
    assert!(!cfg.directory_url.contains("staging"), "should use production URL");
    assert!(cfg.directory_url.contains("acme-v02.api.letsencrypt.org"));
}
