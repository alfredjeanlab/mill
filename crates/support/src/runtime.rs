use std::collections::{HashMap, HashSet};

use mill_config::{AllocId, AllocKind, ContainerSpec, ContainerStats, DataDir};
use mill_containerd::{
    BoxFuture, ContainerHandle, ContainerdError, Containers, RecoveredContainer, RegistryAuth,
    Result,
};
use parking_lot::Mutex;

/// In-memory `Containers` implementation for unit and integration tests.
///
/// Records all stop/remove/run calls so tests can assert on them.
/// Configurable via builder methods for failure injection.
pub struct TestContainers {
    state: Mutex<TestState>,
    data_dir: DataDir,
}

struct TestState {
    /// Alloc IDs returned by `list_managed()`.
    managed: Vec<AllocId>,
    /// Recovered container info keyed by alloc ID.
    recovered: HashMap<AllocId, RecoveredContainer>,
    /// Allocs where `stop()` returns `Err`.
    stop_fail: HashSet<AllocId>,
    /// Allocs where `recover_info()` returns `Err`.
    recover_fail: HashSet<AllocId>,
    /// Images that are already "cached".
    cached_images: HashSet<String>,
    /// Images where `pull_image()` returns `Err`.
    pull_fail: HashSet<String>,
    /// Whether the next `run()` returns `Err`.
    run_fail: bool,
    /// Allocs that were stopped (recorded for assertions).
    stopped: Vec<AllocId>,
    /// Allocs that were removed (recorded for assertions).
    removed: Vec<AllocId>,
    /// Allocs that were run (recorded for assertions).
    run_specs: Vec<ContainerSpec>,
}

impl TestContainers {
    pub fn new(data_dir: DataDir) -> Self {
        Self {
            state: Mutex::new(TestState {
                managed: Vec::new(),
                recovered: HashMap::new(),
                stop_fail: HashSet::new(),
                recover_fail: HashSet::new(),
                cached_images: HashSet::new(),
                pull_fail: HashSet::new(),
                run_fail: false,
                stopped: Vec::new(),
                removed: Vec::new(),
                run_specs: Vec::new(),
            }),
            data_dir,
        }
    }

    /// Set the alloc IDs returned by `list_managed()`.
    pub fn with_managed(self, ids: Vec<AllocId>) -> Self {
        self.state.lock().managed = ids;
        self
    }

    /// Add a recovered container entry.
    pub fn with_recovered(self, id: AllocId, info: RecoveredContainer) -> Self {
        self.state.lock().recovered.insert(id, info);
        self
    }

    /// Make `stop()` fail for the given alloc.
    pub fn with_stop_fail(self, id: AllocId) -> Self {
        self.state.lock().stop_fail.insert(id);
        self
    }

    /// Make `recover_info()` fail for the given alloc.
    pub fn with_recover_fail(self, id: AllocId) -> Self {
        self.state.lock().recover_fail.insert(id);
        self
    }

    /// Alloc IDs that were stopped.
    pub fn stopped(&self) -> Vec<AllocId> {
        self.state.lock().stopped.clone()
    }

    /// Alloc IDs that were removed.
    pub fn removed(&self) -> Vec<AllocId> {
        self.state.lock().removed.clone()
    }

    /// Specs that were passed to `run()`.
    pub fn run_specs(&self) -> Vec<ContainerSpec> {
        self.state.lock().run_specs.clone()
    }
}

/// Build a `RecoveredContainer` with sensible test defaults.
pub fn test_recovered(alloc_id: &str, host_port: Option<u16>) -> RecoveredContainer {
    RecoveredContainer {
        alloc_id: AllocId(alloc_id.into()),
        kind: AllocKind::Service,
        timeout: None,
        command: None,
        host_port,
        cpu: 0.25,
        memory: 268_435_456, // 256 MB
        image: "test:1".into(),
        volumes: vec![],
    }
}

impl Containers for TestContainers {
    fn list_managed(&self) -> BoxFuture<'_, Result<Vec<AllocId>>> {
        let ids = self.state.lock().managed.clone();
        Box::pin(std::future::ready(Ok(ids)))
    }

    fn stop(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<i32>> {
        let mut state = self.state.lock();
        if state.stop_fail.contains(alloc_id) {
            return Box::pin(std::future::ready(Err(ContainerdError::NotFound(
                alloc_id.0.clone(),
            ))));
        }
        state.stopped.push(alloc_id.clone());
        Box::pin(std::future::ready(Ok(0)))
    }

    fn remove(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<()>> {
        self.state.lock().removed.push(alloc_id.clone());
        Box::pin(std::future::ready(Ok(())))
    }

    fn recover_info(&self, alloc_id: &AllocId) -> BoxFuture<'_, Result<RecoveredContainer>> {
        let state = self.state.lock();
        if state.recover_fail.contains(alloc_id) {
            return Box::pin(std::future::ready(Err(ContainerdError::NotFound(
                alloc_id.0.clone(),
            ))));
        }
        let info = state.recovered.get(alloc_id).cloned().unwrap_or_else(|| RecoveredContainer {
            alloc_id: alloc_id.clone(),
            kind: AllocKind::Service,
            timeout: None,
            command: None,
            host_port: None,
            cpu: 0.25,
            memory: 268_435_456,
            image: "test:latest".into(),
            volumes: vec![],
        });
        Box::pin(std::future::ready(Ok(info)))
    }

    fn wait_exit(&self, _alloc_id: &AllocId) -> BoxFuture<'_, Result<i32>> {
        // Return a future that never completes (container stays running).
        Box::pin(std::future::pending())
    }

    fn is_image_cached(&self, image: &str) -> BoxFuture<'_, Result<bool>> {
        let cached = self.state.lock().cached_images.contains(image);
        Box::pin(std::future::ready(Ok(cached)))
    }

    fn pull_image(&self, image: &str, _auth: Option<&RegistryAuth>) -> BoxFuture<'_, Result<()>> {
        let state = self.state.lock();
        if state.pull_fail.contains(image) {
            return Box::pin(std::future::ready(Err(ContainerdError::ImagePull {
                image: image.to_string(),
                reason: "test failure".into(),
            })));
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn run(&self, spec: &ContainerSpec) -> BoxFuture<'_, Result<ContainerHandle>> {
        let mut state = self.state.lock();
        state.run_specs.push(spec.clone());
        if state.run_fail {
            state.run_fail = false;
            return Box::pin(std::future::ready(Err(ContainerdError::SpecBuild(
                "test run failure".into(),
            ))));
        }
        // TestContainers::run is not supported for reconcile tests (only runner tests
        // would need it). Return an error so tests fail clearly if called unexpectedly.
        Box::pin(std::future::ready(Err(ContainerdError::SpecBuild(
            "TestContainers::run not supported".into(),
        ))))
    }

    fn stats_all(&self) -> BoxFuture<'_, Result<Vec<ContainerStats>>> {
        Box::pin(std::future::ready(Ok(vec![])))
    }

    fn data_dir(&self) -> &DataDir {
        &self.data_dir
    }
}
