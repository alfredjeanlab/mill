use std::collections::HashMap;

use mill_config::{ContainerSpec, ContainerVolume, DataDir};
use oci_spec::runtime::{
    LinuxBuilder, LinuxCpuBuilder, LinuxMemoryBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    LinuxResourcesBuilder, MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder,
    get_default_mounts,
};

use crate::error::{ContainerdError, Result};

pub(crate) const LABEL_MANAGED: &str = "mill.managed";
pub(crate) const LABEL_ALLOC_ID: &str = "mill.alloc-id";
pub(crate) const LABEL_HOST_PORT: &str = "mill.host-port";
pub(crate) const LABEL_CPU: &str = "mill.cpu";
pub(crate) const LABEL_MEMORY: &str = "mill.memory";
pub(crate) const LABEL_VOLUMES: &str = "mill.volumes";
pub(crate) const LABEL_KIND: &str = "mill.kind";
pub(crate) const LABEL_TIMEOUT: &str = "mill.timeout";
pub(crate) const LABEL_COMMAND: &str = "mill.command";

/// Build an OCI runtime spec from a mill ContainerSpec.
///
/// `image_defaults` provides the image's ENTRYPOINT + CMD and default ENV
/// (e.g. PATH) from the OCI image config. These are used as fallbacks when
/// not overridden by the ContainerSpec.
pub fn build_oci_spec(
    data_dir: &DataDir,
    spec: &ContainerSpec,
    image_defaults: &crate::image::ImageInfo,
) -> Result<Spec> {
    // Merge env: image defaults first, then spec overrides on top.
    let mut env_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in &image_defaults.default_env {
        if let Some((k, v)) = entry.split_once('=') {
            env_map.insert(k.to_string(), v.to_string());
        }
    }
    for (k, v) in &spec.env {
        env_map.insert(k.clone(), v.clone());
    }
    let env: Vec<String> = env_map.into_iter().map(|(k, v)| format!("{k}={v}")).collect();

    let args = match spec.command {
        Some(ref cmd) => cmd.clone(),
        None => image_defaults.default_command.clone(),
    };
    let process = ProcessBuilder::default()
        .args(args)
        .env(env)
        .cwd("/")
        .build()
        .map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let root = RootBuilder::default()
        .path("rootfs")
        .readonly(false)
        .build()
        .map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let cpu_quota = (spec.cpu * 100_000.0) as i64;
    let cpu = LinuxCpuBuilder::default()
        .quota(cpu_quota)
        .period(100_000u64)
        .build()
        .map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let memory_limit = spec.memory.as_u64() as i64;
    let memory = LinuxMemoryBuilder::default()
        .limit(memory_limit)
        .swap(memory_limit)
        .build()
        .map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let resources = LinuxResourcesBuilder::default()
        .cpu(cpu)
        .memory(memory)
        .build()
        .map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let mut linux_builder = LinuxBuilder::default().resources(resources);

    // If MILL_NETNS_PATH is set, run the container inside that named network
    // namespace. Used by the smoke-test harness so containers share the mill
    // node's netns (and can therefore be reached at the node's WireGuard IP).
    //
    // When overriding the network namespace we must supply a complete namespace
    // list (PID, IPC, UTS, Mount, Network). runc uses the presence of a Mount
    // namespace to decide whether to set up seccomp/sysctl restrictions, and
    // requires UTS to isolate hostname. The full list matches containerd's own
    // defaults so runc can bootstrap the container normally.
    if let Ok(netns_path) = std::env::var("MILL_NETNS_PATH") {
        let build_ns = |typ, path: Option<String>| {
            let b = LinuxNamespaceBuilder::default().typ(typ);
            let b = if let Some(p) = path { b.path(std::path::PathBuf::from(p)) } else { b };
            b.build().map_err(|e| ContainerdError::SpecBuild(e.to_string()))
        };
        let namespaces = vec![
            build_ns(LinuxNamespaceType::Pid, None)?,
            build_ns(LinuxNamespaceType::Ipc, None)?,
            build_ns(LinuxNamespaceType::Uts, None)?,
            build_ns(LinuxNamespaceType::Mount, None)?,
            build_ns(LinuxNamespaceType::Network, Some(netns_path))?,
        ];
        linux_builder = linux_builder.namespaces(namespaces);
    }

    let linux = linux_builder.build().map_err(|e| ContainerdError::SpecBuild(e.to_string()))?;

    let volume_mounts: Vec<_> =
        spec.volumes.iter().map(|v| build_volume_mount(data_dir, v)).collect::<Result<_>>()?;

    // Start from the oci-spec default mounts (proc, dev, sys, etc.) and strip
    // the cgroup v1 mount at /sys/fs/cgroup. That mount fails on cgroup v2
    // kernels (Linux 5.2+) and breaks container initialization with a
    // misleading "stat /bin/sh: no such file or directory" error.
    // We set OCI spec version 1.1.0 so containerd handles cgroup v2 natively.
    let mut all_mounts = get_default_mounts();
    all_mounts.retain(|m| {
        !(m.destination() == std::path::Path::new("/sys/fs/cgroup")
            && m.typ().as_deref() == Some("cgroup"))
    });
    all_mounts.extend(volume_mounts);

    let builder = SpecBuilder::default()
        .version("1.1.0")
        .process(process)
        .root(root)
        .linux(linux)
        .mounts(all_mounts);

    builder.build().map_err(|e| ContainerdError::SpecBuild(e.to_string()))
}

