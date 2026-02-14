use std::net::SocketAddr;
use std::time::Duration;

use mill_config::{
    NodeResponse, SecretSetRequest, ServiceResponse, SpawnTaskRequest, SpawnTaskResponse,
    StatusResponse, TaskResponse, VolumeResponse,
};

pub struct Api {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl Api {
    pub fn new(addr: SocketAddr, token: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            base_url: format!("http://{addr}"),
            token: token.to_owned(),
        }
    }

    pub async fn status(&self) -> reqwest::Result<StatusResponse> {
        self.client
            .get(format!("{}/v1/status", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn nodes(&self) -> reqwest::Result<Vec<NodeResponse>> {
        self.client
            .get(format!("{}/v1/nodes", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn services(&self) -> reqwest::Result<Vec<ServiceResponse>> {
        self.client
            .get(format!("{}/v1/services", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn tasks(&self) -> reqwest::Result<Vec<TaskResponse>> {
        self.client
            .get(format!("{}/v1/tasks", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn volumes(&self) -> reqwest::Result<Vec<VolumeResponse>> {
        self.client
            .get(format!("{}/v1/volumes", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    /// Deploy a config, returning the raw response body (SSE stream as text).
    pub async fn deploy(&self, config_text: &str) -> reqwest::Result<String> {
        self.client
            .post(format!("{}/v1/deploy", self.base_url))
            .bearer_auth(&self.token)
            .body(config_text.to_owned())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
    }

    pub async fn spawn_task(&self, req: SpawnTaskRequest) -> reqwest::Result<SpawnTaskResponse> {
        self.client
            .post(format!("{}/v1/tasks/spawn", self.base_url))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn drain(&self, node_id: &str) -> reqwest::Result<String> {
        self.client
            .post(format!("{}/v1/nodes/{node_id}/drain", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
    }

    pub async fn delete_volume(&self, name: &str) -> reqwest::Result<reqwest::StatusCode> {
        let resp = self
            .client
            .delete(format!("{}/v1/volumes/{name}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.status())
    }

    /// Like `delete_volume` but returns the raw status code without `error_for_status()`.
    pub async fn delete_volume_raw(&self, name: &str) -> reqwest::Result<reqwest::StatusCode> {
        let resp = self
            .client
            .delete(format!("{}/v1/volumes/{name}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Ok(resp.status())
    }

    pub async fn secrets_set(
        &self,
        name: &str,
        value: &str,
    ) -> reqwest::Result<reqwest::StatusCode> {
        let resp = self
            .client
            .put(format!("{}/v1/secrets/{name}", self.base_url))
            .bearer_auth(&self.token)
            .json(&SecretSetRequest { value: value.to_owned() })
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.status())
    }

    pub async fn secrets_list(&self) -> reqwest::Result<Vec<mill_config::SecretListItem>> {
        self.client
            .get(format!("{}/v1/secrets", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn secret_get(&self, name: &str) -> reqwest::Result<mill_config::SecretResponse> {
        self.client
            .get(format!("{}/v1/secrets/{name}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn delete_secret(&self, name: &str) -> reqwest::Result<reqwest::StatusCode> {
        let resp = self
            .client
            .delete(format!("{}/v1/secrets/{name}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.status())
    }

    pub async fn join_info(&self) -> reqwest::Result<mill_node::daemon::JoinInfoResponse> {
        self.client
            .get(format!("{}/v1/join-info", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn post_join(
        &self,
        req: &mill_node::daemon::JoinRequest,
    ) -> reqwest::Result<reqwest::StatusCode> {
        let resp = self
            .client
            .post(format!("{}/v1/join", self.base_url))
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.status())
    }

    pub async fn kill_task(&self, id: &str) -> reqwest::Result<reqwest::StatusCode> {
        let resp = self
            .client
            .delete(format!("{}/v1/tasks/{id}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.status())
    }

    pub async fn cluster_key_bytes(&self) -> reqwest::Result<Vec<u8>> {
        self.client
            .get(format!("{}/v1/cluster-key", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await
            .map(|b| b.to_vec())
    }

    /// Make a request with a wrong token to test auth rejection.
    pub fn with_bad_token(&self) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            base_url: self.base_url.clone(),
            token: "wrong-token".to_owned(),
        }
    }
}
