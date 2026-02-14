use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};

use mill_config::AllocId;

use crate::error::NetError;

struct PortPool {
    start: u16,
    end: u16,
    available: BTreeSet<u16>,
    allocated: HashMap<u16, AllocId>,
}

/// Manages dynamic port allocation for containers.
///
/// Allocates ports from the ephemeral range (49152–65535), tracking which
/// allocation owns each port. Supports snapshot/restore for crash recovery.
pub struct PortAllocator {
    pool: Mutex<PortPool>,
}

impl PortAllocator {
    /// Create a new allocator using the standard ephemeral port range.
    ///
    /// Scans the host for ports already in use and excludes them.
    pub fn new() -> Result<Self, NetError> {
        Self::with_range(49152, 65535)
    }

    /// Create an allocator with a custom port range, scanning the host for in-use ports.
    pub fn with_range(start: u16, end: u16) -> Result<Self, NetError> {
        let mut available: BTreeSet<u16> = (start..=end).collect();
        let in_use = scan_host_ports();
        for port in &in_use {
            available.remove(port);
        }
        Ok(Self { pool: Mutex::new(PortPool { start, end, available, allocated: HashMap::new() }) })
    }

    /// Create an allocator with a custom range, skipping host port scanning.
    /// Intended for tests.
    #[cfg(test)]
    fn with_range_no_scan(start: u16, end: u16) -> Self {
        let available: BTreeSet<u16> = (start..=end).collect();
        Self { pool: Mutex::new(PortPool { start, end, available, allocated: HashMap::new() }) }
    }

    /// Allocate a port for the given allocation. Returns `None` if exhausted.
    pub fn allocate(&self, alloc_id: AllocId) -> Option<u16> {
        let mut pool = self.pool.lock();
        let port = pool.available.iter().next().copied()?;
        pool.available.remove(&port);
        pool.allocated.insert(port, alloc_id);
        Some(port)
    }

    /// Reserve a specific port (e.g. recovered from containerd labels after a crash).
    /// Returns `true` if the port was successfully reserved, `false` if already allocated.
    pub fn allocate_specific(&self, alloc_id: AllocId, port: u16) -> bool {
        let mut pool = self.pool.lock();
        if port < pool.start || port > pool.end {
            return false;
        }
        if pool.allocated.contains_key(&port) {
            return false;
        }
        pool.available.remove(&port);
        pool.allocated.insert(port, alloc_id);
        true
    }

    /// Release a previously allocated port back to the pool.
    pub fn release(&self, port: u16) {
        let mut pool = self.pool.lock();
        if pool.allocated.remove(&port).is_some() {
            pool.available.insert(port);
        }
    }

    /// Return a snapshot of all current allocations for crash recovery.
    pub fn snapshot(&self) -> HashMap<u16, AllocId> {
        self.pool.lock().allocated.clone()
    }

    /// Restore allocations from a saved snapshot, skipping ports now in use on the host.
    pub fn restore(&self, snapshot: HashMap<u16, AllocId>) {
        let in_use = scan_host_ports();
        let mut pool = self.pool.lock();
        for (port, alloc_id) in snapshot {
            if port < pool.start || port > pool.end {
                tracing::warn!(port, "restore: skipping out-of-range port");
                continue;
            }
            if in_use.contains(&port) {
                continue;
            }
            pool.available.remove(&port);
            pool.allocated.insert(port, alloc_id);
        }
    }
}

/// Scan the host for TCP ports currently in use.
///
/// Tries `/proc/net/tcp` and `/proc/net/tcp6` first (Linux), then falls back
/// to `ss -tlnp` output. If neither works, returns an empty set — bind
/// failures at container start will catch conflicts.
fn scan_host_ports() -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();

    // Try /proc/net/tcp and /proc/net/tcp6 (Linux)
    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines().skip(1) {
                if let Some(port) = parse_proc_net_tcp_line(line) {
                    ports.insert(port);
                }
            }
        }
    }

    if !ports.is_empty() {
        return ports;
    }

    // Fallback: try `ss -tlnp`
    if let Ok(output) = std::process::Command::new("ss").args(["-tlnp"]).output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            if let Some(port) = parse_ss_line(line) {
                ports.insert(port);
            }
        }
    }

    ports
}

/// Parse a line from /proc/net/tcp or /proc/net/tcp6.
/// Format: `sl  local_address rem_address ...`
/// local_address is `HEX_IP:HEX_PORT`.
fn parse_proc_net_tcp_line(line: &str) -> Option<u16> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let local_addr = fields.get(1)?;
    let hex_port = local_addr.rsplit(':').next()?;
    u16::from_str_radix(hex_port, 16).ok()
}

