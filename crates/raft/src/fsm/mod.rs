mod apply;
pub mod command;
pub mod state;

pub use apply::apply_command;
pub use command::{Command, Response};
pub use state::{EncryptedSecret, FsmNode, FsmState, FsmVolume};

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use openraft::entry::RaftPayload;
use openraft::{EntryPayload, RaftSnapshotBuilder, StorageError, StoredMembership};
use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;

use crate::config::{self, TypeConfig};
use crate::snapshot;

/// State machine store for openraft with optional snapshot persistence.
///
/// Holds the FSM state, last applied log ID, last membership, and the
/// most recent snapshot. Exposes a watch channel that bumps a monotonic
/// counter after each `apply()` batch so consumers can react to state changes.
///
/// When a `raft_dir` is configured, snapshots are persisted to disk so
/// that the FSM can be restored after a process restart.
#[derive(Debug, Clone)]
pub struct StateMachineStore {
    state: Arc<RwLock<StateMachineInner>>,
    /// Snapshot storage lives behind its own lock so that `build_snapshot`
    /// (which runs on a separate task) never contends with `apply()`.
    current_snapshot: Arc<Mutex<Option<config::Snapshot>>>,
    change_tx: Arc<watch::Sender<u64>>,
    raft_dir: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct StateMachineInner {
    fsm: FsmState,
    last_applied: Option<config::LogId>,
    last_membership: config::StoredMembership,
    version: u64,
}

impl StateMachineStore {
    /// Create a new in-memory state machine store (no persistence). Used for tests.
    pub fn new() -> Self {
        let (change_tx, _) = watch::channel(0u64);
        Self {
            state: Arc::new(RwLock::new(StateMachineInner::default())),
            current_snapshot: Arc::new(Mutex::new(None)),
            change_tx: Arc::new(change_tx),
            raft_dir: None,
        }
    }

    /// Create a state machine store with disk-backed snapshot persistence.
    pub fn with_persistence(raft_dir: PathBuf) -> Self {
        let (change_tx, _) = watch::channel(0u64);
        Self {
            state: Arc::new(RwLock::new(StateMachineInner::default())),
            current_snapshot: Arc::new(Mutex::new(None)),
            change_tx: Arc::new(change_tx),
            raft_dir: Some(raft_dir),
        }
    }

    /// Restore a state machine from a previously persisted snapshot.
    pub fn from_snapshot(
        raft_dir: PathBuf,
        fsm: FsmState,
        meta: config::SnapshotMeta,
        data: Vec<u8>,
    ) -> Self {
        let (change_tx, _) = watch::channel(0u64);
        let snap = config::Snapshot { meta: meta.clone(), snapshot: Box::new(Cursor::new(data)) };
        let inner = StateMachineInner {
            fsm,
            last_applied: meta.last_log_id,
            last_membership: meta.last_membership.clone(),
            version: 0,
        };
        Self {
            state: Arc::new(RwLock::new(inner)),
            current_snapshot: Arc::new(Mutex::new(Some(snap))),
            change_tx: Arc::new(change_tx),
            raft_dir: Some(raft_dir),
        }
    }

    /// Read FSM state with a closure.
    pub fn read_state<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&FsmState) -> T,
    {
        let inner = self.state.read();
        f(&inner.fsm)
    }

    /// Subscribe to state change notifications. The receiver yields the
    /// monotonic version counter that increments after each `apply()` batch.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.change_tx.subscribe()
    }
}

