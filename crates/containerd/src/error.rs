use std::io;

/// Errors from the containerd runtime adapter.
#[derive(Debug, thiserror::Error)]
pub enum ContainerdError {
    #[error("failed to connect to containerd at {path}: {source}")]
    Connect { path: String, source: tonic::transport::Error },

    #[error("containerd gRPC error: {0}")]
    Grpc(#[source] Box<tonic::Status>),

    #[error("failed to pull image {image}: {reason}")]
    ImagePull { image: String, reason: String },

    #[error("container not found: mill-{0}")]
    NotFound(String),

    #[error("container already exists: mill-{0}")]
    AlreadyExists(String),

    #[error("failed to build OCI spec: {0}")]
    SpecBuild(String),

    #[error("log I/O error: {0}")]
    LogIo(#[source] io::Error),

    #[error("failed to decode metrics: {0}")]
    MetricsDecode(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl From<tonic::Status> for ContainerdError {
    fn from(status: tonic::Status) -> Self {
        Self::Grpc(Box::new(status))
    }
}

pub type Result<T> = std::result::Result<T, ContainerdError>;
