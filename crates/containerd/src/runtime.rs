use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mill_config::{AllocId, ContainerSpec, ContainerStats, DataDir};

use crate::{ContainerHandle, Containerd, RecoveredContainer, RegistryAuth, Result};

/// Boxed future for async trait methods that need `dyn` dispatch.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Container runtime interface used by the secondary (reconcile, runner, metrics).
///
/// Production code uses `Containerd` (containerd gRPC).
/// Tests substitute `TestContainers` from `mill-support`.
pub trait Containers: Send + Sync {
    fn list_managed(&self) -> BoxFuture<'_, Result<Vec<AllocId>>>;
    fn stop(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<i32>>;
    fn remove(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<()>>;
    fn recover_info(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<RecoveredContainer>>;
    fn wait_exit(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<i32>>;
    fn is_image_cached(&self, image: &str) -> BoxFuture<'_, Result<bool>>;
    fn pull_image(&self, image: &str, auth: Option<&RegistryAuth>) -> BoxFuture<'_, Result<()>>;
    fn run(&self, spec: &ContainerSpec) -> BoxFuture<'_, Result<ContainerHandle>>;
    fn stats_all(&self) -> BoxFuture<'_, Result<Vec<ContainerStats>>>;
    fn data_dir(&self) -> &DataDir;
}

impl Containers for Containerd {
    fn list_managed(&self) -> BoxFuture<'_, Result<Vec<AllocId>>> {
        Box::pin(Containerd::list_managed(self))
    }

    fn stop(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<i32>> {
        let alloc_id = alloc_id.clone();
        Box::pin(async move { Containerd::stop(self, &alloc_id).await })
    }

    fn remove(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<()>> {
        let alloc_id = alloc_id.clone();
        Box::pin(async move { Containerd::remove(self, &alloc_id).await })
    }

    fn recover_info(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<RecoveredContainer>> {
        let alloc_id = alloc_id.clone();
        Box::pin(async move { Containerd::recover_info(self, &alloc_id).await })
    }

    fn wait_exit(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<i32>> {
        let alloc_id = alloc_id.clone();
        Box::pin(async move { Containerd::wait_exit(self, &alloc_id).await })
    }

    fn is_image_cached(&self, image: &str) -> BoxFuture<'_, Result<bool>> {
        let image = image.to_string();
        Box::pin(async move { Containerd::is_image_cached(self, &image).await })
    }

    fn pull_image(&self, image: &str, auth: Option<&RegistryAuth>) -> BoxFuture<'_, Result<()>> {
        let image = image.to_string();
        let auth = auth.cloned();
        Box::pin(async move { Containerd::pull_image(self, &image, auth.as_ref()).await })
    }

    fn run(&self, spec: &ContainerSpec) -> BoxFuture<'_, Result<ContainerHandle>> {
        let spec = spec.clone();
        Box::pin(async move { Containerd::run(self, &spec).await })
    }

    fn stats_all(&self) -> BoxFuture<'_, Result<Vec<ContainerStats>>> {
        Box::pin(Containerd::stats_all(self))
    }

    fn data_dir(&self) -> &DataDir {
        Containerd::data_dir(self)
    }
}

/// Helper: wrap a `Containerd` into an `Arc<dyn Containers>`.
impl Containerd {
    pub fn into_runtime(self) -> Arc<dyn Containers> {
        Arc::new(self)
    }
}
