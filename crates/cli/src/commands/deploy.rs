use crate::client::MillClient;
use crate::client::sse::SseStream;
use crate::error::CliError;
use crate::output::OutputMode;

pub async fn deploy(client: &MillClient, file: &str, mode: OutputMode) -> Result<(), CliError> {
    let config = std::fs::read_to_string(file)
        .map_err(|e| CliError::Io(std::io::Error::new(e.kind(), format!("{file}: {e}"))))?;

    let resp = client.post_text_sse("/v1/deploy", config).await?;
    render_deploy_stream(resp, mode).await
}

pub async fn restart(client: &MillClient, service: &str, mode: OutputMode) -> Result<(), CliError> {
    let path = format!("/v1/restart/{service}");
    let resp = client.post_empty_sse(&path).await?;
    render_deploy_stream(resp, mode).await
}

/// Render an SSE deploy/restart progress stream.
async fn render_deploy_stream(resp: reqwest::Response, mode: OutputMode) -> Result<(), CliError> {
    let mut stream = SseStream::new(resp);
    let mut failed = false;

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
                                println!("  deployed ({elapsed:.1}s)");
                            } else {
                                println!("  deployed");
                            }
                        }
                        "failed" => {
                            failed = true;
                            let svc = data.get("service").and_then(|v| v.as_str()).unwrap_or("");
                            let reason =
                                data.get("reason").and_then(|v| v.as_str()).unwrap_or("unknown");
                            if svc.is_empty() {
                                eprintln!("  failed: {reason}");
                            } else {
                                eprintln!("  {svc}: failed: {reason}");
                            }
                        }
                        _ => {
                            render_progress_line(&data);
                        }
                    }
                }
            }
        }
    }

    if failed { Err(CliError::Arg("deploy failed".to_string())) } else { Ok(()) }
}

/// Render a single deploy progress event as a human-readable line.
fn render_progress_line(data: &serde_json::Value) {
    let phase = match data.get("phase").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return,
    };

    let service = data.get("service").and_then(|v| v.as_str());

    match (service, phase) {
        (Some(svc), "pulling") => println!("  {svc}: pulling image..."),
        (Some(svc), "scheduled") => {
            let node = data.get("node").and_then(|v| v.as_str()).unwrap_or("unknown");
            println!("  {svc}: scheduled on {node}");
        }
        (Some(svc), "starting") => println!("  {svc}: starting..."),
        (Some(svc), "healthy") => {
            if let Some(elapsed) = data.get("elapsed").and_then(|v| v.as_f64()) {
                println!("  {svc}: healthy ({elapsed:.1}s)");
            } else {
                println!("  {svc}: healthy");
            }
        }
        (Some(svc), "stopped") => println!("  {svc}: old instance stopped"),
        (None, "started") => {} // silent
        (None, "pulling") => println!("  pulling images..."),
        (Some(svc), other) => println!("  {svc}: {other}"),
        (None, other) => println!("  {other}"),
    }
}
