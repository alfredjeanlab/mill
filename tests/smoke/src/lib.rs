// Smoke tests: end-to-end workflow scenarios matching docs/workflows/*.md
//
// These tests spawn real `mill` binary processes and exercise full workflows.
// They require the mill-harness environment (Lima VM or Linux with namespaces).
// Unwrap/panic are appropriate in test harness code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub use mill_config::{SpawnTaskRequest, SpawnTaskResponse};
pub use mill_config::{poll, poll_async};
pub use mill_support::Api;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Harness gate
// ---------------------------------------------------------------------------

/// Panics if `MILL_HARNESS` is not set. Place at the top of every smoke test
/// so that `cargo test --all` fails clearly rather than hanging or crashing
/// on macOS where WireGuard and containerd are unavailable.
#[macro_export]
macro_rules! require_harness {
    () => {
        if std::env::var("MILL_HARNESS").is_err() {
            panic!(
                "smoke tests require the mill-harness environment.\n\
                 Run: make smoke\n\
                 See: harness/mill-harness --help"
            );
        }
    };
}

// ---------------------------------------------------------------------------
// Node index counter
// ---------------------------------------------------------------------------

static NODE_INDEX: AtomicU32 = AtomicU32::new(0);

/// Reset the per-test node counter. Call at the top of each test before
/// creating any `MillNode` instances so namespace indices start at 0.
pub fn reset_node_counter() {
    NODE_INDEX.store(0, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Binary location
// ---------------------------------------------------------------------------

fn is_harness() -> bool {
    std::env::var("MILL_HARNESS").is_ok()
}

/// Locate the `mill` binary in the workspace target directory.
pub fn mill_bin() -> PathBuf {
    if is_harness() {
        // In harness mode, the binary is built for Linux in target-linux/.
        let target_dir =
            std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| {
                let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                manifest.parent().unwrap().parent().unwrap().join("target-linux")
            });
        for profile in ["debug", "release"] {
            let bin = target_dir.join(profile).join("mill");
            if bin.exists() {
                return bin;
            }
        }
        panic!("mill binary not found in {target_dir:?}. Run `mill-harness build` first.");
    } else {
        let target_dir =
            std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| {
                let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                manifest.parent().unwrap().parent().unwrap().join("target")
            });
        for profile in ["debug", "release"] {
            let bin = target_dir.join(profile).join("mill");
            if bin.exists() {
                return bin;
            }
        }
        panic!("mill binary not found in {target_dir:?}. Run `cargo build -p mill` first.");
    }
}

// ---------------------------------------------------------------------------
// Data directory
// ---------------------------------------------------------------------------

/// Data directory for a mill node. In harness mode, uses a fixed path under
/// /var/lib/mill-test/ so the namespace setup script can find it. Otherwise,
/// uses a temp directory that is auto-cleaned on drop.
enum MillDataDir {
    Harness(PathBuf),
    Temp(tempfile::TempDir),
}

impl MillDataDir {
    fn harness(index: u32) -> Self {
        let path = PathBuf::from(format!("/var/lib/mill-test/node-{index}"));
        // Clean and recreate so each test starts fresh.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create harness data dir");
        Self::Harness(path)
    }

    fn temp() -> Self {
        Self::Temp(tempfile::TempDir::new().unwrap())
    }

    fn path(&self) -> &std::path::Path {
        match self {
            Self::Harness(p) => p,
            Self::Temp(t) => t.path(),
        }
    }
}

// ---------------------------------------------------------------------------
// Mill daemon process
// ---------------------------------------------------------------------------

/// Environment variables that speed up timeouts for testing.
pub fn fast_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("MILL_HEARTBEAT_INTERVAL_MS", "500"),
        ("MILL_HEARTBEAT_TIMEOUT_MS", "2000"),
        ("MILL_WATCHDOG_TICK_MS", "500"),
        ("MILL_DEPLOY_TIMEOUT_MS", "10000"),
        ("MILL_HEALTH_TIMEOUT_MS", "5000"),
        ("MILL_METADATA_TIMEOUT_MS", "100"),
        ("MILL_RPC_CONNECT_TIMEOUT_MS", "2000"),
    ]
}

/// Spawn a background thread that drains a child's stderr into a shared buffer.
fn drain_stderr(stderr: std::process::ChildStderr) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let w = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            w.lock().unwrap().push(line);
        }
    });
    lines
}

