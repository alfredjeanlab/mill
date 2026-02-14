mod client;
mod identity;
mod support;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bytesize::ByteSize;
use mill_config::{DataDir, NodeId, NodeResources};
use mill_net::{Mesh, PeerInfo, WireGuard};
use mill_raft::MillRaft;
use mill_raft::fsm::command::Command;
use mill_raft::network::http::HttpNetwork;
use mill_raft::secrets::ClusterKey;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{NodeError, Result};
use crate::primary::{Primary, PrimaryConfig};
use crate::rpc::server::RpcServer;
use crate::secondary::{Secondary, SecondaryConfig};
use crate::storage::Volumes;
use support::{
    default_raft_config, generate_id, generate_token, remove_pid_file, shutdown,
    start_raft_listener, start_subsystems, wait_for_signal, write_pid_file,
};

pub use identity::{NodeIdentity, PeerEntry};

const WIREGUARD_INTERFACE: &str = "mill0";
pub(crate) const WIREGUARD_PORT: u16 = 51820;
const WIREGUARD_SUBNET: &str = "10.99.0.0/16";
const API_PORT: u16 = 4400;
const RPC_PORT: u16 = 4402;
const CONTAINERD_SOCKET: &str = "/run/containerd/containerd.sock";

/// Detect this node's hardware resources.
pub fn detect_resources() -> NodeResources {
    let cpu_total = num_cpus::get() as f64;
    let memory_total = {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        ByteSize::b(sys.total_memory())
    };
    NodeResources {
        cpu_total,
        cpu_available: cpu_total,
        memory_total,
        memory_available: memory_total,
    }
}

struct CloudMetadata {
    instance_id: Option<String>,
    region: Option<String>,
}

/// Query the cloud provider's metadata API for instance_id and region.
/// Currently supports DigitalOcean only. Returns empty metadata for
/// unknown or absent providers.
async fn detect_cloud_metadata(provider: Option<&str>) -> CloudMetadata {
    match provider {
        Some("digitalocean") => {
            let client = reqwest::Client::builder()
                .timeout(crate::env::metadata_timeout())
                .build()
                .unwrap_or_default();
            let instance_id = client
                .get("http://169.254.169.254/metadata/v1/id")
                .send()
                .await
                .ok()
                .and_then(|r| if r.status().is_success() { Some(r) } else { None });
            let instance_id = match instance_id {
                Some(r) => r.text().await.ok().map(|s| s.trim().to_string()),
                None => None,
            };
            let region = client
                .get("http://169.254.169.254/metadata/v1/region")
                .send()
                .await
                .ok()
                .and_then(|r| if r.status().is_success() { Some(r) } else { None });
            let region = match region {
                Some(r) => r.text().await.ok().map(|s| s.trim().to_string()),
                None => None,
            };
            CloudMetadata { instance_id, region }
        }
        _ => CloudMetadata { instance_id: None, region: None },
    }
}

/// Build a volume driver from provider config. Returns `None` if any
/// required field is missing.
fn build_volume_driver(
    provider: Option<&str>,
    token: Option<&str>,
    region: Option<&str>,
) -> Option<Arc<dyn Volumes>> {
    match (provider, token, region) {
        (Some("digitalocean"), Some(tok), Some(reg)) => {
            Some(Arc::new(crate::storage::digitalocean::DigitalOceanVolumes::new(
                tok.to_string(),
                reg.to_string(),
            )))
        }
        _ => None,
    }
}

/// Options for booting the daemon.
pub struct BootOpts {
    pub data_dir: DataDir,
}

/// Boot the daemon from existing `node.json`.
pub async fn boot(opts: BootOpts) -> Result<()> {
    let node_json = opts.data_dir.node_json_path();
    if !node_json.exists() {
        return Err(NodeError::Internal(
            "No node.json found. Run `mill init` or `mill join` first.".into(),
        ));
    }
    start_from_existing(&opts.data_dir).await
}

