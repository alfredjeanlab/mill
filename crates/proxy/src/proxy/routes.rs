use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mill_config::ProxyRoute;
use tokio::sync::watch;

/// An individual route entry within a hostname's route list.
struct RouteEntry {
    path: String,
    backend: SocketAddr,
    websocket: bool,
}

/// The result of a successful route lookup.
#[derive(Debug, Clone)]
pub struct RouteMatch {
    pub backend: SocketAddr,
    pub websocket: bool,
    pub path: String,
}

/// Immutable route table built from a set of `ProxyRoute` definitions.
///
/// Routes are grouped by hostname and sorted by path length (longest first)
/// to implement longest-prefix matching.
pub struct RouteTable {
    routes: HashMap<String, Vec<RouteEntry>>,
}

impl RouteTable {
    /// Build a route table from a slice of proxy routes.
    pub fn new(proxy_routes: &[ProxyRoute]) -> Self {
        let mut routes: HashMap<String, Vec<RouteEntry>> = HashMap::new();
        for r in proxy_routes {
            routes.entry(r.hostname.clone()).or_default().push(RouteEntry {
                path: r.path.clone(),
                backend: r.backend,
                websocket: r.websocket,
            });
        }
        // Sort each hostname's entries by path length descending (longest-prefix first).
        for entries in routes.values_mut() {
            entries.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        }
        Self { routes }
    }

    /// Look up a route by hostname and request path.
    ///
    /// Returns the first route whose path is a prefix of the request path
    /// (longest-prefix match due to sort order).
    pub fn lookup(&self, hostname: &str, path: &str) -> Option<RouteMatch> {
        let entries = self.routes.get(hostname)?;
        for entry in entries {
            if path.starts_with(&entry.path) {
                return Some(RouteMatch {
                    backend: entry.backend,
                    websocket: entry.websocket,
                    path: entry.path.clone(),
                });
            }
        }
        None
    }

    /// Return all unique hostnames in the route table.
    pub fn hostnames(&self) -> Vec<String> {
        self.routes.keys().cloned().collect()
    }
}

/// Thread-safe, atomically-swappable route table using a watch channel.
///
/// Readers get a snapshot via `current()` — in-flight requests complete
/// against the old table while new requests use the updated one.
pub struct SharedRouteTable {
    tx: watch::Sender<Arc<RouteTable>>,
    rx: watch::Receiver<Arc<RouteTable>>,
}

impl SharedRouteTable {
    pub fn new() -> Self {
        let empty = Arc::new(RouteTable::new(&[]));
        let (tx, rx) = watch::channel(empty);
        Self { tx, rx }
    }

    /// Replace the route table atomically.
    pub fn update(&self, routes: &[ProxyRoute]) {
        let table = Arc::new(RouteTable::new(routes));
        let _ = self.tx.send(table);
    }

    /// Get a snapshot of the current route table.
    pub fn current(&self) -> Arc<RouteTable> {
        self.rx.borrow().clone()
    }

    /// Subscribe to route table changes.
    pub fn subscribe(&self) -> watch::Receiver<Arc<RouteTable>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_routes() -> Vec<ProxyRoute> {
        vec![
            ProxyRoute {
                hostname: "app.example.com".to_string(),
                path: "/".to_string(),
                backend: "10.0.0.1:8080".parse().unwrap(),
                websocket: false,
            },
            ProxyRoute {
                hostname: "app.example.com".to_string(),
                path: "/api/".to_string(),
                backend: "10.0.0.2:8081".parse().unwrap(),
                websocket: false,
            },
            ProxyRoute {
                hostname: "app.example.com".to_string(),
                path: "/ws".to_string(),
                backend: "10.0.0.3:8082".parse().unwrap(),
                websocket: true,
            },
            ProxyRoute {
                hostname: "other.example.com".to_string(),
                path: "/".to_string(),
                backend: "10.0.0.4:9090".parse().unwrap(),
                websocket: false,
            },
        ]
    }

    #[test]
    fn lookup_exact_path() {
        let table = RouteTable::new(&make_routes());
        let m = table.lookup("other.example.com", "/foo").unwrap();
        assert_eq!(m.backend.to_string(), "10.0.0.4:9090");
    }

    #[test]
    fn longest_prefix_match() {
        let table = RouteTable::new(&make_routes());

        // /api/users should match the /api/ route, not /
        let m = table.lookup("app.example.com", "/api/users").unwrap();
        assert_eq!(m.backend.to_string(), "10.0.0.2:8081");
        assert!(!m.websocket);
    }

    #[test]
    fn websocket_route() {
        let table = RouteTable::new(&make_routes());
        let m = table.lookup("app.example.com", "/ws").unwrap();
        assert!(m.websocket);
        assert_eq!(m.backend.to_string(), "10.0.0.3:8082");
    }

    #[test]
    fn unknown_hostname_returns_none() {
        let table = RouteTable::new(&make_routes());
        assert!(table.lookup("unknown.com", "/").is_none());
    }

    #[test]
    fn fallback_to_root() {
        let table = RouteTable::new(&make_routes());
        let m = table.lookup("app.example.com", "/static/file.js").unwrap();
        assert_eq!(m.backend.to_string(), "10.0.0.1:8080");
    }

    #[test]
    fn shared_route_table_update() {
        let shared = SharedRouteTable::new();
        assert!(shared.current().lookup("app.example.com", "/").is_none());

        shared.update(&make_routes());
        assert!(shared.current().lookup("app.example.com", "/").is_some());
    }
}