fn build_volume_mount(
    data_dir: &DataDir,
    vol: &ContainerVolume,
) -> Result<oci_spec::runtime::Mount> {
    let host_path = volume_host_path(data_dir, vol);
    MountBuilder::default()
        .destination(&vol.path)
        .source(host_path)
        .typ("bind")
        .options(vec!["rbind".to_string(), "rw".to_string()])
        .build()
        .map_err(|e| ContainerdError::SpecBuild(e.to_string()))
}

/// Compute the host-side path for a volume mount.
fn volume_host_path(data_dir: &DataDir, vol: &ContainerVolume) -> String {
    if vol.ephemeral {
        data_dir.ephemeral_dir().join(&vol.name).to_string_lossy().into_owned()
    } else {
        data_dir.volumes_dir().join(&vol.name).to_string_lossy().into_owned()
    }
}

/// Build containerd container labels for a mill container.
pub fn build_labels(spec: &ContainerSpec) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
    labels.insert(LABEL_ALLOC_ID.to_string(), spec.alloc_id.0.clone());
    if let Some(port) = spec.host_port {
        labels.insert(LABEL_HOST_PORT.to_string(), port.to_string());
    }
    labels.insert(LABEL_CPU.to_string(), spec.cpu.to_string());
    labels.insert(LABEL_MEMORY.to_string(), spec.memory.as_u64().to_string());
    if !spec.volumes.is_empty()
        && let Ok(json) = serde_json::to_string(&spec.volumes)
    {
        labels.insert(LABEL_VOLUMES.to_string(), json);
    }
    labels.insert(LABEL_KIND.to_string(), spec.kind.to_string());
    if let Some(timeout) = spec.timeout {
        labels.insert(LABEL_TIMEOUT.to_string(), timeout.as_secs().to_string());
    }
    if let Some(ref cmd) = spec.command
        && let Ok(json) = serde_json::to_string(cmd)
    {
        labels.insert(LABEL_COMMAND.to_string(), json);
    }
    labels
}

#[cfg(test)]
mod tests {
    use bytesize::ByteSize;
    use mill_config::AllocId;

    use super::*;

    fn test_spec() -> ContainerSpec {
        ContainerSpec {
            alloc_id: AllocId("test-abc-123".to_string()),
            kind: mill_config::AllocKind::Service,
            timeout: None,
            command: None,
            image: "nginx:latest".to_string(),
            env: HashMap::from([
                ("PORT".to_string(), "8080".to_string()),
                ("ENV".to_string(), "production".to_string()),
            ]),
            port: Some(80),
            host_port: Some(8080),
            cpu: 0.5,
            memory: ByteSize::mib(256),
            volumes: vec![
                ContainerVolume {
                    name: "data".to_string(),
                    path: "/app/data".to_string(),
                    ephemeral: false,
                },
                ContainerVolume {
                    name: "cache".to_string(),
                    path: "/tmp/cache".to_string(),
                    ephemeral: true,
                },
            ],
        }
    }

    fn data_dir() -> DataDir {
        DataDir::new("/var/lib/mill")
    }

    fn empty_image_info() -> crate::image::ImageInfo {
        crate::image::ImageInfo {
            chain_id: String::new(),
            default_command: vec![],
            default_env: vec![],
        }
    }

