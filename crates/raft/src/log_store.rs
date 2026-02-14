use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::RangeBounds;
use std::path::PathBuf;
use std::sync::Arc;

use openraft::storage::LogFlushed;
use openraft::{LogId, RaftLogReader, StorageError};
use parking_lot::RwLock;

use crate::config::{self, TypeConfig};
use crate::snapshot;

/// Shared inner state for the in-memory log store.
#[derive(Debug, Default)]
struct LogStoreInner {
    vote: Option<config::Vote>,
    committed: Option<config::LogId>,
    log: BTreeMap<u64, config::Entry>,
    last_purged: Option<config::LogId>,
}

/// In-memory log store for Raft, with optional vote persistence.
///
/// All log entries live in memory behind `Arc<RwLock<_>>`. Durability is
/// provided by snapshots — on restart, a node loads its last snapshot and
/// catches up via raft replication from the leader. The vote is persisted
/// to disk (if a `raft_dir` is configured) to preserve election state
/// across restarts.
#[derive(Debug, Clone)]
pub struct LogStore {
    inner: Arc<RwLock<LogStoreInner>>,
    raft_dir: Option<PathBuf>,
}

impl Default for LogStore {
    fn default() -> Self {
        Self { inner: Arc::new(RwLock::new(LogStoreInner::default())), raft_dir: None }
    }
}

impl LogStore {
    /// Create a new in-memory log store (no persistence). Used for tests.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a log store with disk-backed vote persistence.
    ///
    /// If a persisted vote is provided, it is pre-populated so that
    /// openraft can resume its election state.
    pub fn with_persistence(raft_dir: PathBuf, vote: Option<config::Vote>) -> Self {
        let inner = LogStoreInner { vote, ..Default::default() };
        Self { inner: Arc::new(RwLock::new(inner)), raft_dir: Some(raft_dir) }
    }
}

// LogReader is a clone of LogStore (shared Arc).
impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<config::Entry>, StorageError<u64>> {
        let inner = self.inner.read();
        let entries: Vec<_> = inner.log.range(range).map(|(_, e)| e.clone()).collect();
        Ok(entries)
    }
}

impl openraft::storage::RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = LogStore;

    async fn get_log_state(&mut self) -> Result<config::LogState, StorageError<u64>> {
        let inner = self.inner.read();
        let last_log_id = inner.log.last_key_value().map(|(_, e)| e.log_id).or(inner.last_purged);
        Ok(config::LogState { last_purged_log_id: inner.last_purged, last_log_id })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &config::Vote) -> Result<(), StorageError<u64>> {
        {
            let mut inner = self.inner.write();
            inner.vote = Some(*vote);
        }

        if let Some(dir) = &self.raft_dir
            && let Err(e) = snapshot::disk::write_vote(dir, vote).await
        {
            tracing::warn!("failed to persist vote: {e}");
        }
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<config::Vote>, StorageError<u64>> {
        let inner = self.inner.read();
        Ok(inner.vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write();
        inner.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        let inner = self.inner.read();
        Ok(inner.committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = config::Entry> + Send,
        I::IntoIter: Send,
    {
        let mut inner = self.inner.write();
        for entry in entries {
            inner.log.insert(entry.log_id.index, entry);
        }
        // In-memory: immediately "flushed"
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write();
        let to_remove: Vec<u64> = inner.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in to_remove {
            inner.log.remove(&k);
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write();
        let to_remove: Vec<u64> = inner.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in to_remove {
            inner.log.remove(&k);
        }
        inner.last_purged = Some(log_id);
        Ok(())
    }
}
