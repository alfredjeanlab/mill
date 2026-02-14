use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytesize::ByteSize;
use mill_config::{NodeId, NodeResources, poll, poll_async};
use mill_raft::MillRaft;
use mill_raft::fsm::command::Command;
use mill_raft::network::TestRouter;
use mill_raft::secrets::ClusterKey;
use openraft::Config;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use mill_net::{Mesh, PeerInfo};
use mill_node::{Primary, PrimaryConfig, RpcServer, SecondaryHandle};

use crate::mesh::TestMesh;
use crate::secondary::TestSecondary;
use crate::volume::TestVolumes;

/// Resources assigned to each simulated node.
pub fn test_resources() -> NodeResources {
    NodeResources {
        cpu_total: 4.0,
        cpu_available: 4.0,
        memory_total: ByteSize::gib(8),
        memory_available: ByteSize::gib(8),
    }
}

fn test_raft_config() -> Arc<Config> {
    let config = Config {
        heartbeat_interval: 100,
        election_timeout_min: 200,
        election_timeout_max: 400,
        ..Default::default()
    };
    Arc::new(config.validate().unwrap())
}

/// A test node within the cluster.
struct TestNode {
    raft_id: u64,
    mill_id: NodeId,
    raft: Arc<MillRaft>,
    rpc_addr: SocketAddr,
    secondary: TestSecondary,
}

/// Top-level test harness that creates an N-node cluster with real Raft,
/// real RPC servers, and simulated secondaries.
pub struct TestCluster {
    nodes: Vec<TestNode>,
    raft_router: TestRouter,
    cluster_key: ClusterKey,
    primary: Option<Primary>,
    primary_cancel: CancellationToken,
    volume_driver: Arc<TestVolumes>,
    mesh: Arc<TestMesh>,
}