    #[test]
    fn test_cpu_quota_fraction() {
        let spec = build_oci_spec(&data_dir(), &test_spec(), &empty_image_info());
        assert!(spec.is_ok());
        let spec = spec.ok();
        let cpu = spec
            .as_ref()
            .and_then(|s| s.linux().as_ref())
            .and_then(|l| l.resources().as_ref())
            .and_then(|r| r.cpu().as_ref());
        // 0.5 CPU = 50_000 quota with 100_000 period
        assert_eq!(cpu.and_then(|c| c.quota()), Some(50_000));
        assert_eq!(cpu.and_then(|c| c.period()), Some(100_000));
    }

    #[test]
    fn test_memory_limit() {
        let spec = build_oci_spec(&data_dir(), &test_spec(), &empty_image_info());
        assert!(spec.is_ok());
        let spec = spec.ok();
        let mem = spec
            .as_ref()
            .and_then(|s| s.linux().as_ref())
            .and_then(|l| l.resources().as_ref())
            .and_then(|r| r.memory().as_ref());
        // 256 MiB = 268435456 bytes
        assert_eq!(mem.and_then(|m| m.limit()), Some(268_435_456));
    }

    #[test]
    fn test_swap_limit_matches_memory() {
        let spec = build_oci_spec(&data_dir(), &test_spec(), &empty_image_info());
        assert!(spec.is_ok());
        let spec = spec.ok();
        let mem = spec
            .as_ref()
            .and_then(|s| s.linux().as_ref())
            .and_then(|l| l.resources().as_ref())
            .and_then(|r| r.memory().as_ref());
        // 256 MiB = 268435456 bytes — swap should match memory
        assert_eq!(mem.and_then(|m| m.swap()), Some(268_435_456));
    }

    #[test]
    fn test_env_formatting() {
        let spec = build_oci_spec(&data_dir(), &test_spec(), &empty_image_info());
        assert!(spec.is_ok());
        let spec = spec.ok();
        let env = spec.as_ref().and_then(|s| s.process().as_ref()).and_then(|p| p.env().as_ref());
        let env = env.cloned().unwrap_or_default();
        assert!(env.contains(&"PORT=8080".to_string()));
        assert!(env.contains(&"ENV=production".to_string()));
    }

    #[test]
    fn test_volume_paths() {
        assert_eq!(
            volume_host_path(
                &data_dir(),
                &ContainerVolume {
                    name: "data".to_string(),
                    path: "/app/data".to_string(),
                    ephemeral: false,
                }
            ),
            "/var/lib/mill/volumes/data"
        );
        assert_eq!(
            volume_host_path(
                &data_dir(),
                &ContainerVolume {
                    name: "cache".to_string(),
                    path: "/tmp/cache".to_string(),
                    ephemeral: true,
                }
            ),
            "/var/lib/mill/ephemeral/cache"
        );

        // Custom data dir
        let custom = DataDir::new("/opt/mill");
        assert_eq!(
            volume_host_path(
                &custom,
                &ContainerVolume {
                    name: "db".to_string(),
                    path: "/data".to_string(),
                    ephemeral: false,
                }
            ),
            "/opt/mill/volumes/db"
        );
    }

    #[test]
    fn test_labels() {
        let spec = test_spec();
        let labels = build_labels(&spec);
        assert_eq!(labels.get("mill.managed"), Some(&"true".to_string()));
        assert_eq!(labels.get("mill.alloc-id"), Some(&"test-abc-123".to_string()));
        assert_eq!(labels.get("mill.host-port"), Some(&"8080".to_string()));
        assert_eq!(labels.get("mill.cpu"), Some(&"0.5".to_string()));
        assert_eq!(labels.get("mill.memory"), Some(&ByteSize::mib(256).as_u64().to_string()));
        assert!(labels.contains_key("mill.volumes"));
        assert_eq!(labels.get("mill.kind"), Some(&"service".to_string()));
        assert!(!labels.contains_key("mill.timeout"));
        assert!(!labels.contains_key("mill.command"));
        assert_eq!(labels.len(), 7);
    }

