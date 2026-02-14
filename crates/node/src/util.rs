use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a process-unique suffix for alloc IDs.
///
/// Combines nanosecond timestamp with an atomic counter to avoid
/// collisions within the same process.
pub fn rand_suffix() -> String {
    let nanos =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}", nanos & 0xFFFF_FFFF, seq)
}

/// Format a SystemTime as ISO 8601 UTC (e.g. `2026-02-14T10:00:01Z`).
pub fn format_system_time(t: &SystemTime) -> String {
    let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();

    // Manual UTC breakdown — avoids pulling in chrono/time crate.
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let mins = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    // Days since 1970-01-01 to Y-M-D.
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    days += 719468;
    let era = days / 146097;
    let doe = days % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
pub fn test_alloc(
    name: &str,
    id_suffix: &str,
    status: mill_config::AllocStatus,
    address: Option<std::net::SocketAddr>,
) -> (mill_config::AllocId, mill_config::Alloc) {
    let id = mill_config::AllocId(format!("{name}-{id_suffix}"));
    let alloc = mill_config::Alloc {
        id: id.clone(),
        node: mill_config::NodeId("node-1".into()),
        name: name.to_owned(),
        kind: mill_config::AllocKind::Service,
        status,
        address,
        resources: mill_config::AllocResources { cpu: 0.25, memory: bytesize::ByteSize::mib(256) },
        started_at: None,
    };
    (id, alloc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn unix_epoch() {
        let t = SystemTime::UNIX_EPOCH;
        assert_eq!(format_system_time(&t), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_2026_timestamp() {
        // 2026-02-15T12:30:45Z = 1771158645 seconds since epoch
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1771158645);
        assert_eq!(format_system_time(&t), "2026-02-15T12:30:45Z");
    }

    #[test]
    fn leap_year_feb_29() {
        // 2024-02-29T00:00:00Z = 1709164800 seconds since epoch
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1709164800);
        assert_eq!(format_system_time(&t), "2024-02-29T00:00:00Z");
    }
}
