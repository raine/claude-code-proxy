use std::time::Duration;

use axum::http::StatusCode;
use futures_util::StreamExt;

use crate::provider::{ProviderError, ProviderErrorKind, RequestContext};

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const API_VERSION: &str = "2023-06-01";

#[derive(Clone)]
pub struct AnthropicClient {
    client: reqwest::Client,
    base_url: reqwest::Url,
    api_key: Option<String>,
    headers: http::HeaderMap,
}

impl AnthropicClient {
    pub fn new(base_url: String, api_key: Option<String>) -> anyhow::Result<Self> {
        let base_url = reqwest::Url::parse(base_url.trim_end_matches('/'))?;
        anyhow::ensure!(
            matches!(base_url.scheme(), "http" | "https")
                && base_url.host_str().is_some()
                && base_url.username().is_empty()
                && base_url.password().is_none()
                && base_url.query().is_none()
                && base_url.fragment().is_none(),
            "Anthropic base URL must be an HTTP(S) URL without credentials, query, or fragment"
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            client,
            base_url,
            api_key,
            headers: http::HeaderMap::new(),
        })
    }

    pub fn with_request_headers(&self, headers: &http::HeaderMap) -> Self {
        let mut client = self.clone();
        // Never forward downstream credentials, cookies, host, or unrelated beta headers.
        for name in ["anthropic-version", "anthropic-beta"] {
            client.headers.remove(name);
            for value in headers.get_all(name).iter() {
                client.headers.append(http::HeaderName::from_static(name), value.clone());
            }
        }
        client
    }

    fn endpoint_url(&self, count_tokens: bool) -> reqwest::Url {
        let mut url = self.base_url.clone();
        let path = url.path().trim_end_matches('/');
        let path = path.strip_suffix("/v1").unwrap_or(path);
        let suffix = if count_tokens { "/count_tokens" } else { "" };
        url.set_path(&format!("{path}/v1/messages{suffix}"));
        url
    }

    pub async fn post(
        &self,
        body: &serde_json::Value,
        count_tokens: bool,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, ProviderError> {
        let key = self.api_key.as_deref().filter(|key| !key.trim().is_empty()).ok_or_else(|| {
            ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                "Anthropic API key is not configured; set CCP_ANTHROPIC_API_KEY, ANTHROPIC_API_KEY, or anthropic.apiKey in config.json",
            )
        })?;
        let stream = body.get("stream").and_then(serde_json::Value::as_bool) == Some(true);
        let accept = if stream { "text/event-stream" } else { "application/json" };
        let url = self.endpoint_url(count_tokens);
        if let Some(traffic) = &ctx.traffic {
            traffic.write_json("020-upstream-request", body);
            traffic.write_json("021-upstream-request-metadata", &serde_json::json!({
                "method": "POST", "url": url.as_str(), "provider": "anthropic",
                "headers": {"x-api-key": "[redacted]", "anthropic-version": self.headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or(API_VERSION),
                    "anthropic-beta": self.headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
                    "content-type": "application/json", "accept": accept}
            }));
        }
        if let Some(monitor) = &ctx.monitor {
            monitor.upstream_started(&ctx.req_id);
        }
        let response = self.client.post(url)
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .headers(self.headers.clone())
            .header("accept", accept)
            .json(body)
            .send().await.map_err(|_| upstream_error("Anthropic upstream request failed"))?;
        if let Some(traffic) = &ctx.traffic {
            traffic.write_json("030-upstream-response-headers", &serde_json::json!({
                "status": response.status().as_u16(),
                "request_id": response.headers().get("request-id").and_then(|v| v.to_str().ok())
            }));
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let retry_after = response.headers().get("retry-after")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        // Bound rejected bodies as well, and never include transport errors/URLs in errors.
        let bytes = read_bytes(response, 64 * 1024).await.unwrap_or_default();
        let message = serde_json::from_slice::<serde_json::Value>(&bytes).ok()
            .and_then(|v| v.pointer("/error/message").and_then(|v| v.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("Anthropic upstream returned HTTP {status}"));
        let kind = match status {
            StatusCode::UNAUTHORIZED => ProviderErrorKind::Authentication,
            StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN => ProviderErrorKind::Permission,
            StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimit,
            status if status.is_client_error() => ProviderErrorKind::InvalidRequest,
            _ => ProviderErrorKind::Api,
        };
        let mut error = ProviderError::new(status, kind, message.replace(key, "[redacted]"));
        error.retry_after = retry_after;
        Err(error)
    }
}

pub fn upstream_error(message: &str) -> ProviderError {
    ProviderError::new(StatusCode::BAD_GATEWAY, ProviderErrorKind::Api, message)
}

pub async fn read_json(response: reqwest::Response) -> Result<serde_json::Value, ProviderError> {
    let bytes = read_bytes(response, MAX_RESPONSE_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|_| upstream_error("Invalid Anthropic upstream JSON"))
}

async fn read_bytes(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| upstream_error("Anthropic upstream body failed"))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(upstream_error("Anthropic upstream response exceeds the size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_accept_root_or_v1_base() {
        for base in ["https://api.anthropic.com", "https://api.anthropic.com/v1/"] {
            let client = AnthropicClient::new(base.into(), None).unwrap();
            assert_eq!(client.endpoint_url(false).as_str(), "https://api.anthropic.com/v1/messages");
            assert_eq!(client.endpoint_url(true).as_str(), "https://api.anthropic.com/v1/messages/count_tokens");
        }
    }

    #[test]
    fn rejects_credentials_and_non_http_urls() {
        for base in ["file:///tmp/api", "https://secret@example.com", "https://example.com?key=secret"] {
            assert!(AnthropicClient::new(base.into(), None).is_err());
        }
    }
}