impl TestCluster {
    /// Create and initialize an N-node cluster.
    ///
    /// - Sets up Raft consensus across all nodes.
    /// - Starts a real `RpcServer` on localhost for each node.
    /// - Starts a `SimulatedSecondary` actor on each node's channels.
    /// - Starts a `Primary` on the leader node.
    /// - Registers all nodes in the FSM via `Command::NodeRegister`.
    pub async fn new(node_count: usize) -> Self {
        mill_node::env::init_test_defaults();
        assert!(node_count >= 1, "need at least 1 node");

        let raft_router = TestRouter::new();
        let cluster_key = ClusterKey::generate();
        let raft_config = test_raft_config();

        // 1. Create Raft instances.
        let raft_ids: Vec<u64> = (1..=node_count as u64).collect();
        let mut rafts: Vec<Arc<MillRaft>> = Vec::new();
        for &id in &raft_ids {
            let raft =
                MillRaft::new(id, raft_config.clone(), raft_router.clone(), cluster_key.clone())
                    .await
                    .unwrap();
            raft_router.add_node(id, raft.raft().clone());
            rafts.push(Arc::new(raft));
        }

        // 2. Initialize Raft cluster.
        rafts[0].initialize().await.unwrap();
        // Wait for the leader election to complete and initial membership to
        // be committed before adding learners / changing membership.
        let raft_ref = &rafts[0];
        poll_async(|| async move { raft_ref.ensure_linearizable().await.is_ok() })
            .expect("leader not ready after initialize")
            .await;
        for &id in &raft_ids[1..] {
            rafts[0].add_learner(id, "").await.unwrap();
        }
        rafts[0].change_membership(raft_ids.clone()).await.unwrap();

        // 3. For each node: create channels, start RPC server, start SimulatedSecondary.
        let mut nodes = Vec::new();
        let rpc_token = "test-rpc-token".to_string();
        let volume_driver = Arc::new(TestVolumes::new());
        let mesh = Arc::new(TestMesh::new());

        for (i, raft) in rafts.into_iter().enumerate() {
            let raft_id = raft_ids[i];
            let mill_id = NodeId(format!("node-{raft_id}"));

            // Create channel pairs.
            let (cmd_tx, cmd_rx) = mpsc::channel(256);
            let (report_tx, report_rx) = mpsc::channel(256);

            // Start SimulatedSecondary on one half.
            let secondary =
                TestSecondary::start(cmd_rx, report_tx, mill_id.clone(), test_resources());

            // Start RPC server on a random port, backed by the other half.
            let handle = SecondaryHandle::from_parts(cmd_tx, report_rx);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let rpc_addr = listener.local_addr().unwrap();
            let rpc_tok = rpc_token.clone();
            let rpc_cancel = CancellationToken::new();
            tokio::spawn(async move {
                let _ = RpcServer::start_with_listener(listener, rpc_tok, handle, rpc_cancel).await;
            });

            nodes.push(TestNode { raft_id, mill_id, raft, rpc_addr, secondary });
        }

        // Give RPC servers a moment to start accepting.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // 4. Find the leader.
        let leader_idx = find_leader_idx(&nodes).await;

        // 5. Register all nodes in FSM via Command::NodeRegister.
        for i in 0..nodes.len() {
            let raft_id = nodes[i].raft_id;
            let mill_id = nodes[i].mill_id.clone();
            let rpc_addr = nodes[i].rpc_addr;
            let pubkey = format!("test-pubkey-{raft_id}");
            nodes[leader_idx]
                .raft
                .propose(Command::NodeRegister {
                    id: raft_id,
                    mill_id: mill_id.clone(),
                    address: rpc_addr,
                    resources: test_resources(),
                    wireguard_pubkey: Some(pubkey.clone()),
                    instance_id: Some(format!("test-instance-{raft_id}")),
                    rpc_address: Some(rpc_addr),
                    advertise_addr: None,
                })
                .await
                .unwrap();

            // Pre-populate the test mesh peer (mirrors production post_join).
            let _ = mesh
                .add_peer(&PeerInfo {
                    node_id: mill_id,
                    public_key: pubkey,
                    endpoint: None,
                    tunnel_ip: rpc_addr.ip(),
                })
                .await;
        }

        // Wait for replication.
        {
            let expected = nodes.len();
            let raft_ref = &nodes[leader_idx].raft;
            poll(move || raft_ref.read_state(|fsm| fsm.nodes.len()) == expected)
                .expect("node registration not replicated")
                .await;
        }

        // 6. Start Primary on the leader.
        // The leader's secondary is served via RPC (like remote nodes), so we
        // pass local_node_id = None. The connection watcher will connect to all
        // nodes (including the leader's own SimulatedSecondary) over TCP.
        let primary_cancel = CancellationToken::new();
        let primary = Primary::start(
            PrimaryConfig {
                api_addr: "127.0.0.1:0".parse().unwrap(),
                auth_token: "test-api-token".to_string(),
                rpc_token: rpc_token.clone(),
                local_node_id: None,
                volume_driver: Some(volume_driver.clone() as Arc<dyn mill_node::storage::Volumes>),
                dns: None,
                proxy: None,
                wireguard: Some(mesh.clone() as Arc<dyn Mesh>),
                raft_network: None,
            },
            Arc::clone(&nodes[leader_idx].raft),
            None,
            primary_cancel.clone(),
        )
        .await
        .unwrap();

        // Wait for connection watcher to connect to all nodes.
        let router = primary.router().clone();
        let expected = nodes.len();
        poll(move || router.route_count() >= expected)
            .expect("connection watcher did not connect to all nodes")
            .await;

        Self {
            nodes,
            raft_router,
            cluster_key,
            primary: Some(primary),
            primary_cancel,
            volume_driver,
            mesh,
        }
    }

