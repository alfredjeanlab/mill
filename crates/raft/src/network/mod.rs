pub mod http;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use parking_lot::RwLock;

use openraft::error::{
    NetworkError, RPCError, RaftError, ReplicationClosed, StreamingError, Unreachable,
};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{BasicNode, RaftNetwork, RaftNetworkFactory};

use crate::config::{self, TypeConfig};

/// In-process test router that maps node IDs to Raft handles.
///
/// Calls go directly to peer Raft instances — no real networking.
/// Exported as `pub` for integration tests.
#[derive(Clone, Default)]
pub struct TestRouter {
    nodes: Arc<RwLock<HashMap<u64, config::Raft>>>,
}

impl TestRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Raft node handle.
    pub fn add_node(&self, id: u64, raft: config::Raft) {
        let mut nodes = self.nodes.write();
        nodes.insert(id, raft);
    }

    /// Remove a Raft node handle (simulates node failure).
    pub fn remove_node(&self, id: u64) {
        let mut nodes = self.nodes.write();
        nodes.remove(&id);
    }

    fn get_node(&self, id: u64) -> Result<config::Raft, Unreachable> {
        let nodes = self.nodes.read();
        nodes.get(&id).cloned().ok_or_else(|| {
            Unreachable::new(&std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("node {id} not reachable"),
            ))
        })
    }
}

impl RaftNetworkFactory<TypeConfig> for TestRouter {
    type Network = TestNetwork;

    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Self::Network {
        TestNetwork { router: self.clone(), target }
    }
}

/// A network connection to a single peer, backed by the TestRouter.
pub struct TestNetwork {
    router: TestRouter,
    target: u64,
}

impl RaftNetwork<TypeConfig> for TestNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let raft = self.router.get_node(self.target)?;
        raft.append_entries(rpc).await.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, openraft::error::InstallSnapshotError>>,
    > {
        let raft = self.router.get_node(self.target).map_err(RPCError::Unreachable)?;
        raft.install_snapshot(rpc).await.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let raft = self.router.get_node(self.target)?;
        raft.vote(rpc).await.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    async fn full_snapshot(
        &mut self,
        vote: openraft::Vote<u64>,
        snapshot: config::Snapshot,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<u64>, StreamingError<TypeConfig, openraft::error::Fatal<u64>>>
    {
        let raft = self.router.get_node(self.target).map_err(StreamingError::Unreachable)?;
        raft.install_full_snapshot(vote, snapshot).await.map_err(
            |e: openraft::error::Fatal<u64>| match e {
                openraft::error::Fatal::StorageError(se) => StreamingError::StorageError(se),
                other => StreamingError::Unreachable(Unreachable::new(&other)),
            },
        )
    }
}
