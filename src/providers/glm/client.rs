use std::time::Duration;

use crate::config;

#[derive(Debug)]
pub struct GlmError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

pub struct GlmResponse {
    pub body: Vec<u8>,
    pub status: u16,
}

/// Async HTTP client for the z.ai Anthropic-compatible endpoint.
///
/// z.ai speaks the Anthropic Messages API natively, so the request body is
/// forwarded verbatim (no translation) and the upstream Anthropic SSE/JSON
/// reply is piped straight back to Claude Code.
pub struct GlmHttpClient {
    client: reqwest::Client,
}

impl GlmHttpClient {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self { client })
    }

    pub async fn post_messages(&self, api_key: &str, body: &[u8]) -> Result<GlmResponse, GlmError> {
        let base = config::glm_base_url();
        let url = format!("{}/v1/messages", base.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("anthropic-version", "2023-06-01")
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| GlmError {
                status: 0,
                message: "Network error".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
            })?;

        let status = resp.status().as_u16();

        if status == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let text = resp.text().await.unwrap_or_default();
            return Err(GlmError {
                status: 429,
                message: "Rate limited".to_string(),
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after,
            });
        }

        if status == 401 || status == 403 {
            let text = resp.text().await.unwrap_or_default();
            return Err(GlmError {
                status,
                message: if status == 401 {
                    "Unauthorized"
                } else {
                    "Forbidden"
                }
                .to_string(),
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after: None,
            });
        }

        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(GlmError {
                status,
                message: "Upstream error".to_string(),
                detail: if text.is_empty() { None } else { Some(text) },
                retry_after: None,
            });
        }

        let body_bytes = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        Ok(GlmResponse {
            body: body_bytes,
            status,
        })
    }
}