/// Wait for a marker string in stderr, panicking on early exit or timeout.
async fn wait_for_ready(child: &mut Child, lines: &Arc<Mutex<Vec<String>>>, marker: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let output = lines.lock().unwrap().join("\n");
            panic!("mill exited with {status} before ready.\nStderr:\n{output}");
        }
        if lines.lock().unwrap().iter().any(|l| l.contains(marker)) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let output = lines.lock().unwrap().join("\n");
            panic!("mill did not become ready within 30s.\nStderr:\n{output}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// A running `mill` daemon process (from `mill init` or `mill join`).
pub struct MillNode {
    child: Child,
    _data_dir: MillDataDir,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    namespace_index: Option<u32>,
    pub api_addr: SocketAddr,
    pub token: String,
}

impl MillNode {
    /// Bootstrap a new single-node cluster.
    ///
    /// In harness mode: spawns inside a network namespace with the namespace's
    /// advertise address. Otherwise: spawns directly with 127.0.0.1.
    pub async fn init(token: &str) -> Self {
        if is_harness() { Self::init_harness(token).await } else { Self::init_direct(token).await }
    }

    /// Join an existing cluster.
    ///
    /// In harness mode: spawns inside a network namespace. Otherwise: direct.
    pub async fn join(leader_addr: &str, token: &str) -> Self {
        if is_harness() {
            Self::join_harness(leader_addr, token).await
        } else {
            Self::join_direct(leader_addr, token).await
        }
    }

    /// Bootstrap a new single-node cluster with additional env vars.
    pub async fn init_with(token: &str, extra_env: &[(&str, &str)]) -> Self {
        if is_harness() {
            Self::init_harness_with(token, extra_env).await
        } else {
            Self::init_direct_with(token, extra_env).await
        }
    }

    /// Join an existing cluster with additional env vars.
    pub async fn join_with(leader_addr: &str, token: &str, extra_env: &[(&str, &str)]) -> Self {
        if is_harness() {
            Self::join_harness_with(leader_addr, token, extra_env).await
        } else {
            Self::join_direct_with(leader_addr, token, extra_env).await
        }
    }

    // -- Harness mode (network namespaces) --

    async fn init_harness(token: &str) -> Self {
        let idx = NODE_INDEX.fetch_add(1, Ordering::SeqCst);
        let ns = format!("mill-{idx}");
        let advertise = format!("172.18.0.{}", 10 + idx);
        let data_dir = MillDataDir::harness(idx);

        let mut child = Command::new("ip")
            .args(["netns", "exec", &ns])
            .arg(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["init", "--token", token, "--advertise", &advertise])
            .envs(fast_env())
            .env("MILL_NETNS_PATH", format!("/var/run/netns/{ns}"))
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn mill init in namespace");

        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "To join another node").await;

        let api_addr = Self::extract_api_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: Some(idx),
            api_addr,
            token: token.into(),
        }
    }

    async fn join_harness(leader_addr: &str, token: &str) -> Self {
        let idx = NODE_INDEX.fetch_add(1, Ordering::SeqCst);
        let ns = format!("mill-{idx}");
        let advertise = format!("172.18.0.{}", 10 + idx);
        let data_dir = MillDataDir::harness(idx);

        let mut child = Command::new("ip")
            .args(["netns", "exec", &ns])
            .arg(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["join", leader_addr, "--token", token, "--advertise", &advertise])
            .envs(fast_env())
            .env("MILL_NETNS_PATH", format!("/var/run/netns/{ns}"))
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn mill join in namespace");

        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "Joined cluster").await;

        let api_addr = Self::extract_tunnel_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: Some(idx),
            api_addr,
            token: token.into(),
        }
    }

    async fn init_harness_with(token: &str, extra_env: &[(&str, &str)]) -> Self {
        let idx = NODE_INDEX.fetch_add(1, Ordering::SeqCst);
        let ns = format!("mill-{idx}");
        let advertise = format!("172.18.0.{}", 10 + idx);
        let data_dir = MillDataDir::harness(idx);

        let mut cmd = Command::new("ip");
        cmd.args(["netns", "exec", &ns])
            .arg(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["init", "--token", token, "--advertise", &advertise])
            .envs(fast_env())
            .env("MILL_NETNS_PATH", format!("/var/run/netns/{ns}"))
            .envs(extra_env.iter().copied())
            .stderr(Stdio::piped())
            .stdout(Stdio::null());

        let mut child = cmd.spawn().expect("spawn mill init in namespace");
        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "To join another node").await;

        let api_addr = Self::extract_api_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: Some(idx),
            api_addr,
            token: token.into(),
        }
    }

    async fn join_harness_with(leader_addr: &str, token: &str, extra_env: &[(&str, &str)]) -> Self {
        let idx = NODE_INDEX.fetch_add(1, Ordering::SeqCst);
        let ns = format!("mill-{idx}");
        let advertise = format!("172.18.0.{}", 10 + idx);
        let data_dir = MillDataDir::harness(idx);

        let mut cmd = Command::new("ip");
        cmd.args(["netns", "exec", &ns])
            .arg(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["join", leader_addr, "--token", token, "--advertise", &advertise])
            .envs(fast_env())
            .env("MILL_NETNS_PATH", format!("/var/run/netns/{ns}"))
            .envs(extra_env.iter().copied())
            .stderr(Stdio::piped())
            .stdout(Stdio::null());

        let mut child = cmd.spawn().expect("spawn mill join in namespace");
        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "Joined cluster").await;

        let api_addr = Self::extract_tunnel_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: Some(idx),
            api_addr,
            token: token.into(),
        }
    }

    // -- Direct mode (no namespace, original behavior) --

    async fn init_direct(token: &str) -> Self {
        let data_dir = MillDataDir::temp();
        let mut child = Command::new(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["init", "--token", token, "--advertise", "127.0.0.1"])
            .envs(fast_env())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn mill init");

        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "To join another node").await;

        let api_addr = Self::extract_api_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: None,
            api_addr,
            token: token.into(),
        }
    }

    async fn join_direct(leader_addr: &str, token: &str) -> Self {
        let data_dir = MillDataDir::temp();
        let mut child = Command::new(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["join", leader_addr, "--token", token, "--advertise", "127.0.0.1"])
            .envs(fast_env())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn mill join");

        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "Joined cluster").await;

        let api_addr = Self::extract_tunnel_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: None,
            api_addr,
            token: token.into(),
        }
    }

    async fn init_direct_with(token: &str, extra_env: &[(&str, &str)]) -> Self {
        let data_dir = MillDataDir::temp();
        let mut child = Command::new(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["init", "--token", token, "--advertise", "127.0.0.1"])
            .envs(fast_env())
            .envs(extra_env.iter().copied())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn mill init");

        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "To join another node").await;

        let api_addr = Self::extract_api_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: None,
            api_addr,
            token: token.into(),
        }
    }

    async fn join_direct_with(leader_addr: &str, token: &str, extra_env: &[(&str, &str)]) -> Self {
        let data_dir = MillDataDir::temp();
        let mut child = Command::new(mill_bin())
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["join", leader_addr, "--token", token, "--advertise", "127.0.0.1"])
            .envs(fast_env())
            .envs(extra_env.iter().copied())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn mill join");

        let stderr_lines = drain_stderr(child.stderr.take().unwrap());
        wait_for_ready(&mut child, &stderr_lines, "Joined cluster").await;

        let api_addr = Self::extract_tunnel_addr(&stderr_lines);
        MillNode {
            child,
            _data_dir: data_dir,
            stderr_lines,
            namespace_index: None,
            api_addr,
            token: token.into(),
        }
    }

    // -- Public API --

    /// HTTP API client for this node.
    pub fn api(&self) -> Api {
        Api::new(self.api_addr, &self.token)
    }

    /// Pre-configured `mill` CLI command with `--address` and `--token`.
    pub fn mill_cmd(&self) -> Command {
        let mut cmd = Command::new(mill_bin());
        cmd.arg("--address").arg(format!("http://{}", self.api_addr));
        cmd.arg("--token").arg(&self.token);
        cmd
    }

    /// Collected stderr output from the daemon.
    pub fn stderr_output(&self) -> Vec<String> {
        self.stderr_lines.lock().unwrap().clone()
    }

    /// Check if the daemon is still running.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Resolve a DNS name via the node's local DNS server.
    ///
    /// In harness mode: runs `dig` inside the node's network namespace.
    /// In direct mode: always returns `false` (DNS is inaccessible outside
    /// the namespace).
    pub async fn dns_resolve(&self, name: &str) -> bool {
        let Some(idx) = self.namespace_index else {
            return false;
        };
        let ns = format!("mill-{idx}");
        let output = Command::new("ip")
            .args(["netns", "exec", &ns, "dig", "+short", "+time=2", "@127.0.0.1", name])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                // dig +short returns IP addresses on success, empty on NXDOMAIN
                out.status.success() && !stdout.trim().is_empty()
            }
            Err(_) => false,
        }
    }

    // -- Address extraction --

    fn extract_api_addr(lines: &Arc<Mutex<Vec<String>>>) -> SocketAddr {
        let lines = lines.lock().unwrap();
        lines
            .iter()
            .find_map(|l| {
                l.split("API:")
                    .nth(1)
                    .map(|s| s.trim())
                    .and_then(|s| s.strip_prefix("http://"))
                    .and_then(|s| s.parse().ok())
            })
            .expect("API address not found in daemon stderr")
    }

    fn extract_tunnel_addr(lines: &Arc<Mutex<Vec<String>>>) -> SocketAddr {
        let lines = lines.lock().unwrap();
        if let Some(addr) = lines.iter().find_map(|l| {
            l.split("API:")
                .nth(1)
                .map(|s| s.trim())
                .and_then(|s| s.strip_prefix("http://"))
                .and_then(|s| s.parse().ok())
        }) {
            return addr;
        }
        lines
            .iter()
            .find_map(|l| {
                l.split("Tunnel IP:")
                    .nth(1)
                    .map(|s| s.trim())
                    .and_then(|ip| format!("{ip}:4400").parse().ok())
            })
            .expect("Tunnel IP not found in daemon stderr")
    }
}

impl Drop for MillNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();

        // In harness mode, clean up the WireGuard interface inside the namespace
        // so the next test doesn't collide.
        if let Some(idx) = self.namespace_index {
            let _ = Command::new("ip")
                .args(["netns", "exec", &format!("mill-{idx}"), "ip", "link", "del", "mill0"])
                .status();
        }
    }
}
