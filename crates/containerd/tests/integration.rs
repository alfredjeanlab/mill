//! Integration tests for mill-containerd.
//!
//! These tests require a running containerd daemon and are only supported on Linux.
//! They are skipped on other platforms.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use bytesize::ByteSize;
use mill_config::{AllocId, ContainerSpec, ContainerVolume, DataDir};
use mill_containerd::Containerd;

const CONTAINERD_SOCKET: &str = "/run/containerd/containerd.sock";
const DATA_DIR: &str = "/var/lib/mill";
const TEST_IMAGE: &str = "docker.io/library/alpine:latest";

fn test_spec(alloc_id: &str, image: &str) -> ContainerSpec {
    test_spec_with_command(alloc_id, image, None)
}

fn test_spec_with_command(
    alloc_id: &str,
    image: &str,
    command: Option<Vec<String>>,
) -> ContainerSpec {
    ContainerSpec {
        alloc_id: AllocId(alloc_id.to_string()),
        kind: mill_config::AllocKind::default(),
        timeout: None,
        command,
        image: image.to_string(),
        env: HashMap::from([("TEST".to_string(), "1".to_string())]),
        port: None,
        host_port: None,
        cpu: 0.25,
        memory: ByteSize::mib(64),
        volumes: vec![],
    }
}

#[tokio::test]
async fn test_pull_and_cache_check() {
    let rt = Containerd::connect(CONTAINERD_SOCKET, DataDir::new(DATA_DIR))
        .await
        .expect("connect to containerd");

    rt.pull_image(TEST_IMAGE, None).await.expect("pull image");

    let cached = rt.is_image_cached(TEST_IMAGE).await.expect("cache check");
    assert!(cached);
}

#[tokio::test]
async fn test_run_and_wait() {
    let rt = Containerd::connect(CONTAINERD_SOCKET, DataDir::new(DATA_DIR))
        .await
        .expect("connect to containerd");

    let spec = test_spec("integration-run-wait", TEST_IMAGE);
    let handle = rt.run(&spec).await.expect("run container");
    let exit_code = handle.wait().await.expect("wait for exit");
    assert_eq!(exit_code, 0);

    rt.remove(&spec.alloc_id).await.expect("remove container");
}

#[tokio::test]
async fn test_stop_graceful() {
    let rt = Containerd::connect(CONTAINERD_SOCKET, DataDir::new(DATA_DIR))
        .await
        .expect("connect to containerd");

    // Use a long-running command so the container is alive when SIGTERM arrives.
    let spec = test_spec_with_command(
        "integration-stop",
        TEST_IMAGE,
        Some(vec!["sleep".into(), "60".into()]),
    );
    let _handle = rt.run(&spec).await.expect("run container");

    let exit_code = rt.stop(&spec.alloc_id).await.expect("stop container");
    // SIGTERM results in exit code 143 (128 + 15)
    assert_eq!(exit_code, 143, "SIGTERM should produce exit code 143 (128 + 15)");

    rt.remove(&spec.alloc_id).await.expect("remove container");
}

#[tokio::test]
async fn test_log_streaming() {
    let rt = Containerd::connect(CONTAINERD_SOCKET, DataDir::new(DATA_DIR))
        .await
        .expect("connect to containerd");

    let spec = test_spec_with_command(
        "integration-logs",
        TEST_IMAGE,
        Some(vec!["echo".into(), "hello from mill".into()]),
    );
    let handle = rt.run(&spec).await.expect("run container");

    let mut rx = handle.logs();
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for log line")
        .expect("log channel closed without sending");

    assert_eq!(line.content, "hello from mill");
    assert_eq!(line.stream, mill_containerd::LogStream::Stdout);

    let _ = rt.stop(&spec.alloc_id).await;
    rt.remove(&spec.alloc_id).await.expect("remove container");
}

#[tokio::test]
async fn test_stats_collection() {
    let rt = Containerd::connect(CONTAINERD_SOCKET, DataDir::new(DATA_DIR))
        .await
        .expect("connect to containerd");

    let spec = test_spec("integration-stats", TEST_IMAGE);
    let _handle = rt.run(&spec).await.expect("run container");

    // Give the container a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let stats = rt.stats(&spec.alloc_id).await.expect("collect stats");
    assert_eq!(stats.alloc_id, spec.alloc_id);

    let _ = rt.stop(&spec.alloc_id).await;
    rt.remove(&spec.alloc_id).await.expect("remove container");
}

#[tokio::test]
async fn test_cleanup_orphans() {
    let rt = Containerd::connect(CONTAINERD_SOCKET, DataDir::new(DATA_DIR))
        .await
        .expect("connect to containerd");

    let spec = test_spec("integration-orphan", TEST_IMAGE);
    let _handle = rt.run(&spec).await.expect("run container");

    let managed = rt.list_managed().await.expect("list managed");
    assert!(managed.iter().any(|id| id.0 == "integration-orphan"));

    rt.cleanup_all().await.expect("cleanup all");

    let managed = rt.list_managed().await.expect("list managed after cleanup");
    assert!(!managed.iter().any(|id| id.0 == "integration-orphan"));
}
