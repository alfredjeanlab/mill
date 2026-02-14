use std::io::Cursor;

use crate::fsm::{Command, Response};

openraft::declare_raft_types!(
    pub TypeConfig:
        D = Command,
        R = Response,
        NodeId = u64,
        Node = openraft::BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = openraft::TokioRuntime,
);

pub type Entry = openraft::Entry<TypeConfig>;
pub type Raft = openraft::Raft<TypeConfig>;
pub type LogId = openraft::LogId<u64>;
pub type Vote = openraft::Vote<u64>;
pub type Snapshot = openraft::Snapshot<TypeConfig>;
pub type SnapshotMeta = openraft::SnapshotMeta<u64, openraft::BasicNode>;
pub type StoredMembership = openraft::StoredMembership<u64, openraft::BasicNode>;
pub type LogState = openraft::LogState<TypeConfig>;
