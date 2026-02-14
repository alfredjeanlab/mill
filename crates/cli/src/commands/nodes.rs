use mill_config::NodeResponse;

use crate::client::MillClient;
use crate::client::sse::SseStream;
use crate::error::CliError;
use crate::output::table;
use crate::output::{self, OutputMode};

pub async fn nodes(client: &MillClient, mode: OutputMode) -> Result<(), CliError> {
    let nodes: Vec<NodeResponse> = client.get("/v1/nodes").await?;

    output::print_list(mode, &nodes, |nodes| {
        table::print_nodes(nodes);
    });

    Ok(())
}

pub async fn drain(client: &MillClient, node_id: &str, mode: OutputMode) -> Result<(), CliError> {
    let path = format!("/v1/nodes/{node_id}/drain");
    let resp = client.post_empty_sse(&path).await?;
    let mut stream = SseStream::new(resp);

    loop {
        let event = stream.next_event().await.map_err(CliError::Io)?;
        let Some(event) = event else { break };

        match mode {
            OutputMode::Json => {
                crate::output::json::print_json_line(&serde_json::json!({
                    "event": event.event,
                    "data": event.data,
                }));
            }
            OutputMode::Human => {
                let event_type = event.event.as_deref().unwrap_or("progress");
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) {
                    match event_type {
                        "done" => {
                            if let Some(elapsed) = data.get("elapsed").and_then(|v| v.as_f64()) {
                                println!("  drain complete ({elapsed:.1}s)");
                            } else {
                                println!("  drain complete");
                            }
                        }
                        "failed" => {
                            let reason =
                                data.get("reason").and_then(|v| v.as_str()).unwrap_or("unknown");
                            eprintln!("  drain failed: {reason}");
                        }
                        _ => {
                            if let Some(phase) = data.get("phase").and_then(|v| v.as_str()) {
                                let svc =
                                    data.get("service").and_then(|v| v.as_str()).unwrap_or("");
                                if svc.is_empty() {
                                    println!("  {phase}");
                                } else {
                                    println!("  {svc}: {phase}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

pub async fn remove(client: &MillClient, node_id: &str) -> Result<(), CliError> {
    let path = format!("/v1/nodes/{node_id}");
    client.delete(&path).await?;
    eprintln!("node {node_id} removed");
    Ok(())
}
