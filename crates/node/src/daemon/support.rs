use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use mill_config::DataDir;
use mill_net::PortAllocator;
use mill_proxy::{AcmeConfig, FileCertStore, ProxyConfig, ProxyServer};
use mill_raft::MillRaft;

use crate::error::{NodeError, Result};
use crate::primary::Primary;

pub(super) async fn shutdown(
    primary: Primary,
    raft: Arc<MillRaft>,
    dns: Arc<mill_net::DnsServer>,
    proxy: Arc<ProxyServer>,
) {
    eprintln!("Shutting down...");
    primary.stop();
    // secondary.run() exits when cmd channel closes (handle dropped)
    if let Err(e) = raft.shutdown().await {
        tracing::warn!("raft shutdown: {e}");
    }
    // DNS and proxy need owned values; unwrap the Arc if possible
    if let Ok(dns) = Arc::try_unwrap(dns)
        && let Err(e) = dns.stop().await
    {
        tracing::warn!("dns shutdown: {e}");
    }
    if let Ok(proxy) = Arc::try_unwrap(proxy)
        && let Err(e) = proxy.stop().await
    {
        tracing::warn!("proxy shutdown: {e}");
    }
    eprintln!("Shutdown complete.");
}

pub(super) async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let ctrl_c = tokio::signal::ctrl_c();
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = ctrl_c => {},
                _ = sigterm.recv() => {},
            }
        } else {
            // Fallback: just wait for ctrl-c
            let _ = ctrl_c.await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub(super) async fn write_pid_file(data_dir: &DataDir) {
    let pid = std::process::id();
    if let Err(e) = tokio::fs::write(data_dir.pid_path(), pid.to_string()).await {
        tracing::warn!("failed to write pid file: {e}");
    }
}

pub(super) async fn remove_pid_file(data_dir: &DataDir) {
    let _ = tokio::fs::remove_file(data_dir.pid_path()).await;
}

pub(super) struct Subsystems {
    pub dns: Arc<mill_net::DnsServer>,
    pub ports: Arc<PortAllocator>,
    pub proxy: Arc<ProxyServer>,
}

pub(super) async fn start_subsystems(
    data_dir: &DataDir,
    acme: Option<AcmeConfig>,
) -> Result<Subsystems> {
    let dns = mill_net::DnsServer::start(
        "127.0.0.1:53"
            .parse()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 53)),
    )
    .await?;
    let ports = Arc::new(PortAllocator::new()?);
    let proxy = ProxyServer::start(
        ProxyConfig {
            http_addr: "0.0.0.0:80".parse().unwrap_or_else(|_| {
                SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 80)
            }),
            https_addr: "0.0.0.0:443".parse().unwrap_or_else(|_| {
                SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 443)
            }),
            acme,
        },
        FileCertStore::new(data_dir.certs_dir()),
    )
    .await
    .map_err(|e| NodeError::Internal(format!("proxy startup: {e}")))?;
    Ok(Subsystems { dns: Arc::new(dns), ports, proxy: Arc::new(proxy) })
}

pub(super) fn default_raft_config() -> Arc<openraft::Config> {
    Arc::new(openraft::Config {
        heartbeat_interval: 1000,
        election_timeout_min: 3000,
        election_timeout_max: 5000,
        snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(crate::env::snapshot_threshold()),
        max_in_snapshot_log_to_keep: crate::env::snapshot_log_keep(),
        ..openraft::Config::default()
    })
}

fn generate_hex(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut bytes = vec![0u8; len];
    rng.fill(&mut bytes[..]);
    hex::encode(bytes)
}

pub(super) fn generate_id() -> String {
    generate_hex(8)
}

pub(super) fn generate_token() -> String {
    generate_hex(16)
}

pub(super) async fn start_raft_listener(router: axum::Router, ip: IpAddr, port: u16) {
    let addr = SocketAddr::new(ip, port);
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("failed to bind raft listener on {addr}: {e}");
                return;
            }
        };
        let _ = axum::serve(listener, router).await;
    });
}
