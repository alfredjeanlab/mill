use std::path::{Path, PathBuf};

const DEFAULT_DATA_DIR: &str = "/var/lib/mill";

/// Root data directory for all mill on-disk state.
///
/// Default: `/var/lib/mill`. Subdirectories:
/// - `raft/` — snapshot, vote, cluster key
/// - `wireguard/` — private key
/// - `logs/` — container log fifos
/// - `volumes/` — persistent volume mounts
/// - `ephemeral/` — ephemeral volume mounts
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn raft_dir(&self) -> PathBuf {
        self.root.join("raft")
    }

    pub fn wireguard_dir(&self) -> PathBuf {
        self.root.join("wireguard")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn volumes_dir(&self) -> PathBuf {
        self.root.join("volumes")
    }

    pub fn ephemeral_dir(&self) -> PathBuf {
        self.root.join("ephemeral")
    }

    pub fn certs_dir(&self) -> PathBuf {
        self.root.join("certs")
    }

    pub fn node_json_path(&self) -> PathBuf {
        self.root.join("node.json")
    }

    pub fn pid_path(&self) -> PathBuf {
        self.root.join("mill.pid")
    }
}

impl Default for DataDir {
    fn default() -> Self {
        Self { root: PathBuf::from(DEFAULT_DATA_DIR) }
    }
}

impl From<PathBuf> for DataDir {
    fn from(path: PathBuf) -> Self {
        Self { root: path }
    }
}

impl From<&Path> for DataDir {
    fn from(path: &Path) -> Self {
        Self { root: path.to_owned() }
    }
}

impl AsRef<Path> for DataDir {
    fn as_ref(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root() {
        let d = DataDir::default();
        assert_eq!(d.root(), Path::new("/var/lib/mill"));
    }

    #[test]
    fn custom_root() {
        let d = DataDir::new("/tmp/mill-test");
        assert_eq!(d.root(), Path::new("/tmp/mill-test"));
    }

    #[test]
    fn subdirectories() {
        let d = DataDir::new("/data");
        assert_eq!(d.raft_dir(), PathBuf::from("/data/raft"));
        assert_eq!(d.wireguard_dir(), PathBuf::from("/data/wireguard"));
        assert_eq!(d.logs_dir(), PathBuf::from("/data/logs"));
        assert_eq!(d.volumes_dir(), PathBuf::from("/data/volumes"));
        assert_eq!(d.ephemeral_dir(), PathBuf::from("/data/ephemeral"));
        assert_eq!(d.certs_dir(), PathBuf::from("/data/certs"));
        assert_eq!(d.node_json_path(), PathBuf::from("/data/node.json"));
        assert_eq!(d.pid_path(), PathBuf::from("/data/mill.pid"));
    }
}
