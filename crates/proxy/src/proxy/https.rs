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
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::proxy::forward::{HttpClient, extract_host, proxy_to_backend, simple_response};
use crate::proxy::metrics::ProxyMetrics;
use crate::proxy::routes::SharedRouteTable;

type HttpsResponse = Response<Either<Full<Bytes>, Incoming>>;

/// Accept loop for the HTTPS (port 443) server.
///
/// Each connection goes through TLS handshake first, then HTTP serving.
pub async fn serve_https(
    listener: TcpListener,
    tls_acceptor: TlsAcceptor,
    routes: Arc<SharedRouteTable>,
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
                        tracing::debug!("https accept error: {e}");
                        continue;
                    }
                }
            }
            _ = cancel.cancelled() => break,
        };

        let acceptor = tls_acceptor.clone();
        let routes = Arc::clone(&routes);
        let metrics = Arc::clone(&metrics);
        let client = Arc::clone(&client);
        let cancel = cancel.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(addr = %client_addr, "tls handshake failed: {e}");
                    return;
                }
            };

            let service = service_fn(move |req: Request<Incoming>| {
                let routes = Arc::clone(&routes);
                let metrics = Arc::clone(&metrics);
                let client = Arc::clone(&client);
                async move { handle_https_request(req, client_addr, &routes, &metrics, &client).await }
            });

            let builder = ServerBuilder::new(TokioExecutor::new());
            let conn = builder.serve_connection_with_upgrades(TokioIo::new(tls_stream), service);
            tokio::pin!(conn);

            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(e) = result {
                        tracing::debug!(addr = %client_addr, "https connection error: {e}");
                    }
                }
                _ = cancel.cancelled() => {
                    conn.as_mut().graceful_shutdown();
                    match tokio::time::timeout(Duration::from_secs(5), conn).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::debug!(addr = %client_addr, "https drain error: {e}"),
                        Err(_) => tracing::debug!(addr = %client_addr, "https drain timeout"),
                    }
                }
            }
        });
    }
}

async fn handle_https_request(
    req: Request<Incoming>,
    client_addr: SocketAddr,
    routes: &SharedRouteTable,
    metrics: &ProxyMetrics,
    client: &HttpClient,
) -> std::result::Result<HttpsResponse, std::convert::Infallible> {
    let host = extract_host(&req);

    let Some(hostname) = host else {
        return Ok(
            simple_response(StatusCode::BAD_REQUEST, "missing host header\n").map(Either::Left)
        );
    };

    let table = routes.current();
    proxy_to_backend(req, &hostname, &table, client, client_addr, true, metrics).await
}
