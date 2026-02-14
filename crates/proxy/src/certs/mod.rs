pub mod cache;
pub mod file_store;

use std::time::SystemTime;

use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::error::Result;

/// A certificate and its private key in DER form.
pub struct CertPair {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

/// Metadata about a stored certificate.
#[derive(Debug, Clone)]
pub struct CertInfo {
    pub hostname: String,
    pub expires: SystemTime,
}

/// Trait for loading and saving TLS certificates.
///
/// The primary implements this over the Raft FSM so certificates are
/// replicated across the cluster.
pub trait CertStore: Send + Sync + 'static {
    fn load(&self, hostname: &str) -> impl Future<Output = Result<Option<CertPair>>> + Send;
    fn save(&self, hostname: &str, pair: &CertPair) -> impl Future<Output = Result<()>> + Send;
    fn list(&self) -> impl Future<Output = Result<Vec<CertInfo>>> + Send;
    fn load_acme_credentials(&self) -> impl Future<Output = Result<Option<String>>> + Send;
    fn save_acme_credentials(&self, json: &str) -> impl Future<Output = Result<()>> + Send;
}
