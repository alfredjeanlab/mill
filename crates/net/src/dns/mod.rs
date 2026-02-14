mod forward;
mod handler;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use hickory_proto::op::Message;
use hickory_proto::serialize::binary::BinDecodable;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use mill_config::DnsRecord;

use crate::error::NetError;
use forward::Forwarder;
use handler::{QueryResult, handle_query, response_from};
use hickory_proto::op::ResponseCode;

/// A lightweight DNS server that resolves `.mill.` names locally and
/// forwards everything else to an upstream resolver.
pub struct DnsServer {
    local_addr: SocketAddr,
    records: Arc<RwLock<HashMap<String, Vec<IpAddr>>>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl DnsServer {
    /// Start the DNS server on the given address.
    ///
    /// Spawns a background task that listens for UDP DNS queries.
    /// Use `127.0.0.1:0` to let the OS assign a port.
    pub async fn start(listen_addr: SocketAddr) -> Result<Self, NetError> {
        let socket = UdpSocket::bind(listen_addr)
            .await
            .map_err(|e| NetError::Bind { address: listen_addr.to_string(), source: e })?;

        let local_addr = socket.local_addr()?;
        let forwarder = Arc::new(Forwarder::new().await?);
        let records: Arc<RwLock<HashMap<String, Vec<IpAddr>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let cancel = CancellationToken::new();

        let task_records = Arc::clone(&records);
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            serve_loop(Arc::new(socket), forwarder, task_records, task_cancel).await;
        });

        Ok(Self { local_addr, records, cancel, task })
    }

    /// Returns the address the server is listening on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Update the set of DNS records served by this server.
    ///
    /// Replaces all existing records with the provided ones.
    pub async fn set_records(&self, dns_records: Vec<DnsRecord>) {
        let mut map = self.records.write().await;
        map.clear();
        for record in dns_records {
            let ips: Vec<IpAddr> = record.addresses.iter().map(|sa| sa.ip()).collect();
            map.insert(record.name, ips);
        }
    }

    /// Shut down the DNS server and wait for the background task to exit.
    pub async fn stop(self) -> Result<(), NetError> {
        self.cancel.cancel();
        let _ = self.task.await;
        Ok(())
    }
}

/// Main receive loop for the DNS server.
async fn serve_loop(
    socket: Arc<UdpSocket>,
    forwarder: Arc<Forwarder>,
    records: Arc<RwLock<HashMap<String, Vec<IpAddr>>>>,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; 4096];

    loop {
        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                let (len, src) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("dns: udp recv error: {e}");
                        continue;
                    }
                };

                let query_bytes = &buf[..len];

                let msg = match Message::from_bytes(query_bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("dns: malformed query from {src}: {e}");
                        continue;
                    }
                };

                let records_guard = records.read().await;
                let result = handle_query(&msg, &records_guard);
                drop(records_guard);

                match result {
                    QueryResult::Response(resp) => {
                        match resp.to_vec() {
                            Ok(bytes) => {
                                let _ = socket.send_to(&bytes, src).await;
                            }
                            Err(e) => {
                                tracing::debug!("dns: failed to serialize response: {e}");
                            }
                        }
                    }
                    QueryResult::Forward => {
                        let sock = Arc::clone(&socket);
                        let fwd = Arc::clone(&forwarder);
                        let query = query_bytes.to_vec();
                        let msg_clone = msg.clone();
                        tokio::spawn(async move {
                            match fwd.forward(&query).await {
                                Ok(bytes) => {
                                    let _ = sock.send_to(&bytes, src).await;
                                }
                                Err(e) => {
                                    tracing::debug!("dns: upstream forward failed: {e}");
                                    let mut servfail = response_from(&msg_clone);
                                    servfail.set_response_code(ResponseCode::ServFail);
                                    if let Ok(bytes) = servfail.to_vec() {
                                        let _ = sock.send_to(&bytes, src).await;
                                    }
                                }
                            }
                        });
                    }
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::{MessageType, Query};
    use hickory_proto::rr::{DNSClass, Name, RecordType};

    fn build_query(name: &str, rtype: RecordType) -> Vec<u8> {
        let mut msg = Message::new();
        msg.set_id(42);
        msg.set_message_type(MessageType::Query);
        msg.set_recursion_desired(true);

        let mut query = Query::new();
        let dns_name = Name::from_utf8(name).unwrap_or_else(|_| Name::root());
        query.set_name(dns_name);
        query.set_query_type(rtype);
        query.set_query_class(DNSClass::IN);
        msg.add_query(query);

        msg.to_vec().unwrap_or_default()
    }

    #[tokio::test]
    async fn resolve_mill_a_record() {
        let addr: SocketAddr = "127.0.0.1:0"
            .parse()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0));
        let server = DnsServer::start(addr).await.unwrap();

        let server_addr = server.local_addr();

        server
            .set_records(vec![DnsRecord {
                name: "web".to_string(),
                addresses: vec!["10.99.0.1:0".parse().unwrap_or_else(|_| {
                    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0)
                })],
            }])
            .await;

        // Query the server via raw UDP
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let query_bytes = build_query("web.mill.", RecordType::A);
        let _ = client.send_to(&query_bytes, server_addr).await;

        let mut resp_buf = vec![0u8; 4096];
        let recv =
            tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut resp_buf))
                .await;
        assert!(recv.is_ok());
        let len = recv.unwrap_or(Ok(0)).unwrap_or(0);
        assert!(len > 0);

        let resp = Message::from_bytes(&resp_buf[..len]).unwrap();
        assert_eq!(resp.response_code(), hickory_proto::op::ResponseCode::NoError);
        assert_eq!(resp.answers().len(), 1);

        let _ = server.stop().await;
    }

    #[tokio::test]
    async fn forward_non_mill_query() {
        let addr: SocketAddr = "127.0.0.1:0"
            .parse()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0));
        let server = DnsServer::start(addr).await.unwrap();

        let server_addr = server.local_addr();

        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let query_bytes = build_query("example.com.", RecordType::A);
        let _ = client.send_to(&query_bytes, server_addr).await;

        let mut resp_buf = vec![0u8; 4096];
        let recv =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.recv(&mut resp_buf))
                .await;

        // The forward may succeed or timeout depending on network availability.
        // Either outcome is acceptable in CI.
        if let Ok(Ok(len)) = recv {
            let resp = Message::from_bytes(&resp_buf[..len]);
            assert!(resp.is_ok());
        }

        let _ = server.stop().await;
    }

    #[tokio::test]
    async fn start_and_stop() {
        let addr: SocketAddr = "127.0.0.1:0"
            .parse()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0));
        let server = DnsServer::start(addr).await.unwrap();
        let result = server.stop().await;
        assert!(result.is_ok());
    }
}
