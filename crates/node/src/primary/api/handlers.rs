use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::Event;
use tokio::sync::mpsc;

use mill_config::{
    AllocKind, AllocResponse, ErrorResponse, NodeId, NodeResponse, ReplicaCount, SecretListItem,
    SecretResponse, SecretSetRequest, ServiceResponse, SpawnTaskRequest, SpawnTaskResponse,
    StatusResponse, TaskResponse, VolumeResponse, VolumeState,
};
use mill_raft::MillRaft;
use mill_raft::fsm::command::{Command, Response as RaftResponse};

use super::AppState;
use super::sse::{SseEvent, mpsc_to_sse};
use crate::primary::deploy;
use crate::primary::logs;
use crate::primary::nodes;
use crate::primary::tasks;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(ErrorResponse { error: message.into() }))
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let resp = state.raft.read_state(|fsm| {
        let mut cpu_total = 0.0;
        let mut cpu_available = 0.0;
        let mut mem_total: u64 = 0;
        let mut mem_available: u64 = 0;

        for (raft_id, node) in &fsm.nodes {
            cpu_total += node.resources.cpu_total;
            mem_total += node.resources.memory_total.as_u64();
            if let Some(avail) = fsm.node_available_resources(*raft_id) {
                cpu_available += avail.cpu_available;
                mem_available += avail.memory_available.as_u64();
            }
        }

        let service_count = fsm.config.as_ref().map_or(0, |c| c.services.len());
        let task_count = fsm.allocs.values().filter(|a| a.kind == AllocKind::Task).count();

        StatusResponse {
            node_count: fsm.nodes.len(),
            service_count,
            task_count,
            cpu_total,
            cpu_available,
            memory_total_bytes: mem_total,
            memory_available_bytes: mem_available,
        }
    });
    Json(resp)
}

pub async fn get_nodes(State(state): State<AppState>) -> Json<Vec<NodeResponse>> {
    let resp = state.raft.read_state(|fsm| {
        fsm.nodes
            .iter()
            .map(|(raft_id, node)| {
                let avail = fsm.node_available_resources(*raft_id);
                let alloc_count = fsm
                    .allocs
                    .values()
                    .filter(|a| a.node == node.mill_id && !a.status.is_terminal())
                    .count();

                NodeResponse {
                    id: node.mill_id.0.clone(),
                    address: node.address.to_string(),
                    status: node.status.to_string(),
                    cpu_total: node.resources.cpu_total,
                    cpu_available: avail.as_ref().map_or(0.0, |a| a.cpu_available),
                    memory_total_bytes: node.resources.memory_total.as_u64(),
                    memory_available_bytes: avail.map_or(0, |a| a.memory_available.as_u64()),
                    alloc_count,
                }
            })
            .collect()
    });
    Json(resp)
}

pub async fn get_services(State(state): State<AppState>) -> Json<Vec<ServiceResponse>> {
    let resp = state.raft.read_state(|fsm| {
        let config = match &fsm.config {
            Some(c) => c,
            None => return Vec::new(),
        };

        config
            .services
            .iter()
            .map(|(name, def)| {
                let allocs: Vec<AllocResponse> = fsm
                    .list_service_allocs(name)
                    .into_iter()
                    .filter(|a| a.kind == AllocKind::Service)
                    .map(|a| AllocResponse {
                        id: a.id.0.clone(),
                        node: a.node.0.clone(),
                        status: a.status.to_string(),
                        address: a.address.map(|addr| addr.to_string()),
                        cpu: a.resources.cpu,
                        memory: a.resources.memory.as_u64(),
                        started_at: a.started_at.map(|t| crate::util::format_system_time(&t)),
                    })
                    .collect();

                let healthy = allocs.iter().filter(|a| a.status == "healthy").count();

                ServiceResponse {
                    name: name.clone(),
                    image: def.image.clone(),
                    replicas: ReplicaCount { desired: def.replicas, healthy },
                    allocations: allocs,
                }
            })
            .collect()
    });
    Json(resp)
}

pub async fn get_tasks(State(state): State<AppState>) -> Json<Vec<TaskResponse>> {
    let resp = state.raft.read_state(|fsm| {
        fsm.allocs.values().filter(|a| a.kind == AllocKind::Task).map(TaskResponse::from).collect()
    });
    Json(resp)
}

