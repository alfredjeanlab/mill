use std::path::Path;

use crate::config;
use crate::error::SnapshotError;

/// Write snapshot data and metadata to disk atomically.
///
/// Writes `snapshot.bin.tmp` + fsync + rename, then `snapshot-meta.json.tmp`
/// + fsync + rename. Data is committed before meta so a crash during the
///   meta write leaves the previous consistent pair.
pub async fn write_snapshot(
    dir: &Path,
    data: &[u8],
    meta: &config::SnapshotMeta,
) -> Result<(), SnapshotError> {
    atomic_write(dir, "snapshot.bin", data).await?;

    let meta_json =
        serde_json::to_vec_pretty(meta).map_err(|e| SnapshotError::Encode(e.to_string()))?;
    atomic_write(dir, "snapshot-meta.json", &meta_json).await?;

    Ok(())
}

/// Read a persisted snapshot from disk, if one exists.
pub async fn read_snapshot(
    dir: &Path,
) -> Result<Option<(Vec<u8>, config::SnapshotMeta)>, SnapshotError> {
    let data_path = dir.join("snapshot.bin");
    let meta_path = dir.join("snapshot-meta.json");

    if !tokio::fs::try_exists(&data_path).await.unwrap_or(false)
        || !tokio::fs::try_exists(&meta_path).await.unwrap_or(false)
    {
        return Ok(None);
    }

    let data = tokio::fs::read(&data_path).await?;
    let meta_bytes = tokio::fs::read(&meta_path).await?;
    let meta: config::SnapshotMeta =
        serde_json::from_slice(&meta_bytes).map_err(|e| SnapshotError::Decode(e.to_string()))?;

    Ok(Some((data, meta)))
}

/// Write the current vote to disk atomically.
pub async fn write_vote(dir: &Path, vote: &config::Vote) -> Result<(), SnapshotError> {
    let json = serde_json::to_vec_pretty(vote).map_err(|e| SnapshotError::Encode(e.to_string()))?;
    atomic_write(dir, "vote.json", &json).await
}

/// Read the persisted vote, if one exists.
pub async fn read_vote(dir: &Path) -> Result<Option<config::Vote>, SnapshotError> {
    let path = dir.join("vote.json");
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(None);
    }

    let bytes = tokio::fs::read(&path).await?;
    let vote: config::Vote =
        serde_json::from_slice(&bytes).map_err(|e| SnapshotError::Decode(e.to_string()))?;
    Ok(Some(vote))
}

/// Write data to `dir/filename` via tmp + fsync + rename.
async fn atomic_write(dir: &Path, filename: &str, data: &[u8]) -> Result<(), SnapshotError> {
    let tmp = dir.join(format!("{filename}.tmp"));
    let path = dir.join(filename);
    tokio::fs::write(&tmp, data).await?;
    let file = tokio::fs::File::open(&tmp).await?;
    file.sync_all().await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_log_id(index: u64) -> config::LogId {
        // CommittedLeaderId is an alias for LeaderId.
        config::LogId::new(openraft::CommittedLeaderId::new(1, 1), index)
    }

    #[tokio::test]
    async fn write_read_snapshot_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        let data = b"test-snapshot-data".to_vec();
        let meta = config::SnapshotMeta {
            last_log_id: Some(test_log_id(42)),
            last_membership: config::StoredMembership::default(),
            snapshot_id: "snap-42".to_string(),
        };

        write_snapshot(dir, &data, &meta).await.unwrap();

        let result = read_snapshot(dir).await.unwrap();
        assert!(result.is_some());
        let (read_data, read_meta) = result.unwrap();
        assert_eq!(read_data, data);
        assert_eq!(read_meta.snapshot_id, "snap-42");
        assert_eq!(read_meta.last_log_id.unwrap().index, 42);
    }

    #[tokio::test]
    async fn read_snapshot_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_snapshot(dir.path()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn write_read_vote_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        let vote = config::Vote::new(1, 2);

        write_vote(dir, &vote).await.unwrap();

        let result = read_vote(dir).await.unwrap();
        assert!(result.is_some());
        let read_vote = result.unwrap();
        assert_eq!(read_vote, vote);
    }

    #[tokio::test]
    async fn read_vote_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_vote(dir.path()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn write_snapshot_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        let meta1 = config::SnapshotMeta {
            last_log_id: Some(test_log_id(1)),
            last_membership: config::StoredMembership::default(),
            snapshot_id: "snap-1".to_string(),
        };
        write_snapshot(dir, b"first", &meta1).await.unwrap();

        let meta2 = config::SnapshotMeta {
            last_log_id: Some(test_log_id(2)),
            last_membership: config::StoredMembership::default(),
            snapshot_id: "snap-2".to_string(),
        };
        write_snapshot(dir, b"second", &meta2).await.unwrap();

        let (data, meta) = read_snapshot(dir).await.unwrap().unwrap();
        assert_eq!(data, b"second");
        assert_eq!(meta.snapshot_id, "snap-2");
    }

    #[tokio::test]
    async fn read_snapshot_corrupt_meta_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        // Write valid snapshot data but invalid JSON for meta.
        tokio::fs::write(dir.join("snapshot.bin"), b"data").await.unwrap();
        tokio::fs::write(dir.join("snapshot-meta.json"), b"not-json").await.unwrap();

        let result = read_snapshot(dir).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_snapshot_only_data_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        // Data file without meta — simulates crash between the two writes.
        tokio::fs::write(dir.join("snapshot.bin"), b"data").await.unwrap();

        let result = read_snapshot(dir).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn write_vote_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        let vote1 = config::Vote::new(1, 2);
        write_vote(dir, &vote1).await.unwrap();

        let vote2 = config::Vote::new(3, 4);
        write_vote(dir, &vote2).await.unwrap();

        let read = read_vote(dir).await.unwrap().unwrap();
        assert_eq!(read, vote2);
    }
}
