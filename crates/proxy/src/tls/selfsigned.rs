use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair};
use rustls::crypto::ring::sign::any_supported_type;
use rustls::sign::CertifiedKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::certs::CertPair;
use crate::error::{ProxyError, Result};

/// Generate a self-signed certificate for the given hostname.
pub fn generate_self_signed(hostname: &str) -> Result<CertPair> {
    let mut params = CertificateParams::new(vec![hostname.to_string()])
        .map_err(|e| ProxyError::CertGeneration(e.to_string()))?;
    params.distinguished_name = rcgen::DistinguishedName::new();

    let key_pair = KeyPair::generate().map_err(|e| ProxyError::CertGeneration(e.to_string()))?;
    let cert =
        params.self_signed(&key_pair).map_err(|e| ProxyError::CertGeneration(e.to_string()))?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|e| ProxyError::CertGeneration(e.to_string()))?;

    Ok(CertPair { certs: vec![cert_der], key: key_der })
}

/// Convert a `CertPair` to a rustls `CertifiedKey` for use in the TLS stack.
pub fn cert_pair_to_certified_key(pair: &CertPair) -> Result<Arc<CertifiedKey>> {
    let signing_key = any_supported_type(&pair.key)
        .map_err(|e| ProxyError::Tls(format!("unsupported key type: {e}")))?;
    Ok(Arc::new(CertifiedKey::new(pair.certs.clone(), signing_key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_valid_self_signed() {
        let pair = generate_self_signed("test.example.com");
        assert!(pair.is_ok());
        let pair = pair.unwrap_or_else(|e| {
            panic!("generate failed: {e}");
        });
        assert_eq!(pair.certs.len(), 1);
    }

    #[test]
    fn convert_to_certified_key() {
        let pair = generate_self_signed("test.example.com").unwrap_or_else(|e| {
            panic!("generate failed: {e}");
        });
        let ck = cert_pair_to_certified_key(&pair);
        assert!(ck.is_ok());
    }
}