impl Default for StateMachineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl openraft::storage::RaftStateMachine<TypeConfig> for StateMachineStore {
    type SnapshotBuilder = StateMachineSnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<config::LogId>, config::StoredMembership), StorageError<u64>> {
        let inner = self.state.read();
        Ok((inner.last_applied, inner.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Response>, StorageError<u64>>
    where
        I: IntoIterator<Item = config::Entry> + Send,
        I::IntoIter: Send,
    {
        let mut inner = self.state.write();
        let mut responses = Vec::new();

        for entry in entries {
            inner.last_applied = Some(entry.log_id);

            // Store membership if present
            if let Some(membership) = entry.get_membership() {
                inner.last_membership =
                    StoredMembership::new(Some(entry.log_id), membership.clone());
            }

            match entry.payload {
                EntryPayload::Normal(cmd) => {
                    let resp = apply_command(&mut inner.fsm, cmd);
                    responses.push(resp);
                }
                EntryPayload::Blank | EntryPayload::Membership(_) => {
                    responses.push(Response::Ok);
                }
            }
        }

        // Bump version and notify subscribers after the batch.
        inner.version += 1;
        let version = inner.version;
        drop(inner);
        let _ = self.change_tx.send(version);

        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        let inner = self.state.read();
        StateMachineSnapshotBuilder {
            fsm_snapshot: inner.fsm.clone(),
            last_applied: inner.last_applied,
            last_membership: inner.last_membership.clone(),
            raft_dir: self.raft_dir.clone(),
            snapshot_slot: Arc::clone(&self.current_snapshot),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &config::SnapshotMeta,
        snapshot_data: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let data = snapshot_data.into_inner();
        let fsm = snapshot::codec::decode(&data).map_err(|e| StorageError::IO {
            source: openraft::StorageIOError::read_snapshot(
                Some(meta.signature()),
                openraft::AnyError::new(&e),
            ),
        })?;

        let version = {
            let mut inner = self.state.write();
            inner.fsm = fsm;
            inner.last_applied = meta.last_log_id;
            inner.last_membership = meta.last_membership.clone();
            inner.version += 1;
            inner.version
        };

        // Store snapshot in the dedicated slot (separate lock from state).
        *self.current_snapshot.lock() = Some(config::Snapshot {
            meta: meta.clone(),
            snapshot: Box::new(Cursor::new(data.clone())),
        });

        let _ = self.change_tx.send(version);

        if let Some(dir) = &self.raft_dir
            && let Err(e) = snapshot::disk::write_snapshot(dir, &data, meta).await
        {
            tracing::warn!("failed to persist snapshot: {e}");
        }

        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<config::Snapshot>, StorageError<u64>> {
        let snap = self.current_snapshot.lock();
        Ok(snap.as_ref().map(|s| {
            let data = s.snapshot.get_ref().clone();
            config::Snapshot { meta: s.meta.clone(), snapshot: Box::new(Cursor::new(data)) }
        }))
    }
}

/// Builds a snapshot from a point-in-time copy of the FSM state.
pub struct StateMachineSnapshotBuilder {
    fsm_snapshot: FsmState,
    last_applied: Option<config::LogId>,
    last_membership: config::StoredMembership,
    raft_dir: Option<PathBuf>,
    snapshot_slot: Arc<Mutex<Option<config::Snapshot>>>,
}

impl std::fmt::Debug for StateMachineSnapshotBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateMachineSnapshotBuilder").finish()
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachineSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<config::Snapshot, StorageError<u64>> {
        let data = snapshot::codec::encode(&self.fsm_snapshot).map_err(|e| StorageError::IO {
            source: openraft::StorageIOError::read_snapshot(None, openraft::AnyError::new(&e)),
        })?;

        let snapshot_id =
            format!("snapshot-{}", self.last_applied.as_ref().map(|id| id.index).unwrap_or(0));

        let meta = config::SnapshotMeta {
            last_log_id: self.last_applied,
            last_membership: self.last_membership.clone(),
            snapshot_id,
        };

        if let Some(dir) = &self.raft_dir
            && let Err(e) = snapshot::disk::write_snapshot(dir, &data, &meta).await
        {
            tracing::warn!("failed to persist locally-built snapshot: {e}");
        }

        // Store the snapshot so get_current_snapshot() can return it.
        // Without this, openraft can't send snapshots to new learners
        // that need to catch up. Uses a dedicated Mutex (not the state
        // RwLock) to avoid contending with apply().
        *self.snapshot_slot.lock() = Some(config::Snapshot {
            meta: meta.clone(),
            snapshot: Box::new(Cursor::new(data.clone())),
        });

        Ok(config::Snapshot { meta, snapshot: Box::new(Cursor::new(data)) })
    }
}
