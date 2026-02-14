use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use mill_config::NodeId;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::NodeError;
use crate::rpc::codec::{read_frame, write_frame};
use crate::rpc::protocol::{AuthRequest, AuthResponse};
use crate::rpc::{NodeCommand, NodeReport};

/// Routes commands to the correct node's channel (local or remote).
///
/// Local and remote secondaries look identical to the router — both are just
/// `mpsc::Sender<NodeCommand>`. The local secondary's sender is registered
/// directly; a remote connection creates an mpsc pair where the sender goes
/// into the router and the receiver is drained by a TCP writer task.
#[derive(Clone)]
pub struct CommandRouter {
    inner: Arc<RwLock<HashMap<NodeId, mpsc::Sender<NodeCommand>>>>,
    /// Cancel tokens for remote connections (keyed by node_id).
    connections: Arc<RwLock<HashMap<NodeId, CancellationToken>>>,
}

impl Default for CommandRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRouter {
    /// Create an empty router.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a command sender for a node (local secondaries).
    pub async fn register(&self, node_id: NodeId, tx: mpsc::Sender<NodeCommand>) {
        self.inner.write().await.insert(node_id, tx);
    }

    /// Remove a node's sender.
    pub async fn deregister(&self, node_id: &NodeId) {
        self.inner.write().await.remove(node_id);
    }

    /// Send a command to a specific node.
    pub async fn send(&self, node_id: &NodeId, cmd: NodeCommand) -> Result<(), NodeError> {
        let inner = self.inner.read().await;
        let tx = inner.get(node_id).ok_or_else(|| NodeError::NodeUnreachable(node_id.0.clone()))?;
        tx.send(cmd).await.map_err(|_| NodeError::ChannelClosed)
    }

    /// Broadcast a command to all registered nodes.
    ///
    /// Continues sending to remaining nodes even if some channels are closed.
    /// Returns an error if any send failed (needed for callers that use `let _ =`).
    pub async fn broadcast(&self, cmd: NodeCommand) -> Result<(), NodeError> {
        let inner = self.inner.read().await;
        let mut last_err = None;
        for (node_id, tx) in inner.iter() {
            if tx.send(cmd.clone()).await.is_err() {
                tracing::warn!("broadcast: channel closed for {}", node_id.0);
                last_err = Some(NodeError::ChannelClosed);
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Connect to a remote secondary over TCP.
    ///
    /// Performs auth handshake, then spawns reader/writer tasks. Commands sent
    /// through `self.send(node_id, ...)` are forwarded over TCP. Reports from
    /// the remote secondary are forwarded to `report_tx`.
    ///
    /// On TCP error, spawns a reconnect loop with exponential backoff.
    pub async fn connect(
        &self,
        node_id: NodeId,
        addr: SocketAddr,
        token: String,
        report_tx: mpsc::Sender<NodeReport>,
        cancel: CancellationToken,
    ) -> Result<(), NodeError> {
        let timeout = crate::env::rpc_connect_timeout();
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| NodeError::Bind {
                address: addr.to_string(),
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, "rpc connect timeout"),
            })?
            .map_err(|e| NodeError::Bind { address: addr.to_string(), source: e })?;

        let (read_half, write_half) = do_auth(stream, &token).await?;
        let params = ConnParams { router: self.clone(), node_id, addr, token, report_tx, cancel };
        wire_connection(params, read_half, write_half).await;
        Ok(())
    }

    /// Number of registered routes (local + remote).
    ///
    /// Non-async so callers can use it inside `poll()` conditions.
    pub fn route_count(&self) -> usize {
        self.inner.try_read().map(|g| g.len()).unwrap_or(0)
    }

    /// Disconnect from a remote secondary. Cancels the reader/writer tasks and
    /// deregisters the node.
    pub async fn disconnect(&self, node_id: &NodeId) {
        let cancel = self.connections.write().await.remove(node_id);
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        self.deregister(node_id).await;
    }
}

/// Shared parameters for establishing and re-establishing a connection.
struct ConnParams {
    router: CommandRouter,
    node_id: NodeId,
    addr: SocketAddr,
    token: String,
    report_tx: mpsc::Sender<NodeReport>,
    cancel: CancellationToken,
}

/// Register channels and spawn reader/writer tasks for an authenticated connection.
///
/// Returns a boxed future to break the recursive type cycle with `reconnect_loop`.
fn wire_connection(
    params: ConnParams,
    read_half: OwnedReadHalf,
    write_half: OwnedWriteHalf,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        // Per-connection command channel.
        let (cmd_tx, cmd_rx) = mpsc::channel::<NodeCommand>(256);

        // Register sender and connection cancel token.
        params.router.inner.write().await.insert(params.node_id.clone(), cmd_tx);
        let conn_cancel = CancellationToken::new();
        params.router.connections.write().await.insert(params.node_id.clone(), conn_cancel.clone());

        // Spawn writer task.
        let writer_cancel = conn_cancel.clone();
        let writer_node = params.node_id.clone();
        let writer_router = params.router.clone();
        let writer_report_tx = params.report_tx.clone();
        let writer_addr = params.addr;
        let writer_token = params.token.clone();
        let writer_outer_cancel = params.cancel.clone();
        tokio::spawn(async move {
            let disconnected = writer_loop(write_half, cmd_rx, writer_cancel).await;
            if disconnected {
                tracing::warn!("rpc connection to {} lost, will reconnect", writer_node.0);
                writer_router.deregister(&writer_node).await;
                tokio::spawn(reconnect_loop(ConnParams {
                    router: writer_router,
                    node_id: writer_node,
                    addr: writer_addr,
                    token: writer_token,
                    report_tx: writer_report_tx,
                    cancel: writer_outer_cancel,
                }));
            }
        });

        // Spawn reader task.
        let reader_cancel = conn_cancel.clone();
        tokio::spawn(async move {
            reader_loop(read_half, params.report_tx, reader_cancel).await;
        });
    })
}