pub async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskResponse>, ApiError> {
    let resp = state.raft.read_state(|fsm| {
        let alloc_id = mill_config::AllocId(id.clone());
        fsm.allocs.get(&alloc_id).and_then(|a| {
            if a.kind != AllocKind::Task {
                return None;
            }
            Some(TaskResponse::from(a))
        })
    });
    resp.map(Json).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "task not found"))
}

pub async fn list_secrets(State(state): State<AppState>) -> Json<Vec<SecretListItem>> {
    let resp = state.raft.read_state(|fsm| {
        fsm.list_secret_names()
            .into_iter()
            .map(|name| SecretListItem { name: name.to_owned() })
            .collect()
    });
    Json(resp)
}

pub async fn get_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SecretResponse>, ApiError> {
    let encrypted = state.raft.read_state(|fsm| fsm.get_secret(&name).cloned());
    let secret = encrypted.ok_or_else(|| api_error(StatusCode::NOT_FOUND, "secret not found"))?;
    let plaintext = state
        .raft
        .decrypt_secret(&secret.encrypted_value, &secret.nonce)
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "decryption failed"))?;
    let value = String::from_utf8(plaintext)
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "decryption failed"))?;
    Ok(Json(SecretResponse { name, value }))
}

pub async fn get_volumes(State(state): State<AppState>) -> Json<Vec<VolumeResponse>> {
    let resp = state.raft.read_state(|fsm| {
        fsm.list_volumes()
            .into_iter()
            .map(|(name, vol)| {
                let state_str = vol.state.to_string();
                let node = match &vol.state {
                    VolumeState::Attached { node } => Some(node.0.clone()),
                    _ => None,
                };
                VolumeResponse {
                    name: name.to_owned(),
                    state: state_str,
                    node,
                    cloud_id: vol.cloud_id.clone(),
                }
            })
            .collect()
    });
    Json(resp)
}

