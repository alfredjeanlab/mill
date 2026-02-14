use std::collections::HashMap;

use mill_net::{BoxFuture, Mesh, NetError, PeerInfo};

/// In-memory `Mesh` implementation for integration tests.
///
/// Tracks peers in a `HashMap` behind a `parking_lot::Mutex` so the watchdog
/// and test assertions can inspect peer state without root or a kernel module.
pub struct TestMesh {
    peers: parking_lot::Mutex<HashMap<String, PeerInfo>>,
}

impl TestMesh {
    pub fn new() -> Self {
        Self { peers: parking_lot::Mutex::new(HashMap::new()) }
    }

    pub fn has_peer(&self, pubkey: &str) -> bool {
        self.peers.lock().contains_key(pubkey)
    }

    pub fn peer_count(&self) -> usize {
        self.peers.lock().len()
    }
}

impl Mesh for TestMesh {
    fn add_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), NetError>> {
        self.peers.lock().insert(peer.public_key.clone(), peer.clone());
        Box::pin(std::future::ready(Ok(())))
    }

    fn remove_peer(&self, public_key: &str) -> BoxFuture<'_, Result<(), NetError>> {
        self.peers.lock().remove(public_key);
        Box::pin(std::future::ready(Ok(())))
    }
}
