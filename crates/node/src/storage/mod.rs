pub mod digitalocean;

use std::future::Future;
use std::pin::Pin;

use bytesize::ByteSize;

/// Boxed future for async trait methods that need `dyn` dispatch.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors from volume storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("volume not found: {0}")]
    NotFound(String),

    #[error("volume already exists: {0}")]
    AlreadyExists(String),

    #[error("volume is attached: {0}")]
    Attached(String),

    #[error("api error: {0}")]
    Api(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// Information about a cloud volume.
#[derive(Debug, Clone)]
pub struct CloudVolumeInfo {
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
}

/// Interface for managing persistent block storage volumes.
pub trait Volumes: Send + Sync {
    /// Create a new volume. Returns the cloud-provider volume ID.
    fn create(&self, name: &str, size: ByteSize) -> BoxFuture<'_, Result<String, StorageError>>;

    /// Destroy a volume by name.
    fn destroy(&self, name: &str) -> BoxFuture<'_, Result<(), StorageError>>;

    /// Attach a volume to a node (by droplet/instance ID).
    fn attach(&self, name: &str, instance_id: &str) -> BoxFuture<'_, Result<(), StorageError>>;

    /// Detach a volume from its current node.
    fn detach(&self, name: &str) -> BoxFuture<'_, Result<(), StorageError>>;
}