    /// Create an N-node cluster where the leader uses a direct `SecondaryHandle`
    /// instead of an `RpcServer`.
    ///
    /// This mirrors the daemon's wiring pattern (`daemon::init`): the local node
    /// passes a `SecondaryHandle` directly to `Primary::start` while remote nodes
    /// use TCP RPC. Tests using this constructor catch wiring bugs specific to
    /// the mixed local+remote pattern.
    pub async fn new_with_local_leader(node_count: usize) -> Self {
        mill_node::env::init_test_defaults();
        assert!(node_count >= 1, "need at least 1 node");

        let raft_router = TestRouter::new();
        let cluster_key = ClusterKey::generate();
        let raft_config = test_raft_config();

        // 1. Create Raft instances.
        let raft_ids: Vec<u64> = (1..=node_count as u64).collect();
        let mut rafts: Vec<Arc<MillRaft>> = Vec::new();
        for &id in &raft_ids {
            let raft =
                MillRaft::new(id, raft_config.clone(), raft_router.clone(), cluster_key.clone())
                    .await
                    .unwrap();
            raft_router.add_node(id, raft.raft().clone());
            rafts.push(Arc::new(raft));
        }

        // 2. Initialize Raft cluster.
        rafts[0].initialize().await.unwrap();
        let raft_ref = &rafts[0];
        poll_async(|| async move { raft_ref.ensure_linearizable().await.is_ok() })
            .expect("leader not ready after initialize")
            .await;
        for &id in &raft_ids[1..] {
            rafts[0].add_learner(id, "").await.unwrap();
        }
        rafts[0].change_membership(raft_ids.clone()).await.unwrap();

        // 3. Set up nodes. Leader gets direct channels; remotes get RPC servers.
        let mut nodes = Vec::new();
        let rpc_token = "test-rpc-token".to_string();
        let volume_driver = Arc::new(TestVolumes::new());
        let mesh = Arc::new(TestMesh::new());

        let leader_idx = find_leader_idx_rafts(&rafts).await;
        let leader_mill_id = NodeId(format!("node-{}", raft_ids[leader_idx]));

        // Leader's secondary handle (passed directly to Primary, no RPC).
        let mut leader_handle: Option<SecondaryHandle> = None;

        for (i, raft) in rafts.into_iter().enumerate() {
            let raft_id = raft_ids[i];
            let mill_id = NodeId(format!("node-{raft_id}"));

            let (cmd_tx, cmd_rx) = mpsc::channel(256);
            let (report_tx, report_rx) = mpsc::channel(256);

            let secondary =
                TestSecondary::start(cmd_rx, report_tx, mill_id.clone(), test_resources());

            if i == leader_idx {
                // Leader: keep SecondaryHandle for direct wiring (no RPC server).
                leader_handle = Some(SecondaryHandle::from_parts(cmd_tx, report_rx));
                // Use 127.0.0.1:0 as a placeholder rpc_addr (leader won't be
                // reached via RPC, but we need an address for NodeRegister).
                let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
                nodes.push(TestNode { raft_id, mill_id, raft, rpc_addr, secondary });
            } else {
                // Remote: start RPC server.
                let handle = SecondaryHandle::from_parts(cmd_tx, report_rx);
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let rpc_addr = listener.local_addr().unwrap();
                let rpc_tok = rpc_token.clone();
                let rpc_cancel = CancellationToken::new();
                tokio::spawn(async move {
                    let _ =
                        RpcServer::start_with_listener(listener, rpc_tok, handle, rpc_cancel).await;
                });
                nodes.push(TestNode { raft_id, mill_id, raft, rpc_addr, secondary });
            }
        }

        // 4. Register all nodes.
        for i in 0..nodes.len() {
            let raft_id = nodes[i].raft_id;
            let mill_id = nodes[i].mill_id.clone();
            let rpc_addr = nodes[i].rpc_addr;
            let pubkey = format!("test-pubkey-{raft_id}");
            nodes[leader_idx]
                .raft
                .propose(Command::NodeRegister {
                    id: raft_id,
                    mill_id: mill_id.clone(),
                    address: rpc_addr,
                    resources: test_resources(),
                    wireguard_pubkey: Some(pubkey.clone()),
                    instance_id: Some(format!("test-instance-{raft_id}")),
                    rpc_address: Some(rpc_addr),
                    advertise_addr: None,
                })
                .await
                .unwrap();

            let _ = mesh
                .add_peer(&PeerInfo {
                    node_id: mill_id,
                    public_key: pubkey,
                    endpoint: None,
                    tunnel_ip: rpc_addr.ip(),
                })
                .await;
        }

        // Wait for replication.
        let expected = nodes.len();
        let raft_ref = &nodes[leader_idx].raft;
        poll(move || raft_ref.read_state(|fsm| fsm.nodes.len()) == expected)
            .expect("node registration not replicated")
            .await;

        // 5. Start Primary with local_node_id and the leader's direct handle.
        let primary_cancel = CancellationToken::new();
        let primary = Primary::start(
            PrimaryConfig {
                api_addr: "127.0.0.1:0".parse().unwrap(),
                auth_token: "test-api-token".to_string(),
                rpc_token: rpc_token.clone(),
                local_node_id: Some(leader_mill_id),
                volume_driver: Some(volume_driver.clone() as Arc<dyn mill_node::storage::Volumes>),
                dns: None,
                proxy: None,
                wireguard: Some(mesh.clone() as Arc<dyn Mesh>),
                raft_network: None,
            },
            Arc::clone(&nodes[leader_idx].raft),
            leader_handle,
            primary_cancel.clone(),
        )
        .await
        .unwrap();

        // Wait for connection watcher to connect to all nodes.
        let router = primary.router().clone();
        let expected = node_count;
        poll(move || router.route_count() >= expected)
            .expect("connection watcher did not connect to all nodes")
            .await;

        Self {
            nodes,
            raft_router,
            cluster_key,
            primary: Some(primary),
            primary_cancel,
            volume_driver,
            mesh,
        }
    }

    /// The API address for HTTP requests.
    pub fn leader_api_addr(&self) -> SocketAddr {
        self.primary.as_ref().unwrap().api_addr()
    }

    /// The auth token for API requests.
    pub fn api_token(&self) -> &str {
        "test-api-token"
    }

