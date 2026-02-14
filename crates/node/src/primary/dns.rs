use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mill_config::DnsRecord;
use mill_net::DnsServer;
use mill_raft::MillRaft;
use mill_raft::fsm::FsmState;
use tokio_util::sync::CancellationToken;

/// Compute DNS records from the current FSM state.
///
/// Groups running/healthy allocations by name, producing one `DnsRecord`
/// per group. Names are bare (e.g. `"web"`) — the DNS handler in mill-net
/// strips `.mill.` from queries before lookup.
pub fn compute_dns_records(fsm: &FsmState) -> Vec<DnsRecord> {
    let mut groups: HashMap<&str, Vec<SocketAddr>> = HashMap::new();

    for alloc in fsm.allocs.values() {
        if !alloc.status.is_active() {
            continue;
        }
        if let Some(addr) = alloc.address {
            groups.entry(&alloc.name).or_default().push(addr);
        }
    }

    groups
        .into_iter()
        .map(|(name, addresses)| DnsRecord { name: name.to_owned(), addresses })
        .collect()
}

/// Spawn a background task that watches FSM changes and updates DNS records.
pub fn spawn_dns_watcher(raft: Arc<MillRaft>, dns: Arc<DnsServer>, cancel: CancellationToken) {
    super::spawn_fsm_watcher(
        raft,
        compute_dns_records,
        move |records| {
            let dns = Arc::clone(&dns);
            async move { dns.set_records(records).await }
        },
        "dns",
        cancel,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::{Alloc, AllocId, AllocStatus};

    fn make_alloc(
        name: &str,
        status: AllocStatus,
        address: Option<SocketAddr>,
    ) -> (AllocId, Alloc) {
        crate::util::test_alloc(name, "0", status, address)
    }

    #[test]
    fn groups_running_allocs_by_name() {
        let mut fsm = FsmState::default();
        let addr1: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let addr2: SocketAddr = "10.0.0.2:8080".parse().unwrap();

        let (id1, alloc1) = make_alloc("web", AllocStatus::Running, Some(addr1));
        let (_id2, alloc2) = make_alloc("web", AllocStatus::Healthy, Some(addr2));
        // Give the second alloc a unique id.
        let id2 = AllocId("web-1".into());
        let alloc2 = Alloc { id: id2.clone(), ..alloc2 };

        fsm.allocs.insert(id1, alloc1);
        fsm.allocs.insert(id2, alloc2);

        let records = compute_dns_records(&fsm);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "web");
        assert_eq!(records[0].addresses.len(), 2);
    }

    #[test]
    fn excludes_stopped_allocs() {
        let mut fsm = FsmState::default();
        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();

        let (id1, alloc1) = make_alloc("web", AllocStatus::Running, Some(addr));
        let (id2, alloc2) = make_alloc("api", AllocStatus::Stopped { exit_code: 0 }, Some(addr));

        fsm.allocs.insert(id1, alloc1);
        fsm.allocs.insert(id2, alloc2);

        let records = compute_dns_records(&fsm);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "web");
    }

    #[test]
    fn excludes_allocs_without_address() {
        let mut fsm = FsmState::default();
        let (id, alloc) = make_alloc("web", AllocStatus::Running, None);
        fsm.allocs.insert(id, alloc);

        let records = compute_dns_records(&fsm);
        assert!(records.is_empty());
    }
}
