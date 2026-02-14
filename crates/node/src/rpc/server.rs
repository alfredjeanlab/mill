use std::net::SocketAddr;
use std::sync::Arc;

use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::NodeError;
use crate::rpc::codec::{read_frame, write_frame};
use crate::rpc::protocol::{AuthRequest, AuthResponse};
use crate::rpc::{NodeCommand, NodeReport};
use crate::secondary::SecondaryHandle;

/// TCP server that bridges a remote primary to a local secondary.
///
/// Accepts one connection at a time (one primary per secondary). If a new
/// connection arrives, the old one is dropped (primary reconnected).
pub struct RpcServer;

impl RpcServer {
    /// Start the RPC server, listening for primary connections.
    ///
    /// Consumes the `SecondaryHandle` — commands received over TCP are forwarded
    /// to the secondary's command channel, and reports from the secondary are
    /// sent back over TCP.
    pub async fn start(
        bind_addr: SocketAddr,
        auth_token: String,
        handle: SecondaryHandle,
        cancel: CancellationToken,
    ) -> Result<(), NodeError> {
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(|source| NodeError::Bind { address: bind_addr.to_string(), source })?;

        tracing::info!("rpc server listening on {bind_addr}");
        Self::run_accept_loop(listener, auth_token, handle, cancel).await
    }

    /// Start the RPC server with a pre-bound listener (for testing).
    pub async fn start_with_listener(
        listener: TcpListener,
        auth_token: String,
        handle: SecondaryHandle,
        cancel: CancellationToken,
    ) -> Result<(), NodeError> {
        Self::run_accept_loop(listener, auth_token, handle, cancel).await
    }

    async fn run_accept_loop(
        listener: TcpListener,
        auth_token: String,
        handle: SecondaryHandle,
        cancel: CancellationToken,
    ) -> Result<(), NodeError> {
        let cmd_tx = handle.cmd_tx().clone();
        // Wrapped in Arc so spawned connection handlers can share it. The
        // inner Mutex ensures only one handler relays reports at a time;
        // a new connection takes over once the old handler releases the lock.
        let report_rx = Arc::new(tokio::sync::Mutex::new(handle.into_report_rx()));

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    let (stream, peer) = match accept {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("rpc accept error: {e}");
                            continue;
                        }
                    };
                    tracing::info!("rpc connection from {peer}");

                    let auth = auth_token.clone();
                    let tx = cmd_tx.clone();
                    let rx = Arc::clone(&report_rx);
                    let conn_cancel = cancel.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(
                            stream, &auth, &tx, &rx, conn_cancel,
                        ).await {
                            tracing::warn!("rpc connection from {peer} failed: {e}");
                        }
                    });
                }
                _ = cancel.cancelled() => {
                    tracing::info!("rpc server shutting down");
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    auth_token: &str,
    cmd_tx: &mpsc::Sender<NodeCommand>,
    report_rx: &tokio::sync::Mutex<mpsc::Receiver<NodeReport>>,
    cancel: CancellationToken,
) -> Result<(), NodeError> {
    // Authenticate.
    let auth: AuthRequest = read_frame(&mut stream)
        .await
        .map_err(|e| NodeError::BadRequest(format!("auth read: {e}")))?
        .ok_or_else(|| NodeError::BadRequest("connection closed before auth".into()))?;

    let token_ok: bool = auth.token.as_bytes().ct_eq(auth_token.as_bytes()).into();

    if !token_ok {
        let _ = write_frame(&mut stream, &AuthResponse { ok: false }).await;
        return Err(NodeError::BadRequest("invalid auth token".into()));
    }
    write_frame(&mut stream, &AuthResponse { ok: true })
        .await
        .map_err(|e| NodeError::BadRequest(format!("auth write: {e}")))?;

    // Split into read/write halves.
    let (mut read_half, mut write_half) = stream.into_split();

    let cmd_tx = cmd_tx.clone();
    let conn_cancel = cancel.clone();

    // Reader task: TCP → cmd_tx
    let reader = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = read_frame::<_, NodeCommand>(&mut read_half) => {
                    match result {
                        Ok(Some(cmd)) => {
                            if cmd_tx.send(cmd).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break, // clean EOF
                        Err(e) => {
                            tracing::warn!("rpc read error: {e}");
                            break;
                        }
                    }
                }
                _ = conn_cancel.cancelled() => break,
            }
        }
    });

    // Writer task: report_rx → TCP
    let mut report_rx = report_rx.lock().await;
    loop {
        tokio::select! {
            report = report_rx.recv() => {
                match report {
                    Some(report) => {
                        if let Err(e) = write_frame(&mut write_half, &report).await {
                            tracing::warn!("rpc write error: {e}");
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = cancel.cancelled() => break,
        }
    }

    reader.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::AllocId;
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn auth_handshake_success() {
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (cmd_tx, _cmd_rx) = mpsc::channel(256);
        let report_rx = tokio::sync::Mutex::new(mpsc::channel::<NodeReport>(256).1);

        let server_cancel = cancel.clone();
        let cmd_tx_clone = cmd_tx.clone();
        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, "secret-token", &cmd_tx_clone, &report_rx, server_cancel)
                .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        write_frame(&mut client, &AuthRequest { token: "secret-token".into() }).await.unwrap();
        let resp: AuthResponse = read_frame(&mut client).await.unwrap().unwrap();
        assert!(resp.ok);

        cancel.cancel();
        drop(client);
    }

    #[tokio::test]
    async fn auth_rejection() {
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (cmd_tx, _cmd_rx) = mpsc::channel(256);
        let report_rx = tokio::sync::Mutex::new(mpsc::channel(256).1);

        let server_cancel = cancel.clone();
        let cmd_tx_clone = cmd_tx.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, "correct-token", &cmd_tx_clone, &report_rx, server_cancel)
                .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        write_frame(&mut client, &AuthRequest { token: "wrong-token".into() }).await.unwrap();
        let resp: Option<AuthResponse> = read_frame(&mut client).await.unwrap();
        assert!(resp.is_some());
        assert!(!resp.unwrap().ok);

        cancel.cancel();
        let result = server.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn command_forwarding_over_tcp() {
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (cmd_tx, mut cmd_rx) = mpsc::channel(256);
        let (report_tx, report_rx) = mpsc::channel::<NodeReport>(256);
        let report_rx = tokio::sync::Mutex::new(report_rx);

        let server_cancel = cancel.clone();
        let cmd_tx_clone = cmd_tx.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, "token", &cmd_tx_clone, &report_rx, server_cancel).await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();

        // Auth.
        write_frame(&mut client, &AuthRequest { token: "token".into() }).await.unwrap();
        let resp: AuthResponse = read_frame(&mut client).await.unwrap().unwrap();
        assert!(resp.ok);

        // Split client.
        let (mut client_read, mut client_write) = client.into_split();

        // Send a command.
        write_frame(&mut client_write, &NodeCommand::Stop { alloc_id: AllocId("a".into()) })
            .await
            .unwrap();

        // Verify it arrives.
        let cmd = cmd_rx.recv().await.unwrap();
        assert!(matches!(cmd, NodeCommand::Stop { alloc_id } if alloc_id.0 == "a"));

        // Send a report from the secondary side.
        report_tx
            .send(NodeReport::Stopped { alloc_id: AllocId("b".into()), exit_code: 0 })
            .await
            .unwrap();

        // Read it on the client side.
        let report: NodeReport = read_frame(&mut client_read).await.unwrap().unwrap();
        assert!(matches!(report, NodeReport::Stopped { exit_code: 0, .. }));

        cancel.cancel();
        drop(client_write);
        let _ = server.await;
    }
}
