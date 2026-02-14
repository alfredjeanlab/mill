use std::io::BufReader;
use std::time::Duration;

use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::certs::CertPair;
use crate::error::{ProxyError, Result};

use super::challenges::ChallengeStore;

/// ACME client configuration.
pub struct AcmeConfig {
    /// Contact emails, e.g. `["mailto:admin@example.com"]`.
    pub contact: Vec<String>,
    /// ACME directory URL (e.g. `LetsEncrypt::Production.url()`).
    pub directory_url: String,
}

/// ACME client for acquiring certificates via HTTP-01 challenges.
pub struct AcmeClient {
    account: Account,
    challenges: ChallengeStore,
}

impl AcmeClient {
    /// Create a new ACME client, restoring from saved credentials if available.
    ///
    /// Returns `(client, Some(json))` when a new account is registered (caller
    /// should persist the JSON), or `(client, None)` when restored from saved
    /// credentials.
    pub async fn new(
        config: &AcmeConfig,
        saved_credentials: Option<&str>,
    ) -> Result<(Self, Option<String>)> {
        let builder = Account::builder()
            .map_err(|e| ProxyError::Acme(format!("failed to create account builder: {e}")))?;

        let (account, new_creds_json) = if let Some(json) = saved_credentials {
            let creds: AccountCredentials = serde_json::from_str(json)
                .map_err(|e| ProxyError::Acme(format!("failed to parse saved credentials: {e}")))?;
            let account = builder
                .from_credentials(creds)
                .await
                .map_err(|e| ProxyError::Acme(format!("failed to restore account: {e}")))?;
            (account, None)
        } else {
            let contact: Vec<&str> = config.contact.iter().map(|s| s.as_str()).collect();
            let (account, creds) = builder
                .create(
                    &NewAccount {
                        contact: &contact,
                        terms_of_service_agreed: true,
                        only_return_existing: false,
                    },
                    config.directory_url.clone(),
                    None,
                )
                .await
                .map_err(|e| ProxyError::Acme(format!("failed to create account: {e}")))?;
            let json = serde_json::to_string(&creds)
                .map_err(|e| ProxyError::Acme(format!("failed to serialize credentials: {e}")))?;
            (account, Some(json))
        };

        Ok((Self { account, challenges: ChallengeStore::new() }, new_creds_json))
    }

    /// Reference to the challenge store (shared with the HTTP handler).
    pub fn challenge_store(&self) -> &ChallengeStore {
        &self.challenges
    }

    /// Acquire a certificate for `hostname` via ACME HTTP-01.
    pub async fn acquire_certificate(&self, hostname: &str) -> Result<CertPair> {
        let identifiers = vec![Identifier::Dns(hostname.to_string())];
        let mut order = self
            .account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| ProxyError::Acme(format!("new order for {hostname}: {e}")))?;

        // Process authorizations, collecting tokens for cleanup
        let mut tokens: Vec<String> = Vec::new();
        let mut authz_stream = order.authorizations();
        while let Some(authz_result) = authz_stream.next().await {
            let mut authz = authz_result
                .map_err(|e| ProxyError::Acme(format!("authorization for {hostname}: {e}")))?;

            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| ProxyError::Acme(format!("no HTTP-01 challenge for {hostname}")))?;

            let token = challenge.token.clone();
            let key_auth = challenge.key_authorization().as_str().to_string();

            self.challenges.insert(token.clone(), key_auth);
            tokens.push(token.clone());

            let ready_result = challenge.set_ready().await;
            if let Err(e) = ready_result {
                for t in &tokens {
                    self.challenges.remove(t);
                }
                return Err(ProxyError::Acme(format!("set_ready for {hostname}: {e}")));
            }
        }

        // Wait for order to become ready
        let retry = RetryPolicy::new().timeout(Duration::from_secs(60));
        let status = order
            .poll_ready(&retry)
            .await
            .map_err(|e| ProxyError::Acme(format!("poll_ready for {hostname}: {e}")))?;

        if status != OrderStatus::Ready {
            for t in &tokens {
                self.challenges.remove(t);
            }
            return Err(ProxyError::Acme(format!("order for {hostname} not ready: {status:?}")));
        }

        // Finalize — generates CSR via rcgen, returns private key PEM
        let private_key_pem = order
            .finalize()
            .await
            .map_err(|e| ProxyError::Acme(format!("finalize for {hostname}: {e}")))?;

        // Poll for certificate chain PEM
        let cert_chain_pem = order
            .poll_certificate(&retry)
            .await
            .map_err(|e| ProxyError::Acme(format!("poll_certificate for {hostname}: {e}")))?;

        let result = pem_to_cert_pair(&cert_chain_pem, &private_key_pem);

        // Clean up all challenge tokens
        for t in &tokens {
            self.challenges.remove(t);
        }

        result
    }
}

/// Parse PEM-encoded certificate chain and private key into a `CertPair`.
fn pem_to_cert_pair(cert_pem: &str, key_pem: &str) -> Result<CertPair> {
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| ProxyError::Acme(format!("failed to parse cert PEM: {e}")))?;

    if certs.is_empty() {
        return Err(ProxyError::Acme("no certificates in PEM".to_string()));
    }

    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
            .map_err(|e| ProxyError::Acme(format!("failed to parse key PEM: {e}")))?
            .ok_or_else(|| ProxyError::Acme("no private key in PEM".to_string()))?;

    Ok(CertPair { certs, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate known-good PEM via rcgen to test our parser.
    #[test]
    fn pem_to_cert_pair_roundtrip() {
        let mut params = rcgen::CertificateParams::new(vec!["test.example.com".to_string()])
            .unwrap_or_else(|e| panic!("params: {e}"));
        params.distinguished_name = rcgen::DistinguishedName::new();
        let key_pair = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("keygen: {e}"));
        let cert = params.self_signed(&key_pair).unwrap_or_else(|e| panic!("self_signed: {e}"));

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        let pair = pem_to_cert_pair(&cert_pem, &key_pem);
        assert!(pair.is_ok(), "parse failed: {:?}", pair.err());

        let pair = pair.unwrap_or_else(|e| panic!("unwrap: {e}"));
        assert_eq!(pair.certs.len(), 1);
    }

    #[test]
    fn pem_to_cert_pair_empty_cert_errors() {
        let result = pem_to_cert_pair("", "");
        assert!(result.is_err());
    }
}