/// Initialize a new single-node cluster.
pub async fn init(
    data_dir: &DataDir,
    token: Option<String>,
    advertise: Option<String>,
    provider: Option<String>,
    provider_token: Option<String>,
    acme_email: Option<String>,
    acme_staging: bool,
) -> Result<()> {
    // 1. Generate identity
    let node_id = generate_id();
    let raft_id: u64 = 1;
    let cluster_token = token.unwrap_or_else(generate_token);
    let advertise_addr = advertise.unwrap_or_else(|| "0.0.0.0".to_string());

    // 2. Init WireGuard
    let key_path = data_dir.wireguard_dir().join("private.key");
    let (wg, wg_identity) =
        WireGuard::init(WIREGUARD_INTERFACE, WIREGUARD_PORT, WIREGUARD_SUBNET, Some(&key_path))
            .await?;
    let wg = Arc::new(wg);

    // 3. Generate and save cluster key
    let cluster_key = ClusterKey::generate();
    cluster_key.save(&data_dir.raft_dir()).await?;

    // 4. Write node.json
    let identity = NodeIdentity {
        node_id: node_id.clone(),
        raft_id,
        cluster_token: cluster_token.clone(),
        rpc_port: API_PORT,
        advertise_addr: advertise_addr.clone(),
        provider: provider.clone(),
        provider_token: provider_token.clone(),
        region: None,
        wireguard_subnet: WIREGUARD_SUBNET.to_string(),
        peers: Vec::new(),
        acme_email,
        acme_staging: if acme_staging { Some(true) } else { None },
    };
    identity.save(&data_dir.node_json_path()).await?;

    // 5. Create HTTP Raft network and start transport
    let network = HttpNetwork::new();

    // 6. Open Raft (persistent)
    let raft_config = default_raft_config();
    let raft =
        MillRaft::open(raft_id, raft_config, network.clone(), cluster_key, &data_dir.raft_dir())
            .await?;
    let raft = Arc::new(raft);

    // 7. Bootstrap single-node cluster
    raft.initialize().await?;

    // 8. Start subsystems
    let subs = start_subsystems(data_dir, identity.acme_config()).await?;

    // 9. Detect cloud metadata and update identity
    let cloud = detect_cloud_metadata(provider.as_deref()).await;
    if cloud.region.is_some() {
        let mut identity = identity;
        identity.region = cloud.region.clone();
        identity.save(&data_dir.node_json_path()).await?;
    }

    // 10. Register this node in Raft
    let resources = detect_resources();
    // Register the tunnel IP so remote nodes can reach the API via WireGuard
    let registered_api_addr = SocketAddr::new(wg_identity.tunnel_ip, API_PORT);
    // But bind to 0.0.0.0 so the API is accessible on all local interfaces
    let api_addr = SocketAddr::new(IpAddr::from([0, 0, 0, 0]), API_PORT);
    let rpc_addr: SocketAddr = SocketAddr::new(wg_identity.tunnel_ip, RPC_PORT);
    let register_cmd = Command::NodeRegister {
        id: raft_id,
        mill_id: NodeId(node_id.clone()),
        address: registered_api_addr,
        resources,
        wireguard_pubkey: Some(wg_identity.public_key.clone()),
        instance_id: cloud.instance_id,
        rpc_address: Some(rpc_addr),
        advertise_addr: Some(advertise_addr.clone()),
    };
    raft.propose(register_cmd).await?;

    // 11. Create and run secondary
    let secondary_config = SecondaryConfig {
        containerd_socket: CONTAINERD_SOCKET.to_string(),
        data_dir: data_dir.clone(),
        expected_allocs: Vec::new(),
        node_id: NodeId(node_id.clone()),
        mesh_ip: Some(wg_identity.tunnel_ip),
    };
    let (secondary, secondary_handle) =
        Secondary::new(secondary_config, Arc::clone(&subs.ports)).await?;
    tokio::spawn(async move { secondary.run().await });

    // 12. Build volume driver
    let volume_driver = build_volume_driver(
        provider.as_deref(),
        provider_token.as_deref(),
        cloud.region.as_deref(),
    );

    // 13. Start primary with local secondary
    let cancel = CancellationToken::new();
    let primary_config = PrimaryConfig {
        api_addr,
        auth_token: cluster_token.clone(),
        rpc_token: cluster_token.clone(),
        local_node_id: Some(NodeId(node_id.clone())),
        volume_driver,
        dns: Some(subs.dns.clone()),
        proxy: Some(subs.proxy.clone()),
        wireguard: Some(wg.clone() as Arc<dyn Mesh>),
        raft_network: Some(network.clone()),
    };
    let raft_router = HttpNetwork::axum_router(raft.raft().clone());
    let primary =
        Primary::start(primary_config, Arc::clone(&raft), Some(secondary_handle), cancel.clone())
            .await?;

    start_raft_listener(raft_router, wg_identity.tunnel_ip, API_PORT + 1).await;

    write_pid_file(data_dir).await;

    eprintln!("Mill cluster initialized.");
    eprintln!("  Node ID:    {node_id}");
    eprintln!("  API:        http://{}:{}", advertise_addr, API_PORT);
    eprintln!("  Token:      {cluster_token}");
    eprintln!("  Tunnel IP:  {}", wg_identity.tunnel_ip);
    eprintln!();
    eprintln!("To join another node:");
    eprintln!("  mill join {advertise_addr} --token {cluster_token}");

    wait_for_signal().await;
    shutdown(primary, raft, subs.dns, subs.proxy).await;
    remove_pid_file(data_dir).await;
    Ok(())
}

