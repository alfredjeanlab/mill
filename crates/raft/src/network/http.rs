use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use openraft::error::{
    NetworkError, RPCError, RaftError, ReplicationClosed, StreamingError, Unreachable,
};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{BasicNode, RaftNetwork, RaftNetworkFactory};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::config::{self, TypeConfig};

/// HTTP-based Raft network transport.
///
/// Each node exposes axum routes at `/raft/append`, `/raft/vote`, `/raft/snapshot`
/// and this factory creates HTTP clients that send requests to those endpoints.
#[derive(Clone)]
pub struct HttpNetwork {
    /// Map of raft node ID → base URL (e.g. "http://10.99.0.2:4400")
    peers: Arc<RwLock<HashMap<u64, String>>>,
    client: reqwest::Client,
}

impl Default for HttpNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpNetwork {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { peers: Arc::new(RwLock::new(HashMap::new())), client }
    }

    /// Register or update a peer's base URL.
    pub fn set_peer(&self, id: u64, base_url: String) {
        self.peers.write().insert(id, base_url);
    }

    /// Remove a peer.
    pub fn remove_peer(&self, id: u64) {
        self.peers.write().remove(&id);
    }

    pub(crate) fn get_peer_url(&self, id: u64) -> Result<String, Unreachable> {
        self.peers.read().get(&id).cloned().ok_or_else(|| {
            Unreachable::new(&std::io::Error::other(format!("node {id} not in peer map")))
        })
    }

    /// Build an axum router that handles inbound Raft RPCs.
    ///
    /// Embed this into the daemon's listener so peers can reach this node.
    pub fn axum_router(raft: config::Raft) -> Router {
        Router::new()
            .route("/raft/append", post(handle_append))
            .route("/raft/vote", post(handle_vote))
            .route("/raft/snapshot", post(handle_snapshot))
            .with_state(raft)
    }
}

impl RaftNetworkFactory<TypeConfig> for HttpNetwork {
    type Network = HttpNetworkConnection;

    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Self::Network {
        HttpNetworkConnection { network: self.clone(), target }
    }
}

/// A network connection to a single peer, backed by HTTP.
pub struct HttpNetworkConnection {
    network: HttpNetwork,
    target: u64,
}

