use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

use mill_containerd::LogLine;
use tokio::sync::broadcast;

const MAX_LINES: usize = 10_000;
use mill_containerd::logs::BROADCAST_CAPACITY;

/// Ring buffer for container log lines with broadcast support for followers.
pub struct LogBuffer {
    buffer: Mutex<VecDeque<LogLine>>,
    tx: broadcast::Sender<LogLine>,
}

impl LogBuffer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { buffer: Mutex::new(VecDeque::with_capacity(MAX_LINES)), tx }
    }

    /// Append a log line, evicting the oldest if at capacity.
    pub fn push(&self, line: LogLine) {
        // Broadcast to followers (ignore if no receivers)
        let _ = self.tx.send(line.clone());

        let mut buf = self.buffer.lock();
        if buf.len() >= MAX_LINES {
            buf.pop_front();
        }
        buf.push_back(line);
    }

    /// Return the last `n` lines from the buffer.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        self.buffer.lock().iter().rev().take(n).rev().cloned().collect()
    }

    /// Subscribe to live log lines.
    pub fn follow(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }
}

/// Spawn a task that forwards log lines from a broadcast receiver into a LogBuffer.
pub fn spawn_log_forwarder(mut rx: broadcast::Receiver<LogLine>, buf: Arc<LogBuffer>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(line) => buf.push(line),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("log forwarder lagged, skipped {n} lines");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_containerd::LogStream;
    use std::time::SystemTime;

    fn make_line(content: &str) -> LogLine {
        LogLine { timestamp: SystemTime::now(), stream: LogStream::Stdout, content: content.into() }
    }

    #[test]
    fn tail_returns_last_n_lines() {
        let buf = LogBuffer::new();
        for i in 0..20 {
            buf.push(make_line(&format!("line-{i}")));
        }
        let tail = buf.tail(5);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0].content, "line-15");
        assert_eq!(tail[4].content, "line-19");
    }

    #[test]
    fn tail_returns_all_when_fewer_than_n() {
        let buf = LogBuffer::new();
        buf.push(make_line("only"));
        let tail = buf.tail(100);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].content, "only");
    }

    #[test]
    fn evicts_oldest_beyond_capacity() {
        let buf = LogBuffer::new();
        for i in 0..MAX_LINES + 50 {
            buf.push(make_line(&format!("line-{i}")));
        }
        let tail = buf.tail(MAX_LINES);
        assert_eq!(tail.len(), MAX_LINES);
        assert_eq!(tail[0].content, "line-50");
    }

    #[tokio::test]
    async fn follow_receives_new_lines() {
        let buf = LogBuffer::new();
        let mut rx = buf.follow();
        buf.push(make_line("hello"));
        let received = rx.recv().await;
        assert!(received.is_ok());
        assert_eq!(received.unwrap_or_else(|_| make_line("fail")).content, "hello");
    }
}