/// Perform auth handshake and return split stream halves.
async fn do_auth(
    mut stream: TcpStream,
    token: &str,
) -> Result<(OwnedReadHalf, OwnedWriteHalf), NodeError> {
    write_frame(&mut stream, &AuthRequest { token: token.to_owned() })
        .await
        .map_err(|e| NodeError::BadRequest(format!("rpc auth write: {e}")))?;
    let resp: AuthResponse = read_frame(&mut stream)
        .await
        .map_err(|e| NodeError::BadRequest(format!("rpc auth read: {e}")))?
        .ok_or_else(|| NodeError::BadRequest("rpc: connection closed during auth".into()))?;
    if !resp.ok {
        return Err(NodeError::BadRequest("rpc: auth rejected".into()));
    }
    Ok(stream.into_split())
}

async fn writer_loop(
    mut write_half: OwnedWriteHalf,
    mut cmd_rx: mpsc::Receiver<NodeCommand>,
    cancel: CancellationToken,
) -> bool {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        if let Err(e) = write_frame(&mut write_half, &cmd).await {
                            tracing::warn!("rpc write error: {e}");
                            return true; // disconnected
                        }
                    }
                    None => return false, // channel closed normally
                }
            }
            _ = cancel.cancelled() => return false,
        }
    }
}

