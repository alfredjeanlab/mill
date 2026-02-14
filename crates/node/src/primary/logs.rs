use std::sync::Arc;

use mill_config::{AllocId, NodeId};
use mill_raft::MillRaft;
use tokio::sync::mpsc;

use super::api::sse::SseEvent;
use super::report::LogSubscribers;
use crate::rpc::{CommandRouter, NodeCommand};
use crate::util::format_system_time;

/// Stream logs for all allocations matching a given name.
///
/// Registers log subscribers, sends `LogTail` commands to the correct nodes,
/// and forwards log lines as SSE events. Returns when all channels close
/// or the progress sender is dropped.
pub async fn stream_logs(
    raft: &Arc<MillRaft>,
    router: &CommandRouter,
    log_subscribers: &LogSubscribers,
    name: &str,
    progress: &mpsc::Sender<SseEvent>,
    follow: bool,
    tail: usize,
) {
    // Find alloc IDs and their nodes.
    let allocs: Vec<(AllocId, NodeId)> = raft.read_state(|fsm| {
        fsm.allocs
            .values()
            .filter(|a| a.name == name || a.id.0 == name)
            .map(|a| (a.id.clone(), a.node.clone()))
            .collect()
    });
    let alloc_ids: Vec<AllocId> = allocs.iter().map(|(id, _)| id.clone()).collect();

    if alloc_ids.is_empty() {
        let _ = progress
            .send(SseEvent {
                event: "error".into(),
                data: format!("no allocations found for '{name}'"),
            })
            .await;
        return;
    }

    // Create subscriber channels and register them.
    let (log_tx, mut log_rx) = mpsc::channel(256);

    {
        let mut subs = log_subscribers.lock().await;
        for alloc_id in &alloc_ids {
            subs.entry(alloc_id.clone()).or_default().push(log_tx.clone());
        }
    }

    // Send LogTail commands to the correct nodes.
    for (alloc_id, node_id) in &allocs {
        let _ = router
            .send(node_id, NodeCommand::LogTail { alloc_id: alloc_id.clone(), lines: tail })
            .await;
    }

    // If following, also send LogFollow to subscribe to live updates.
    if follow {
        for (alloc_id, node_id) in &allocs {
            let _ =
                router.send(node_id, NodeCommand::LogFollow { alloc_id: alloc_id.clone() }).await;
        }
    }

    // Forward log lines as SSE events.
    // Drop our sender so log_rx closes when report processor senders close.
    drop(log_tx);

    while let Some(lines) = log_rx.recv().await {
        for line in &lines {
            let stream = match line.stream {
                mill_containerd::LogStream::Stdout => "stdout",
                mill_containerd::LogStream::Stderr => "stderr",
            };
            let data = serde_json::json!({
                "ts": format_system_time(&line.timestamp),
                "stream": stream,
                "line": line.content,
            })
            .to_string();
            if progress.send(SseEvent { event: String::new(), data }).await.is_err() {
                // Client disconnected.
                break;
            }
        }

        if !follow {
            break;
        }
    }

    // Clean up subscriptions and cancel follow streams.
    if follow {
        for (alloc_id, node_id) in &allocs {
            let _ =
                router.send(node_id, NodeCommand::LogUnfollow { alloc_id: alloc_id.clone() }).await;
        }
    }
    let mut subs = log_subscribers.lock().await;
    for alloc_id in &alloc_ids {
        if let Some(subscribers) = subs.get_mut(alloc_id) {
            subscribers.retain(|tx| !tx.is_closed());
            if subscribers.is_empty() {
                subs.remove(alloc_id);
            }
        }
    }
}
