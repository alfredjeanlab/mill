use assert_cmd::Command;
use mill_support::TestCluster;

/// Pre-configured `assert_cmd::Command` for the `mill` binary.
///
/// Clears mill-specific env vars to prevent test environment leaks.
pub fn mill_cmd() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mill"));
    cmd.env_remove("MILL_TOKEN");
    cmd.env_remove("MILL_ADDRESS");
    cmd.env_remove("MILL_DATA_DIR");
    cmd
}

/// Single-node cluster for CLI binary testing.
pub struct TestServer {
    cluster: TestCluster,
}

impl TestServer {
    /// Start a single-node cluster with a random API port.
    pub async fn start() -> Self {
        Self { cluster: TestCluster::new(1).await }
    }

    /// API address as a URL string.
    pub fn address(&self) -> String {
        let addr = self.cluster.leader_api_addr();
        format!("http://127.0.0.1:{}", addr.port())
    }

    /// Auth token for API requests.
    pub fn token(&self) -> &str {
        self.cluster.api_token()
    }

    /// `mill` command pre-configured with `--address` and `--token`.
    pub fn mill(&self) -> Command {
        let mut cmd = mill_cmd();
        let addr = self.address();
        cmd.arg("--address").arg(addr);
        cmd.arg("--token").arg(self.token());
        cmd
    }

    /// Same as `mill()` with `--json`.
    pub fn mill_json(&self) -> Command {
        let mut cmd = self.mill();
        cmd.arg("--json");
        cmd
    }

    /// Shut down the cluster.
    pub async fn shutdown(self) {
        self.cluster.shutdown().await;
    }
}
