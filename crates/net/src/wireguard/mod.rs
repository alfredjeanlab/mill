mod cmd;

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use mill_config::NodeId;

use crate::error::NetError;
use cmd::{generate_keypair, run_command, run_command_stdin};

/// Information about a WireGuard peer in the mesh.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub node_id: NodeId,
    pub public_key: String,
    pub endpoint: Option<SocketAddr>,
    pub tunnel_ip: IpAddr,
}

/// The local node's WireGuard identity returned after init/join.
#[derive(Debug, Clone)]
pub struct WireGuardIdentity {
    pub public_key: String,
    pub tunnel_ip: IpAddr,
}

/// Load an existing private key from disk, or generate and persist a new one.
///
/// If `key_path` is `Some` and the file exists, reads the private key.
/// If `key_path` is `Some` and the file is missing, generates a new keypair
/// and writes the private key with mode 0600.
/// If `key_path` is `None`, generates an ephemeral keypair (tests).
async fn load_or_generate_keypair(key_path: Option<&Path>) -> Result<(String, String), NetError> {
    match key_path {
        Some(path) if path.exists() => {
            let private_key = tokio::fs::read_to_string(path).await?;
            let private_key = private_key.trim().to_string();
            let public_key = run_command_stdin("wg", &["pubkey"], &private_key).await?;
            Ok((private_key, public_key))
        }
        Some(path) => {
            let (private_key, public_key) = generate_keypair().await?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, &private_key).await?;
            let f = tokio::fs::File::open(path).await?;
            f.sync_all().await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                tokio::fs::set_permissions(path, perms).await?;
            }
            Ok((private_key, public_key))
        }
        None => generate_keypair().await,
    }
}

/// Boxed future for async trait methods that need `dyn` dispatch.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Peer management interface used by the watchdog and join handler.
///
/// Production code uses `WireGuard`; tests substitute an in-memory
/// implementation from `mill-support`.
pub trait Mesh: Send + Sync {
    fn add_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), NetError>>;
    fn remove_peer(&self, public_key: &str) -> BoxFuture<'_, Result<(), NetError>>;
}

/// Manages a WireGuard network interface for the mesh overlay.
pub struct WireGuard {
    interface_name: String,
}

impl Mesh for WireGuard {
    fn add_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), NetError>> {
        let peer = peer.clone();
        Box::pin(self.add_peer_impl(peer))
    }

    fn remove_peer(&self, public_key: &str) -> BoxFuture<'_, Result<(), NetError>> {
        let public_key = public_key.to_string();
        Box::pin(self.remove_peer_impl(public_key))
    }
}

impl WireGuard {
    /// Wrap an existing WireGuard interface by name (for teardown/leave).
    pub fn from_existing(interface_name: &str) -> Self {
        Self { interface_name: interface_name.to_string() }
    }

    /// Load or generate a WireGuard keypair and return the public key.
    ///
    /// If `key_path` is `Some` and the file exists, reads the existing private
    /// key. If the file is missing, generates a new keypair and writes the
    /// private key. If `key_path` is `None`, generates an ephemeral keypair.
    ///
    /// Does NOT create or modify any network interface. Used to obtain the
    /// public key before the WireGuard interface is set up (e.g. for a join
    /// request that must be sent before the tunnel is established).
    pub async fn prepare_public_key(key_path: Option<&Path>) -> Result<String, NetError> {
        let (_, public_key) = load_or_generate_keypair(key_path).await?;
        Ok(public_key)
    }

    /// Initialize a new WireGuard interface as the first node in the mesh.
    ///
    /// `subnet` should be a CIDR like `"10.99.0.0/16"`. The first usable IP
    /// (e.g. `10.99.0.1`) is assigned to this node.
    ///
    /// If `key_path` is `Some`, the private key is loaded from (or generated
    /// and saved to) that path. Pass `None` for ephemeral keys (tests).
    pub async fn init(
        interface_name: &str,
        listen_port: u16,
        subnet: &str,
        key_path: Option<&Path>,
    ) -> Result<(Self, WireGuardIdentity), NetError> {
        let (private_key, public_key) = load_or_generate_keypair(key_path).await?;
        let (tunnel_ip, prefix_len) = parse_subnet_first_ip(subnet)?;
        let iface = interface_name;

        // Create interface (delete first if it already exists from a previous run)
        let _ = run_command("ip", &["link", "del", iface]).await;
        run_command("ip", &["link", "add", iface, "type", "wireguard"]).await?;

        // Set private key and listen port
        let port_str = listen_port.to_string();
        run_command_stdin(
            "wg",
            &["set", iface, "private-key", "/dev/stdin", "listen-port", &port_str],
            &private_key,
        )
        .await?;

        // Assign tunnel IP
        let addr_cidr = format!("{tunnel_ip}/{prefix_len}");
        run_command("ip", &["addr", "add", &addr_cidr, "dev", iface]).await?;

        // Bring interface up
        run_command("ip", &["link", "set", iface, "up"]).await?;

        // Add explicit subnet route (the connected route from ip addr add may
        // not cover all peers if allowed-ips uses /32 entries).
        add_subnet_route(iface, subnet).await;

        let identity = WireGuardIdentity { public_key, tunnel_ip };

        let wg = Self { interface_name: iface.to_string() };

        Ok((wg, identity))
    }

