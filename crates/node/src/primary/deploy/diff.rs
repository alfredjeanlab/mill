use mill_config::{ClusterConfig, ServiceDef};

/// Plan produced by comparing old and new cluster configurations.
pub struct DeployPlan {
    /// Services present in the new config but not the old.
    pub added: Vec<(String, ServiceDef)>,
    /// Service names present in the old config but not the new.
    pub removed: Vec<String>,
    /// Services present in both but with a different definition (carries the new def).
    pub changed: Vec<(String, ServiceDef)>,
    /// Service names present in both with identical definitions.
    pub unchanged: Vec<String>,
}

/// Compare old and new configs to produce a deploy plan.
///
/// If `old` is `None` (first deploy), all services in `new` are "added".
pub fn diff_config(old: Option<&ClusterConfig>, new: &ClusterConfig) -> DeployPlan {
    let old_services = old.map(|c| &c.services);

    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();

    for (name, new_def) in &new.services {
        match old_services.and_then(|s| s.get(name)) {
            None => added.push((name.clone(), new_def.clone())),
            Some(old_def) if old_def != new_def => {
                changed.push((name.clone(), new_def.clone()));
            }
            Some(_) => unchanged.push(name.clone()),
        }
    }

    let mut removed = Vec::new();
    if let Some(old_services) = old_services {
        for name in old_services.keys() {
            if !new.services.contains_key(name) {
                removed.push(name.clone());
            }
        }
    }

    DeployPlan { added, removed, changed, unchanged }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytesize::ByteSize;
    use mill_config::{HealthCheck, Resources, ServiceDef};
    use std::collections::HashMap;
    use std::time::Duration;

    fn service(image: &str, replicas: u32) -> ServiceDef {
        ServiceDef {
            image: image.into(),
            port: 8080,
            replicas,
            resources: Resources { cpu: 0.25, memory: ByteSize::mib(256) },
            env: HashMap::new(),
            command: None,
            health: None,
            routes: vec![],
            volumes: vec![],
        }
    }

    fn config(services: Vec<(&str, ServiceDef)>) -> ClusterConfig {
        ClusterConfig {
            services: services.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            tasks: HashMap::new(),
        }
    }

    #[test]
    fn no_old_config_all_added() {
        let new = config(vec![("web", service("nginx:1", 2)), ("api", service("api:1", 1))]);
        let plan = diff_config(None, &new);

        assert_eq!(plan.added.len(), 2);
        assert!(plan.removed.is_empty());
        assert!(plan.changed.is_empty());
        assert!(plan.unchanged.is_empty());
    }

    #[test]
    fn added_service() {
        let old = config(vec![("web", service("nginx:1", 1))]);
        let new = config(vec![("web", service("nginx:1", 1)), ("api", service("api:1", 1))]);
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.added.len(), 1);
        assert_eq!(plan.added[0].0, "api");
        assert_eq!(plan.unchanged.len(), 1);
        assert!(plan.removed.is_empty());
        assert!(plan.changed.is_empty());
    }

    #[test]
    fn removed_service() {
        let old = config(vec![("web", service("nginx:1", 1)), ("api", service("api:1", 1))]);
        let new = config(vec![("web", service("nginx:1", 1))]);
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.removed.len(), 1);
        assert_eq!(plan.removed[0], "api");
        assert_eq!(plan.unchanged.len(), 1);
        assert!(plan.added.is_empty());
        assert!(plan.changed.is_empty());
    }

    #[test]
    fn changed_service() {
        let old = config(vec![("web", service("nginx:1", 1))]);
        let new = config(vec![("web", service("nginx:2", 1))]); // image changed
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.changed.len(), 1);
        assert_eq!(plan.changed[0].0, "web");
        assert_eq!(plan.changed[0].1.image, "nginx:2");
        assert!(plan.added.is_empty());
        assert!(plan.removed.is_empty());
        assert!(plan.unchanged.is_empty());
    }

    #[test]
    fn unchanged_service() {
        let old = config(vec![("web", service("nginx:1", 1))]);
        let new = config(vec![("web", service("nginx:1", 1))]);
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.unchanged.len(), 1);
        assert!(plan.added.is_empty());
        assert!(plan.removed.is_empty());
        assert!(plan.changed.is_empty());
    }

    #[test]
    fn mixed_changes() {
        let old = config(vec![
            ("web", service("nginx:1", 1)),
            ("api", service("api:1", 1)),
            ("worker", service("worker:1", 2)),
        ]);
        let new = config(vec![
            ("web", service("nginx:1", 1)), // unchanged
            ("api", service("api:2", 1)),   // changed (image)
            ("cache", service("redis:7", 1)), // added
                                            // worker removed
        ]);
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.unchanged, vec!["web"]);
        assert_eq!(plan.changed.len(), 1);
        assert_eq!(plan.changed[0].0, "api");
        assert_eq!(plan.added.len(), 1);
        assert_eq!(plan.added[0].0, "cache");
        assert_eq!(plan.removed, vec!["worker"]);
    }

    #[test]
    fn replicas_change_is_detected() {
        let old = config(vec![("web", service("nginx:1", 1))]);
        let new = config(vec![("web", service("nginx:1", 3))]); // replicas changed
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.changed.len(), 1);
        assert!(plan.unchanged.is_empty());
    }

    #[test]
    fn health_check_change_is_detected() {
        let mut svc = service("nginx:1", 1);
        let old = config(vec![("web", svc.clone())]);
        svc.health = Some(HealthCheck {
            path: "/health".into(),
            interval: Duration::from_secs(10),
            failure_threshold: 3,
        });
        let new = config(vec![("web", svc)]);
        let plan = diff_config(Some(&old), &new);

        assert_eq!(plan.changed.len(), 1);
        assert!(plan.unchanged.is_empty());
    }
}
