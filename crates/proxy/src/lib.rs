#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod acme;
pub mod certs;
mod error;
mod proxy;
mod tls;

pub use acme::{AcmeClient, AcmeConfig, ChallengeStore};
pub use certs::cache::CertCache;
pub use certs::file_store::FileCertStore;
pub use certs::{CertInfo, CertPair, CertStore};
pub use error::{ProxyError, Result};
pub use proxy::metrics::ProxyMetrics;

use std::net::SocketAddr;
use std::sync::Arc;

use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use mill_config::ProxyRoute;
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::proxy::forward::HttpClient;

use crate::proxy::routes::SharedRouteTable;
use crate::tls::resolver::SniByCertCache;
use crate::tls::selfsigned::{cert_pair_to_certified_key, generate_self_signed};

/// Configuration for the proxy server.
pub struct ProxyConfig {
    /// Address to bind the HTTP listener (typically port 80).
    pub http_addr: SocketAddr,
    /// Address to bind the HTTPS listener (typically port 443).
    pub https_addr: SocketAddr,
    /// ACME configuration for automatic certificate acquisition.
    /// When `None`, self-signed certificates are used as fallback.
    pub acme: Option<AcmeConfig>,
}

/// Built-in reverse proxy with TLS termination.
///
/// Routes HTTP/HTTPS traffic to container backends. TLS is terminated
/// at the proxy; backend connections use plain HTTP over the WireGuard
/// mesh.
///
/// Follows the start/use/stop lifecycle pattern:
///
/// ```ignore
/// let proxy = ProxyServer::start(config, store).await?;
/// proxy.update_routes(&routes);
/// // ... serve traffic ...
/// proxy.stop().await?;
/// ```
pub struct ProxyServer {
    http_addr: SocketAddr,
    https_addr: SocketAddr,
    routes: Arc<SharedRouteTable>,
    cert_cache: Arc<CertCache>,
    challenge_store: ChallengeStore,
    metrics: Arc<ProxyMetrics>,
    cancel: CancellationToken,
    http_task: JoinHandle<()>,
    https_task: JoinHandle<()>,
    cert_task: JoinHandle<()>,
    renewal_task: Option<JoinHandle<()>>,
}

