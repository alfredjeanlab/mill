use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bytesize::ByteSize;
use mill_config::{AllocId, ContainerStats, NodeResources};
use mill_containerd::Containers;
use tokio::sync::RwLock;

/// Cached container and host resource metrics, refreshed periodically.
///
/// CPU usage from cgroups is cumulative seconds. This collector converts
/// to a rate (cores) by comparing successive readings.
pub struct MetricsCollector {
    cache: Arc<RwLock<(NodeResources, Vec<ContainerStats>)>>,
}

impl MetricsCollector {
    pub fn new() -> Self {
        let initial = (
            NodeResources {
                cpu_total: 0.0,
                cpu_available: 0.0,
                memory_total: ByteSize::b(0),
                memory_available: ByteSize::b(0),
            },
            vec![],
        );
        Self { cache: Arc::new(RwLock::new(initial)) }
    }

    /// Run the polling loop, updating cached metrics every 10 seconds.
    /// Exits when the cancellation token is triggered.
    pub async fn run(
        &self,
        runtime: Arc<dyn Containers>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let cache = Arc::clone(&self.cache);
        let mut interval = tokio::time::interval(crate::env::metrics_poll_interval());
        let mut prev_cpu: HashMap<AllocId, (f64, Instant)> = HashMap::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let resources = read_host_resources();
                    let raw_stats = runtime.stats_all().await.unwrap_or_default();
                    let now = Instant::now();

                    // Convert cumulative CPU seconds to rate (cores).
                    let rate_stats = compute_cpu_rates(&raw_stats, &mut prev_cpu, now);

                    let mut c = cache.write().await;
                    *c = (resources, rate_stats);
                }
            }
        }
    }

    /// Return the most recently cached metrics snapshot.
    pub async fn current(&self) -> (NodeResources, Vec<ContainerStats>) {
        self.cache.read().await.clone()
    }
}

/// Read host CPU and memory resources.
///
/// On Linux, reads from /proc/meminfo and /proc/stat.
/// On other platforms, returns zeroed values (sufficient for CP2 single-node).
fn read_host_resources() -> NodeResources {
    #[cfg(target_os = "linux")]
    {
        let mem = read_linux_memory();
        let cpu = read_linux_cpu();
        NodeResources {
            cpu_total: cpu,
            cpu_available: cpu, // simplified — not tracking per-container CPU reservation
            memory_total: mem.0,
            memory_available: mem.1,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        NodeResources {
            cpu_total: 0.0,
            cpu_available: 0.0,
            memory_total: ByteSize::b(0),
            memory_available: ByteSize::b(0),
        }
    }
}

#[cfg(target_os = "linux")]
fn read_linux_memory() -> (ByteSize, ByteSize) {
    let contents = match std::fs::read_to_string("/proc/meminfo") {
        Ok(c) => c,
        Err(_) => return (ByteSize::b(0), ByteSize::b(0)),
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_meminfo_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_meminfo_kb(rest);
        }
    }
    (ByteSize::kib(total), ByteSize::kib(available))
}

#[cfg(target_os = "linux")]
fn parse_meminfo_kb(s: &str) -> u64 {
    s.trim().trim_end_matches("kB").trim().parse().unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn read_linux_cpu() -> f64 {
    // Count CPU cores from /proc/stat (cpu0, cpu1, ...)
    let contents = match std::fs::read_to_string("/proc/stat") {
        Ok(c) => c,
        Err(_) => return 0.0,
    };
    contents.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count() as f64
}

/// Convert cumulative CPU seconds to a rate (cores) by comparing with previous readings.
///
/// First reading for a container produces 0.0. Negative deltas (container restart)
/// are clamped to 0.0. Stale entries (containers no longer in `stats`) are cleaned up.
fn compute_cpu_rates(
    stats: &[ContainerStats],
    prev_cpu: &mut HashMap<AllocId, (f64, Instant)>,
    now: Instant,
) -> Vec<ContainerStats> {
    let current_ids: std::collections::HashSet<&AllocId> =
        stats.iter().map(|s| &s.alloc_id).collect();

    // Clean up stale entries for containers that are no longer reported.
    prev_cpu.retain(|id, _| current_ids.contains(id));

    stats
        .iter()
        .map(|s| {
            let rate = match prev_cpu.get(&s.alloc_id) {
                Some(&(prev_cumulative, prev_time)) => {
                    let elapsed = now.duration_since(prev_time).as_secs_f64();
                    if elapsed > 0.0 {
                        let delta = s.cpu_usage - prev_cumulative;
                        (delta / elapsed).max(0.0)
                    } else {
                        0.0
                    }
                }
                None => 0.0, // First reading
            };

            prev_cpu.insert(s.alloc_id.clone(), (s.cpu_usage, now));

            ContainerStats {
                alloc_id: s.alloc_id.clone(),
                cpu_usage: rate,
                memory_usage: s.memory_usage,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn cpu_rate_first_reading_is_zero() {
        let mut prev = HashMap::new();
        let now = Instant::now();
        let stats = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 100.0,
            memory_usage: ByteSize::mib(128),
        }];

        let result = compute_cpu_rates(&stats, &mut prev, now);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].cpu_usage, 0.0);
        // prev_cpu should now have an entry
        assert!(prev.contains_key(&AllocId("svc-1".into())));
    }

    #[test]
    fn cpu_rate_two_successive_readings() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();
        let stats1 = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 10.0,
            memory_usage: ByteSize::mib(128),
        }];

        // First reading → 0.0
        compute_cpu_rates(&stats1, &mut prev, t0);

        // Second reading 10 seconds later, cumulative went from 10.0 to 20.0
        let t1 = t0 + Duration::from_secs(10);
        let stats2 = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 20.0,
            memory_usage: ByteSize::mib(130),
        }];

        let result = compute_cpu_rates(&stats2, &mut prev, t1);
        assert_eq!(result.len(), 1);
        // (20.0 - 10.0) / 10s = 1.0 core
        assert!((result[0].cpu_usage - 1.0).abs() < 0.001);
        assert_eq!(result[0].memory_usage, ByteSize::mib(130));
    }

    #[test]
    fn cpu_rate_negative_delta_clamped() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();

        // Seed with a high cumulative value
        prev.insert(AllocId("svc-1".into()), (50.0, t0));

        // Container restarted — cumulative reset to a lower value
        let t1 = t0 + Duration::from_secs(10);
        let stats = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 2.0,
            memory_usage: ByteSize::mib(64),
        }];

        let result = compute_cpu_rates(&stats, &mut prev, t1);
        assert_eq!(result[0].cpu_usage, 0.0);
    }

    #[test]
    fn cpu_rate_stale_entries_cleaned() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();

        prev.insert(AllocId("svc-1".into()), (10.0, t0));
        prev.insert(AllocId("svc-gone".into()), (5.0, t0));

        // Only svc-1 is in the new stats
        let stats = vec![ContainerStats {
            alloc_id: AllocId("svc-1".into()),
            cpu_usage: 20.0,
            memory_usage: ByteSize::mib(128),
        }];

        let t1 = t0 + Duration::from_secs(10);
        compute_cpu_rates(&stats, &mut prev, t1);

        assert!(prev.contains_key(&AllocId("svc-1".into())));
        assert!(!prev.contains_key(&AllocId("svc-gone".into())));
    }
}
