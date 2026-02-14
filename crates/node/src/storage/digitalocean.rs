use bytesize::ByteSize;
use serde::{Deserialize, Serialize};

use super::{BoxFuture, CloudVolumeInfo, StorageError, Volumes};

/// DigitalOcean Block Storage Volumes driver.
///
/// Uses the DO API v2 to create, attach, detach, and destroy volumes.
pub struct DigitalOceanVolumes {
    client: reqwest::Client,
    token: String,
    region: String,
}

impl DigitalOceanVolumes {
    pub fn new(token: String, region: String) -> Self {
        Self { client: reqwest::Client::new(), token, region }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// List all volumes in this region. Used internally by attach/detach to
    /// resolve volume names to IDs.
    async fn list_volumes(&self) -> Result<Vec<CloudVolumeInfo>, StorageError> {
        let url = format!("https://api.digitalocean.com/v2/volumes?region={}", self.region);
        let resp = self.client.get(&url).header("Authorization", self.auth_header()).send().await?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(StorageError::Api(text));
        }

        let body: ListVolumesResponse = resp.json().await?;
        Ok(body
            .volumes
            .into_iter()
            .map(|v| CloudVolumeInfo {
                id: v.id,
                name: v.name,
                size_bytes: v.size_gigabytes * ByteSize::gib(1).as_u64(),
            })
            .collect())
    }

    async fn volume_action(
        &self,
        vol_id: &str,
        body: &VolumeActionRequest,
    ) -> Result<(), StorageError> {
        let url = format!("https://api.digitalocean.com/v2/volumes/{vol_id}/actions");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(StorageError::Api(text))
        }
    }

    async fn create_impl(&self, name: String, size: ByteSize) -> Result<String, StorageError> {
        let size_gib = (size.as_u64() / ByteSize::gib(1).as_u64()).max(1);
        let body = CreateVolumeRequest {
            size_gigabytes: size_gib,
            name,
            region: self.region.clone(),
            filesystem_type: "ext4".to_owned(),
        };

        let resp = self
            .client
            .post("https://api.digitalocean.com/v2/volumes")
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(StorageError::Api(text));
        }

        let created: CreateVolumeResponse = resp.json().await?;
        Ok(created.volume.id)
    }

    async fn destroy_impl(&self, name: String) -> Result<(), StorageError> {
        let url =
            format!("https://api.digitalocean.com/v2/volumes?name={name}&region={}", self.region);
        let resp =
            self.client.delete(&url).header("Authorization", self.auth_header()).send().await?;

        if resp.status().is_success() {
            Ok(())
        } else if resp.status().as_u16() == 404 {
            Err(StorageError::NotFound(name))
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(StorageError::Api(text))
        }
    }

    async fn attach_impl(&self, name: String, instance_id: String) -> Result<(), StorageError> {
        let volumes = self.list_volumes().await?;
        let vol =
            volumes.iter().find(|v| v.name == name).ok_or_else(|| StorageError::NotFound(name))?;

        let droplet_id: u64 =
            instance_id.parse().map_err(|_| StorageError::Api("invalid droplet id".into()))?;

        self.volume_action(
            &vol.id,
            &VolumeActionRequest {
                r#type: "attach".to_owned(),
                droplet_id: Some(droplet_id),
                region: Some(self.region.clone()),
            },
        )
        .await
    }

    async fn detach_impl(&self, name: String) -> Result<(), StorageError> {
        let volumes = self.list_volumes().await?;
        let vol =
            volumes.iter().find(|v| v.name == name).ok_or_else(|| StorageError::NotFound(name))?;

        self.volume_action(
            &vol.id,
            &VolumeActionRequest {
                r#type: "detach".to_owned(),
                droplet_id: None,
                region: Some(self.region.clone()),
            },
        )
        .await
    }
}

impl Volumes for DigitalOceanVolumes {
    fn create(&self, name: &str, size: ByteSize) -> BoxFuture<'_, Result<String, StorageError>> {
        let name = name.to_string();
        Box::pin(self.create_impl(name, size))
    }

    fn destroy(&self, name: &str) -> BoxFuture<'_, Result<(), StorageError>> {
        let name = name.to_string();
        Box::pin(self.destroy_impl(name))
    }

    fn attach(&self, name: &str, instance_id: &str) -> BoxFuture<'_, Result<(), StorageError>> {
        let name = name.to_string();
        let instance_id = instance_id.to_string();
        Box::pin(self.attach_impl(name, instance_id))
    }

    fn detach(&self, name: &str) -> BoxFuture<'_, Result<(), StorageError>> {
        let name = name.to_string();
        Box::pin(self.detach_impl(name))
    }
}

#[derive(Serialize)]
struct CreateVolumeRequest {
    size_gigabytes: u64,
    name: String,
    region: String,
    filesystem_type: String,
}

#[derive(Deserialize)]
struct CreateVolumeResponse {
    volume: DoVolume,
}

#[derive(Deserialize)]
struct ListVolumesResponse {
    volumes: Vec<DoVolume>,
}

#[derive(Deserialize)]
struct DoVolume {
    id: String,
    name: String,
    size_gigabytes: u64,
}

#[derive(Serialize)]
struct VolumeActionRequest {
    r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    droplet_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
}