    /// Join an existing mesh by creating a WireGuard interface with a given
    /// tunnel IP and adding the provided peers.
    ///
    /// If `key_path` is `Some`, the private key is loaded from (or generated
    /// and saved to) that path. Pass `None` for ephemeral keys (tests).
    pub async fn join(
        interface_name: &str,
        listen_port: u16,
        tunnel_ip: IpAddr,
        prefix_len: u8,
        peers: &[PeerInfo],
        key_path: Option<&Path>,
    ) -> Result<(Self, WireGuardIdentity), NetError> {
        let (private_key, public_key) = load_or_generate_keypair(key_path).await?;
        let iface = interface_name;

        // Delete interface if it already exists from a previous run
        let _ = run_command("ip", &["link", "del", iface]).await;
        run_command("ip", &["link", "add", iface, "type", "wireguard"]).await?;

        let port_str = listen_port.to_string();
        run_command_stdin(
            "wg",
            &["set", iface, "private-key", "/dev/stdin", "listen-port", &port_str],
            &private_key,
        )
        .await?;

        let addr_cidr = format!("{tunnel_ip}/{prefix_len}");
        run_command("ip", &["addr", "add", &addr_cidr, "dev", iface]).await?;
        run_command("ip", &["link", "set", iface, "up"]).await?;

        let wg = Self { interface_name: iface.to_string() };

        for peer in peers {
            wg.add_peer_impl(peer.clone()).await?;
        }

        let identity = WireGuardIdentity { public_key, tunnel_ip };

        Ok((wg, identity))
    }

    async fn add_peer_impl(&self, peer: PeerInfo) -> Result<(), NetError> {
        let allowed_ips = format!("{}/32", peer.tunnel_ip);
        let mut args = vec!["set", &self.interface_name, "peer", &peer.public_key];
        let endpoint_str;
        if let Some(ep) = peer.endpoint {
            endpoint_str = ep.to_string();
            args.push("endpoint");
            args.push(&endpoint_str);
        }
        args.push("allowed-ips");
        args.push(&allowed_ips);
        run_command("wg", &args).await?;
        Ok(())
    }

    async fn remove_peer_impl(&self, public_key: String) -> Result<(), NetError> {
        run_command("wg", &["set", &self.interface_name, "peer", &public_key, "remove"]).await?;
        Ok(())
    }

    /// List current peers by parsing `wg show <iface> dump`.
    ///
    /// Note: This returns raw WireGuard peer data. The `node_id` field is
    /// populated with an empty string since WireGuard doesn't track it;
    /// the caller maps public keys to node IDs.
    pub async fn list_peers(&self) -> Result<Vec<PeerInfo>, NetError> {
        let output = run_command("wg", &["show", &self.interface_name, "dump"]).await?;
        let mut peers = Vec::new();

        // Skip first line (interface info)
        for line in output.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 4 {
                continue;
            }
            let public_key = fields[0].to_string();
            // fields[2] = endpoint (ip:port or "(none)")
            let endpoint: Option<SocketAddr> = if fields[2] == "(none)" {
                None
            } else {
                Some(
                    fields[2]
                        .parse()
                        .map_err(|_| NetError::InvalidAddress(fields[2].to_string()))?,
                )
            };
            // fields[3] = allowed-ips (comma-separated CIDRs)
            let allowed_ips = fields[3];
            let tunnel_ip = allowed_ips
                .split(',')
                .next()
                .and_then(|cidr| cidr.split('/').next())
                .and_then(|ip| ip.parse::<IpAddr>().ok())
                .ok_or_else(|| NetError::InvalidAddress(allowed_ips.to_string()))?;

            peers.push(PeerInfo {
                node_id: NodeId(String::new()),
                public_key,
                endpoint,
                tunnel_ip,
            });
        }