impl ProxyServer {
    /// Start the proxy server, binding listeners and spawning accept loops.
    ///
    /// Loads existing certificates from the `CertStore` into the in-memory
    /// cache, then starts HTTP and HTTPS servers plus background tasks
    /// that acquire certificates (ACME if configured, self-signed fallback)
    /// and renew them before expiry.
    pub async fn start<S: CertStore>(config: ProxyConfig, cert_store: S) -> Result<Self> {
        // Ensure ring is the active crypto provider. This is a no-op if
        // already installed, and is necessary when multiple providers
        // (ring + aws-lc-rs) are linked into the same binary.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let cert_store = Arc::new(cert_store);
        let cert_cache = Arc::new(CertCache::new());

        // Load existing certs from store
        let cert_infos = cert_store
            .list()
            .await
            .map_err(|e| ProxyError::Tls(format!("failed to list certs: {e}")))?;
        for info in &cert_infos {
            if let Ok(Some(pair)) = cert_store.load(&info.hostname).await
                && let Ok(ck) = cert_pair_to_certified_key(&pair)
            {
                cert_cache.insert(info.hostname.clone(), ck);
            }
        }

        // Build rustls config with SNI resolver
        let resolver = Arc::new(SniByCertCache::new(Arc::clone(&cert_cache)));
        let tls_config = ServerConfig::builder().with_no_client_auth().with_cert_resolver(resolver);
        let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

        // Bind listeners
        let http_listener = TcpListener::bind(config.http_addr)
            .await
            .map_err(|e| ProxyError::Bind { address: config.http_addr.to_string(), source: e })?;
        let https_listener = TcpListener::bind(config.https_addr)
            .await
            .map_err(|e| ProxyError::Bind { address: config.https_addr.to_string(), source: e })?;

        let http_addr = http_listener.local_addr()?;
        let https_addr = https_listener.local_addr()?;

        // Set up ACME client if configured
        let acme_client = if let Some(acme_config) = &config.acme {
            let saved = cert_store.load_acme_credentials().await.unwrap_or(None);
            match AcmeClient::new(acme_config, saved.as_deref()).await {
                Ok((client, new_creds)) => {
                    if let Some(json) = new_creds
                        && let Err(e) = cert_store.save_acme_credentials(&json).await
                    {
                        tracing::warn!("failed to persist ACME credentials: {e}");
                    }
                    tracing::info!("ACME client initialized");
                    Some(Arc::new(client))
                }
                Err(e) => {
                    tracing::warn!("failed to initialize ACME client, continuing without: {e}");
                    None
                }
            }
        } else {
            None
        };

        let challenge_store =
            acme_client.as_ref().map(|c| c.challenge_store().clone()).unwrap_or_default();

        let routes = Arc::new(SharedRouteTable::new());
        let metrics = ProxyMetrics::new();
        let client: Arc<HttpClient> = Arc::new(Client::builder(TokioExecutor::new()).build_http());
        let cancel = CancellationToken::new();

        // Spawn HTTP server
        let http_task = {
            let routes = Arc::clone(&routes);
            let cert_cache = Arc::clone(&cert_cache);
            let challenge_store = challenge_store.clone();
            let metrics = Arc::clone(&metrics);
            let client = Arc::clone(&client);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                proxy::http::serve_http(
                    http_listener,
                    routes,
                    cert_cache,
                    challenge_store,
                    metrics,
                    client,
                    cancel,
                )
                .await;
            })
        };

        // Spawn HTTPS server
        let https_task = {
            let routes = Arc::clone(&routes);
            let metrics = Arc::clone(&metrics);
            let client = Arc::clone(&client);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                proxy::https::serve_https(
                    https_listener,
                    tls_acceptor,
                    routes,
                    metrics,
                    client,
                    cancel,
                )
                .await;
            })
        };

        // Spawn cert watcher: watches for new hostnames, acquires certs
        let cert_task = {
            let cert_store = Arc::clone(&cert_store);
            let cert_cache = Arc::clone(&cert_cache);
            let acme_client = acme_client.clone();
            let mut route_rx = routes.subscribe();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        result = route_rx.changed() => {
                            if result.is_err() {
                                break;
                            }
                            let table = route_rx.borrow_and_update().clone();
                            for hostname in table.hostnames() {
                                if cert_cache.has_cert(&hostname) {
                                    continue;
                                }
                                // Try to load from store first
                                if let Ok(Some(pair)) = cert_store.load(&hostname).await
                                    && let Ok(ck) = cert_pair_to_certified_key(&pair)
                                {
                                    cert_cache.insert(hostname.clone(), ck);
                                    continue;
                                }
                                // Try ACME if available
                                if let Some(acme) = &acme_client {
                                    match acme.acquire_certificate(&hostname).await {
                                        Ok(pair) => {
                                            let _ = cert_store.save(&hostname, &pair).await;
                                            if let Ok(ck) = cert_pair_to_certified_key(&pair) {
                                                cert_cache.insert(hostname.clone(), ck);
                                                tracing::info!(hostname = %hostname, "acquired cert via ACME");
                                                continue;
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(hostname = %hostname, "ACME failed, falling back to self-signed: {e}");
                                        }
                                    }
                                }
                                // Generate self-signed fallback
                                match generate_self_signed(&hostname) {
                                    Ok(pair) => {
                                        let _ = cert_store.save(&hostname, &pair).await;
                                        if let Ok(ck) = cert_pair_to_certified_key(&pair) {
                                            cert_cache.insert(hostname.clone(), ck);
                                            tracing::info!(hostname = %hostname, "generated self-signed cert");
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(hostname = %hostname, "failed to generate cert: {e}");
                                    }
                                }
                            }
                        }
                        _ = cancel.cancelled() => break,
                    }
                }
            })
        };

        // Spawn renewal loop if ACME is enabled
        let renewal_task = acme_client.map(|acme| {
            let cert_store = Arc::clone(&cert_store);
            let cert_cache = Arc::clone(&cert_cache);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                acme::renewal_loop(&acme, &*cert_store, &cert_cache, cancel).await;
            })
        });

        Ok(Self {
            http_addr,
            https_addr,
            routes,
            cert_cache,
            challenge_store,
            metrics,
            cancel,
            http_task,
            https_task,
            cert_task,
            renewal_task,
        })
    }

    /// Replace the route table atomically.
    ///
    /// In-flight requests complete against the old table; new requests
    /// use the updated routes.
    pub fn update_routes(&self, routes: &[ProxyRoute]) {
        self.routes.update(routes);
    }

    /// The address the HTTP server is listening on.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// The address the HTTPS server is listening on.
    pub fn https_addr(&self) -> SocketAddr {
        self.https_addr
    }

    /// Reference to the certificate cache (for external cert loading).
    pub fn cert_cache(&self) -> &Arc<CertCache> {
        &self.cert_cache
    }

    /// Reference to the ACME challenge store.
    pub fn challenge_store(&self) -> &ChallengeStore {
        &self.challenge_store
    }

    /// Reference to the proxy request metrics.
    pub fn metrics(&self) -> &Arc<ProxyMetrics> {
        &self.metrics
    }

    /// Shut down the proxy server and wait for all tasks to exit.
    pub async fn stop(self) -> Result<()> {
        self.cancel.cancel();
        let _ = self.http_task.await;
        let _ = self.https_task.await;
        // Abort the cert watcher — it may be blocked on a slow CertStore
        // operation that doesn't respect the cancellation token.
        self.cert_task.abort();
        let _ = self.cert_task.await;
        if let Some(renewal_task) = self.renewal_task {
            renewal_task.abort();
            let _ = renewal_task.await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A no-op CertStore for testing.
    struct NullCertStore;

    impl CertStore for NullCertStore {
        async fn load(&self, _hostname: &str) -> Result<Option<CertPair>> {
            Ok(None)
        }
        async fn save(&self, _hostname: &str, _pair: &CertPair) -> Result<()> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<CertInfo>> {
            Ok(vec![])
        }
        async fn load_acme_credentials(&self) -> Result<Option<String>> {
            Ok(None)
        }
        async fn save_acme_credentials(&self, _json: &str) -> Result<()> {
            Ok(())
        }
    }

    /// A CertStore that delays load/save, giving tests time to exercise
    /// the HTTP proxy path before certs are generated.
    struct SlowCertStore;

    impl CertStore for SlowCertStore {
        async fn load(&self, _hostname: &str) -> Result<Option<CertPair>> {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(None)
        }
        async fn save(&self, _hostname: &str, _pair: &CertPair) -> Result<()> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<CertInfo>> {
            Ok(vec![])
        }
        async fn load_acme_credentials(&self) -> Result<Option<String>> {
            Ok(None)
        }
        async fn save_acme_credentials(&self, _json: &str) -> Result<()> {
            Ok(())
        }
    }

    fn localhost_config() -> ProxyConfig {
        ProxyConfig {
            http_addr: "127.0.0.1:0".parse().unwrap(),
            https_addr: "127.0.0.1:0".parse().unwrap(),
            acme: None,
        }
    }

    #[tokio::test]
    async fn start_and_stop() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await;
        assert!(server.is_ok());
        let server = server.unwrap();
        let result = server.stop().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_routes_lifecycle() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await.unwrap();

        let routes = vec![ProxyRoute {
            hostname: "app.test".to_string(),
            path: "/".to_string(),
            backend: "127.0.0.1:9999".parse().unwrap(),
            websocket: false,
        }];

        server.update_routes(&routes);

        // Give cert watcher a moment to generate certs
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result = server.stop().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn healthz_returns_200() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await.unwrap();

        let http_addr = server.http_addr();

        // Send a raw HTTP request for /healthz
        let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

        let request = "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 4096];
        let n = stream.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("200 OK"), "expected 200 OK, got: {response_str}");
        assert!(response_str.contains("ok\n"), "expected body 'ok\\n', got: {response_str}");

        let _ = server.stop().await;
    }

    #[tokio::test]
    async fn unknown_host_returns_404() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await.unwrap();

        let http_addr = server.http_addr();

        let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

        let request = "GET / HTTP/1.1\r\nHost: unknown.example.com\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 4096];
        let n = stream.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("404 Not Found"), "expected 404, got: {response_str}");

        let _ = server.stop().await;
    }

    #[tokio::test]
    async fn proxy_to_mock_backend() {
        // Start a mock backend server
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();

        let backend_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        // Use SlowCertStore so the cert watcher doesn't generate certs
        // before we can test the HTTP proxy path.
        let server = ProxyServer::start(localhost_config(), SlowCertStore).await.unwrap();

        server.update_routes(&[ProxyRoute {
            hostname: "app.test".to_string(),
            path: "/".to_string(),
            backend: backend_addr,
            websocket: false,
        }]);

        let http_addr = server.http_addr();

        let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

        let request = "GET /page HTTP/1.1\r\nHost: app.test\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("200 OK"), "expected 200 from backend, got: {response_str}");
        assert!(response_str.contains("hello"), "expected 'hello' body, got: {response_str}");

        let _ = server.stop().await;
        let _ = backend_task.await;
    }

    #[tokio::test]
    async fn https_redirect_when_cert_exists() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await.unwrap();

        // Manually insert a cert so the redirect logic triggers
        let pair = crate::tls::selfsigned::generate_self_signed("secure.test").unwrap();
        let ck = crate::tls::selfsigned::cert_pair_to_certified_key(&pair).unwrap();
        server.cert_cache().insert("secure.test".to_string(), ck);

        let http_addr = server.http_addr();

        let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

        let request = "GET /foo HTTP/1.1\r\nHost: secure.test\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 4096];
        let n = stream.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("308"), "expected 308 redirect, got: {response_str}");
        assert!(
            response_str.contains("https://secure.test/foo"),
            "expected redirect location, got: {response_str}"
        );

        let _ = server.stop().await;
    }

    #[tokio::test]
    async fn proxy_headers_forwarded_correctly() {
        // Start a mock backend that echoes headers in response body
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();

        let backend_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

                // Echo the relevant headers back in the response body
                let body = request_text;
                let response =
                    format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let server = ProxyServer::start(localhost_config(), SlowCertStore).await.unwrap();

        server.update_routes(&[ProxyRoute {
            hostname: "app.test".to_string(),
            path: "/".to_string(),
            backend: backend_addr,
            websocket: false,
        }]);

        let http_addr = server.http_addr();

        let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

        // Send request with pre-existing X-Forwarded-For
        let request = "GET /test HTTP/1.1\r\nHost: app.test\r\nX-Forwarded-For: 1.2.3.4\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        // Backend should have received appended X-Forwarded-For
        assert!(
            response_str.contains("x-forwarded-for: 1.2.3.4, 127.0.0.1"),
            "expected appended x-forwarded-for, got: {response_str}"
        );
        // Backend should have received X-Forwarded-Host
        assert!(
            response_str.contains("x-forwarded-host: app.test"),
            "expected x-forwarded-host, got: {response_str}"
        );
        // Backend should have received X-Forwarded-Proto
        assert!(
            response_str.contains("x-forwarded-proto: http"),
            "expected x-forwarded-proto, got: {response_str}"
        );

        let _ = server.stop().await;
        let _ = backend_task.await;
    }

    #[tokio::test]
    async fn acme_challenge_intercept() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await.unwrap();

        // Insert a challenge token into the server's store
        server
            .challenge_store()
            .insert("test-token-123".to_string(), "test-key-auth-456".to_string());

        let http_addr = server.http_addr();

        // Matching challenge token should return 200 with key authorization
        {
            let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

            let request = "GET /.well-known/acme-challenge/test-token-123 HTTP/1.1\r\nHost: example.com\r\n\r\n";
            stream.write_all(request.as_bytes()).await.unwrap();

            let mut response = vec![0u8; 4096];
            let n = stream.read(&mut response).await.unwrap();
            let response_str = String::from_utf8_lossy(&response[..n]);

            assert!(
                response_str.contains("200 OK"),
                "expected 200 for valid challenge token, got: {response_str}"
            );
            assert!(
                response_str.contains("test-key-auth-456"),
                "expected key authorization body, got: {response_str}"
            );
        }

        // Non-matching challenge token should fall through to 404
        {
            let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();

            let request =
                "GET /.well-known/acme-challenge/nonexistent HTTP/1.1\r\nHost: example.com\r\n\r\n";
            stream.write_all(request.as_bytes()).await.unwrap();

            let mut response = vec![0u8; 4096];
            let n = stream.read(&mut response).await.unwrap();
            let response_str = String::from_utf8_lossy(&response[..n]);

            assert!(
                response_str.contains("404 Not Found"),
                "expected 404 for unknown challenge token, got: {response_str}"
            );
        }

        let _ = server.stop().await;
    }

    #[tokio::test]
    async fn websocket_proxy_round_trip() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        // Start a mock WS backend (echo server)
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();

        let ws_task = tokio::spawn(async move {
            if let Ok((stream, _)) = ws_listener.accept().await {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                // Echo one message back
                if let Some(Ok(msg)) = ws.next().await {
                    let _ = ws.send(msg).await;
                }
            }
        });

        let server = ProxyServer::start(localhost_config(), SlowCertStore).await.unwrap();

        server.update_routes(&[ProxyRoute {
            hostname: "ws.test".to_string(),
            path: "/".to_string(),
            backend: ws_addr,
            websocket: true,
        }]);

        let http_addr = server.http_addr();

        // Connect via tokio-tungstenite through the proxy
        let url = format!("ws://{http_addr}/echo");
        let ws_req = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(&url)
            .header("Host", "ws.test")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .unwrap();

        let tcp = tokio::net::TcpStream::connect(http_addr).await.unwrap();

        let (mut ws_stream, _response) =
            tokio_tungstenite::client_async(ws_req, tcp).await.unwrap();

        // Send a message
        ws_stream.send(Message::Text("hello proxy".into())).await.unwrap();

        // Receive echo
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(reply, Message::Text("hello proxy".into()));

        let _ = server.stop().await;
        let _ = ws_task.await;
    }

    #[tokio::test]
    async fn tls_proxy_serves_traffic() {
        let server = ProxyServer::start(localhost_config(), NullCertStore).await.unwrap();

        // Generate and insert a self-signed cert
        let pair = crate::tls::selfsigned::generate_self_signed("tls.test").unwrap();
        let ck = crate::tls::selfsigned::cert_pair_to_certified_key(&pair).unwrap();
        server.cert_cache().insert("tls.test".to_string(), ck);

        // Start a mock HTTP backend
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();

        let backend_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\nsecure";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        server.update_routes(&[ProxyRoute {
            hostname: "tls.test".to_string(),
            path: "/".to_string(),
            backend: backend_addr,
            websocket: false,
        }]);

        let https_addr = server.https_addr();

        // Build rustls ClientConfig trusting our self-signed cert
        let mut root_store = rustls::RootCertStore::empty();
        for cert_der in &pair.certs {
            root_store.add(cert_der.clone()).unwrap();
        }
        let tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls_config));

        // Connect to the proxy's HTTPS port
        let tcp = tokio::net::TcpStream::connect(https_addr).await.unwrap();
        let domain = rustls_pki_types::ServerName::try_from("tls.test").unwrap();
        let mut tls_stream = connector.connect(domain, tcp).await.unwrap();

        // Send an HTTP request over the TLS connection
        let request = "GET /secure HTTP/1.1\r\nHost: tls.test\r\n\r\n";
        tls_stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 4096];
        let n =
            tokio::time::timeout(std::time::Duration::from_secs(5), tls_stream.read(&mut response))
                .await
                .unwrap()
                .unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("200 OK"), "expected 200, got: {response_str}");
        assert!(response_str.contains("secure"), "expected 'secure' body, got: {response_str}");

        let _ = server.stop().await;
        let _ = backend_task.await;
    }
}
