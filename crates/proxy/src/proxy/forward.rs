use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http::{HeaderValue, StatusCode, Uri};
use http_body_util::{Either, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;

use crate::error::{ProxyError, Result};
use crate::proxy::metrics::ProxyMetrics;
use crate::proxy::routes::RouteTable;
use crate::proxy::websocket::{handle_websocket_upgrade, is_upgrade_request};

/// Shared HTTP client type, reused across requests for connection pooling.
pub type HttpClient = Client<HttpConnector, Incoming>;

/// Forward an HTTP request to a backend service.
///
/// Rewrites the URI to point at the backend, sets standard proxy headers,
/// and strips hop-by-hop headers before forwarding.
pub async fn forward_request(
    mut req: hyper::Request<Incoming>,
    backend: SocketAddr,
    client_addr: SocketAddr,
    is_tls: bool,
    client: &HttpClient,
) -> Result<Response<Incoming>> {
    // Rewrite URI to backend
    let path_and_query = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let uri = format!("http://{backend}{path_and_query}")
        .parse::<Uri>()
        .map_err(|e| ProxyError::Http(Box::new(e)))?;
    *req.uri_mut() = uri;

    // Capture Host before hop-by-hop stripping
    let forwarded_host =
        req.headers().get("host").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    // Set proxy headers
    let client_ip = client_addr.ip().to_string();
    let xff_value = match req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(existing) => format!("{existing}, {client_ip}"),
        None => client_ip.clone(),
    };
    if let Ok(v) = HeaderValue::from_str(&xff_value) {
        req.headers_mut().insert("x-forwarded-for", v);
    }
    if let Ok(v) = HeaderValue::from_str(&client_ip) {
        req.headers_mut().insert("x-real-ip", v);
    }
    let proto = if is_tls { "https" } else { "http" };
    if let Ok(v) = HeaderValue::from_str(proto) {
        req.headers_mut().insert("x-forwarded-proto", v);
    }
    if let Some(host) = &forwarded_host
        && let Ok(v) = HeaderValue::from_str(host)
    {
        req.headers_mut().insert("x-forwarded-host", v);
    }

    // Strip hop-by-hop headers
    for name in &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ] {
        req.headers_mut().remove(*name);
    }

    client.request(req).await.map_err(|e| ProxyError::BackendConnect {
        backend: backend.to_string(),
        reason: e.to_string(),
    })
}

/// Build a simple response with the given status and body text.
pub fn simple_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("internal error"))))
}

/// Build a 308 redirect to the HTTPS version of the request.
pub fn https_redirect(host: &str, path: &str) -> Response<Full<Bytes>> {
    let location = format!("https://{host}{path}");
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header("location", location)
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// Build a 200 OK response for the health check endpoint.
pub fn healthz_response() -> Response<Full<Bytes>> {
    simple_response(StatusCode::OK, "ok\n")
}

/// Shared proxy-to-backend handler used by both HTTP and HTTPS servers.
///
/// Route lookup → websocket check → forward_request → metrics → error handling → 404.
pub(crate) async fn proxy_to_backend(
    req: Request<Incoming>,
    hostname: &str,
    route_table: &RouteTable,
    client: &HttpClient,
    client_addr: SocketAddr,
    is_tls: bool,
    metrics: &ProxyMetrics,
) -> std::result::Result<Response<Either<Full<Bytes>, Incoming>>, Infallible> {
    let path = req.uri().path();
    if let Some(route_match) = route_table.lookup(hostname, path) {
        let start = std::time::Instant::now();

        if route_match.websocket && is_upgrade_request(&req) {
            match handle_websocket_upgrade(req, route_match.backend, client_addr).await {
                Ok(resp) => {
                    metrics.record(hostname, &route_match.path, start.elapsed());
                    return Ok(resp.map(Either::Left));
                }
                Err(e) => {
                    metrics.record(hostname, &route_match.path, start.elapsed());
                    tracing::debug!(addr = %client_addr, "websocket upgrade failed: {e}");
                    return Ok(simple_response(
                        StatusCode::BAD_GATEWAY,
                        "websocket upgrade failed\n",
                    )
                    .map(Either::Left));
                }
            }
        }

        match forward_request(req, route_match.backend, client_addr, is_tls, client).await {
            Ok(resp) => {
                metrics.record(hostname, &route_match.path, start.elapsed());
                return Ok(resp.map(Either::Right));
            }
            Err(e) => {
                metrics.record(hostname, &route_match.path, start.elapsed());
                tracing::debug!(addr = %client_addr, "backend error: {e}");
                return Ok(simple_response(StatusCode::BAD_GATEWAY, "backend unavailable\n")
                    .map(Either::Left));
            }
        }
    }

    Ok(simple_response(StatusCode::NOT_FOUND, "not found\n").map(Either::Left))
}

/// Extract the hostname from a request, trying the Host header first
/// (stripping any port), then falling back to the URI authority.
pub(crate) fn extract_host(req: &hyper::Request<Incoming>) -> Option<String> {
    if let Some(host) = req.headers().get("host").and_then(|v| v.to_str().ok()) {
        let hostname = host.split(':').next().unwrap_or(host);
        if !hostname.is_empty() {
            return Some(hostname.to_string());
        }
    }
    req.uri().host().map(|h| h.to_string())
}
