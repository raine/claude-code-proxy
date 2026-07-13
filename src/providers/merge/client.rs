//! HTTP client that forwards Anthropic Messages requests to a compatible upstream.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::StatusCode;

use crate::anthropic::schema::MessagesRequest;
use crate::traffic::TrafficCapture;

use super::auth::{load_merge_token, missing_auth_message};

const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct MergeClient {
    client: Arc<reqwest::Client>,
    messages_url: String,
}

pub struct MergeResponse {
    response: reqwest::Response,
}

pub struct MergeError {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub message: String,
}

impl MergeResponse {
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    pub fn into_stream(
        self,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, MergeError>> + Send {
        self.response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| MergeError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                message: "Anthropic-compatible upstream stream failed".into(),
            })
        })
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, MergeError> {
        let mut stream = self.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(MergeError {
                    status: StatusCode::BAD_GATEWAY,
                    retry_after: None,
                    message: "Anthropic-compatible upstream response exceeds the size limit".into(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl MergeClient {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        let client = Arc::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .build()?,
        );
        Ok(Self {
            client,
            messages_url: messages_url_for(&base_url),
        })
    }

    pub async fn post_messages(
        &self,
        body: &MessagesRequest,
        traffic: Option<Arc<TrafficCapture>>,
    ) -> Result<MergeResponse, MergeError> {
        let Some(token) = load_merge_token() else {
            if let Some(capture) = traffic.as_ref() {
                capture.write_json(
                    "031-upstream-error-body",
                    &serde_json::json!({"error":"missing_auth"}),
                );
            }
            return Err(MergeError {
                status: StatusCode::UNAUTHORIZED,
                retry_after: None,
                message: missing_auth_message(),
            });
        };

        if let Some(capture) = traffic.as_ref() {
            let body_value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
            capture.write_json("020-upstream-request", &body_value);
            capture.write_json(
                "021-upstream-request-metadata",
                &serde_json::json!({
                    "method": "POST",
                    "url": self.messages_url,
                    "provider": "merge",
                    "transport": "http",
                    "headers": {
                        "accept": "application/json",
                        "content-type": "application/json",
                        "authorization": "[redacted]",
                        "x-api-key": "[redacted]",
                        "anthropic-version": "2023-06-01",
                    },
                    "body_bytes": serde_json::to_vec(body).map(|v| v.len()).unwrap_or(0),
                }),
            );
        }

        let mut request = self
            .client
            .post(&self.messages_url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("authorization", format!("Bearer {token}"))
            .header("x-api-key", &token)
            .json(body);

        if body.stream {
            request = request.header("accept", "text/event-stream");
        } else {
            request = request.header("accept", "application/json");
        }

        let response = request.send().await.map_err(|_| MergeError {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            message: "Anthropic-compatible upstream request failed".into(),
        })?;

        let status = response.status();
        if let Some(capture) = traffic.as_ref() {
            capture.write_json(
                "030-upstream-response-headers",
                &serde_json::json!({
                    "status": status.as_u16(),
                }),
            );
        }

        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body_bytes = response.bytes().await.unwrap_or_default();
            let message = extract_error_message(&body_bytes)
                .unwrap_or_else(|| format!("upstream returned HTTP {status}"));
            if let Some(capture) = traffic.as_ref() {
                let detail = serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .unwrap_or_else(|_| {
                        serde_json::json!({"body_bytes": body_bytes.len()})
                    });
                capture.write_json(
                    "031-upstream-error-body",
                    &serde_json::json!({
                        "status": status.as_u16(),
                        "body": detail,
                    }),
                );
            }
            return Err(MergeError {
                status,
                retry_after,
                message,
            });
        }

        Ok(MergeResponse { response })
    }
}

fn messages_url_for(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1/messages") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        // Merge Gateway Anthropic path is already `.../v1/anthropic`; append Messages.
        format!("{trimmed}/v1/messages")
    }
}

fn extract_error_message(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_appends_v1_messages() {
        assert_eq!(
            messages_url_for("https://api-gateway.merge.dev/v1/anthropic"),
            "https://api-gateway.merge.dev/v1/anthropic/v1/messages"
        );
    }

    #[test]
    fn messages_url_respects_existing_messages_path() {
        assert_eq!(
            messages_url_for("http://127.0.0.1:9/v1/messages"),
            "http://127.0.0.1:9/v1/messages"
        );
    }

    #[test]
    fn messages_url_appends_to_v1() {
        assert_eq!(
            messages_url_for("https://example.com/v1"),
            "https://example.com/v1/messages"
        );
    }
}