    /// Access the shared in-memory volume driver for assertions.
    pub fn volume_driver(&self) -> &Arc<TestVolumes> {
        &self.volume_driver
    }

    /// Access the test mesh peers for WireGuard assertions.
    pub fn mesh(&self) -> &Arc<TestMesh> {
        &self.mesh
    }

    /// Get a mutable reference to a node's simulated secondary for failure injection.
    pub fn secondary(&mut self, node_idx: usize) -> &mut TestSecondary {
        &mut self.nodes[node_idx].secondary
    }

    /// The Raft TestRouter for partition simulation.
    pub fn raft_router(&self) -> &TestRouter {
        &self.raft_router
    }

    /// Access a node's raft instance.
    pub fn raft(&self, node_idx: usize) -> &Arc<MillRaft> {
        &self.nodes[node_idx].raft
    }

    /// Get the raft ID for a node.
    pub fn raft_id(&self, node_idx: usize) -> u64 {
        self.nodes[node_idx].raft_id
    }

    /// Get the mill NodeId for a node.
    pub fn mill_id(&self, node_idx: usize) -> &NodeId {
        &self.nodes[node_idx].mill_id
    }

    /// Find the current leader index.
    pub async fn find_leader(&self) -> usize {
        find_leader_idx(&self.nodes).await
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Register a new joiner raft node in the TestRouter so that `post_join`'s
    /// `add_learner` / `change_membership` calls can reach it.
    pub async fn add_joiner_to_router(&self, raft_id: u64) -> Arc<MillRaft> {
        let raft = MillRaft::new(
            raft_id,
            test_raft_config(),
            self.raft_router.clone(),
            self.cluster_key.clone(),
        )
        .await
        .unwrap();
        let raft = Arc::new(raft);
        self.raft_router.add_node(raft_id, raft.raft().clone());
        raft
    }

    /// Stop the current Primary and start a new one on the given node.
    ///
    /// Used for testing leader failover: after a new Raft leader emerges,
    /// start a new Primary on that node to accept API requests.
    pub async fn restart_primary_on(&mut self, node_idx: usize) {
        // Stop the old primary.
        self.primary_cancel.cancel();
        if let Some(p) = self.primary.take() {
            p.stop();
        }

        // Start a new Primary on the specified node.
        let new_cancel = CancellationToken::new();
        let primary = Primary::start(
            PrimaryConfig {
                api_addr: "127.0.0.1:0".parse().unwrap(),
                auth_token: "test-api-token".to_string(),
                rpc_token: "test-rpc-token".to_string(),
                local_node_id: None,
                volume_driver: Some(
                    self.volume_driver.clone() as Arc<dyn mill_node::storage::Volumes>
                ),
                dns: None,
                proxy: None,
                wireguard: Some(self.mesh.clone() as Arc<dyn Mesh>),
                raft_network: None,
            },
            Arc::clone(&self.nodes[node_idx].raft),
            None,
            new_cancel.clone(),
        )
        .await
        .unwrap();

        // Wait for connection watcher to connect to all nodes.
        let router = primary.router().clone();
        let expected = self.nodes.len();
        poll(move || router.route_count() >= expected)
            .expect("connection watcher did not connect to all nodes")
            .await;

        self.primary = Some(primary);
        self.primary_cancel = new_cancel;
    }

    /// Shut down the cluster.
    pub async fn shutdown(mut self) {
        self.primary_cancel.cancel();
        if let Some(p) = self.primary.take() {
            p.stop();
        }
        for node in &self.nodes {
            let _ = node.raft.shutdown().await;
        }
    }
}

/// Find the leader index from a slice of Raft instances (used before TestNodes are built).
async fn find_leader_idx_rafts(rafts: &[Arc<MillRaft>]) -> usize {
    poll_async(|| async move {
        for raft in rafts {
            if raft.ensure_linearizable().await.is_ok() {
                return true;
            }
        }
        false
    })
    .expect("no leader found")
    .await;

    for (i, raft) in rafts.iter().enumerate() {
        if raft.ensure_linearizable().await.is_ok() {
            return i;
        }
    }
    unreachable!("poll_async confirmed a leader exists")
}

async fn find_leader_idx(nodes: &[TestNode]) -> usize {
    poll_async(|| async move {
        for node in nodes {
            if node.raft.ensure_linearizable().await.is_ok() {
                return true;
            }
        }
        false
    })
    .expect("no leader found")
    .await;

    for (i, node) in nodes.iter().enumerate() {
        if node.raft.ensure_linearizable().await.is_ok() {
            return i;
        }
    }
    unreachable!("poll_async confirmed a leader exists")
}
