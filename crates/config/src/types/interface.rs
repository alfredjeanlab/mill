use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use super::AllocId;
use super::runtime::AllocKind;

/// Fully-resolved specification passed to the container runtime to start a container.
///
/// The `env` map contains plaintext values — the primary resolves all
/// `${secret(...)}` and `${service.NAME.address}` references before
/// constructing this type. This keeps secret resolution in one place (the
/// primary, which holds the Raft FSM and encryption keys) and prevents the
/// secondary from needing a back-channel to decrypt secrets.
///
/// # Security
///
/// Because `env` may contain secret values, `Debug` is implemented manually
/// to redact environment variable values. Use `serde` serialization only for
/// wire transport over encrypted channels (WireGuard at CP3), never for
/// logging or diagnostics.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ContainerSpec {
    pub alloc_id: AllocId,
    #[serde(default)]
    pub kind: AllocKind,
    pub timeout: Option<Duration>,
    pub command: Option<Vec<String>>,
    pub image: String,
    pub env: HashMap<String, String>,
    pub port: Option<u16>,
    pub host_port: Option<u16>,
    pub cpu: f64,
    #[serde(with = "super::bytesize_serde")]
    pub memory: ByteSize,
    pub volumes: Vec<ContainerVolume>,
}

impl fmt::Debug for ContainerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact env values — keys are safe, values may contain secrets.
        let redacted_env: HashMap<&str, &str> =
            self.env.keys().map(|k| (k.as_str(), "<redacted>")).collect();

        f.debug_struct("ContainerSpec")
            .field("alloc_id", &self.alloc_id)
            .field("kind", &self.kind)
            .field("timeout", &self.timeout)
            .field("command", &self.command)
            .field("image", &self.image)
            .field("env", &redacted_env)
            .field("port", &self.port)
            .field("host_port", &self.host_port)
            .field("cpu", &self.cpu)
            .field("memory", &self.memory)
            .field("volumes", &self.volumes)
            .finish()
    }
}

/// A volume mount in a container spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContainerVolume {
    pub name: String,
    pub path: String,
    pub ephemeral: bool,
}

/// A route entry for the reverse proxy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyRoute {
    pub hostname: String,
    pub path: String,
    pub backend: SocketAddr,
    pub websocket: bool,
}

/// A DNS record managed by Mill's internal DNS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DnsRecord {
    pub name: String,
    pub addresses: Vec<SocketAddr>,
}

/// Resource usage stats from a running container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContainerStats {
    pub alloc_id: AllocId,
    pub cpu_usage: f64,
    #[serde(with = "super::bytesize_serde")]
    pub memory_usage: ByteSize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_spec_debug_redacts_env_values() {
        let spec = ContainerSpec {
            alloc_id: AllocId("test-1".into()),
            kind: AllocKind::default(),
            timeout: None,
            command: None,
            image: "nginx:latest".into(),
            env: HashMap::from([
                ("DB_PASSWORD".into(), "hunter2".into()),
                ("API_KEY".into(), "sk-secret-key-12345".into()),
            ]),
            port: Some(8080),
            host_port: Some(50000),
            cpu: 1.0,
            memory: ByteSize::mib(256),
            volumes: vec![],
        };

        let debug_output = format!("{spec:?}");

        // Keys are visible
        assert!(debug_output.contains("DB_PASSWORD"));
        assert!(debug_output.contains("API_KEY"));

        // Values are redacted
        assert!(!debug_output.contains("hunter2"));
        assert!(!debug_output.contains("sk-secret-key-12345"));
        assert!(debug_output.contains("<redacted>"));
    }
}