pub async fn get_metrics(State(state): State<AppState>) -> String {
    let live = state.live_metrics.snapshot_all();
    let restart_counts = state.live_metrics.restart_counts();
    let task_spawn_counts = state.live_metrics.task_spawn_counts();
    let task_durations = state.live_metrics.task_duration_snapshots();
    let proxy_snap = state.proxy_metrics.as_ref().map(|pm| pm.snapshot());

    let mut out = state.raft.read_state(|fsm| {
        let mut out = String::new();

        // --- Node metrics: actual usage from LiveMetrics, FSM fallback ---
        out.push_str("# HELP mill_node_cpu_available Free CPU cores per node\n");
        out.push_str("# TYPE mill_node_cpu_available gauge\n");
        for (raft_id, node) in &fsm.nodes {
            let id = &node.mill_id.0;
            let cpu_avail = live
                .get(&node.mill_id)
                .map(|s| s.resources.cpu_available)
                .or_else(|| fsm.node_available_resources(*raft_id).map(|a| a.cpu_available));
            if let Some(v) = cpu_avail {
                out.push_str(&format!("mill_node_cpu_available{{node=\"{id}\"}} {v}\n"));
            }
        }

        out.push_str("# HELP mill_node_memory_available_bytes Free memory per node in bytes\n");
        out.push_str("# TYPE mill_node_memory_available_bytes gauge\n");
        for (raft_id, node) in &fsm.nodes {
            let id = &node.mill_id.0;
            let mem_avail =
                live.get(&node.mill_id).map(|s| s.resources.memory_available.as_u64()).or_else(
                    || fsm.node_available_resources(*raft_id).map(|a| a.memory_available.as_u64()),
                );
            if let Some(v) = mem_avail {
                out.push_str(&format!("mill_node_memory_available_bytes{{node=\"{id}\"}} {v}\n"));
            }
        }

        // --- Alloc metrics: actual usage from LiveMetrics container stats ---
        let mut live_stats: std::collections::HashMap<&mill_config::AllocId, (f64, u64)> =
            std::collections::HashMap::new();
        for snap in live.values() {
            for cs in &snap.container_stats {
                live_stats.insert(&cs.alloc_id, (cs.cpu_usage, cs.memory_usage.as_u64()));
            }
        }

        out.push_str("# HELP mill_alloc_cpu_usage CPU usage per allocation in cores\n");
        out.push_str("# TYPE mill_alloc_cpu_usage gauge\n");
        for alloc in fsm.allocs.values() {
            let name = &alloc.name;
            let node = &alloc.node.0;
            let cpu = live_stats.get(&alloc.id).map(|(c, _)| *c).unwrap_or(0.0);
            out.push_str(&format!(
                "mill_alloc_cpu_usage{{service=\"{name}\",node=\"{node}\"}} {cpu}\n",
            ));
        }

        out.push_str("# HELP mill_alloc_memory_bytes Memory usage per allocation in bytes\n");
        out.push_str("# TYPE mill_alloc_memory_bytes gauge\n");
        for alloc in fsm.allocs.values() {
            let name = &alloc.name;
            let node = &alloc.node.0;
            let mem = live_stats.get(&alloc.id).map(|(_, m)| *m).unwrap_or(0);
            out.push_str(&format!(
                "mill_alloc_memory_bytes{{service=\"{name}\",node=\"{node}\"}} {mem}\n",
            ));
        }

        // --- Restart counts ---
        if !restart_counts.is_empty() {
            out.push_str("# HELP mill_alloc_restarts_total Total service restart count\n");
            out.push_str("# TYPE mill_alloc_restarts_total counter\n");
            for (service, count) in &restart_counts {
                // Find the node for the active alloc of this service.
                let node = fsm
                    .allocs
                    .values()
                    .find(|a| &a.name == service && a.kind == mill_config::AllocKind::Service)
                    .map(|a| a.node.0.as_str())
                    .unwrap_or("unknown");
                out.push_str(&format!(
                    "mill_alloc_restarts_total{{service=\"{service}\",node=\"{node}\"}} {count}\n",
                ));
            }
        }

        out
    });

    // --- Proxy metrics (outside Raft read_state since they come from ProxyServer) ---
    if let Some(routes) = proxy_snap
        && !routes.is_empty()
    {
        out.push_str("# HELP mill_proxy_requests_total Total proxied requests per route\n");
        out.push_str("# TYPE mill_proxy_requests_total counter\n");
        for r in &routes {
            let route = &r.hostname;
            let path = &r.path;
            out.push_str(&format!(
                "mill_proxy_requests_total{{route=\"{route}\",path=\"{path}\"}} {}\n",
                r.request_count
            ));
        }

        out.push_str(
            "# HELP mill_proxy_latency_seconds Proxy request latency histogram per route\n",
        );
        out.push_str("# TYPE mill_proxy_latency_seconds histogram\n");
        for r in &routes {
            let route = &r.hostname;
            let path = &r.path;
            for &(le, count) in &r.buckets {
                if le == f64::INFINITY {
                    out.push_str(&format!(
                            "mill_proxy_latency_seconds_bucket{{route=\"{route}\",path=\"{path}\",le=\"+Inf\"}} {count}\n",
                        ));
                } else {
                    out.push_str(&format!(
                            "mill_proxy_latency_seconds_bucket{{route=\"{route}\",path=\"{path}\",le=\"{le}\"}} {count}\n",
                        ));
                }
            }
            out.push_str(&format!(
                "mill_proxy_latency_seconds_sum{{route=\"{route}\",path=\"{path}\"}} {}\n",
                r.latency_sum_seconds
            ));
            out.push_str(&format!(
                "mill_proxy_latency_seconds_count{{route=\"{route}\",path=\"{path}\"}} {}\n",
                r.request_count
            ));
        }
    }

    // --- Task spawn counts ---
    if !task_spawn_counts.is_empty() {
        out.push_str("# HELP mill_tasks_spawned_total Total task spawn count\n");
        out.push_str("# TYPE mill_tasks_spawned_total counter\n");
        for (task, count) in &task_spawn_counts {
            out.push_str(&format!("mill_tasks_spawned_total{{task=\"{task}\"}} {count}\n",));
        }
    }

    // --- Task duration histogram ---
    if !task_durations.is_empty() {
        out.push_str("# HELP mill_tasks_duration_seconds Task execution duration histogram\n");
        out.push_str("# TYPE mill_tasks_duration_seconds histogram\n");
        for d in &task_durations {
            let task = &d.name;
            for &(le, count) in &d.buckets {
                if le == f64::INFINITY {
                    out.push_str(&format!(
                        "mill_tasks_duration_seconds_bucket{{task=\"{task}\",le=\"+Inf\"}} {count}\n",
                    ));
                } else {
                    out.push_str(&format!(
                        "mill_tasks_duration_seconds_bucket{{task=\"{task}\",le=\"{le}\"}} {count}\n",
                    ));
                }
            }
            out.push_str(&format!(
                "mill_tasks_duration_seconds_sum{{task=\"{task}\"}} {}\n",
                d.sum_seconds
            ));
            out.push_str(&format!(
                "mill_tasks_duration_seconds_count{{task=\"{task}\"}} {}\n",
                d.count
            ));
        }
    }

    out
}

