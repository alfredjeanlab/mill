use std::collections::HashMap;

use bytesize::ByteSize;
use parking_lot::Mutex;

use mill_node::storage::{BoxFuture, StorageError, Volumes};

/// In-memory volume driver for integration tests.
pub struct TestVolumes {
    inner: Mutex<MemoryVolumeInner>,
}

struct MemoryVolumeInner {
    volumes: HashMap<String, MemoryVolume>,
    /// If set, the next `create()` call will fail with this reason.
    fail_next_create: Option<String>,
}

struct MemoryVolume {
    _cloud_id: String,
    _size: ByteSize,
    attached_to: Option<String>,
}

impl TestVolumes {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MemoryVolumeInner {
                volumes: HashMap::new(),
                fail_next_create: None,
            }),
        }
    }

    /// Make the next `create()` call fail with `StorageError::Api(reason)`.
    pub fn fail_next_create(&self, reason: impl Into<String>) {
        self.inner.lock().fail_next_create = Some(reason.into());
    }

    /// Check which instance a volume is attached to (if any).
    pub fn attached_to(&self, name: &str) -> Option<String> {
        self.inner.lock().volumes.get(name).and_then(|v| v.attached_to.clone())
    }

    /// Check whether a volume exists in the driver.
    pub fn exists(&self, name: &str) -> bool {
        self.inner.lock().volumes.contains_key(name)
    }
}

impl Volumes for TestVolumes {
    fn create(&self, name: &str, size: ByteSize) -> BoxFuture<'_, Result<String, StorageError>> {
        let mut inner = self.inner.lock();
        if let Some(reason) = inner.fail_next_create.take() {
            return Box::pin(std::future::ready(Err(StorageError::Api(reason))));
        }
        let result = if inner.volumes.contains_key(name) {
            Err(StorageError::AlreadyExists(name.into()))
        } else {
            let cloud_id = format!("mem-vol-{name}");
            inner.volumes.insert(
                name.into(),
                MemoryVolume { _cloud_id: cloud_id.clone(), _size: size, attached_to: None },
            );
            Ok(cloud_id)
        };
        Box::pin(std::future::ready(result))
    }

    fn destroy(&self, name: &str) -> BoxFuture<'_, Result<(), StorageError>> {
        let mut inner = self.inner.lock();
        let result = if inner.volumes.remove(name).is_none() {
            Err(StorageError::NotFound(name.into()))
        } else {
            Ok(())
        };
        Box::pin(std::future::ready(result))
    }

    fn attach(&self, name: &str, instance_id: &str) -> BoxFuture<'_, Result<(), StorageError>> {
        let mut inner = self.inner.lock();
        let result = match inner.volumes.get_mut(name) {
            Some(vol) => {
                vol.attached_to = Some(instance_id.into());
                Ok(())
            }
            None => Err(StorageError::NotFound(name.into())),
        };
        Box::pin(std::future::ready(result))
    }

    fn detach(&self, name: &str) -> BoxFuture<'_, Result<(), StorageError>> {
        let mut inner = self.inner.lock();
        let result = match inner.volumes.get_mut(name) {
            Some(vol) => {
                vol.attached_to = None;
                Ok(())
            }
            None => Err(StorageError::NotFound(name.into())),
        };
        Box::pin(std::future::ready(result))
    }
}
