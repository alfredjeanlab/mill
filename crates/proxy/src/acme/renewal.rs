use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::certs::CertStore;
use crate::certs::cache::CertCache;
use crate::tls::selfsigned::cert_pair_to_certified_key;

use super::client::AcmeClient;

const RENEWAL_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

/// Background loop that renews certificates expiring within 30 days.
///
/// Checks immediately on startup, then sleeps 24 hours between checks.
/// On failure, logs a warning and retries on the next cycle.
pub async fn renewal_loop<S: CertStore>(
    acme: &AcmeClient,
    store: &S,
    cache: &Arc<CertCache>,
    cancel: CancellationToken,
) {
    loop {
        check_and_renew(acme, store, cache).await;

        tokio::select! {
            _ = tokio::time::sleep(CHECK_INTERVAL) => {}
            _ = cancel.cancelled() => break,
        }
    }
}

async fn check_and_renew<S: CertStore>(acme: &AcmeClient, store: &S, cache: &Arc<CertCache>) {
    let cert_infos = match store.list().await {
        Ok(infos) => infos,
        Err(e) => {
            tracing::warn!("renewal: failed to list certs: {e}");
            return;
        }
    };

    let renewal_threshold = SystemTime::now() + RENEWAL_WINDOW;

    for info in &cert_infos {
        if info.expires > renewal_threshold {
            continue;
        }

        tracing::info!(hostname = %info.hostname, "certificate expiring soon, renewing via ACME");

        match acme.acquire_certificate(&info.hostname).await {
            Ok(pair) => {
                // Save to store
                if let Err(e) = store.save(&info.hostname, &pair).await {
                    tracing::warn!(hostname = %info.hostname, "renewal: failed to save cert: {e}");
                    continue;
                }
                // Update in-memory cache
                match cert_pair_to_certified_key(&pair) {
                    Ok(ck) => {
                        cache.insert(info.hostname.clone(), ck);
                        tracing::info!(hostname = %info.hostname, "renewed certificate via ACME");
                    }
                    Err(e) => {
                        tracing::warn!(hostname = %info.hostname, "renewal: failed to load renewed cert: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(hostname = %info.hostname, "renewal: ACME failed, keeping existing cert: {e}");
            }
        }
    }
}
