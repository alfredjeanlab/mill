use mill_config::{NodeResponse, ServiceResponse, StatusResponse, TaskResponse};
use serde::Serialize;

use crate::client::MillClient;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::output::table;

#[derive(Serialize)]
struct StatusOutput {
    cluster: StatusResponse,
    services: Vec<ServiceResponse>,
    tasks: Vec<TaskResponse>,
    nodes: Vec<NodeResponse>,
}

pub async fn status(client: &MillClient, mode: OutputMode) -> Result<(), CliError> {
    let cluster: StatusResponse = client.get("/v1/status").await?;
    let services: Vec<ServiceResponse> = client.get("/v1/services").await?;
    let tasks: Vec<TaskResponse> = client.get("/v1/tasks").await?;
    let nodes: Vec<NodeResponse> = client.get("/v1/nodes").await?;

    match mode {
        OutputMode::Json => {
            let output = StatusOutput { cluster, services, tasks, nodes };
            crate::output::json::print_json(&output);
        }
        OutputMode::Human => {
            let all_healthy = nodes.iter().all(|n| n.status == "ready");
            let health = if all_healthy { "all healthy" } else { "degraded" };
            println!("CLUSTER: {} nodes, {}", cluster.node_count, health);
            println!();

            if !services.is_empty() {
                println!("SERVICES");
                table::print_services(&services);
                println!();
            }

            if !tasks.is_empty() {
                println!("TASKS");
                table::print_tasks(&tasks);
                println!();
            }

            if !nodes.is_empty() {
                println!("NODES");
                table::print_nodes(&nodes);
            }
        }
    }

    Ok(())
}
