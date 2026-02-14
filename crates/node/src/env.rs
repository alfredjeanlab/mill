use std::sync::OnceLock;
use std::time::Duration;

pub fn heartbeat_interval() -> Duration {
    *HEARTBEAT_INTERVAL.get_or_init(|| read_ms("MILL_HEARTBEAT_INTERVAL_MS", 5000))
}

pub fn metrics_poll_interval() -> Duration {
    *METRICS_POLL_INTERVAL.get_or_init(|| read_ms("MILL_METRICS_POLL_INTERVAL_MS", 10000))
}

pub fn watchdog_tick() -> Duration {
    *WATCHDOG_TICK.get_or_init(|| read_ms("MILL_WATCHDOG_TICK_MS", 1000))
}

pub fn heartbeat_timeout() -> Duration {
    *HEARTBEAT_TIMEOUT.get_or_init(|| read_ms("MILL_HEARTBEAT_TIMEOUT_MS", 15000))
}

pub fn restart_backoff() -> &'static [Duration] {
    RESTART_BACKOFF.get_or_init(|| {
        vec![
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ]
    })
}

pub fn health_timeout() -> Duration {
    *HEALTH_TIMEOUT.get_or_init(|| read_ms("MILL_HEALTH_TIMEOUT_MS", 30000))
}

pub fn deploy_timeout() -> Duration {
    *DEPLOY_TIMEOUT.get_or_init(|| read_ms("MILL_DEPLOY_TIMEOUT_MS", 60000))
}

pub fn poll_address_timeout() -> Duration {
    *POLL_ADDRESS_TIMEOUT.get_or_init(|| read_ms("MILL_POLL_ADDRESS_TIMEOUT_MS", 10000))
}

pub fn health_check_http_timeout() -> Duration {
    *HEALTH_CHECK_HTTP_TIMEOUT.get_or_init(|| read_ms("MILL_HEALTH_CHECK_HTTP_TIMEOUT_MS", 5000))
}

/// Interval between TCP connect attempts in the readiness probe.
/// 100ms is short enough to avoid adding meaningful deploy latency (worst case
/// one interval after the port opens) while a failed connect() returns almost
/// instantly so the per-attempt cost is negligible.
pub fn tcp_ready_poll_interval() -> Duration {
    *TCP_READY_POLL_INTERVAL.get_or_init(|| read_ms("MILL_TCP_READY_POLL_INTERVAL_MS", 100))
}

pub fn rpc_connect_timeout() -> Duration {
    *RPC_CONNECT_TIMEOUT.get_or_init(|| read_ms("MILL_RPC_CONNECT_TIMEOUT_MS", 5000))
}

pub fn api_client_timeout() -> Duration {
    *API_CLIENT_TIMEOUT.get_or_init(|| read_ms("MILL_API_CLIENT_TIMEOUT_MS", 30000))
}

pub fn metadata_timeout() -> Duration {
    *METADATA_TIMEOUT.get_or_init(|| read_ms("MILL_METADATA_TIMEOUT_MS", 2000))
}

pub fn snapshot_threshold() -> u64 {
    *SNAPSHOT_THRESHOLD.get_or_init(|| read_u64("MILL_SNAPSHOT_THRESHOLD", 5000))
}

pub fn snapshot_log_keep() -> u64 {
    *SNAPSHOT_LOG_KEEP.get_or_init(|| read_u64("MILL_SNAPSHOT_LOG_KEEP", 1000))
}

/// Pre-populate all OnceLocks with fast test values.
///
/// Must be called before any accessor is used. Uses `OnceLock::get_or_init`
/// semantics: if called first, env vars are never read.
pub fn init_test_defaults() {
    HEARTBEAT_INTERVAL.get_or_init(|| Duration::from_millis(100));
    METRICS_POLL_INTERVAL.get_or_init(|| Duration::from_secs(2));
    WATCHDOG_TICK.get_or_init(|| Duration::from_millis(100));
    HEARTBEAT_TIMEOUT.get_or_init(|| Duration::from_millis(300));
    RESTART_BACKOFF.get_or_init(|| {
        vec![
            Duration::from_millis(200),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ]
    });
    HEALTH_TIMEOUT.get_or_init(|| Duration::from_secs(1));
    DEPLOY_TIMEOUT.get_or_init(|| Duration::from_secs(3));
    POLL_ADDRESS_TIMEOUT.get_or_init(|| Duration::from_secs(1));
    HEALTH_CHECK_HTTP_TIMEOUT.get_or_init(|| Duration::from_secs(2));
    TCP_READY_POLL_INTERVAL.get_or_init(|| Duration::from_millis(50));
    RPC_CONNECT_TIMEOUT.get_or_init(|| Duration::from_secs(1));
    API_CLIENT_TIMEOUT.get_or_init(|| Duration::from_secs(5));
    METADATA_TIMEOUT.get_or_init(|| Duration::from_millis(100));
    SNAPSHOT_THRESHOLD.get_or_init(|| 1);
    SNAPSHOT_LOG_KEEP.get_or_init(|| 0);
}

static HEARTBEAT_INTERVAL: OnceLock<Duration> = OnceLock::new();
static METRICS_POLL_INTERVAL: OnceLock<Duration> = OnceLock::new();
static WATCHDOG_TICK: OnceLock<Duration> = OnceLock::new();
static HEARTBEAT_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static RESTART_BACKOFF: OnceLock<Vec<Duration>> = OnceLock::new();
static HEALTH_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static DEPLOY_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static POLL_ADDRESS_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static HEALTH_CHECK_HTTP_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static TCP_READY_POLL_INTERVAL: OnceLock<Duration> = OnceLock::new();
static RPC_CONNECT_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static API_CLIENT_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static METADATA_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static SNAPSHOT_THRESHOLD: OnceLock<u64> = OnceLock::new();
static SNAPSHOT_LOG_KEEP: OnceLock<u64> = OnceLock::new();

fn read_ms(var: &str, default: u64) -> Duration {
    match std::env::var(var) {
        Ok(val) => Duration::from_millis(val.parse().unwrap_or(default)),
        Err(_) => Duration::from_millis(default),
    }
}

fn read_u64(var: &str, default: u64) -> u64 {
    match std::env::var(var) {
        Ok(val) => val.parse().unwrap_or(default),
        Err(_) => default,
    }
}
