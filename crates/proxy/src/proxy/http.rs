use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::StatusCode;
use http_body_util::{Either, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::acme::ChallengeStore;
use crate::certs::cache::CertCache;
use crate::proxy::forward::{
    HttpClient, extract_host, healthz_response, https_redirect, proxy_to_backend, simple_response,
};
use crate::proxy::metrics::ProxyMetrics;
use crate::proxy::routes::SharedRouteTable;

type HttpResponse = Response<Either<Full<Bytes>, Incoming>>;

/// Accept loop for the HTTP (port 80) server.
///
/// Priority: ACME challenge -> healthz -> HTTPS redirect (if cert exists) -> proxy -> 404.
pub async fn serve_http(
    listener: TcpListener,
    routes: Arc<SharedRouteTable>,
    cert_cache: Arc<CertCache>,
    challenge_store: ChallengeStore,
    metrics: Arc<ProxyMetrics>,
    client: Arc<HttpClient>,
    cancel: CancellationToken,
) {
    loop {
        let (stream, client_addr) = tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("http accept error: {e}");
                        continue;
                    }
                }
            }
            _ = cancel.cancelled() => break,
        };

        let routes = Arc::clone(&routes);
        let cert_cache = Arc::clone(&cert_cache);
        let challenge_store = challenge_store.clone();
        let metrics = Arc::clone(&metrics);
        let client = Arc::clone(&client);
        let cancel = cancel.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let routes = Arc::clone(&routes);
                let cert_cache = Arc::clone(&cert_cache);
                let challenge_store = challenge_store.clone();
                let metrics = Arc::clone(&metrics);
                let client = Arc::clone(&client);
                async move {
                    handle_http_request(
                        req,
                        client_addr,
                        &routes,
                        &cert_cache,
                        &challenge_store,
                        &metrics,
                        &client,
                    )
                    .await
                }
            });

            let builder = ServerBuilder::new(TokioExecutor::new());
            let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
            tokio::pin!(conn);

            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(e) = result {
                        tracing::debug!(addr = %client_addr, "http connection error: {e}");
                    }
                }
                _ = cancel.cancelled() => {
                    conn.as_mut().graceful_shutdown();
                    match tokio::time::timeout(Duration::from_secs(5), conn).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::debug!(addr = %client_addr, "http drain error: {e}"),
                        Err(_) => tracing::debug!(addr = %client_addr, "http drain timeout"),
                    }
                }
            }
        });
    }
}

async fn handle_http_request(
    req: Request<Incoming>,
    client_addr: SocketAddr,
    routes: &SharedRouteTable,
    cert_cache: &CertCache,
    challenge_store: &ChallengeStore,
    metrics: &ProxyMetrics,
    client: &HttpClient,
) -> std::result::Result<HttpResponse, std::convert::Infallible> {
    // 1. ACME challenge
    if let Some(token) = req.uri().path().strip_prefix("/.well-known/acme-challenge/")
        && let Some(key_auth) = challenge_store.get(token)
    {
        return Ok(simple_response(StatusCode::OK, &key_auth).map(Either::Left));
    }

    // 2. Health check
    if req.uri().path() == "/healthz" && req.method() == http::Method::GET {
        return Ok(healthz_response().map(Either::Left));
    }

    let host = extract_host(&req);

    let Some(hostname) = host else {
        return Ok(
            simple_response(StatusCode::BAD_REQUEST, "missing host header\n").map(Either::Left)
        );
    };

    // 3. HTTPS redirect if this hostname has a certificate
    if cert_cache.has_cert(&hostname) {
        let path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        return Ok(https_redirect(&hostname, path).map(Either::Left));
    }

    // 4. Route lookup and proxy
    let table = routes.current();
    proxy_to_backend(req, &hostname, &table, client, client_addr, false, metrics).await
}