        Ok(peers)
    }

    /// Ping a tunnel IP through the WireGuard interface.
    ///
    /// Returns `Ok(Some(duration))` on success, `Ok(None)` if the ping ran but
    /// no RTT could be parsed, or `Err` if the command failed (e.g. packet loss).
    pub async fn ping(&self, tunnel_ip: IpAddr) -> Result<Option<Duration>, NetError> {
        let ip_str = tunnel_ip.to_string();
        let output =
            run_command("ping", &["-c", "1", "-W", "3", "-I", &self.interface_name, &ip_str])
                .await?;

        // Parse "time=X.XX ms" from ping output
        let duration = output.lines().find_map(|line| {
            let time_idx = line.find("time=")?;
            let after_time = &line[time_idx + 5..];
            let ms_str = after_time.split_whitespace().next()?;
            let ms: f64 = ms_str.parse().ok()?;
            Some(Duration::from_secs_f64(ms / 1000.0))
        });

        Ok(duration)
    }

    /// Add the subnet route for this node's tunnel IP after joining the mesh.
    ///
    /// Must be called AFTER the leader has added us as a WireGuard peer (i.e.
    /// after `/v1/join` succeeds), otherwise traffic to the leader's tunnel IP
    /// is routed through the WireGuard interface before the handshake can
    /// complete.
    pub async fn add_subnet_route(&self, tunnel_ip: IpAddr, prefix_len: u8) {
        let net_addr = network_address(tunnel_ip, prefix_len);
        let subnet = format!("{net_addr}/{prefix_len}");
        add_subnet_route(&self.interface_name, &subnet).await;
    }

    /// Tear down the WireGuard interface.
    pub async fn teardown(&self) -> Result<(), NetError> {
        run_command("ip", &["link", "del", &self.interface_name]).await?;
        Ok(())
    }
}

/// Add a subnet route to a WireGuard interface, ignoring "File exists" errors
/// (the connected route from `ip addr add` may already cover the subnet).
async fn add_subnet_route(iface: &str, subnet: &str) {
    if let Err(e) = run_command("ip", &["route", "add", subnet, "dev", iface]).await {
        let msg = e.to_string();
        if !msg.contains("File exists") {
            tracing::warn!("failed to add subnet route {subnet} dev {iface}: {e}");
        }
    }
}

/// Compute the network address by masking an IP with the given prefix length.
fn network_address(ip: IpAddr, prefix_len: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from_be_bytes(v4.octets());
            let mask = if prefix_len >= 32 { u32::MAX } else { u32::MAX << (32 - prefix_len) };
            IpAddr::V4(std::net::Ipv4Addr::from(bits & mask))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from_be_bytes(v6.octets());
            let mask = if prefix_len >= 128 { u128::MAX } else { u128::MAX << (128 - prefix_len) };
            IpAddr::V6(std::net::Ipv6Addr::from(bits & mask))
        }
    }
}

/// Parse a CIDR subnet string and return the first usable IP and prefix length.
/// E.g. `"10.99.0.0/16"` → `(10.99.0.1, 16)`.
fn parse_subnet_first_ip(subnet: &str) -> Result<(IpAddr, u8), NetError> {
    let parts: Vec<&str> = subnet.split('/').collect();
    if parts.len() != 2 {
        return Err(NetError::InvalidAddress(subnet.to_string()));
    }

    let base_ip: std::net::Ipv4Addr =
        parts[0].parse().map_err(|_| NetError::InvalidAddress(subnet.to_string()))?;
    let prefix_len: u8 =
        parts[1].parse().map_err(|_| NetError::InvalidAddress(subnet.to_string()))?;

    // First usable IP = base + 1
    let octets = base_ip.octets();
    let num = u32::from_be_bytes(octets) + 1;
    let first_ip = std::net::Ipv4Addr::from(num);

    Ok((IpAddr::V4(first_ip), prefix_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subnet_first_ip_basic() {
        let (ip, prefix) = parse_subnet_first_ip("10.99.0.0/16").unwrap_or_else(|_| {
            panic!("should parse");
        });
        assert_eq!(ip, "10.99.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(prefix, 16);
    }

    #[test]
    fn parse_subnet_invalid() {
        assert!(parse_subnet_first_ip("not-a-subnet").is_err());
        assert!(parse_subnet_first_ip("10.0.0.0").is_err());
    }

    #[test]
    fn network_address_v4() {
        let ip: IpAddr = "10.99.1.5".parse().unwrap();
        assert_eq!(network_address(ip, 16), "10.99.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(network_address(ip, 24), "10.99.1.0".parse::<IpAddr>().unwrap());
        assert_eq!(network_address(ip, 32), "10.99.1.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn network_address_v6() {
        let ip: IpAddr = "fd00::1:5".parse().unwrap();
        assert_eq!(network_address(ip, 64), "fd00::".parse::<IpAddr>().unwrap());
    }

    #[ignore] // Requires root + WireGuard kernel module
    #[tokio::test]
    async fn wireguard_init_teardown() {
        let result = WireGuard::init("wg0", 51820, "10.99.0.0/16", None).await;
        assert!(result.is_ok());
        let (wg, identity) = result.unwrap_or_else(|e| {
            panic!("init failed: {e}");
        });
        assert!(!identity.public_key.is_empty());
        assert_eq!(identity.tunnel_ip, "10.99.0.1".parse::<IpAddr>().unwrap());
        let _ = wg.teardown().await;
    }
}
