use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::{ContainerdError, Result};

pub const BROADCAST_CAPACITY: usize = 4096;

/// A single line of container output.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub timestamp: SystemTime,
    pub stream: LogStream,
    pub content: String,
}

/// Which output stream a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// Create named fifos for stdout/stderr and spawn async readers that push
/// lines into a broadcast channel.
///
/// Returns the broadcast sender and the paths to the two fifos.
pub fn setup_fifos(
    log_base_dir: &Path,
    alloc_id: &str,
    cancel: CancellationToken,
) -> Result<(broadcast::Sender<LogLine>, PathBuf, PathBuf)> {
    let log_dir = log_base_dir.join(alloc_id);
    std::fs::create_dir_all(&log_dir).map_err(ContainerdError::LogIo)?;

    let stdout_path = log_dir.join("stdout");
    let stderr_path = log_dir.join("stderr");

    create_fifo(&stdout_path)?;
    create_fifo(&stderr_path)?;

    let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);

    spawn_reader(stdout_path.clone(), LogStream::Stdout, tx.clone(), cancel.clone());
    spawn_reader(stderr_path.clone(), LogStream::Stderr, tx.clone(), cancel);

    Ok((tx, stdout_path, stderr_path))
}

/// Reconnect to existing fifos for a crash-recovered container.
///
/// If the fifo directory and both pipes still exist from before the crash,
/// reopens them and returns a broadcast sender. Returns `None` if the fifos
/// are gone (e.g. cleaned up by another process).
pub fn reconnect_fifos(
    log_base_dir: &Path,
    alloc_id: &str,
    cancel: CancellationToken,
) -> Option<broadcast::Sender<LogLine>> {
    let log_dir = log_base_dir.join(alloc_id);
    let stdout_path = log_dir.join("stdout");
    let stderr_path = log_dir.join("stderr");
    if !stdout_path.exists() || !stderr_path.exists() {
        return None;
    }
    let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
    spawn_reader(stdout_path, LogStream::Stdout, tx.clone(), cancel.clone());
    spawn_reader(stderr_path, LogStream::Stderr, tx.clone(), cancel);
    Some(tx)
}

/// Remove fifo directory for a container.
pub fn cleanup_fifos(log_base_dir: &Path, alloc_id: &str) {
    let log_dir = log_base_dir.join(alloc_id);
    let _ = std::fs::remove_dir_all(log_dir);
}

fn create_fifo(path: &Path) -> Result<()> {
    // Remove stale fifo if it exists
    if path.exists() {
        std::fs::remove_file(path).map_err(ContainerdError::LogIo)?;
    }

    nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR)
        .map_err(|e| ContainerdError::LogIo(std::io::Error::from(e)))?;

    Ok(())
}

fn spawn_reader(
    path: PathBuf,
    stream: LogStream,
    tx: broadcast::Sender<LogLine>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        // open blocks on FIFO until the writer (containerd) connects;
        // wrap in select so cancellation can interrupt the blocking open
        let file = tokio::select! {
            result = tokio::fs::File::open(&path) => {
                match result {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!("failed to open fifo {}: {e}", path.display());
                        return;
                    }
                }
            }
            _ = cancel.cancelled() => return,
        };
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        loop {
            tokio::select! {
                result = lines.next_line() => {
                    match result {
                        Ok(Some(content)) => {
                            let line = LogLine { timestamp: SystemTime::now(), stream, content };
                            // If no receivers, just drop the line
                            let _ = tx.send(line);
                        }
                        Ok(None) => break, // EOF — writer closed
                        Err(e) => {
                            tracing::error!("error reading fifo {}: {e}", path.display());
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
    });
}