pub async fn set_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SecretSetRequest>,
) -> Result<StatusCode, ApiError> {
    let (encrypted_value, nonce) = state
        .raft
        .encrypt_secret(body.value.as_bytes())
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "encryption failed"))?;

    propose(&state.raft, Command::SecretSet { name, encrypted_value, nonce }).await
}

pub async fn delete_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Check existence
    let exists = state.raft.read_state(|fsm| fsm.get_secret(&name).is_some());
    if !exists {
        return Err(api_error(StatusCode::NOT_FOUND, "secret not found"));
    }

    propose(&state.raft, Command::SecretDelete { name }).await
}

pub async fn delete_volume(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Check existence and that it's not attached
    let vol_state = state.raft.read_state(|fsm| fsm.volumes.get(&name).map(|v| v.state.clone()));

    match vol_state {
        None => return Err(api_error(StatusCode::NOT_FOUND, "volume not found")),
        Some(VolumeState::Attached { .. }) => {
            return Err(api_error(StatusCode::CONFLICT, "volume is attached"));
        }
        Some(VolumeState::Ready) => {}
    }

    // Destroy the cloud volume before removing from FSM
    if let Some(driver) = &state.volumes {
        driver.destroy(&name).await.map_err(|e| {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("cloud destroy failed: {e}"))
        })?;
    }

    propose(&state.raft, Command::VolumeDestroyed { name }).await
}

pub async fn post_deploy(
    State(state): State<AppState>,
    body: String,
) -> Result<
    axum::response::sse::Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>,
    ApiError,
> {
    let config = mill_config::parse(&body)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, format!("invalid config: {e}")))?;

    let (tx, rx) = mpsc::channel::<SseEvent>(64);

    let error_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) =
            deploy::run_deploy(state.raft, state.router, state.volumes, state.health, config, tx)
                .await
        {
            let _ = error_tx
                .send(SseEvent {
                    event: "failed".to_owned(),
                    data: serde_json::json!({"reason": e.to_string()}).to_string(),
                })
                .await;
        }
    });

    Ok(mpsc_to_sse(rx))
}

pub async fn post_restart(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> Result<
    axum::response::sse::Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>,
    ApiError,
> {
    // Verify the service exists.
    let exists = state
        .raft
        .read_state(|fsm| fsm.config.as_ref().is_some_and(|c| c.services.contains_key(&service)));
    if !exists {
        return Err(api_error(StatusCode::NOT_FOUND, "service not found"));
    }

    let (tx, rx) = mpsc::channel::<SseEvent>(64);

    let error_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) =
            deploy::run_restart(state.raft, state.router, state.volumes, state.health, service, tx)
                .await
        {
            let _ = error_tx
                .send(SseEvent {
                    event: "failed".to_owned(),
                    data: serde_json::json!({"reason": e.to_string()}).to_string(),
                })
                .await;
        }
    });

    Ok(mpsc_to_sse(rx))
}

pub async fn post_spawn_task(
    State(state): State<AppState>,
    Json(body): Json<SpawnTaskRequest>,
) -> Result<Json<SpawnTaskResponse>, ApiError> {
    match tasks::spawn_task(&state.raft, &state.router, &state.live_metrics, body).await {
        Ok(resp) => Ok(Json(resp)),
        Err(deploy::DeployError::SchedulingFailed { .. }) => {
            Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "no node with sufficient resources"))
        }
        Err(deploy::DeployError::Node(crate::error::NodeError::BadRequest(msg))) => {
            Err(api_error(StatusCode::BAD_REQUEST, msg))
        }
        Err(deploy::DeployError::Node(crate::error::NodeError::ServiceNotFound(_))) => {
            Err(api_error(StatusCode::NOT_FOUND, "task template not found"))
        }
        Err(_) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")),
    }
}

