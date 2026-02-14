use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls_pki_types::CertificateDer;

use super::{CertInfo, CertPair, CertStore};
use crate::error::{ProxyError, Result};

/// A `CertStore` backed by a directory on disk.
///
/// Layout:
/// ```text
/// <root>/
///   <hostname>.crt   — PEM-encoded certificate chain
///   <hostname>.key   — PEM-encoded private key
///   acme.json        — ACME account credentials
/// ```
pub struct FileCertStore {
    root: PathBuf,
}

impl FileCertStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn cert_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{hostname}.crt"))
    }

    fn key_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{hostname}.key"))
    }

    fn acme_path(&self) -> PathBuf {
        self.root.join("acme.json")
    }
}

impl CertStore for FileCertStore {
    async fn load(&self, hostname: &str) -> Result<Option<CertPair>> {
        let cert_path = self.cert_path(hostname);
        let key_path = self.key_path(hostname);

        if !cert_path.exists() || !key_path.exists() {
            return Ok(None);
        }

        let cert_pem = tokio::fs::read(&cert_path)
            .await
            .map_err(|e| ProxyError::Tls(format!("read {}: {e}", cert_path.display())))?;
        let key_pem = tokio::fs::read(&key_path)
            .await
            .map_err(|e| ProxyError::Tls(format!("read {}: {e}", key_path.display())))?;

        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut &cert_pem[..]).filter_map(|r| r.ok()).collect();
        if certs.is_empty() {
            return Err(ProxyError::Tls(format!("no certs found in {}", cert_path.display())));
        }

        let key = rustls_pemfile::private_key(&mut &key_pem[..])
            .map_err(|e| ProxyError::Tls(format!("read key {}: {e}", key_path.display())))?
            .ok_or_else(|| ProxyError::Tls(format!("no key found in {}", key_path.display())))?;

        Ok(Some(CertPair { certs, key }))
    }

    async fn save(&self, hostname: &str, pair: &CertPair) -> Result<()> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|e| ProxyError::Tls(format!("create dir {}: {e}", self.root.display())))?;

        // Write cert chain as PEM
        let mut cert_pem = Vec::new();
        for cert in &pair.certs {
            let encoded = pem_encode("CERTIFICATE", cert.as_ref());
            cert_pem.extend_from_slice(encoded.as_bytes());
        }
        tokio::fs::write(self.cert_path(hostname), &cert_pem)
            .await
            .map_err(|e| ProxyError::Tls(format!("write cert: {e}")))?;

        // Write key as PEM
        let key_pem = pem_encode("PRIVATE KEY", pair.key.secret_der());
        tokio::fs::write(self.key_path(hostname), key_pem.as_bytes())
            .await
            .map_err(|e| ProxyError::Tls(format!("write key: {e}")))?;

        Ok(())
    }

    async fn list(&self) -> Result<Vec<CertInfo>> {
        if !self.root.exists() {
            return Ok(vec![]);
        }
        let mut infos = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.root)
            .await
            .map_err(|e| ProxyError::Tls(format!("read dir: {e}")))?;

        while let Some(entry) =
            entries.next_entry().await.map_err(|e| ProxyError::Tls(format!("read entry: {e}")))?
        {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(hostname) = name.strip_suffix(".crt") {
                let expires = match parse_cert_expiry(&entry.path()).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("failed to parse cert expiry for {hostname}: {e}");
                        UNIX_EPOCH
                    }
                };
                infos.push(CertInfo { hostname: hostname.to_string(), expires });
            }
        }
        Ok(infos)
    }

    async fn load_acme_credentials(&self) -> Result<Option<String>> {
        let path = self.acme_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ProxyError::Acme(format!("read acme.json: {e}")))?;
        Ok(Some(data))
    }

    async fn save_acme_credentials(&self, json: &str) -> Result<()> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|e| ProxyError::Acme(format!("create dir: {e}")))?;
        tokio::fs::write(self.acme_path(), json)
            .await
            .map_err(|e| ProxyError::Acme(format!("write acme.json: {e}")))?;
        Ok(())
    }
}

/// Read a PEM certificate file and extract the `not_after` expiry from the first X.509 cert.
async fn parse_cert_expiry(path: &std::path::Path) -> std::result::Result<SystemTime, String> {
    let pem_bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &pem_bytes[..]).filter_map(|r| r.ok()).collect();
    let first = certs.first().ok_or_else(|| "no certificate found".to_string())?;
    let (_, cert) =
        x509_parser::parse_x509_certificate(first.as_ref()).map_err(|e| e.to_string())?;
    let timestamp = cert.validity().not_after.timestamp();
    if timestamp < 0 {
        return Ok(UNIX_EPOCH);
    }
    Ok(UNIX_EPOCH + Duration::from_secs(timestamp as u64))
}

fn pem_encode(label: &str, data: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileCertStore::new(dir.path().to_path_buf());

        // Generate a self-signed cert to test with
        let pair = crate::tls::selfsigned::generate_self_signed("test.example.com")
            .expect("generate cert");

        store.save("test.example.com", &pair).await.expect("save");
        let loaded = store.load("test.example.com").await.expect("load");
        assert!(loaded.is_some());
        let loaded = loaded.expect("some");
        assert_eq!(loaded.certs.len(), pair.certs.len());
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileCertStore::new(dir.path().to_path_buf());
        let result = store.load("nonexistent.com").await.expect("load");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_returns_saved_hostnames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileCertStore::new(dir.path().to_path_buf());

        let pair = crate::tls::selfsigned::generate_self_signed("a.test").expect("generate cert");
        store.save("a.test", &pair).await.expect("save");

        let pair = crate::tls::selfsigned::generate_self_signed("b.test").expect("generate cert");
        store.save("b.test", &pair).await.expect("save");

        let infos = store.list().await.expect("list");
        let mut hostnames: Vec<String> = infos.into_iter().map(|i| i.hostname).collect();
        hostnames.sort();
        assert_eq!(hostnames, vec!["a.test", "b.test"]);
    }

    #[tokio::test]
    async fn list_returns_cert_expiry_not_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileCertStore::new(dir.path().to_path_buf());

        let pair =
            crate::tls::selfsigned::generate_self_signed("expiry.test").expect("generate cert");
        store.save("expiry.test", &pair).await.expect("save");

        let infos = store.list().await.expect("list");
        assert_eq!(infos.len(), 1);

        // rcgen generates certs valid for ~10 years by default.
        // The expiry should be at least 1 year in the future — well beyond any file mtime.
        let one_year_from_now =
            SystemTime::now() + std::time::Duration::from_secs(365 * 24 * 60 * 60);
        assert!(
            infos[0].expires > one_year_from_now,
            "expiry {:?} should be >1 year from now",
            infos[0].expires,
        );
    }

    #[tokio::test]
    async fn acme_credentials_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileCertStore::new(dir.path().to_path_buf());

        assert!(store.load_acme_credentials().await.expect("load").is_none());

        store.save_acme_credentials(r#"{"key":"value"}"#).await.expect("save");
        let loaded = store.load_acme_credentials().await.expect("load");
        assert_eq!(loaded.as_deref(), Some(r#"{"key":"value"}"#));
    }
}
