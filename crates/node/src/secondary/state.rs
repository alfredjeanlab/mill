use std::collections::HashMap;
use std::sync::Arc;

use mill_config::{AllocId, AllocStatus, ContainerSpec};
use tokio::sync::RwLock;

use super::logs::LogBuffer;
use crate::rpc::VolumeInfo;

/// Local state for a running allocation on this secondary node.
pub struct LocalAlloc {
    pub alloc_id: AllocId,
    pub spec: ContainerSpec,
    pub status: AllocStatus,
    pub host_port: Option<u16>,
    pub log_buffer: Arc<LogBuffer>,
}

/// Thread-safe map of all allocations managed by this secondary.
pub struct AllocationMap {
    inner: RwLock<HashMap<AllocId, LocalAlloc>>,
}

impl AllocationMap {
    pub fn new() -> Self {
        Self { inner: RwLock::new(HashMap::new()) }
    }

    /// Insert a new allocation. Returns error if already exists.
    pub async fn insert(&self, alloc: LocalAlloc) -> Result<(), AllocId> {
        let mut map = self.inner.write().await;
        if map.contains_key(&alloc.alloc_id) {
            return Err(alloc.alloc_id);
        }
        map.insert(alloc.alloc_id.clone(), alloc);
        Ok(())
    }

    /// Remove an allocation by ID.
    pub async fn remove(&self, alloc_id: &AllocId) -> Option<LocalAlloc> {
        self.inner.write().await.remove(alloc_id)
    }

    /// Get the log buffer for an allocation.
    pub async fn log_buffer(&self, alloc_id: &AllocId) -> Option<Arc<LogBuffer>> {
        self.inner.read().await.get(alloc_id).map(|a| Arc::clone(&a.log_buffer))
    }

    /// Get the host port for an allocation.
    pub async fn host_port(&self, alloc_id: &AllocId) -> Option<u16> {
        self.inner.read().await.get(alloc_id).and_then(|a| a.host_port)
    }

    /// Build a status map for heartbeats.
    pub async fn all_statuses(&self) -> HashMap<AllocId, AllocStatus> {
        self.inner.read().await.iter().map(|(k, v)| (k.clone(), v.status.clone())).collect()
    }

    /// Check if an allocation exists.
    pub async fn contains(&self, alloc_id: &AllocId) -> bool {
        self.inner.read().await.contains_key(alloc_id)
    }

    /// Update the status of an allocation.
    pub async fn set_status(&self, alloc_id: &AllocId, status: AllocStatus) {
        if let Some(alloc) = self.inner.write().await.get_mut(alloc_id) {
            alloc.status = status;
        }
    }

    /// Total CPU reserved by all local allocations.
    pub async fn total_reserved_cpu(&self) -> f64 {
        self.inner.read().await.values().map(|a| a.spec.cpu).sum()
    }

    /// Total memory reserved by all local allocations.
    pub async fn total_reserved_memory(&self) -> u64 {
        self.inner.read().await.values().map(|a| a.spec.memory.as_u64()).sum()
    }

