pub mod auth;
pub mod handlers;
pub mod leader;
pub mod sse;

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post, put};
use mill_proxy::ProxyMetrics;
use mill_raft::MillRaft;

use crate::rpc::CommandRouter;
use crate::storage::Volumes;

use self::auth::require_auth;
use self::handlers::*;
use self::leader::require_leader;
use super::health::HealthPoller;
use super::report::{LiveMetrics, LogSubscribers};

/// Shared state for the API server.
#[derive(Clone)]
pub struct AppState {
    pub raft: Arc<MillRaft>,
    pub auth_token: String,
    pub router: CommandRouter,
    pub health: Arc<HealthPoller>,
    pub volumes: Option<Arc<dyn Volumes>>,
    pub log_subscribers: LogSubscribers,
    pub live_metrics: LiveMetrics,
    pub proxy_metrics: Option<Arc<ProxyMetrics>>,
    pub wireguard: Option<Arc<dyn mill_net::Mesh>>,
    pub raft_network: Option<mill_raft::network::http::HttpNetwork>,
}

/// Build the API router with all routes and auth middleware.
pub fn router(state: AppState) -> Router {
    // Read routes — served by any node (eventually consistent)
    let read_routes = Router::new()
        .route("/v1/status", get(get_status))
        .route("/v1/nodes", get(get_nodes))
        .route("/v1/services", get(get_services))
        .route("/v1/tasks", get(get_tasks))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/secrets", get(list_secrets))
        .route("/v1/secrets/{name}", get(get_secret))
        .route("/v1/volumes", get(get_volumes))
        .route("/v1/metrics", get(get_metrics))
        .route("/v1/logs/{name}", get(get_logs))
        .route("/v1/join-info", get(get_join_info))
        .route("/v1/cluster-key", get(get_cluster_key));

    // Write routes — must go to leader
    let write_routes = Router::new()
        .route("/v1/deploy", post(post_deploy))
        .route("/v1/restart/{service}", post(post_restart))
        .route("/v1/tasks/spawn", post(post_spawn_task))
        .route("/v1/tasks/{id}", delete(delete_task))
        .route("/v1/secrets/{name}", put(set_secret).delete(delete_secret))
        .route("/v1/volumes/{name}", delete(delete_volume))
        .route("/v1/nodes/{id}/drain", post(post_drain))
        .route("/v1/nodes/{id}", delete(delete_node))
        .route("/v1/join", post(post_join))
        .layer(from_fn_with_state(state.clone(), require_leader));

    Router::new()
        .merge(read_routes)
        .merge(write_routes)
        .layer(from_fn_with_state(state.clone(), require_auth))
        .with_state(state)
}