async fn reader_loop(
    mut read_half: OwnedReadHalf,
    report_tx: mpsc::Sender<NodeReport>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            result = read_frame::<_, NodeReport>(&mut read_half) => {
                match result {
                    Ok(Some(report)) => {
                        if report_tx.send(report).await.is_err() {
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
            _ = cancel.cancelled() => break,
        }
    }
}

async fn reconnect_loop(params: ConnParams) -> Result<(), NodeError> {
    let mut delay = Duration::from_secs(1);
    let max_delay = Duration::from_secs(30);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = params.cancel.cancelled() => return Ok(()),
        }

        let timeout = crate::env::rpc_connect_timeout();
        let stream = match tokio::time::timeout(timeout, TcpStream::connect(params.addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!("rpc reconnect to {} failed: {e}", params.node_id.0);
                delay = (delay * 2).min(max_delay);
                continue;
            }
            Err(_) => {
                tracing::warn!("rpc reconnect to {} timed out", params.node_id.0);
                delay = (delay * 2).min(max_delay);
                continue;
            }
        };

        match do_auth(stream, &params.token).await {
            Ok((read_half, write_half)) => {
                tracing::info!("rpc reconnected to {}", params.node_id.0);
                wire_connection(params, read_half, write_half).await;
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("rpc reconnect auth to {} failed: {e}", params.node_id.0);
            }
        }

        delay = (delay * 2).min(max_delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::AllocId;

    use crate::rpc::server::RpcServer;
    use crate::secondary::SecondaryHandle;

    #[tokio::test]
    async fn register_and_send() {
        let router = CommandRouter::new();
        let (tx, mut rx) = mpsc::channel(16);
        let node = NodeId("node-1".into());
        router.register(node.clone(), tx).await;

        let cmd = NodeCommand::Stop { alloc_id: AllocId("alloc-1".into()) };
        router.send(&node, cmd).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert!(matches!(received, NodeCommand::Stop { alloc_id } if alloc_id.0 == "alloc-1"));
    }

    #[tokio::test]
    async fn send_to_unknown_node_returns_unreachable() {
        let router = CommandRouter::new();
        let node = NodeId("ghost".into());
        let cmd = NodeCommand::Stop { alloc_id: AllocId("x".into()) };
        let err = router.send(&node, cmd).await.unwrap_err();
        assert!(matches!(err, NodeError::NodeUnreachable(ref s) if s == "ghost"));
    }

    #[tokio::test]
    async fn deregister_removes_node() {
        let router = CommandRouter::new();
        let (tx, _rx) = mpsc::channel(16);
        let node = NodeId("node-1".into());
        router.register(node.clone(), tx).await;
        router.deregister(&node).await;

        let cmd = NodeCommand::Stop { alloc_id: AllocId("x".into()) };
        let err = router.send(&node, cmd).await.unwrap_err();
        assert!(matches!(err, NodeError::NodeUnreachable(_)));
    }

    #[tokio::test]
    async fn broadcast_sends_to_all() {
        let router = CommandRouter::new();
        let (tx1, mut rx1) = mpsc::channel(16);
        let (tx2, mut rx2) = mpsc::channel(16);
        router.register(NodeId("n1".into()), tx1).await;
        router.register(NodeId("n2".into()), tx2).await;

        let cmd = NodeCommand::Pull { image: "redis:7".into(), auth: None };
        router.broadcast(cmd).await.unwrap();

        assert!(rx1.recv().await.is_some());
        assert!(rx2.recv().await.is_some());
    }

    #[tokio::test]
    async fn broadcast_continues_on_closed_channel() {
        let router = CommandRouter::new();
        let (tx1, mut rx1) = mpsc::channel(16);
        let (tx2, rx2) = mpsc::channel(16);
        let (tx3, mut rx3) = mpsc::channel(16);
        router.register(NodeId("n1".into()), tx1).await;
        router.register(NodeId("n2".into()), tx2).await;
        router.register(NodeId("n3".into()), tx3).await;

        // Drop n2's receiver so its channel is closed.
        drop(rx2);

        let cmd = NodeCommand::Pull { image: "redis:7".into(), auth: None };
        let result = router.broadcast(cmd).await;

        // Should return an error because one channel was closed.
        assert!(result.is_err());

        // But the other two nodes should still receive the command.
        // Drain any messages that arrived.
        let mut n1_count = 0;
        while rx1.try_recv().is_ok() {
            n1_count += 1;
        }
        let mut n3_count = 0;
        while rx3.try_recv().is_ok() {
            n3_count += 1;
        }
        assert_eq!(n1_count + n3_count, 2);
    }

    #[tokio::test]
    async fn connect_round_trip() {
        // Start RPC server with a fake secondary.
        let (cmd_tx, mut cmd_rx) = mpsc::channel(256);
        let (report_tx, report_rx) = mpsc::channel(256);
        let handle = SecondaryHandle::from_parts(cmd_tx, report_rx);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_cancel = CancellationToken::new();

        let sc = server_cancel.clone();
        tokio::spawn(async move {
            RpcServer::start_with_listener(listener, "test-token".into(), handle, sc).await
        });

        // Give server a moment to start accepting.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Connect router.
        let router = CommandRouter::new();
        let (fan_in_tx, mut fan_in_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let node = NodeId("remote-1".into());
        router
            .connect(node.clone(), addr, "test-token".into(), fan_in_tx, cancel.clone())
            .await
            .unwrap();

        // Send a command through the router.
        router
            .send(&node, NodeCommand::Stop { alloc_id: AllocId("alloc-1".into()) })
            .await
            .unwrap();

        // Verify it arrives in the secondary's cmd_rx.
        let cmd = cmd_rx.recv().await.unwrap();
        assert!(matches!(cmd, NodeCommand::Stop { alloc_id } if alloc_id.0 == "alloc-1"));

        // Send a report from the secondary.
        report_tx
            .send(NodeReport::Stopped { alloc_id: AllocId("alloc-1".into()), exit_code: 0 })
            .await
            .unwrap();

        // Verify it arrives in our fan-in channel.
        let report = fan_in_rx.recv().await.unwrap();
        assert!(matches!(report, NodeReport::Stopped { exit_code: 0, .. }));

        // Disconnect.
        router.disconnect(&node).await;
        let err = router.send(&node, NodeCommand::Stop { alloc_id: AllocId("x".into()) }).await;
        assert!(err.is_err());

        server_cancel.cancel();
    }

    #[tokio::test]
    async fn connect_auth_rejection() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(256);
        let (_report_tx, report_rx) = mpsc::channel(256);
        let handle = SecondaryHandle::from_parts(cmd_tx, report_rx);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_cancel = CancellationToken::new();

        let sc = server_cancel.clone();
        tokio::spawn(async move {
            RpcServer::start_with_listener(listener, "correct-token".into(), handle, sc).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let router = CommandRouter::new();
        let (fan_in_tx, _fan_in_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let result = router
            .connect(NodeId("bad".into()), addr, "wrong-token".into(), fan_in_tx, cancel)
            .await;
        assert!(result.is_err());

        server_cancel.cancel();
    }
}