    /// Collect volume info from all allocations (non-ephemeral volumes only).
    pub async fn volume_info(&self) -> Vec<VolumeInfo> {
        let map = self.inner.read().await;
        let mut volumes = Vec::new();
        for alloc in map.values() {
            for vol in &alloc.spec.volumes {
                if !vol.ephemeral {
                    volumes.push(VolumeInfo {
                        name: vol.name.clone(),
                        size_bytes: 0,
                        alloc_id: Some(alloc.alloc_id.clone()),
                    });
                }
            }
        }
        volumes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytesize::ByteSize;
    use std::collections::HashMap as StdMap;

    fn make_alloc(id: &str) -> LocalAlloc {
        LocalAlloc {
            alloc_id: AllocId(id.into()),
            spec: ContainerSpec {
                alloc_id: AllocId(id.into()),
                kind: mill_config::AllocKind::Service,
                timeout: None,
                command: None,
                image: "test:latest".into(),
                env: StdMap::new(),
                port: Some(8080),
                host_port: Some(50000),
                cpu: 1.0,
                memory: ByteSize::mib(256),
                volumes: vec![],
            },
            status: AllocStatus::Running,
            host_port: Some(50000),
            log_buffer: Arc::new(LogBuffer::new()),
        }
    }

    #[tokio::test]
    async fn insert_and_remove() {
        let map = AllocationMap::new();
        assert!(map.insert(make_alloc("a")).await.is_ok());
        assert!(map.contains(&AllocId("a".into())).await);
        assert!(map.remove(&AllocId("a".into())).await.is_some());
        assert!(!map.contains(&AllocId("a".into())).await);
    }

    #[tokio::test]
    async fn insert_duplicate_fails() {
        let map = AllocationMap::new();
        assert!(map.insert(make_alloc("a")).await.is_ok());
        assert!(map.insert(make_alloc("a")).await.is_err());
    }

    #[tokio::test]
    async fn total_reserved_cpu_empty() {
        let map = AllocationMap::new();
        assert_eq!(map.total_reserved_cpu().await, 0.0);
    }

    #[tokio::test]
    async fn volume_info_excludes_ephemeral() {
        use mill_config::ContainerVolume;

        let map = AllocationMap::new();
        let mut alloc = make_alloc("a");
        alloc.spec.volumes = vec![
            ContainerVolume { name: "data".into(), path: "/data".into(), ephemeral: false },
            ContainerVolume { name: "tmp".into(), path: "/tmp".into(), ephemeral: true },
        ];
        map.insert(alloc).await.ok();

        let vols = map.volume_info().await;
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].name, "data");
        assert_eq!(vols[0].alloc_id, Some(AllocId("a".into())));
        assert_eq!(vols[0].size_bytes, 0);
    }

    #[tokio::test]
    async fn total_reserved_cpu_sums_allocs() {
        let map = AllocationMap::new();
        let mut a1 = make_alloc("a");
        a1.spec.cpu = 1.5;
        let mut a2 = make_alloc("b");
        a2.spec.cpu = 2.0;
        map.insert(a1).await.ok();
        map.insert(a2).await.ok();
        assert_eq!(map.total_reserved_cpu().await, 3.5);
    }

    #[tokio::test]
    async fn all_statuses_returns_current_state() {
        let map = AllocationMap::new();
        map.insert(make_alloc("a")).await.ok();
        map.insert(make_alloc("b")).await.ok();

        let statuses = map.all_statuses().await;
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[&AllocId("a".into())], AllocStatus::Running);
        assert_eq!(statuses[&AllocId("b".into())], AllocStatus::Running);
    }

    #[tokio::test]
    async fn set_status_updates_alloc() {
        let map = AllocationMap::new();
        map.insert(make_alloc("a")).await.ok();

        map.set_status(&AllocId("a".into()), AllocStatus::Stopped { exit_code: 0 }).await;
        let statuses = map.all_statuses().await;
        assert_eq!(statuses[&AllocId("a".into())], AllocStatus::Stopped { exit_code: 0 });
    }

    #[tokio::test]
    async fn set_status_noop_for_missing_alloc() {
        let map = AllocationMap::new();
        // Should not panic
        map.set_status(&AllocId("missing".into()), AllocStatus::Failed { reason: "gone".into() })
            .await;
    }

    #[tokio::test]
    async fn total_reserved_memory_empty() {
        let map = AllocationMap::new();
        assert_eq!(map.total_reserved_memory().await, 0);
    }

    #[tokio::test]
    async fn total_reserved_memory_sums_allocs() {
        let map = AllocationMap::new();
        let mut a1 = make_alloc("a");
        a1.spec.memory = ByteSize::mib(256);
        let mut a2 = make_alloc("b");
        a2.spec.memory = ByteSize::mib(512);
        map.insert(a1).await.ok();
        map.insert(a2).await.ok();
        assert_eq!(
            map.total_reserved_memory().await,
            ByteSize::mib(256).as_u64() + ByteSize::mib(512).as_u64()
        );
    }
}
