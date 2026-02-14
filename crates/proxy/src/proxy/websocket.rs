use std::net::SocketAddr;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use hyper::{Request, Response};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::http as ts_http;

use crate::error::{ProxyError, Result};

/// Check whether an HTTP request is a WebSocket upgrade request.
pub fn is_upgrade_request(req: &Request<Incoming>) -> bool {
    req.headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Handle a WebSocket upgrade: connect to the backend, perform the
/// handshake, and spawn a relay task.
///
/// Returns a 101 Switching Protocols response to the client.
pub async fn handle_websocket_upgrade(
    req: Request<Incoming>,
    backend: SocketAddr,
    client_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>> {
    // Capture the client's Sec-WebSocket-Key before consuming the request.
    let client_key =
        req.headers().get("sec-websocket-key").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    // Connect to backend over plain TCP
    let backend_tcp = TcpStream::connect(backend).await.map_err(|e| {
        ProxyError::BackendConnect { backend: backend.to_string(), reason: e.to_string() }
    })?;

    // Build the backend WS request using ClientRequestBuilder so that
    // tungstenite generates required handshake headers (Key, Version, etc.)
    let path_and_query = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let ws_uri: ts_http::Uri = format!("ws://{backend}{path_and_query}")
        .parse()
        .map_err(|e: http::uri::InvalidUri| ProxyError::WebSocket(e.to_string()))?;

    let mut builder = ClientRequestBuilder::new(ws_uri);

    // Forward relevant headers from the client request.
    // Skip sec-websocket-key and sec-websocket-version — tungstenite generates
    // its own key/version for the backend handshake and validates against them.
    for (name, value) in req.headers() {
        let name_str = name.as_str();
        if name_str == "sec-websocket-key" || name_str == "sec-websocket-version" {
            continue;
        }
        if (name_str.starts_with("sec-websocket") || name_str == "origin")
            && let Ok(v) = value.to_str()
        {
            builder = builder.with_header(name_str, v);
        }
    }

    // Perform the backend WebSocket handshake
    let (backend_ws, backend_response) = tokio_tungstenite::client_async(builder, backend_tcp)
        .await
        .map_err(|e| ProxyError::WebSocket(e.to_string()))?;

    // Get the client's upgrade future
    let on_upgrade = hyper::upgrade::on(req);

    // Spawn the bidirectional relay
    tokio::spawn(relay_websocket(on_upgrade, backend_ws, client_addr));

    // Return 101 to the client with proper Sec-WebSocket-Accept
    let mut resp = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("upgrade", "websocket")
        .header("connection", "upgrade");

    if let Some(key) = &client_key {
        resp = resp.header("sec-websocket-accept", derive_accept_key(key.as_bytes()));
    }

    if let Some(protocol) = backend_response.headers().get("sec-websocket-protocol") {
        resp = resp.header("sec-websocket-protocol", protocol.to_str().unwrap_or_default());
    }

    resp.body(Full::new(Bytes::new())).map_err(|e| ProxyError::WebSocket(e.to_string()))
}

/// Relay WebSocket frames bidirectionally between client and backend.
async fn relay_websocket(
    on_upgrade: OnUpgrade,
    backend_ws: WebSocketStream<TcpStream>,
    client_addr: SocketAddr,
) {
    let upgraded = match on_upgrade.await {
        Ok(u) => u,
        Err(e) => {
            tracing::debug!(addr = %client_addr, "websocket upgrade failed: {e}");
            return;
        }
    };

    let client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;

    let (mut client_tx, mut client_rx) = client_ws.split();
    let (mut backend_tx, mut backend_rx) = backend_ws.split();

    let client_to_backend = async {
        while let Some(msg) = client_rx.next().await {
            match msg {
                Ok(m) => {
                    if backend_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    let backend_to_client = async {
        while let Some(msg) = backend_rx.next().await {
            match msg {
                Ok(m) => {
                    if client_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = client_to_backend => {}
        _ = backend_to_client => {}
    }
}
