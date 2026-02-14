use predicates::prelude::*;

use crate::harness::mill_cmd;

#[test]
fn no_args_shows_help() {
    mill_cmd().assert().failure().code(2).stderr(predicate::str::contains("Usage"));
}

#[test]
fn version_flag() {
    mill_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_flag() {
    mill_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("deploy"))
        .stdout(predicate::str::contains("nodes"));
}

#[test]
fn spawn_missing_args() {
    mill_cmd()
        .arg("spawn")
        .assert()
        .failure()
        .code(64)
        .stderr(predicate::str::contains("either a task template name or --image is required"));
}

#[test]
fn unknown_subcommand() {
    mill_cmd().arg("bogus").assert().failure().code(2);
}

#[test]
fn unknown_flag() {
    mill_cmd().args(["status", "--bogus"]).assert().failure().code(2);
}

#[test]
fn status_connection_refused() {
    mill_cmd()
        .args(["--address", "http://127.0.0.1:1", "status"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("connection refused"));
}

#[test]
fn nodes_connection_refused() {
    mill_cmd()
        .args(["--address", "http://127.0.0.1:1", "nodes"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("connection refused"));
}
