use std::sync::Arc;

use mill_config::ProxyRoute;
use mill_proxy::ProxyServer;
use mill_raft::MillRaft;
use mill_raft::fsm::FsmState;
use tokio_util::sync::CancellationToken;

/// Compute proxy routes from the current FSM state.
///
/// For each service with `routes` in its config, collects running/healthy
/// allocation addresses and emits one `ProxyRoute` per (route_def, backend) pair.
pub fn compute_proxy_routes(fsm: &FsmState) -> Vec<ProxyRoute> {
    let config = match &fsm.config {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut routes = Vec::new();

    for (name, def) in &config.services {
        if def.routes.is_empty() {
            continue;
        }

        // Collect addresses of running/healthy allocs for this service.
        let addrs: Vec<_> = fsm
            .allocs
            .values()
            .filter(|a| a.name == *name && a.status.is_active() && a.address.is_some())
            .filter_map(|a| a.address)
            .collect();

        for route_def in &def.routes {
            for &backend in &addrs {
                routes.push(ProxyRoute {
                    hostname: route_def.hostname.clone(),
                    path: route_def.path.clone(),
                    backend,
                    websocket: route_def.websocket,
                });
            }
        }
    }

    routes
}

/// Spawn a background task that watches FSM changes and updates proxy routes.
pub fn spawn_proxy_watcher(
    raft: Arc<MillRaft>,
    proxy: Arc<ProxyServer>,
    cancel: CancellationToken,
) {
    super::spawn_fsm_watcher(
        raft,
        compute_proxy_routes,
        move |routes| {
            proxy.update_routes(&routes);
            std::future::ready(())
        },
        "proxy",
        cancel,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::{AllocId, AllocStatus, ClusterConfig, Resources, RouteDef, ServiceDef};
    use std::collections::HashMap;
    use std::net::SocketAddr;

    fn service_with_routes(routes: Vec<RouteDef>) -> ServiceDef {
        ServiceDef {
            image: "nginx:latest".into(),
            port: 80,
            replicas: 1,
            resources: Resources { cpu: 0.25, memory: bytesize::ByteSize::mib(256) },
            env: HashMap::new(),
            command: None,
            health: None,
            routes,
            volumes: vec![],
        }
    }

    fn running_alloc(
        name: &str,
        id_suffix: &str,
        addr: SocketAddr,
    ) -> (AllocId, mill_config::Alloc) {
        crate::util::test_alloc(name, id_suffix, AllocStatus::Healthy, Some(addr))
    }

    #[test]
    fn computes_routes_for_running_allocs() {
        let mut fsm = FsmState::default();

        let route =
            RouteDef { hostname: "app.example.com".into(), path: "/".into(), websocket: false };
        let svc = service_with_routes(vec![route]);
        fsm.config = Some(ClusterConfig {
            services: HashMap::from([("web".into(), svc)]),
            tasks: HashMap::new(),
        });

        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let (id, alloc) = running_alloc("web", "0", addr);
        fsm.allocs.insert(id, alloc);

        let routes = compute_proxy_routes(&fsm);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].hostname, "app.example.com");
        assert_eq!(routes[0].backend, addr);
    }

    #[test]
    fn excludes_stopped_allocs() {
        let mut fsm = FsmState::default();

        let route =
            RouteDef { hostname: "app.example.com".into(), path: "/".into(), websocket: false };
        let svc = service_with_routes(vec![route]);
        fsm.config = Some(ClusterConfig {
            services: HashMap::from([("web".into(), svc)]),
            tasks: HashMap::new(),
        });

        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let (id, alloc) =
            crate::util::test_alloc("web", "0", AllocStatus::Stopped { exit_code: 0 }, Some(addr));
        fsm.allocs.insert(id, alloc);

        let routes = compute_proxy_routes(&fsm);
        assert!(routes.is_empty());
    }

    #[test]
    fn no_config_returns_empty() {
        let fsm = FsmState::default();
        let routes = compute_proxy_routes(&fsm);
        assert!(routes.is_empty());
    }

    #[test]
    fn services_without_routes_skipped() {
        let mut fsm = FsmState::default();

        let svc = service_with_routes(vec![]);
        fsm.config = Some(ClusterConfig {
            services: HashMap::from([("web".into(), svc)]),
            tasks: HashMap::new(),
        });

        let routes = compute_proxy_routes(&fsm);
        assert!(routes.is_empty());
    }
}
