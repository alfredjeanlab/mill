use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use rustls::sign::CertifiedKey;

/// In-memory cache of TLS certificates keyed by hostname.
///
/// Uses `std::sync::RwLock` (not tokio) because `ResolvesServerCert::resolve`
/// is a synchronous method.
pub struct CertCache {
    certs: RwLock<HashMap<String, Arc<CertifiedKey>>>,
}

impl Default for CertCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CertCache {
    pub fn new() -> Self {
        Self { certs: RwLock::new(HashMap::new()) }
    }

    /// Insert or replace the certificate for a hostname.
    pub fn insert(&self, hostname: String, key: Arc<CertifiedKey>) {
        self.certs.write().insert(hostname, key);
    }

    /// Look up the certificate for a hostname.
    pub fn get(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
        self.certs.read().get(hostname).cloned()
    }

    /// Check whether a certificate exists for a hostname.
    pub fn has_cert(&self, hostname: &str) -> bool {
        self.certs.read().contains_key(hostname)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::selfsigned::cert_pair_to_certified_key;
    use crate::tls::selfsigned::generate_self_signed;

    #[test]
    fn insert_and_get() {
        let cache = CertCache::new();
        let pair = generate_self_signed("example.com").unwrap_or_else(|e| {
            panic!("generate_self_signed failed: {e}");
        });
        let ck = cert_pair_to_certified_key(&pair).unwrap_or_else(|e| {
            panic!("cert_pair_to_certified_key failed: {e}");
        });

        cache.insert("example.com".to_string(), ck);
        assert!(cache.has_cert("example.com"));
        assert!(cache.get("example.com").is_some());
        assert!(!cache.has_cert("other.com"));
        assert!(cache.get("other.com").is_none());
    }
}