/// Join an existing cluster.
// TODO(refactor): group init/join params into an opts struct
#[allow(clippy::too_many_arguments)]
pub async fn join(
    data_dir: &DataDir,
    address: &str,
    token: &str,
    advertise: Option<String>,
    provider: Option<String>,
    provider_token: Option<String>,
    acme_email: Option<String>,
    acme_staging: bool,
) -> Result<()> {
    let api = client::DaemonClient::new(address.to_string(), token.to_string());
    let advertise_addr = advertise.unwrap_or_else(|| "0.0.0.0".to_string());

    // 1. Get join info from existing cluster
    let join_info: JoinInfoResponse = api.get("/v1/join-info").await?;

    // 2. Generate identity
    let node_id = generate_id();
    let raft_id = join_info.next_raft_id;

    // 3. Init WireGuard with peers from the cluster
    let tunnel_ip = join_info.tunnel_ip;
    let key_path = data_dir.wireguard_dir().join("private.key");
    let peers: Vec<PeerInfo> = join_info
        .peers
        .iter()
        .map(|p| PeerInfo {
            node_id: NodeId(String::new()),
            public_key: p.public_key.clone(),
            endpoint: p.endpoint.parse().ok(),
            tunnel_ip: p.tunnel_ip,
        })
        .collect();

    let (wg, wg_identity) = WireGuard::join(
        WIREGUARD_INTERFACE,
        WIREGUARD_PORT,
        tunnel_ip,
        join_info.prefix_len,
        &peers,
        Some(&key_path),
    )
    .await?;
    let wg = Arc::new(wg);

    // 4. Get cluster key via the advertise address (not the tunnel, since
    //    the leader hasn't added us as a WireGuard peer yet — that happens
    //    in post_join after step 9).
    let key_bytes: Vec<u8> = api.get_bytes("/v1/cluster-key").await?.to_vec();

    if key_bytes.len() != 32 {
        return Err(NodeError::Internal(format!(
            "cluster key: expected 32 bytes, got {}",
            key_bytes.len()
        )));
    }
    let mut key_arr = [0u8; 32];
    key_arr.copy_from_slice(&key_bytes);
    let cluster_key = ClusterKey::from_bytes(&key_arr);
    cluster_key.save(&data_dir.raft_dir()).await?;

    // 5. Write node.json
    let peer_entries: Vec<PeerEntry> = join_info
        .peers
        .iter()
        .map(|p| PeerEntry {
            raft_id: p.raft_id,
            advertise_addr: p.endpoint.clone(),
            tunnel_ip: p.tunnel_ip,
            public_key: p.public_key.clone(),
        })
        .collect();
    let mut identity = NodeIdentity {
        node_id: node_id.clone(),
        raft_id,
        cluster_token: token.to_string(),
        rpc_port: API_PORT,
        advertise_addr: advertise_addr.clone(),
        provider: provider.clone(),
        provider_token: provider_token.clone(),
        region: None,
        wireguard_subnet: join_info.subnet.clone(),
        peers: peer_entries,
        acme_email,
        acme_staging: if acme_staging { Some(true) } else { None },
    };

    // 5b. Detect cloud metadata and update identity with region
    let cloud = detect_cloud_metadata(provider.as_deref()).await;
    if cloud.region.is_some() {
        identity.region = cloud.region.clone();
    }
    identity.save(&data_dir.node_json_path()).await?;

    // 6. Create HTTP Raft network
    let network = HttpNetwork::new();
    for p in &join_info.peers {
        network.set_peer(p.raft_id, format!("http://{}:{}", p.tunnel_ip, join_info.api_port + 1));
    }

    // 7. Open Raft (no initialize — we'll be added as a learner)
    let raft_config = default_raft_config();
    let raft =
        MillRaft::open(raft_id, raft_config, network.clone(), cluster_key, &data_dir.raft_dir())
            .await?;
    let raft = Arc::new(raft);

    // 8. Start Raft listener BEFORE sending join request, because the
    //    leader's add_learner() will try to connect back to our Raft transport.
    let raft_router = HttpNetwork::axum_router(raft.raft().clone());
    start_raft_listener(raft_router, tunnel_ip, API_PORT + 1).await;

    // 9. Tell the leader to add us
    let resources = detect_resources();
    // Bind API to 0.0.0.0 so it's accessible on all local interfaces (bridge, tunnel, etc).
    // The leader will construct the API address from the tunnel_ip in the JoinRequest.
    let our_api_addr = SocketAddr::new(IpAddr::from([0, 0, 0, 0]), API_PORT);
    let rpc_addr: SocketAddr = SocketAddr::new(tunnel_ip, RPC_PORT);

    let join_req = JoinRequest {
        node_id: node_id.clone(),
        raft_id,
        advertise_addr: advertise_addr.clone(),
        tunnel_ip,
        public_key: wg_identity.public_key.clone(),
        resources: resources.clone(),
        instance_id: cloud.instance_id,
        rpc_address: Some(rpc_addr),
    };
    api.post_json("/v1/join", &join_req).await?;

    // 10. Start subsystems
    let subs = start_subsystems(data_dir, identity.acme_config()).await?;

    let secondary_config = SecondaryConfig {
        containerd_socket: CONTAINERD_SOCKET.to_string(),
        data_dir: data_dir.clone(),
        expected_allocs: Vec::new(),
        node_id: NodeId(node_id.clone()),
        mesh_ip: Some(tunnel_ip),
    };
    let (secondary, secondary_handle) =
        Secondary::new(secondary_config, Arc::clone(&subs.ports)).await?;
    tokio::spawn(async move { secondary.run().await });

    // 11. Build volume driver
    let volume_driver = build_volume_driver(
        provider.as_deref(),
        provider_token.as_deref(),
        cloud.region.as_deref(),
    );

    // 11. Start primary with local secondary
    let cancel = CancellationToken::new();
    let primary_config = PrimaryConfig {
        api_addr: our_api_addr,
        auth_token: token.to_string(),
        rpc_token: token.to_string(),
        local_node_id: Some(NodeId(node_id.clone())),
        volume_driver,
        dns: Some(subs.dns.clone()),
        proxy: Some(subs.proxy.clone()),
        wireguard: Some(wg.clone() as Arc<dyn Mesh>),
        raft_network: Some(network.clone()),
    };
    let primary =
        Primary::start(primary_config, Arc::clone(&raft), Some(secondary_handle), cancel.clone())
            .await?;

    write_pid_file(data_dir).await;

    eprintln!("Joined cluster via {address}.");
    eprintln!("  Node ID:    {node_id}");
    eprintln!("  API:        http://{}:{}", advertise_addr, API_PORT);
    eprintln!("  Tunnel IP:  {tunnel_ip}");

    wait_for_signal().await;
    shutdown(primary, raft, subs.dns, subs.proxy).await;
    remove_pid_file(data_dir).await;
    Ok(())
}

