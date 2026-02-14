use std::net::SocketAddr;
use std::time::Duration;

use mill_config::HealthCheck;

/// Readiness probes for deploys: HTTP health checks or TCP connect.
pub struct HealthPoller {
    client: reqwest::Client,
}

impl Default for HealthPoller {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthPoller {
    pub fn new() -> Self {
        // reqwest::Client::builder only fails for invalid TLS config, which
        // we don't customize. unwrap_or_default gives a usable fallback.
        let client = reqwest::Client::builder()
            .timeout(crate::env::health_check_http_timeout())
            .build()
            .unwrap_or_default();
        Self { client }
    }

    /// Poll until healthy or timeout. Returns `true` if a 2xx response is
    /// received within the timeout period.
    pub async fn wait_healthy(
        &self,
        address: SocketAddr,
        health: &HealthCheck,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut consecutive_failures = 0u32;

        loop {
            if self.probe(address, &health.path).await {
                return true;
            }

            consecutive_failures += 1;
            if consecutive_failures >= health.failure_threshold {
                return false;
            }

            if tokio::time::Instant::now() >= deadline {
                return false;
            }

            // Wait one health-check interval before retrying, but don't exceed deadline.
            let remaining = deadline - tokio::time::Instant::now();
            let sleep = health.interval.min(remaining);
            tokio::time::sleep(sleep).await;

            if tokio::time::Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// Poll until a TCP connection to `address` succeeds, or timeout.
    /// Returns `true` if the port is accepting connections.
    pub async fn wait_tcp_ready(&self, address: SocketAddr, timeout: Duration) -> bool {
        let interval = crate::env::tcp_ready_poll_interval();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            let remaining = deadline - tokio::time::Instant::now();
            tokio::time::sleep(interval.min(remaining)).await;
        }
    }

    /// Single health check probe. Returns `true` for any 2xx status.
    async fn probe(&self, address: SocketAddr, path: &str) -> bool {
        let url = format!("http://{address}{path}");
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_returns_false_for_unreachable_address() {
        let poller = HealthPoller::new();
        // Nothing is listening on this port
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let result = poller.probe(addr, "/health").await;
        assert!(!result);
    }

    #[tokio::test]
    async fn wait_tcp_ready_succeeds_for_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let poller = HealthPoller::new();
        let result = poller.wait_tcp_ready(addr, Duration::from_secs(1)).await;
        assert!(result);
    }

    #[tokio::test]
    async fn wait_tcp_ready_times_out_for_unreachable() {
        let poller = HealthPoller::new();
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let result = poller.wait_tcp_ready(addr, Duration::from_millis(200)).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn wait_healthy_times_out_for_unreachable() {
        let poller = HealthPoller::new();
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let hc = HealthCheck {
            path: "/health".into(),
            interval: Duration::from_millis(50),
            failure_threshold: 3,
        };

        let result = poller.wait_healthy(addr, &hc, Duration::from_millis(200)).await;
        assert!(!result);
    }
}
