use bytesize::ByteSize;
use containerd_client::{
    services::v1::{MetricsRequest, tasks_client::TasksClient},
    with_namespace,
};
use mill_config::{AllocId, ContainerStats};
use prost::Message;
use tonic::Request;
use tonic::transport::Channel;

use crate::cgroups;
use crate::container::container_id;
use crate::error::{ContainerdError, Result};
use crate::image::NAMESPACE;

/// Collect resource usage stats for a single container.
pub async fn collect(channel: Channel, alloc_id: &AllocId) -> Result<ContainerStats> {
    let container_id = container_id(alloc_id);
    let mut client = TasksClient::new(channel);

    let req = MetricsRequest { filters: vec![format!("id=={container_id}")] };
    let req = with_namespace!(req, NAMESPACE);
    let resp = client.metrics(req).await?;
    let metrics = resp.into_inner().metrics;

    let metric = metrics
        .into_iter()
        .find(|m| m.id == container_id)
        .ok_or_else(|| ContainerdError::NotFound(alloc_id.0.clone()))?;

    decode_metric(alloc_id, &metric)
}

/// Collect resource usage stats for all mill-managed containers.
pub async fn collect_all(channel: Channel) -> Result<Vec<ContainerStats>> {
    let mut client = TasksClient::new(channel);

    let req = MetricsRequest::default();
    let req = with_namespace!(req, NAMESPACE);
    let resp = client.metrics(req).await?;
    let metrics = resp.into_inner().metrics;

    let mut stats = Vec::new();
    for metric in &metrics {
        if let Some(alloc_id_str) = metric.id.strip_prefix("mill-") {
            let alloc_id = AllocId(alloc_id_str.to_string());
            match decode_metric(&alloc_id, metric) {
                Ok(s) => stats.push(s),
                Err(e) => {
                    tracing::warn!("failed to decode metrics for {}: {e}", metric.id);
                }
            }
        }
    }

    Ok(stats)
}

fn decode_metric(
    alloc_id: &AllocId,
    metric: &containerd_client::types::Metric,
) -> Result<ContainerStats> {
    let data = metric
        .data
        .as_ref()
        .ok_or_else(|| ContainerdError::MetricsDecode("missing metric data".to_string()))?;

    let cgroup_metrics = cgroups::Metrics::decode(data.value.as_slice())
        .map_err(|e| ContainerdError::MetricsDecode(format!("failed to decode cgroups: {e}")))?;

    let cpu_usage = cgroup_metrics
        .cpu
        .as_ref()
        .map(|cpu| (cpu.user_usec + cpu.system_usec) as f64 / 1_000_000.0)
        .unwrap_or(0.0);

    let memory_usage =
        cgroup_metrics.memory.as_ref().map(|mem| ByteSize::b(mem.usage)).unwrap_or(ByteSize::b(0));

    Ok(ContainerStats { alloc_id: alloc_id.clone(), cpu_usage, memory_usage })
}