pub async fn delete_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    match tasks::kill_task(&state.raft, &state.router, &id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(deploy::DeployError::Node(crate::error::NodeError::AllocNotFound(_))) => {
            Err(api_error(StatusCode::NOT_FOUND, "task not found"))
        }
        Err(deploy::DeployError::Node(crate::error::NodeError::NotATask { .. })) => {
            Err(api_error(StatusCode::BAD_REQUEST, "allocation is not a task"))
        }
        Err(_) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")),
    }
}

#[derive(serde::Deserialize)]
pub struct LogQuery {
    #[serde(default = "default_follow")]
    pub follow: bool,
    #[serde(default = "default_tail")]
    pub tail: usize,
}

fn default_follow() -> bool {
    true
}
fn default_tail() -> usize {
    200
}

pub async fn get_logs(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<LogQuery>,
) -> axum::response::sse::Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<SseEvent>(256);

    tokio::spawn(async move {
        logs::stream_logs(
            &state.raft,
            &state.router,
            &state.log_subscribers,
            &name,
            &tx,
            query.follow,
            query.tail,
        )
        .await;
    });

    mpsc_to_sse(rx)
}

pub async fn post_drain(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    axum::response::sse::Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>,
    ApiError,
> {
    // Resolve mill ID to raft node ID.
    let raft_id = state.raft.read_state(|fsm| fsm.raft_id_for(&NodeId(id.clone())));
    let raft_id = raft_id.ok_or_else(|| api_error(StatusCode::NOT_FOUND, "node not found"))?;

    let (tx, rx) = mpsc::channel::<SseEvent>(64);

    tokio::spawn(async move {
        if let Err(e) =
            nodes::drain_node(state.raft, state.router, state.volumes, state.health, raft_id, tx)
                .await
        {
            tracing::error!("drain failed: {e}");
        }
    });

    Ok(mpsc_to_sse(rx))
}

pub async fn delete_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Resolve mill ID to raft node ID.
    let raft_id = state.raft.read_state(|fsm| fsm.raft_id_for(&NodeId(id.clone())));
    let raft_id = raft_id.ok_or_else(|| api_error(StatusCode::NOT_FOUND, "node not found"))?;

    match nodes::remove_node(&state.raft, raft_id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(crate::error::NodeError::RaftRejected(_)) => {
            Err(api_error(StatusCode::CONFLICT, "node has active allocations"))
        }
        Err(_) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")),
    }
}

pub async fn get_join_info(
    State(state): State<AppState>,
) -> Result<Json<crate::daemon::JoinInfoResponse>, ApiError> {
    let (peers, next_raft_id) = state.raft.read_state(|fsm| {
        let peers: Vec<crate::daemon::JoinPeerInfo> = fsm
            .nodes
            .values()
            .map(|n| {
                // Use the advertise address + WireGuard port as the endpoint,
                // since WireGuard needs an externally-routable address.
                // Fall back to tunnel IP if no advertise address is stored.
                let endpoint = n
                    .advertise_addr
                    .as_ref()
                    .map(|a| format!("{a}:{}", crate::daemon::WIREGUARD_PORT))
                    .unwrap_or_else(|| n.address.to_string());
                crate::daemon::JoinPeerInfo {
                    raft_id: n.raft_id,
                    endpoint,
                    tunnel_ip: n.address.ip(),
                    public_key: n.wireguard_pubkey.clone().unwrap_or_default(),
                }
            })
            .collect();
        let max_id = fsm.nodes.keys().copied().max().unwrap_or(0);
        (peers, max_id + 1)
    });

    // Allocate a tunnel IP for the new node
    let tunnel_ip = {
        // Find next available IP in the subnet by checking existing nodes
        let used_ips: Vec<IpAddr> = peers.iter().map(|p| p.tunnel_ip).collect();
        allocate_tunnel_ip(&used_ips).ok_or_else(|| {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "no tunnel IPs available")
        })?
    };

    let resp = crate::daemon::JoinInfoResponse {
        subnet: "10.99.0.0/16".to_string(),
        prefix_len: 16,
        api_port: 4400,
        tunnel_ip,
        next_raft_id,
        peers,
    };

    Ok(Json(resp))
}

