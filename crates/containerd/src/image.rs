use std::collections::HashMap;

use base64::Engine as _;
use containerd_client::{
    services::v1::{
        GetImageRequest, ReadContentRequest, TransferRequest, content_client::ContentClient,
        images_client::ImagesClient, transfer_client::TransferClient,
    },
    with_namespace,
};
use prost_types::Any;
use sha2::{Digest as _, Sha256};
use tokio_stream::StreamExt as _;
use tonic::Request;
use tonic::transport::Channel;

use crate::error::{ContainerdError, Result};

pub(crate) const NAMESPACE: &str = "mill";

/// Optional registry authentication credentials.
#[derive(Debug, Clone)]
pub struct RegistryAuth {
    pub username: String,
    pub password: String,
}

/// Minimal prost representation of containerd's Resolver for registry authentication.
#[derive(Clone, PartialEq, prost::Message)]
struct Resolver {
    #[prost(map = "string, string", tag = "1")]
    headers: HashMap<String, String>,
}

impl prost::Name for Resolver {
    const NAME: &'static str = "Resolver";
    const PACKAGE: &'static str = "containerd.types.transfer";
    fn full_name() -> String {
        "containerd.types.transfer.Resolver".into()
    }
    fn type_url() -> String {
        "/containerd.types.transfer.Resolver".into()
    }
}

/// Minimal prost representation of containerd's OciRegistry transfer source.
#[derive(Clone, PartialEq, prost::Message)]
struct OciRegistry {
    #[prost(string, tag = "1")]
    reference: String,
    #[prost(message, optional, tag = "2")]
    resolver: Option<Resolver>,
}

impl prost::Name for OciRegistry {
    const NAME: &'static str = "OCIRegistry";
    const PACKAGE: &'static str = "containerd.types.transfer";
    fn full_name() -> String {
        "containerd.types.transfer.OCIRegistry".into()
    }
    fn type_url() -> String {
        "/containerd.types.transfer.OCIRegistry".into()
    }
}

/// Minimal prost representation of containerd's ImageStore transfer destination.
#[derive(Clone, PartialEq, prost::Message)]
struct ImageStore {
    #[prost(string, tag = "1")]
    name: String,
}

impl prost::Name for ImageStore {
    const NAME: &'static str = "ImageStore";
    const PACKAGE: &'static str = "containerd.types.transfer";
    fn full_name() -> String {
        "containerd.types.transfer.ImageStore".into()
    }
    fn type_url() -> String {
        "/containerd.types.transfer.ImageStore".into()
    }
}

/// Check if an image is already present in the local containerd image store.
pub async fn is_cached(channel: Channel, image: &str) -> Result<bool> {
    let mut client = ImagesClient::new(channel);
    let req = containerd_client::services::v1::GetImageRequest { name: image.to_string() };
    let req = with_namespace!(req, NAMESPACE);
    match client.get(req).await {
        Ok(_) => Ok(true),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(false),
        Err(status) => Err(ContainerdError::Grpc(Box::new(status))),
    }
}

/// Pull an image from an OCI registry into the containerd image store.
///
/// Tries the containerd Transfer API (2.0+) first. If the server returns
/// `Unimplemented` (containerd 1.x), falls back to shelling out to `ctr`.
pub async fn pull(channel: Channel, image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
    match pull_transfer(channel, image, auth).await {
        Ok(()) => return Ok(()),
        Err(ContainerdError::ImagePull { ref reason, .. })
            if reason.contains("not implemented") || reason.contains("Unimplemented") =>
        {
            tracing::info!(image = %image, "Transfer API unavailable, falling back to ctr");
        }
        Err(e) => return Err(e),
    }
    pull_ctr(image, auth).await
}

/// Pull via the containerd 2.0+ Transfer gRPC API.
async fn pull_transfer(channel: Channel, image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
    let mut client = TransferClient::new(channel);

    let resolver = auth.map(|a| {
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", a.username, a.password));
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Basic {credentials}"));
        Resolver { headers }
    });

    let source = OciRegistry { reference: image.to_string(), resolver };
    let destination = ImageStore { name: image.to_string() };

    let source_any = Any::from_msg(&source).map_err(|e| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: format!("failed to encode source: {e}"),
    })?;
    let destination_any = Any::from_msg(&destination).map_err(|e| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: format!("failed to encode destination: {e}"),
    })?;

    let req = TransferRequest {
        source: Some(source_any),
        destination: Some(destination_any),
        options: None,
    };
    let req = with_namespace!(req, NAMESPACE);

    tracing::info!(image = %image, "pulling image via Transfer API");
    client.transfer(req).await.map_err(|status| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: status.message().to_string(),
    })?;
    tracing::info!(image = %image, "image pull complete");

    Ok(())
}

/// Fallback: pull via the `ctr` CLI (works with containerd 1.x and 2.x).
async fn pull_ctr(image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
    let mut cmd = tokio::process::Command::new("ctr");
    cmd.args(["-n", NAMESPACE, "images", "pull"]);
    if let Some(a) = auth {
        cmd.args(["--user", &format!("{}:{}", a.username, a.password)]);
    }
    cmd.arg(image);

    tracing::info!(image = %image, "pulling image via ctr");
    let output = cmd.output().await.map_err(|e| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: format!("failed to run ctr: {e}"),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ContainerdError::ImagePull {
            image: image.to_string(),
            reason: format!("ctr pull failed: {stderr}"),
        });
    }

    tracing::info!(image = %image, "image pull complete (via ctr)");
    Ok(())
}

