use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Histogram bucket upper bounds in seconds.
const BUCKET_BOUNDS: [f64; 12] =
    [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, f64::INFINITY];

/// Per-route atomic counters for request count and latency histogram.
struct RouteCounters {
    request_count: AtomicU64,
    latency_sum_us: AtomicU64,
    /// Buckets: [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, +Inf]
    buckets: [AtomicU64; 12],
}

impl RouteCounters {
    fn new() -> Self {
        Self {
            request_count: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// Snapshot of metrics for a single route.
#[derive(Debug, Clone)]
pub struct RouteMetricsSnapshot {
    pub hostname: String,
    pub path: String,
    pub request_count: u64,
    pub latency_sum_seconds: f64,
    /// Cumulative bucket counts matching `BUCKET_BOUNDS`.
    pub buckets: [(f64, u64); 12],
}

/// Thread-safe proxy request metrics collector.
///
/// Routes are created lazily on first request. All operations are lock-free
/// on the hot path (atomic increments), with a mutex only for route creation.
pub struct ProxyMetrics {
    routes: parking_lot::Mutex<HashMap<(String, String), Arc<RouteCounters>>>,
}

impl ProxyMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { routes: parking_lot::Mutex::new(HashMap::new()) })
    }

    /// Record a completed request for the given hostname and path.
    pub fn record(&self, hostname: &str, path: &str, latency: Duration) {
        let key = (hostname.to_string(), path.to_string());
        let counters = {
            let guard = self.routes.lock();
            if let Some(c) = guard.get(&key) {
                Arc::clone(c)
            } else {
                drop(guard);
                let mut guard = self.routes.lock();
                Arc::clone(guard.entry(key).or_insert_with(|| Arc::new(RouteCounters::new())))
            }
        };

        counters.request_count.fetch_add(1, Ordering::Relaxed);
        let us = latency.as_micros() as u64;
        counters.latency_sum_us.fetch_add(us, Ordering::Relaxed);

        // Increment only the first (smallest) matching bucket.
        // Cumulative counts are computed in snapshot().
        let secs = latency.as_secs_f64();
        for (i, &bound) in BUCKET_BOUNDS.iter().enumerate() {
            if secs <= bound {
                counters.buckets[i].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    /// Take a snapshot of all route metrics.
    pub fn snapshot(&self) -> Vec<RouteMetricsSnapshot> {
        let guard = self.routes.lock();
        guard
            .iter()
            .map(|((hostname, path), counters)| {
                let request_count = counters.request_count.load(Ordering::Relaxed);
                let latency_sum_us = counters.latency_sum_us.load(Ordering::Relaxed);

                // Build cumulative buckets.
                let mut cumulative = 0u64;
                let mut buckets = [(0.0, 0u64); 12];
                for (i, &bound) in BUCKET_BOUNDS.iter().enumerate() {
                    cumulative += counters.buckets[i].load(Ordering::Relaxed);
                    buckets[i] = (bound, cumulative);
                }

                RouteMetricsSnapshot {
                    hostname: hostname.clone(),
                    path: path.clone(),
                    request_count,
                    latency_sum_seconds: latency_sum_us as f64 / 1_000_000.0,
                    buckets,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_snapshot_counts() {
        let m = ProxyMetrics::new();
        m.record("app.test", "/", Duration::from_millis(50));
        m.record("app.test", "/", Duration::from_millis(200));
        m.record("other.test", "/", Duration::from_millis(1));

        let snap = m.snapshot();
        assert_eq!(snap.len(), 2);

        let app = snap.iter().find(|s| s.hostname == "app.test").unwrap();
        assert_eq!(app.request_count, 2);
        assert!(app.latency_sum_seconds > 0.0);
        assert_eq!(app.path, "/");

        let other = snap.iter().find(|s| s.hostname == "other.test").unwrap();
        assert_eq!(other.request_count, 1);
        assert_eq!(other.path, "/");
    }

    #[test]
    fn separate_paths_tracked_independently() {
        let m = ProxyMetrics::new();
        m.record("app.test", "/", Duration::from_millis(10));
        m.record("app.test", "/api/", Duration::from_millis(20));
        m.record("app.test", "/api/", Duration::from_millis(30));

        let snap = m.snapshot();
        assert_eq!(snap.len(), 2);

        let root = snap.iter().find(|s| s.hostname == "app.test" && s.path == "/").unwrap();
        assert_eq!(root.request_count, 1);

        let api = snap.iter().find(|s| s.hostname == "app.test" && s.path == "/api/").unwrap();
        assert_eq!(api.request_count, 2);
    }

    #[test]
    fn bucket_boundaries() {
        let m = ProxyMetrics::new();

        // 5ms request → hits 0.005 bucket and all above
        m.record("a.test", "/", Duration::from_millis(5));
        let snap = m.snapshot();
        let a = snap.iter().find(|s| s.hostname == "a.test").unwrap();

        // 5ms = 0.005s, so bucket[0] (<=0.005) should be 1
        assert_eq!(a.buckets[0].1, 1); // 0.005
        assert_eq!(a.buckets[11].1, 1); // +Inf (cumulative)

        // 100ms request → should NOT hit 0.005, 0.01, 0.025, 0.05 buckets
        m.record("a.test", "/", Duration::from_millis(100));
        let snap = m.snapshot();
        let a = snap.iter().find(|s| s.hostname == "a.test").unwrap();
        assert_eq!(a.request_count, 2);
        // bucket[0] (0.005): still 1 (only 5ms request)
        assert_eq!(a.buckets[0].1, 1);
        // bucket[4] (0.1): cumulative 2 (both requests)
        assert_eq!(a.buckets[4].1, 2);
    }

    #[test]
    fn snapshot_empty() {
        let m = ProxyMetrics::new();
        assert!(m.snapshot().is_empty());
    }
}
