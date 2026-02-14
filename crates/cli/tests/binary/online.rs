use predicates::prelude::*;

use crate::harness::{TestServer, mill_cmd};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_json_valid() {
    let server = TestServer::start().await;

    let assert = server.mill_json().arg("status").assert().success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(json.get("cluster").is_some(), "missing 'cluster' key");
    assert!(json.get("services").is_some(), "missing 'services' key");
    assert!(json.get("tasks").is_some(), "missing 'tasks' key");
    assert!(json.get("nodes").is_some(), "missing 'nodes' key");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_json_array() {
    let server = TestServer::start().await;

    let assert = server.mill_json().arg("nodes").assert().success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(json.is_array(), "expected JSON array, got: {json}");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_human_contains_cluster() {
    let server = TestServer::start().await;

    server
        .mill()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("CLUSTER:"))
        .stdout(predicate::str::contains("NODES"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_round_trip() {
    let server = TestServer::start().await;

    // Set secret.
    server.mill().args(["secret", "set", "foo", "bar"]).assert().success();

    // Get secret — stdout should contain the value.
    server
        .mill()
        .args(["secret", "get", "foo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bar"));

    // List secrets — output should include the name.
    server
        .mill()
        .args(["secret", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("foo"));

    // Delete secret.
    server.mill().args(["secret", "delete", "foo"]).assert().success();

    // Get deleted secret — should fail.
    server.mill().args(["secret", "get", "foo"]).assert().failure().code(1);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_token_rejected() {
    let server = TestServer::start().await;

    let addr = server.address();
    mill_cmd()
        .args(["--address", &addr, "--token", "wrong-token", "status"])
        .assert()
        .failure()
        .code(1);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_token_rejected() {
    let server = TestServer::start().await;

    let addr = server.address();
    mill_cmd().args(["--address", &addr, "status"]).assert().failure().code(1);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_via_env() {
    let server = TestServer::start().await;

    let addr = server.address();
    mill_cmd()
        .env("MILL_TOKEN", server.token())
        .args(["--address", &addr, "status"])
        .assert()
        .success();

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_missing_file() {
    let server = TestServer::start().await;

    server
        .mill()
        .args(["deploy", "-f", "/nonexistent.mill"])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("/nonexistent.mill"));

    server.shutdown().await;
}
