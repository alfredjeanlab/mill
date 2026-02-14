use std::collections::HashMap;

use base64::Engine as _;
use containerd_client::{
    services::v1::{TransferRequest, images_client::ImagesClient, transfer_client::TransferClient},
    with_namespace,
};
use prost_types::Any;
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
pub async fn pull(channel: Channel, image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
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

    tracing::info!(image = %image, "pulling image");
    client.transfer(req).await.map_err(|status| ContainerdError::ImagePull {
        image: image.to_string(),
        reason: status.message().to_string(),
    })?;
    tracing::info!(image = %image, "image pull complete");

    Ok(())
}
