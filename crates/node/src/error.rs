use mill_containerd::ContainerdError;
use mill_net::NetError;
use mill_raft::error::RaftError;

/// Errors from the mill-node runtime.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Containerd(#[from] ContainerdError),

    #[error(transparent)]
    Net(#[from] NetError),

    #[error(transparent)]
    Raft(#[from] RaftError),

    #[error("failed to bind {address}")]
    Bind { address: String, source: std::io::Error },

    #[error("no ports available")]
    PortExhaustion,

    #[error("allocation not found: {0}")]
    AllocNotFound(String),

    #[error("service not found: {0}")]
    ServiceNotFound(String),

    #[error("allocation {alloc_id} is not a task")]
    NotATask { alloc_id: String },

    #[error("raft rejected: {0}")]
    RaftRejected(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("allocation already exists: {0}")]
    AllocAlreadyExists(String),

    #[error("node unreachable: {0}")]
    NodeUnreachable(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("channel closed")]
    ChannelClosed,

    #[error("deploy failed for {service}: {reason}")]
    DeployFailed { service: String, reason: String },
}

pub type Result<T> = std::result::Result<T, NodeError>;
