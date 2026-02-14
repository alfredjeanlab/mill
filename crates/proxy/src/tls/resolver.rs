use std::sync::Arc;

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::certs::cache::CertCache;

/// SNI-based certificate resolver backed by `CertCache`.
///
/// When a TLS client presents a ServerName in the ClientHello, this
/// resolver looks up the corresponding certificate in the cache.
pub struct SniByCertCache {
    cache: Arc<CertCache>,
}

impl std::fmt::Debug for SniByCertCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniByCertCache").finish()
    }
}

impl SniByCertCache {
    pub fn new(cache: Arc<CertCache>) -> Self {
        Self { cache }
    }
}

impl ResolvesServerCert for SniByCertCache {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let hostname = client_hello.server_name()?;
        self.cache.get(hostname)
    }
}
