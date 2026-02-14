#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod config;
pub mod error;
pub mod fsm;
mod log_store;
pub mod network;
pub mod secrets;
pub mod snapshot;

pub use config::{Raft, TypeConfig};
pub use error::Result;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use openraft::{BasicNode, ChangeMembers, Config, RaftNetworkFactory};
use tokio::sync::watch;

use crate::error::{DecryptError, EncryptError, RaftError};
use crate::fsm::command::{Command, Response};
use crate::fsm::{FsmState, StateMachineStore};
use crate::log_store::LogStore;
use crate::secrets::ClusterKey;

/// Public API for the Raft subsystem, used by `mill-node`.
pub struct MillRaft {
    id: u64,
    raft: Raft,
    sm: StateMachineStore,
    cluster_key: ClusterKey,
}

impl MillRaft {
    /// Create a new in-memory MillRaft instance (no persistence). Used for tests.
    pub async fn new<N: RaftNetworkFactory<TypeConfig>>(
        id: u64,
        raft_config: Arc<Config>,
        network: N,
        cluster_key: ClusterKey,
    ) -> Result<Self> {
        let log_store = LogStore::new();
        let sm = StateMachineStore::new();

        let raft =
            openraft::Raft::<TypeConfig>::new(id, raft_config, network, log_store, sm.clone())
                .await
                .map_err(|e: openraft::error::Fatal<u64>| RaftError::Raft(e.to_string()))?;

        Ok(Self { id, raft, sm, cluster_key })
    }

    /// Open a MillRaft instance with disk-backed persistence.
    ///
    /// Restores from a persisted snapshot if one exists, otherwise starts
    /// empty. The vote is also restored to preserve election state.
    pub async fn open<N: RaftNetworkFactory<TypeConfig>>(
        id: u64,
        raft_config: Arc<Config>,
        network: N,
        cluster_key: ClusterKey,
        raft_dir: &Path,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(raft_dir).await?;

        let vote = snapshot::disk::read_vote(raft_dir).await?;

        let log_store = LogStore::with_persistence(raft_dir.to_owned(), vote);

        let sm = match snapshot::disk::read_snapshot(raft_dir).await? {
            Some((data, meta)) => {
                let fsm = snapshot::codec::decode(&data)?;
                tracing::info!(
                    "restored raft snapshot: id={}, last_log_index={}",
                    meta.snapshot_id,
                    meta.last_log_id.map(|id| id.index).unwrap_or(0),
                );
                StateMachineStore::from_snapshot(raft_dir.to_owned(), fsm, meta, data)
            }
            None => {
                tracing::info!("no persisted raft snapshot found, starting empty");
                StateMachineStore::with_persistence(raft_dir.to_owned())
            }
        };

        let raft =
            openraft::Raft::<TypeConfig>::new(id, raft_config, network, log_store, sm.clone())
                .await
                .map_err(|e: openraft::error::Fatal<u64>| RaftError::Raft(e.to_string()))?;

        Ok(Self { id, raft, sm, cluster_key })
    }

    /// Bootstrap a single-node cluster.
    pub async fn initialize(&self) -> Result<()> {
        let mut members = BTreeMap::new();
        members.insert(self.id, BasicNode::new(""));
        self.raft.initialize(members).await.map_err(|e| RaftError::Raft(e.to_string()))
    }

    /// Add a learner node (non-voting follower that receives log replication).
    pub async fn add_learner(&self, id: u64, addr: &str) -> Result<()> {
        self.raft
            .add_learner(id, BasicNode::new(addr), true)
            .await
            .map_err(|e| RaftError::Raft(e.to_string()))?;
        Ok(())
    }

    /// Change membership — promote learners to voters.
    pub async fn change_membership(&self, member_ids: Vec<u64>) -> Result<()> {
        let members: std::collections::BTreeSet<u64> = member_ids.into_iter().collect();
        self.raft
            .change_membership(ChangeMembers::AddVoterIds(members), false)
            .await
            .map_err(|e| RaftError::Raft(e.to_string()))?;
        Ok(())
    }

    /// Propose a command through Raft consensus.
    pub async fn propose(&self, cmd: Command) -> Result<Response> {
        let result =
            self.raft.client_write(cmd).await.map_err(|e| RaftError::Raft(e.to_string()))?;
        Ok(result.data)
    }

    /// Confirm this node is the leader (linearizable read barrier).
    pub async fn ensure_linearizable(&self) -> Result<()> {
        self.raft.ensure_linearizable().await.map_err(|e| RaftError::Raft(e.to_string()))?;
        Ok(())
    }

    /// Read FSM state via a closure. Does NOT go through consensus.
    pub fn read_state<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&FsmState) -> T,
    {
        self.sm.read_state(f)
    }

    /// Subscribe to FSM state change notifications.
    ///
    /// Returns a watch receiver that yields a monotonic version counter
    /// bumped after each `apply()` batch. Consumers should call
    /// `read_state()` when notified to recompute derived views.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.sm.subscribe()
    }

    /// Encrypt a secret value for storage in the FSM.
    pub fn encrypt_secret(
        &self,
        plaintext: &[u8],
    ) -> std::result::Result<(Vec<u8>, Vec<u8>), EncryptError> {
        self.cluster_key.encrypt(plaintext)
    }

    /// Decrypt a secret value retrieved from the FSM.
    pub fn decrypt_secret(
        &self,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> std::result::Result<Vec<u8>, DecryptError> {
        self.cluster_key.decrypt(ciphertext, nonce)
    }

    /// Get the Raft node ID.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Get a reference to the underlying openraft Raft handle.
    pub fn raft(&self) -> &Raft {
        &self.raft
    }

    /// Get a reference to the cluster key.
    pub fn cluster_key(&self) -> &ClusterKey {
        &self.cluster_key
    }

    /// Shut down the Raft node.
    pub async fn shutdown(&self) -> Result<()> {
        self.raft.shutdown().await.map_err(|e| RaftError::Raft(e.to_string()))
    }
}