impl RaftNetwork<TypeConfig> for HttpNetworkConnection {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let url = self.network.get_peer_url(self.target)?;
        let resp = self
            .network
            .client
            .post(format!("{url}/raft/append"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        let result: RpcResponse<AppendEntriesResponse<u64>> =
            resp.json().await.map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        match result {
            RpcResponse::Ok(v) => Ok(v),
            RpcResponse::Err(msg) => {
                Err(RPCError::Network(NetworkError::new(&std::io::Error::other(msg))))
            }
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, openraft::error::InstallSnapshotError>>,
    > {
        let url = self.network.get_peer_url(self.target).map_err(RPCError::Unreachable)?;
        let resp = self
            .network
            .client
            .post(format!("{url}/raft/snapshot"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        let result: RpcResponse<InstallSnapshotResponse<u64>> =
            resp.json().await.map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        match result {
            RpcResponse::Ok(v) => Ok(v),
            RpcResponse::Err(msg) => {
                Err(RPCError::Network(NetworkError::new(&std::io::Error::other(msg))))
            }
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let url = self.network.get_peer_url(self.target)?;
        let resp = self
            .network
            .client
            .post(format!("{url}/raft/vote"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        let result: RpcResponse<VoteResponse<u64>> =
            resp.json().await.map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        match result {
            RpcResponse::Ok(v) => Ok(v),
            RpcResponse::Err(msg) => {
                Err(RPCError::Network(NetworkError::new(&std::io::Error::other(msg))))
            }
        }
    }

    async fn full_snapshot(
        &mut self,
        vote: openraft::Vote<u64>,
        snapshot: config::Snapshot,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<u64>, StreamingError<TypeConfig, openraft::error::Fatal<u64>>>
    {
        // Serialize the full snapshot as a single request
        let data = snapshot.snapshot.into_inner();
        let rpc = FullSnapshotRequest { vote, meta: snapshot.meta, data };
        let url = self.network.get_peer_url(self.target).map_err(StreamingError::Unreachable)?;
        let resp = self
            .network
            .client
            .post(format!("{url}/raft/snapshot"))
            .json(&rpc)
            .send()
            .await
            .map_err(|e| StreamingError::Unreachable(Unreachable::new(&e)))?;

        let result: RpcResponse<SnapshotResponse<u64>> =
            resp.json().await.map_err(|e| StreamingError::Unreachable(Unreachable::new(&e)))?;

        match result {
            RpcResponse::Ok(r) => Ok(r),
            RpcResponse::Err(msg) => {
                Err(StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(msg))))
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct FullSnapshotRequest {
    vote: openraft::Vote<u64>,
    meta: config::SnapshotMeta,
    data: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status")]
enum RpcResponse<T> {
    #[serde(rename = "ok")]
    Ok(T),
    #[serde(rename = "err")]
    Err(String),
}

async fn handle_append(
    State(raft): State<config::Raft>,
    Json(rpc): Json<AppendEntriesRequest<TypeConfig>>,
) -> (StatusCode, Json<RpcResponse<AppendEntriesResponse<u64>>>) {
    match raft.append_entries(rpc).await {
        Ok(resp) => (StatusCode::OK, Json(RpcResponse::Ok(resp))),
        Err(e) => (StatusCode::OK, Json(RpcResponse::Err(e.to_string()))),
    }
}

async fn handle_vote(
    State(raft): State<config::Raft>,
    Json(rpc): Json<VoteRequest<u64>>,
) -> (StatusCode, Json<RpcResponse<VoteResponse<u64>>>) {
    match raft.vote(rpc).await {
        Ok(resp) => (StatusCode::OK, Json(RpcResponse::Ok(resp))),
        Err(e) => (StatusCode::OK, Json(RpcResponse::Err(e.to_string()))),
    }
}

async fn handle_snapshot(
    State(raft): State<config::Raft>,
    Json(rpc): Json<FullSnapshotRequest>,
) -> (StatusCode, Json<RpcResponse<SnapshotResponse<u64>>>) {
    let snapshot =
        config::Snapshot { meta: rpc.meta, snapshot: Box::new(std::io::Cursor::new(rpc.data)) };
    match raft.install_full_snapshot(rpc.vote, snapshot).await {
        Ok(resp) => (StatusCode::OK, Json(RpcResponse::Ok(resp))),
        Err(e) => (StatusCode::OK, Json(RpcResponse::Err(e.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use mill_config::poll;
    use openraft::Config;
    use tokio::net::TcpListener;

    use crate::MillRaft;
    use crate::fsm::StateMachineStore;
    use crate::log_store::LogStore;
    use crate::secrets::ClusterKey;

    /// Boot a single Raft node behind an axum listener, returning the
    /// raft handle and the base URL.
    async fn boot_raft_node(id: u64) -> (config::Raft, String) {
        let cfg = Arc::new(Config { enable_heartbeat: false, ..Config::default() });
        let log_store = LogStore::new();
        let sm = StateMachineStore::new();
        let network = HttpNetwork::new();
        let raft =
            openraft::Raft::<TypeConfig>::new(id, cfg, network, log_store, sm).await.expect("raft");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let router = HttpNetwork::axum_router(raft.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        (raft, format!("http://{addr}"))
    }

    /// Boot a full MillRaft node with HTTP transport, returning the MillRaft
    /// handle, HttpNetwork, and base URL.
    async fn boot_mill_raft_node(
        id: u64,
        cluster_key: ClusterKey,
        raft_config: Arc<Config>,
    ) -> (Arc<MillRaft>, HttpNetwork, String) {
        let network = HttpNetwork::new();
        let raft =
            MillRaft::new(id, raft_config, network.clone(), cluster_key).await.expect("mill raft");
        let raft = Arc::new(raft);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let router = HttpNetwork::axum_router(raft.raft().clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        (raft, network, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn http_vote_round_trip() {
        let (raft, url) = boot_raft_node(1).await;
        let _ = raft; // keep alive

        let mut network = HttpNetwork::new();
        network.set_peer(1, url);

        let mut conn = network.new_client(1, &BasicNode::new("")).await;
        let vote_req = VoteRequest { vote: openraft::Vote::new(1, 1), last_log_id: None };
        let result = conn.vote(vote_req, RPCOption::new(std::time::Duration::from_secs(5))).await;
        assert!(result.is_ok(), "vote RPC should succeed: {result:?}");
    }

    #[tokio::test]
    async fn http_append_round_trip() {
        let (raft, url) = boot_raft_node(2).await;
        let _ = raft;

        let mut network = HttpNetwork::new();
        network.set_peer(2, url);

        let mut conn = network.new_client(2, &BasicNode::new("")).await;
        let append_req = AppendEntriesRequest {
            vote: openraft::Vote::new_committed(1, 2),
            prev_log_id: None,
            entries: vec![],
            leader_commit: None,
        };
        let result = conn
            .append_entries(append_req, RPCOption::new(std::time::Duration::from_secs(5)))
            .await;
        // May succeed or return a Raft-level error (not leader), but should not be a network error
        assert!(
            result.is_ok() || matches!(result, Err(RPCError::RemoteError(_))),
            "append RPC should not fail with network error: {result:?}"
        );
    }

    #[tokio::test]
    async fn http_two_node_cluster_replicates() {
        use crate::fsm::command::Command;

        let cluster_key = ClusterKey::generate();
        let cfg = Arc::new(
            Config {
                heartbeat_interval: 100,
                election_timeout_min: 200,
                election_timeout_max: 400,
                ..Default::default()
            }
            .validate()
            .unwrap(),
        );

        // Boot two MillRaft nodes with HTTP transport
        let (raft1, net1, url1) = boot_mill_raft_node(1, cluster_key.clone(), cfg.clone()).await;
        let (raft2, net2, url2) = boot_mill_raft_node(2, cluster_key, cfg).await;

        // Wire peer maps so they can reach each other
        net1.set_peer(2, url2);
        net2.set_peer(1, url1);

        // Initialize node 1 as single-node, then add node 2
        raft1.initialize().await.unwrap();
        raft1.add_learner(2, "").await.unwrap();
        raft1.change_membership(vec![1, 2]).await.unwrap();

        // change_membership waits for quorum commit; raft1 is still leader.

        // Propose a SecretSet on the leader
        let (enc, nonce) = raft1.encrypt_secret(b"hello").unwrap();
        raft1
            .propose(Command::SecretSet {
                name: "test-secret".to_string(),
                encrypted_value: enc,
                nonce,
            })
            .await
            .unwrap();

        // Wait for replication to node 2.
        poll(|| raft2.read_state(|fsm| fsm.get_secret("test-secret").is_some()))
            .expect("secret not replicated to node 2")
            .await;

        raft1.shutdown().await.unwrap();
        raft2.shutdown().await.unwrap();
    }

    #[test]
    fn peer_map_add_remove() {
        let network = HttpNetwork::new();

        // Initially empty
        assert!(network.get_peer_url(1).is_err());

        // Add peer
        network.set_peer(1, "http://10.99.0.1:4401".to_string());
        assert_eq!(network.get_peer_url(1).unwrap(), "http://10.99.0.1:4401");

        // Update peer
        network.set_peer(1, "http://10.99.0.1:4402".to_string());
        assert_eq!(network.get_peer_url(1).unwrap(), "http://10.99.0.1:4402");

        // Remove peer
        network.remove_peer(1);
        assert!(network.get_peer_url(1).is_err());
    }
}
