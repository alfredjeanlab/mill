use mill_config::ServiceResponse;

use crate::client::MillClient;
use crate::client::sse::SseStream;
use crate::error::CliError;
use crate::output::OutputMode;

pub async fn logs(
    client: &MillClient,
    name: Option<String>,
    follow: bool,
    tail: Option<u64>,
    all: bool,
    mode: OutputMode,
) -> Result<(), CliError> {
    if all {
        return logs_all(client, follow, tail, mode).await;
    }

    let name = name.ok_or_else(|| {
        CliError::Arg("a service or task name is required (or use --all)".to_string())
    })?;

    let mut path = format!("/v1/logs/{name}");
    let mut params = Vec::new();
    if follow {
        params.push("follow=true".to_string());
    }
    if let Some(n) = tail {
        params.push(format!("tail={n}"));
    }
    if !params.is_empty() {
        path = format!("{path}?{}", params.join("&"));
    }

    let resp = client.get_sse(&path).await?;
    stream_logs(resp, None, mode).await
}

/// Stream logs from all services concurrently.
async fn logs_all(
    client: &MillClient,
    follow: bool,
    tail: Option<u64>,
    mode: OutputMode,
) -> Result<(), CliError> {
    let services: Vec<ServiceResponse> = client.get("/v1/services").await?;

    if services.is_empty() {
        if mode == OutputMode::Human {
            println!("no services running");
        }
        return Ok(());
    }

    // Build query params once.
    let mut query_parts = Vec::new();
    if follow {
        query_parts.push("follow=true".to_string());
    }
    if let Some(n) = tail {
        query_parts.push(format!("tail={n}"));
    }
    let query =
        if query_parts.is_empty() { String::new() } else { format!("?{}", query_parts.join("&")) };

    // Open one SSE stream per service.
    let mut handles = Vec::new();
    for svc in &services {
        let path = format!("/v1/logs/{}{}", svc.name, query);
        let resp = client.get_sse(&path).await?;
        let svc_name = svc.name.clone();
        handles.push((svc_name, resp));
    }

    // Interleave using tokio::select! across all streams.
    let mut streams: Vec<(String, SseStream)> =
        handles.into_iter().map(|(name, resp)| (name, SseStream::new(resp))).collect();

    // Simple round-robin poll. For truly concurrent interleaving, we poll
    // each stream in sequence. This is acceptable for CLI output.
    loop {
        let mut any_open = false;
        let mut to_remove = Vec::new();

        for (i, (svc_name, stream)) in streams.iter_mut().enumerate() {
            match stream.next_event().await {
                Ok(Some(event)) => {
                    any_open = true;
                    render_log_event(&event.data, Some(svc_name), mode);
                }
                Ok(None) => {
                    to_remove.push(i);
                }
                Err(e) => {
                    eprintln!("error reading logs for {svc_name}: {e}");
                    to_remove.push(i);
                }
            }
        }

        // Remove closed streams in reverse order.
        for i in to_remove.into_iter().rev() {
            streams.remove(i);
        }

        if !any_open && streams.is_empty() {
            break;
        }
    }

    Ok(())
}

/// Stream log events from a single SSE response.
async fn stream_logs(
    resp: reqwest::Response,
    prefix: Option<&str>,
    mode: OutputMode,
) -> Result<(), CliError> {
    let mut stream = SseStream::new(resp);

    loop {
        let event = stream.next_event().await.map_err(CliError::Io)?;
        let Some(event) = event else { break };
        render_log_event(&event.data, prefix, mode);
    }

    Ok(())
}

fn render_log_event(data: &str, prefix: Option<&str>, mode: OutputMode) {
    match mode {
        OutputMode::Json => {
            // Pass through the raw JSON data line.
            println!("{data}");
        }
        OutputMode::Human => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                let line = parsed.get("line").and_then(|v| v.as_str()).unwrap_or(data);
                match prefix {
                    Some(p) => println!("[{p}] {line}"),
                    None => println!("{line}"),
                }
            } else {
                match prefix {
                    Some(p) => println!("[{p}] {data}"),
                    None => println!("{data}"),
                }
            }
        }
    }
}