pub async fn post_join(
    State(state): State<AppState>,
    Json(body): Json<crate::daemon::JoinRequest>,
) -> Result<StatusCode, ApiError> {
    // 1. Add WireGuard peer on this node
    if let Some(wg) = &state.wireguard {
        // Construct the WireGuard endpoint from the advertise address and the
        // standard WireGuard port. The advertise_addr is just an IP (e.g. "172.18.0.11"),
        // so we need to append the port for WireGuard to know where to send packets.
        let wg_endpoint: Option<std::net::SocketAddr> = body
            .advertise_addr
            .parse::<std::net::IpAddr>()
            .ok()
            .map(|ip| std::net::SocketAddr::new(ip, crate::daemon::WIREGUARD_PORT));
        let peer = mill_net::PeerInfo {
            node_id: mill_config::NodeId(body.node_id.clone()),
            public_key: body.public_key.clone(),
            endpoint: wg_endpoint,
            tunnel_ip: body.tunnel_ip,
        };
        wg.add_peer(&peer).await.map_err(|e| {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("wireguard add_peer: {e}"))
        })?;
    }

    // 2. Update HTTP Raft network peer map
    if let Some(network) = &state.raft_network {
        network.set_peer(body.raft_id, format!("http://{}:4401", body.tunnel_ip));
    }

    // 3. Add learner to Raft
    let addr = format!("{}:4401", body.tunnel_ip);
    state
        .raft
        .add_learner(body.raft_id, &addr)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("add_learner: {e}")))?;

    // 4. Promote to voter
    let member_ids: Vec<u64> = state.raft.read_state(|fsm| {
        let mut ids: Vec<u64> = fsm.nodes.keys().copied().collect();
        ids.push(body.raft_id);
        ids
    });
    state.raft.change_membership(member_ids).await.map_err(|e| {
        api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("change_membership: {e}"))
    })?;

    // 5. Register node in FSM
    let api_addr: std::net::SocketAddr = format!("{}:4400", body.tunnel_ip)
        .parse()
        .unwrap_or_else(|_| std::net::SocketAddr::new(body.tunnel_ip, 4400));
    let cmd = Command::NodeRegister {
        id: body.raft_id,
        mill_id: mill_config::NodeId(body.node_id),
        address: api_addr,
        resources: body.resources,
        wireguard_pubkey: Some(body.public_key),
        instance_id: body.instance_id,
        rpc_address: body.rpc_address,
        advertise_addr: Some(body.advertise_addr),
    };
    propose(&state.raft, cmd).await
}

pub async fn get_cluster_key(State(state): State<AppState>) -> Result<Vec<u8>, ApiError> {
    let bytes = state.raft.cluster_key().as_bytes().to_vec();
    Ok(bytes)
}

/// Allocate the next available tunnel IP in the 10.99.0.0/16 range.
pub(crate) fn allocate_tunnel_ip(used: &[IpAddr]) -> Option<IpAddr> {
    use std::net::Ipv4Addr;
    // Start from 10.99.0.1, find the first unused
    for i in 1u32..65534 {
        let ip = Ipv4Addr::from(0x0A630000u32 + i); // 10.99.0.0 + i
        let addr = IpAddr::V4(ip);
        if !used.contains(&addr) {
            return Some(addr);
        }
    }
    None
}

async fn propose(raft: &Arc<MillRaft>, cmd: Command) -> Result<StatusCode, ApiError> {
    match raft.propose(cmd).await {
        Ok(RaftResponse::Ok) => Ok(StatusCode::NO_CONTENT),
        Ok(RaftResponse::Error(_)) => {
            Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "raft proposal failed"))
        }
        Err(_) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "raft proposal failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn allocate_tunnel_ip_first_available() {
        let used = vec![];
        let ip = allocate_tunnel_ip(&used);
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1))));
    }

    #[test]
    fn allocate_tunnel_ip_skips_used() {
        let used =
            vec![IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2))];
        let ip = allocate_tunnel_ip(&used);
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 3))));
    }

    #[test]
    fn allocate_tunnel_ip_exhaustion() {
        let used: Vec<IpAddr> =
            (1u32..65534).map(|i| IpAddr::V4(Ipv4Addr::from(0x0A630000u32 + i))).collect();
        let ip = allocate_tunnel_ip(&used);
        assert_eq!(ip, None);
    }
}
