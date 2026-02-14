use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::error::NetError;

/// Raw UDP forwarder for non-.mill DNS queries.
pub struct Forwarder {
    upstream: SocketAddr,
    socket: Mutex<UdpSocket>,
}

impl Forwarder {
    /// Create a new forwarder by parsing `/etc/resolv.conf` for the first
    /// nameserver. Falls back to `8.8.8.8:53` if parsing fails.
    ///
    /// Binds a single UDP socket for reuse across all forwarded queries.
    pub async fn new() -> Result<Self, NetError> {
        let upstream = read_upstream_nameserver();
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| NetError::Bind { address: "0.0.0.0:0".to_string(), source: e })?;
        Ok(Self { upstream, socket: Mutex::new(socket) })
    }

    /// Forward raw DNS query bytes to the upstream resolver and return the
    /// raw response. Times out after 3 seconds.
    pub async fn forward(&self, query_bytes: &[u8]) -> Result<Vec<u8>, NetError> {
        let socket = self.socket.lock().await;
        socket.send_to(query_bytes, self.upstream).await?;

        let mut buf = vec![0u8; 4096];
        let len =
            tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buf)).await.map_err(
                |_| NetError::DnsProtocol(format!("upstream {} timed out after 3s", self.upstream)),
            )??;

        buf.truncate(len);
        Ok(buf)
    }
}

/// Parse `/etc/resolv.conf` for the first `nameserver` entry.
/// Returns `8.8.8.8:53` as a fallback.
fn read_upstream_nameserver() -> SocketAddr {
    let fallback: SocketAddr = "8.8.8.8:53".parse().unwrap_or_else(|_| {
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)), 53)
    });

    let contents = match std::fs::read_to_string("/etc/resolv.conf") {
        Ok(c) => c,
        Err(_) => return fallback,
    };

    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver") {
            let ip_str = rest.trim();
            if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                return SocketAddr::new(ip, 53);
            }
        }
    }

    fallback
}
