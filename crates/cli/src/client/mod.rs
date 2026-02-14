pub mod sse;

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::CliError;
use mill_config::ErrorResponse;

pub struct MillClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl MillClient {
    pub fn new(base_url: &str, token: Option<&str>) -> Self {
        // Custom redirect policy: follow 307 redirects preserving method/body.
        let policy = Policy::limited(5);

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(policy)
            .build()
            .unwrap_or_default();

        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.map(String::from),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(ref token) = self.token
            && let Ok(val) = HeaderValue::from_str(&format!("Bearer {token}"))
        {
            headers.insert(AUTHORIZATION, val);
        }
        headers
    }

    /// GET request returning deserialized JSON.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CliError> {
        let resp = self
            .http
            .get(self.url(path))
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::handle_json_response(resp).await
    }

    /// POST with JSON body, returning deserialized JSON.
    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, CliError> {
        let resp = self
            .http
            .post(self.url(path))
            .headers(self.auth_headers())
            .json(body)
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::handle_json_response(resp).await
    }

    /// POST with text/plain body, returning SSE response.
    pub async fn post_text_sse(
        &self,
        path: &str,
        body: String,
    ) -> Result<reqwest::Response, CliError> {
        let resp = self
            .http
            .post(self.url(path))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "text/plain")
            .body(body)
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::check_status_sse(resp).await
    }

    /// POST with empty body, returning SSE response.
    pub async fn post_empty_sse(&self, path: &str) -> Result<reqwest::Response, CliError> {
        let resp = self
            .http
            .post(self.url(path))
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::check_status_sse(resp).await
    }

    /// PUT with JSON body, no meaningful response body.
    pub async fn put_json<B: Serialize>(&self, path: &str, body: &B) -> Result<(), CliError> {
        let resp = self
            .http
            .put(self.url(path))
            .headers(self.auth_headers())
            .json(body)
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::handle_empty_response(resp).await
    }

    /// DELETE request, no meaningful response body.
    pub async fn delete(&self, path: &str) -> Result<(), CliError> {
        let resp = self
            .http
            .delete(self.url(path))
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::handle_empty_response(resp).await
    }

    /// GET request returning SSE response.
    pub async fn get_sse(&self, path: &str) -> Result<reqwest::Response, CliError> {
        let resp = self
            .http
            .get(self.url(path))
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|e| CliError::from_reqwest(e, &self.base_url))?;

        Self::check_status_sse(resp).await
    }

    async fn handle_json_response<T: DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, CliError> {
        let status = resp.status();
        if status.is_success() {
            let body = resp.text().await.map_err(CliError::Request)?;
            serde_json::from_str(&body).map_err(|e| CliError::Http {
                status: status.as_u16(),
                body: format!("invalid JSON: {e}"),
            })
        } else {
            Self::handle_error(status.as_u16(), resp).await
        }
    }

    async fn handle_empty_response(resp: reqwest::Response) -> Result<(), CliError> {
        let status = resp.status();
        if status.is_success() { Ok(()) } else { Self::handle_error(status.as_u16(), resp).await }
    }

    async fn check_status_sse(resp: reqwest::Response) -> Result<reqwest::Response, CliError> {
        let status = resp.status();
        if status.is_success() { Ok(resp) } else { Self::handle_error(status.as_u16(), resp).await }
    }

    async fn handle_error<T>(status: u16, resp: reqwest::Response) -> Result<T, CliError> {
        let body = resp.text().await.unwrap_or_default();
        if let Ok(err_resp) = serde_json::from_str::<ErrorResponse>(&body) {
            Err(CliError::Api(err_resp))
        } else {
            Err(CliError::Http { status, body })
        }
    }
}