/// Leave the cluster gracefully.
pub async fn leave(data_dir: &DataDir) -> Result<()> {
    let node_json = data_dir.node_json_path();
    let identity = NodeIdentity::load(&node_json).await?;

    let api_base = format!("http://127.0.0.1:{}", identity.rpc_port);
    let client = client::DaemonClient::new(api_base, identity.cluster_token.clone());

    // 1. Drain via SSE — the stream closes when drain completes.
    let drain_path = format!("/v1/nodes/{}/drain", identity.node_id);
    let mut resp = client.post_sse(&drain_path).await?;
    while let Some(_chunk) =
        resp.chunk().await.map_err(|e| NodeError::NodeUnreachable(e.to_string()))?
    {}

    // 2. Remove node from cluster
    let remove_path = format!("/v1/nodes/{}", identity.node_id);
    client.delete(&remove_path).await?;

    // 3. Teardown WireGuard
    let wg = WireGuard::from_existing(WIREGUARD_INTERFACE);
    if let Err(e) = wg.teardown().await {
        tracing::warn!("wireguard teardown: {e}");
    }

    // 4. Delete node.json
    let _ = tokio::fs::remove_file(&node_json).await;

    // 5. Stop the daemon
    stop_daemon(data_dir).await;

    eprintln!("Left cluster.");
    Ok(())
}