    #[test]
    fn test_labels_no_host_port() {
        let mut spec = test_spec();
        spec.host_port = None;
        let labels = build_labels(&spec);
        assert_eq!(labels.get("mill.managed"), Some(&"true".to_string()));
        assert_eq!(labels.get("mill.alloc-id"), Some(&"test-abc-123".to_string()));
        assert!(!labels.contains_key("mill.host-port"));
        assert_eq!(labels.get("mill.cpu"), Some(&"0.5".to_string()));
        assert_eq!(labels.get("mill.memory"), Some(&ByteSize::mib(256).as_u64().to_string()));
        assert!(labels.contains_key("mill.volumes"));
        assert_eq!(labels.len(), 6);
    }

    #[test]
    fn test_volume_mounts() {
        let spec = build_oci_spec(&data_dir(), &test_spec(), &empty_image_info());
        assert!(spec.is_ok());
        let spec = spec.ok();
        let mounts = spec.as_ref().and_then(|s| s.mounts().as_ref());
        let mounts = mounts.cloned().unwrap_or_default();
        // Find our bind mounts (filter out any default mounts)
        let bind_mounts: Vec<_> =
            mounts.iter().filter(|m| m.typ().as_deref() == Some("bind")).collect();
        assert_eq!(bind_mounts.len(), 2);
    }

    #[test]
    fn test_full_cpu_quota() {
        let mut spec = test_spec();
        spec.cpu = 2.0;
        let oci = build_oci_spec(&data_dir(), &spec, &empty_image_info());
        assert!(oci.is_ok());
        let oci = oci.ok();
        let cpu = oci
            .as_ref()
            .and_then(|s| s.linux().as_ref())
            .and_then(|l| l.resources().as_ref())
            .and_then(|r| r.cpu().as_ref());
        // 2 CPUs = 200_000 quota
        assert_eq!(cpu.and_then(|c| c.quota()), Some(200_000));
    }

    #[test]
    fn test_labels_no_volumes() {
        let mut spec = test_spec();
        spec.volumes = vec![];
        let labels = build_labels(&spec);
        assert!(!labels.contains_key(LABEL_VOLUMES));
        assert_eq!(labels.len(), 6);
    }

    #[test]
    fn test_oci_spec_with_command() {
        let mut spec = test_spec();
        spec.command = Some(vec!["nginx".to_string(), "-g".to_string(), "daemon off;".to_string()]);
        let oci = build_oci_spec(&data_dir(), &spec, &empty_image_info()).unwrap();
        let args = oci.process().as_ref().and_then(|p| p.args().as_ref());
        assert_eq!(
            args.cloned().unwrap(),
            vec!["nginx".to_string(), "-g".to_string(), "daemon off;".to_string()]
        );
    }

    #[test]
    fn test_oci_spec_without_command_uses_image_default() {
        let spec = test_spec();
        assert!(spec.command.is_none());
        let info = crate::image::ImageInfo {
            chain_id: String::new(),
            default_command: vec!["sh".to_string()],
            default_env: vec!["PATH=/usr/local/bin:/usr/bin:/bin".to_string()],
        };
        let oci = build_oci_spec(&data_dir(), &spec, &info).unwrap();
        let args = oci.process().as_ref().and_then(|p| p.args().as_ref());
        // When command is None, the image's default command is used
        assert_eq!(args.cloned().unwrap(), vec!["sh".to_string()]);
        // Image's default env is merged with spec env
        let env =
            oci.process().as_ref().and_then(|p| p.env().as_ref()).cloned().unwrap_or_default();
        assert!(env.iter().any(|e| e.starts_with("PATH=")));
        // Spec's env takes precedence (PORT=8080 from test_spec)
        assert!(env.iter().any(|e| e == "PORT=8080"));
    }

    #[test]
    fn test_labels_with_kind_and_command() {
        let mut spec = test_spec();
        spec.kind = mill_config::AllocKind::Task;
        spec.timeout = Some(std::time::Duration::from_secs(3600));
        spec.command = Some(vec!["migrate".to_string(), "--run".to_string()]);
        let labels = build_labels(&spec);
        assert_eq!(labels.get("mill.kind"), Some(&"task".to_string()));
        assert_eq!(labels.get("mill.timeout"), Some(&"3600".to_string()));
        let cmd: Vec<String> = serde_json::from_str(labels.get("mill.command").unwrap()).unwrap();
        assert_eq!(cmd, vec!["migrate".to_string(), "--run".to_string()]);
    }
}