/// Parse a line from `ss -tlnp` output to extract the listening port.
/// Typical format: `LISTEN  0  128  0.0.0.0:8080  0.0.0.0:*`
fn parse_ss_line(line: &str) -> Option<u16> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let local_addr = fields.get(3)?;
    let port_str = local_addr.rsplit(':').next()?;
    port_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_returns_port_in_range() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50010);
        let port = alloc.allocate(AllocId("a1".into()));
        assert!(port.is_some());
        let port = port.unwrap_or(0);
        assert!((50000..=50010).contains(&port));
    }

    #[test]
    fn allocate_tracks_alloc_id() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50010);
        let port = alloc.allocate(AllocId("svc-1".into()));
        assert!(port.is_some());
        let snap = alloc.snapshot();
        assert_eq!(snap.get(&port.unwrap_or(0)), Some(&AllocId("svc-1".into())));
    }

    #[test]
    fn release_returns_port_to_pool() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50000);
        let port = alloc.allocate(AllocId("a".into()));
        assert!(port.is_some());
        // Pool exhausted
        assert!(alloc.allocate(AllocId("b".into())).is_none());
        // Release and reallocate
        alloc.release(port.unwrap_or(0));
        let port2 = alloc.allocate(AllocId("c".into()));
        assert_eq!(port2, port);
    }

    #[test]
    fn exhaustion_returns_none() {
        let alloc = PortAllocator::with_range_no_scan(60000, 60002);
        assert!(alloc.allocate(AllocId("a".into())).is_some());
        assert!(alloc.allocate(AllocId("b".into())).is_some());
        assert!(alloc.allocate(AllocId("c".into())).is_some());
        assert!(alloc.allocate(AllocId("d".into())).is_none());
    }

    #[test]
    fn snapshot_restore_round_trips() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50005);
        alloc.allocate(AllocId("a".into()));
        alloc.allocate(AllocId("b".into()));
        let snap = alloc.snapshot();
        assert_eq!(snap.len(), 2);

        // Restore into a fresh allocator
        let alloc2 = PortAllocator::with_range_no_scan(50000, 50005);
        alloc2.restore(snap.clone());
        let snap2 = alloc2.snapshot();
        assert_eq!(snap, snap2);
    }

    #[test]
    fn allocate_specific_reserves_port() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50010);
        assert!(alloc.allocate_specific(AllocId("a".into()), 50005));
        // Port should now be allocated
        let snap = alloc.snapshot();
        assert_eq!(snap.get(&50005), Some(&AllocId("a".into())));
        // Double-allocating the same port should fail
        assert!(!alloc.allocate_specific(AllocId("b".into()), 50005));
        // Normal allocation should skip 50005
        let port = alloc.allocate(AllocId("c".into()));
        assert!(port.is_some());
        assert_ne!(port.unwrap_or(50005), 50005);
    }

    #[test]
    fn allocate_specific_rejects_out_of_range() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50010);
        // Below range
        assert!(!alloc.allocate_specific(AllocId("a".into()), 49999));
        // Above range
        assert!(!alloc.allocate_specific(AllocId("b".into()), 50011));
        // In range should work
        assert!(alloc.allocate_specific(AllocId("c".into()), 50005));
    }

    #[test]
    fn restore_skips_out_of_range_ports() {
        let alloc = PortAllocator::with_range_no_scan(50000, 50005);
        let mut snapshot = HashMap::new();
        snapshot.insert(50001, AllocId("a".into()));
        snapshot.insert(49000, AllocId("b".into())); // out of range
        snapshot.insert(60000, AllocId("c".into())); // out of range
        alloc.restore(snapshot);
        let snap = alloc.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key(&50001));
    }

    #[test]
    fn concurrent_allocations_never_return_same_port() {
        use std::sync::Arc;
        use std::thread;

        let alloc = Arc::new(PortAllocator::with_range_no_scan(50000, 50099));
        let mut handles = vec![];
        for i in 0..100 {
            let alloc = Arc::clone(&alloc);
            handles.push(thread::spawn(move || alloc.allocate(AllocId(format!("alloc-{i}")))));
        }

        let mut ports: Vec<u16> =
            handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect();
        let count = ports.len();
        ports.sort();
        ports.dedup();
        assert_eq!(count, ports.len(), "duplicate ports allocated");
        assert_eq!(count, 100);
    }
}