/// Send SIGTERM to the daemon process via its PID file.
async fn stop_daemon(data_dir: &DataDir) {
    let Ok(pid_str) = tokio::fs::read_to_string(data_dir.pid_path()).await else {
        return;
    };
    let pid = pid_str.trim();
    if pid.is_empty() {
        return;
    }

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").args(["-s", "TERM", pid]).status();
    }
}

/// Start from an existing node.json (daemon restart).
async fn start_from_existing(data_dir: &DataDir) -> Result<()> {
    // 1. Read identity
    let identity = NodeIdentity::load(&data_dir.node_json_path()).await?;

    // 2. WireGuard — reuse existing interface (idempotent init, reuses key)
    let key_path = data_dir.wireguard_dir().join("private.key");
    let (wg, wg_identity) = WireGuard::init(
        WIREGUARD_INTERFACE,
        WIREGUARD_PORT,
        &identity.wireguard_subnet,
        Some(&key_path),
    )
    .await?;
    let wg = Arc::new(wg);

    // Add persisted peers
    for peer in &identity.peers {
        let peer_info = PeerInfo {
            node_id: NodeId(String::new()),
            public_key: peer.public_key.clone(),
            endpoint: peer.advertise_addr.parse().ok(),
            tunnel_ip: peer.tunnel_ip,
        };
        if let Err(e) = wg.add_peer(&peer_info).await {
            tracing::warn!("failed to add peer {}: {e}", peer.tunnel_ip);
        }
    }

    // 3. Load cluster key
    let cluster_key = ClusterKey::load(&data_dir.raft_dir()).await?;

    // 4. Create HTTP Raft network
    let network = HttpNetwork::new();
    for peer in &identity.peers {
        network.set_peer(peer.raft_id, format!("http://{}:{}", peer.tunnel_ip, API_PORT + 1));
    }

    // 5. Open Raft (restores from snapshot)
    let raft_config = default_raft_config();
    let raft = MillRaft::open(
        identity.raft_id,
        raft_config,
        network.clone(),
        cluster_key,
        &data_dir.raft_dir(),
    )
    .await?;
    let raft = Arc::new(raft);

    // 6. Start subsystems
    let subs = start_subsystems(data_dir, identity.acme_config()).await?;

    // Get expected allocs from FSM for crash recovery
    let mill_id = NodeId(identity.node_id.clone());
    let expected_allocs = raft.read_state(|fsm| {
        fsm.allocs
            .values()
            .filter(|a| a.node == mill_id && !a.status.is_terminal())
            .map(|a| a.id.clone())
            .collect::<Vec<_>>()
    });

    let secondary_config = SecondaryConfig {
        containerd_socket: CONTAINERD_SOCKET.to_string(),
        data_dir: data_dir.clone(),
        expected_allocs,
        node_id: NodeId(identity.node_id.clone()),
        mesh_ip: Some(wg_identity.tunnel_ip),
    };
    let (secondary, handle) = Secondary::new(secondary_config, Arc::clone(&subs.ports)).await?;
    tokio::spawn(async move { secondary.run().await });

    // 7. Start RPC server
    let rpc_addr: SocketAddr = SocketAddr::new(wg_identity.tunnel_ip, RPC_PORT);
    let rpc_cancel = CancellationToken::new();
    let rpc_token = identity.cluster_token.clone();
    tokio::spawn(async move {
        if let Err(e) = RpcServer::start(rpc_addr, rpc_token, handle, rpc_cancel).await {
            tracing::error!("RPC server failed: {e}");
        }
    });

    // 8. Build volume driver
    let volume_driver = build_volume_driver(
        identity.provider.as_deref(),
        identity.provider_token.as_deref(),
        identity.region.as_deref(),
    );

    let cancel = CancellationToken::new();
    // Bind API to 0.0.0.0 so it's accessible on all interfaces (bridge, tunnel, etc).
    let api_addr = SocketAddr::new(IpAddr::from([0, 0, 0, 0]), API_PORT);
    let primary_config = PrimaryConfig {
        api_addr,
        auth_token: identity.cluster_token.clone(),
        rpc_token: identity.cluster_token.clone(),
        local_node_id: None,
        volume_driver,
        dns: Some(subs.dns.clone()),
        proxy: Some(subs.proxy.clone()),
        wireguard: Some(wg.clone() as Arc<dyn Mesh>),
        raft_network: Some(network.clone()),
    };
    let raft_router = HttpNetwork::axum_router(raft.raft().clone());
    let primary = Primary::start(primary_config, Arc::clone(&raft), None, cancel.clone()).await?;

    start_raft_listener(raft_router, wg_identity.tunnel_ip, API_PORT + 1).await;

    write_pid_file(data_dir).await;

    eprintln!("Mill daemon started (restored from node.json).");
    eprintln!("  Node ID:    {}", identity.node_id);
    eprintln!("  API:        http://{}:{}", identity.advertise_addr, API_PORT);

    wait_for_signal().await;
    shutdown(primary, raft, subs.dns, subs.proxy).await;
    remove_pid_file(data_dir).await;
    Ok(())
}

/// Response from GET /v1/join-info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinInfoResponse {
    pub subnet: String,
    pub prefix_len: u8,
    pub api_port: u16,
    pub tunnel_ip: IpAddr,
    pub next_raft_id: u64,
    pub peers: Vec<JoinPeerInfo>,
}

/// Peer info returned in join-info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinPeerInfo {
    pub raft_id: u64,
    pub endpoint: String,
    pub tunnel_ip: IpAddr,
    pub public_key: String,
}

/// Request body for POST /v1/join.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub node_id: String,
    pub raft_id: u64,
    pub advertise_addr: String,
    pub tunnel_ip: IpAddr,
    pub public_key: String,
    pub resources: NodeResources,
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub rpc_address: Option<SocketAddr>,
}

#[cfg(test)]
mod tests;
