use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{NodeError, Result};

const ACME_PRODUCTION_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";
const ACME_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Persistent identity for this node, stored in `<data_dir>/node.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub raft_id: u64,
    pub cluster_token: String,
    pub rpc_port: u16,
    pub advertise_addr: String,
    pub provider: Option<String>,
    pub provider_token: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    pub wireguard_subnet: String,
    pub peers: Vec<PeerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_staging: Option<bool>,
}

/// A peer as persisted in node.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub raft_id: u64,
    pub advertise_addr: String,
    pub tunnel_ip: IpAddr,
    pub public_key: String,
}

impl NodeIdentity {
    /// Build an `AcmeConfig` from persisted ACME fields.
    /// Returns `None` when no `acme_email` is set.
    pub fn acme_config(&self) -> Option<mill_proxy::AcmeConfig> {
        let email = self.acme_email.as_ref()?;
        let staging = self.acme_staging.unwrap_or(false);
        Some(mill_proxy::AcmeConfig {
            contact: vec![format!("mailto:{email}")],
            directory_url: if staging {
                ACME_STAGING_URL.to_string()
            } else {
                ACME_PRODUCTION_URL.to_string()
            },
        })
    }

    pub(super) async fn save(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| NodeError::Internal(e.to_string()))?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }

    pub(super) async fn load(path: &Path) -> Result<Self> {
        let data = tokio::fs::read_to_string(path).await?;
        let identity: Self =
            serde_json::from_str(&data).map_err(|e| NodeError::Internal(e.to_string()))?;
        Ok(identity)
    }
}