/// Information extracted from the OCI image config needed to create a container.
pub struct ImageInfo {
    /// The OCI chain ID for the image's layer stack, used as the parent
    /// when preparing a writable overlayfs snapshot.
    pub chain_id: String,
    /// The default command (ENTRYPOINT + CMD) from the image config.
    /// Empty if the image defines neither.
    pub default_command: Vec<String>,
    /// The default environment variables from the image config (e.g. PATH).
    pub default_env: Vec<String>,
}

/// Read the OCI image config and return the chain ID and default command.
///
/// The chain ID is derived from the image's diff IDs (uncompressed layer hashes):
///   - Single layer: chainID = diffID
///   - N layers:     chainID = SHA256(prevChainID + " " + diffID)
///
/// The default command is `ENTRYPOINT + CMD` from the image config, matching
/// Docker/OCI semantics.
pub async fn image_info(channel: Channel, image: &str) -> Result<ImageInfo> {
    // Get the image's manifest digest.
    let mut images_client = ImagesClient::new(channel.clone());
    let req = GetImageRequest { name: image.to_string() };
    let req = with_namespace!(req, NAMESPACE);
    let resp = images_client.get(req).await.map_err(|e| ContainerdError::Grpc(Box::new(e)))?;
    let img = resp.into_inner().image.ok_or_else(|| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: "image record missing".to_string(),
    })?;
    let top_digest = img.target.ok_or_else(|| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: "image has no manifest target".to_string(),
    })?;

    // Read the content at that digest (may be a manifest or an image index).
    let manifest_bytes = read_content(channel.clone(), &top_digest.digest).await?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(|e| ContainerdError::ImagePull {
            image: image.to_string(),
            reason: format!("invalid manifest JSON: {e}"),
        })?;

    // If this is a manifest index (multi-platform), pick the platform manifest.
    let manifest = if manifest.get("manifests").is_some() {
        let arch = current_oci_arch();
        let digest = manifest["manifests"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .find(|m| {
                let plat = m.get("platform");
                plat.and_then(|p| p.get("os")).and_then(|v| v.as_str()) == Some("linux")
                    && plat.and_then(|p| p.get("architecture")).and_then(|v| v.as_str())
                        == Some(arch)
            })
            .and_then(|m| m["digest"].as_str())
            .map(|d| d.to_string())
            .ok_or_else(|| ContainerdError::ImagePull {
                image: image.to_string(),
                reason: format!("no linux/{arch} manifest found in image index"),
            })?;
        let bytes = read_content(channel.clone(), &digest).await?;
        serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| {
            ContainerdError::ImagePull {
                image: image.to_string(),
                reason: format!("invalid platform manifest JSON: {e}"),
            }
        })?
    } else {
        manifest
    };

    // Read the image config to get diff IDs.
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| ContainerdError::ImagePull {
            image: image.to_string(),
            reason: "manifest missing config.digest".to_string(),
        })?
        .to_string();
    let config_bytes = read_content(channel.clone(), &config_digest).await?;
    let config: serde_json::Value =
        serde_json::from_slice(&config_bytes).map_err(|e| ContainerdError::ImagePull {
            image: image.to_string(),
            reason: format!("invalid image config JSON: {e}"),
        })?;

    let diff_ids: Vec<String> = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| ContainerdError::ImagePull {
            image: image.to_string(),
            reason: "image config missing rootfs.diff_ids".to_string(),
        })?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    if diff_ids.is_empty() {
        return Err(ContainerdError::ImagePull {
            image: image.to_string(),
            reason: "image has no layers".to_string(),
        });
    }

    // Compute chain ID: for a single layer it equals the diff ID;
    // for each additional layer: SHA256(prevChainID + " " + diffID).
    let mut chain_id = diff_ids[0].clone();
    for diff_id in &diff_ids[1..] {
        let input = format!("{chain_id} {diff_id}");
        let hash = Sha256::digest(input.as_bytes());
        chain_id = format!("sha256:{hash:x}");
    }

    // Extract default command: ENTRYPOINT + CMD from the image config.
    let img_config = &config["config"];
    let entrypoint: Vec<String> = img_config["Entrypoint"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cmd: Vec<String> = img_config["Cmd"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let default_command = [entrypoint, cmd].concat();

    let default_env: Vec<String> = img_config["Env"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    Ok(ImageInfo { chain_id, default_command, default_env })
}

/// Read all bytes from a containerd content store blob.
async fn read_content(channel: Channel, digest: &str) -> Result<Vec<u8>> {
    let mut client = ContentClient::new(channel);
    let req = ReadContentRequest { digest: digest.to_string(), offset: 0, size: 0 };
    let req = with_namespace!(req, NAMESPACE);
    let mut stream =
        client.read(req).await.map_err(|e| ContainerdError::Grpc(Box::new(e)))?.into_inner();
    let mut data = Vec::new();
    while let Some(resp) = stream.next().await {
        let resp = resp.map_err(|e| ContainerdError::Grpc(Box::new(e)))?;
        data.extend_from_slice(&resp.data);
    }
    Ok(data)
}

/// Map `std::env::consts::ARCH` to the OCI architecture string.
fn current_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}
