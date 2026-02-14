/// Minimal prost-derived cgroups v2 metric types.
///
/// These match the protobuf schema at:
/// <https://github.com/containerd/cgroups/blob/main/cgroup2/stats/metrics.proto>
///
/// Only CPU and memory fields are decoded; all others are skipped.

#[derive(Clone, prost::Message)]
pub struct Metrics {
    /// CPU stats (tag 2 in cgroups v2 proto)
    #[prost(message, optional, tag = "2")]
    pub cpu: Option<CpuStat>,
    /// Memory stats (tag 4 in cgroups v2 proto)
    #[prost(message, optional, tag = "4")]
    pub memory: Option<MemoryStat>,
}

#[derive(Clone, prost::Message)]
pub struct CpuStat {
    #[prost(uint64, tag = "2")]
    pub user_usec: u64,
    #[prost(uint64, tag = "3")]
    pub system_usec: u64,
}

#[derive(Clone, prost::Message)]
pub struct MemoryStat {
    #[prost(uint64, tag = "1")]
    pub usage: u64,
    #[prost(uint64, tag = "2")]
    pub usage_limit: u64,
}
