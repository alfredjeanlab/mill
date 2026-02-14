use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::bytes::Bytes;

use crate::env;
use crate::error::{NodeError, Result};

pub(super) struct DaemonClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl DaemonClient {
    pub fn new(base_url: String, token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(env::api_client_timeout())
            .build()
            .unwrap_or_default();
        Self { http, base_url, token }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(map_err)?
            .error_for_status()
            .map_err(map_err)?
            .json()
            .await
            .map_err(map_err)
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Bytes> {
        self.http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(map_err)?
            .error_for_status()
            .map_err(map_err)?
            .bytes()
            .await
            .map_err(map_err)
    }

    pub async fn post_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(map_err)?;
        let status = resp.status();
        if status.is_server_error() || status.is_client_error() {
            let body = resp.text().await.unwrap_or_default();
            return Err(NodeError::NodeUnreachable(format!("HTTP {status} for {path}: {body}")));
        }
        Ok(())
    }

    /// POST and return the raw response for SSE streaming.
    pub async fn post_sse(&self, path: &str) -> Result<reqwest::Response> {
        self.http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(map_err)?
            .error_for_status()
            .map_err(map_err)
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.http
            .delete(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(map_err)?
            .error_for_status()
            .map_err(map_err)?;
        Ok(())
    }
}

fn map_err(e: reqwest::Error) -> NodeError {
    NodeError::NodeUnreachable(e.to_string())
}
