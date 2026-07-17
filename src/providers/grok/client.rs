use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use http::StatusCode;

use super::auth::manager::GrokAuthManager;
use super::auth::token_store::{StoredAuth, file_store};
use super::translate::request::GrokResponsesRequest;
use crate::traffic::TrafficCapture;

const DEFAULT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Per-request routing / affinity headers for Grok (matches official CLI).
#[derive(Debug, Clone, Default)]
pub struct GrokRequestMeta {
    pub req_id: Option<String>,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
}

pub struct GrokClient {
    client: Arc<reqwest::Client>,
    auth: Arc<GrokAuthManager<crate::auth::FileAuthStore<StoredAuth>>>,
    url: String,
    client_version: String,
}

pub struct GrokResponse {
    response: reqwest::Response,
}
pub struct GrokError {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub message: String,
}

impl GrokResponse {
    pub fn into_response(self) -> reqwest::Response {
        self.response
    }

    pub fn into_stream(
        self,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, GrokError>> + Send {
        self.response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| GrokError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                message: "Grok upstream stream failed".into(),
            })
        })
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, GrokError> {
        let mut stream = self.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(GrokError {
                    status: StatusCode::BAD_GATEWAY,
                    retry_after: None,
                    message: "Grok upstream response exceeds the size limit".into(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl GrokClient {
    pub fn new(base_url: String, client_version: String) -> anyhow::Result<Self> {
        // No whole-request `.timeout(...)`: long multi-turn / tool streams routinely
        // exceed 120s. Liveness is left to TCP/H2 keepalive + upstream idle behaviour
        // (mirrors grok-build's sampling client).
        let client = Arc::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_nodelay(true)
                .http2_keep_alive_interval(Duration::from_secs(15))
                .http2_keep_alive_timeout(Duration::from_secs(5))
                .http2_keep_alive_while_idle(true)
                .build()?,
        );
        let auth = Arc::new(GrokAuthManager::new(file_store())?);
        Ok(Self::with_shared(
            url_for(base_url)?,
            client_version,
            client,
            auth,
        ))
    }

    fn with_shared(
        url: String,
        client_version: String,
        client: Arc<reqwest::Client>,
        auth: Arc<GrokAuthManager<crate::auth::FileAuthStore<StoredAuth>>>,
    ) -> Self {
        Self {
            client,
            auth,
            url,
            client_version,
        }
    }

    pub async fn post(
        &self,
        body: &GrokResponsesRequest,
        meta: &GrokRequestMeta,
        traffic: Option<Arc<TrafficCapture>>,
    ) -> Result<GrokResponse, GrokError> {
        if let Some(capture) = traffic.as_ref() {
            let body_value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
            capture.write_json("020-upstream-request", &body_value);
            capture.write_json("021-upstream-request-metadata", &serde_json::json!({
                "method": "POST", "url": safe_url(&self.url), "provider": "grok", "transport": "http",
                "headers": {
                    "accept":"text/event-stream",
                    "content-type":"application/json",
                    "authorization":"[redacted]",
                    "x-xai-token-auth":"[redacted]",
                    "x-grok-session-id": meta.session_id.as_deref().unwrap_or(""),
                    "x-grok-req-id": meta.req_id.as_deref().unwrap_or(""),
                },
                "body_bytes": serde_json::to_vec(body).map(|v| v.len()).unwrap_or(0),
            }));
        }
        let auth = match self.auth.get_auth().await {
            Ok(auth) => auth,
            Err(error) => {
                capture_failure(traffic.as_deref(), "auth", "authentication", 0);
                return Err(auth_error(error));
            }
        };
        let response = self
            .attempt(&auth.access, body, meta, 1, traffic.as_deref())
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let refreshed = self
                .auth
                .force_refresh(&auth.access)
                .await
                .map_err(|error| {
                    capture_failure(traffic.as_deref(), "auth", "refresh", 1);
                    auth_error(error)
                })?;
            let replay = self
                .attempt(&refreshed.access, body, meta, 2, traffic.as_deref())
                .await?;
            if replay.status() == StatusCode::UNAUTHORIZED {
                capture_failure(traffic.as_deref(), "auth", "unauthorized", 2);
                return Err(auth_error(anyhow::anyhow!("unauthorized")));
            }
            return Ok(self.captured_response(replay, traffic.as_deref()));
        }
        Ok(self.captured_response(response, traffic.as_deref()))
    }

    fn captured_response(
        &self,
        response: reqwest::Response,
        traffic: Option<&TrafficCapture>,
    ) -> GrokResponse {
        if let Some(capture) = traffic.as_ref() {
            capture.write_json("030-upstream-response-headers", &serde_json::json!({
                "status": response.status().as_u16(), "headers": safe_headers(response.headers()),
            }));
        }
        GrokResponse { response }
    }

    async fn attempt(
        &self,
        access: &str,
        body: &GrokResponsesRequest,
        meta: &GrokRequestMeta,
        attempt: u8,
        traffic: Option<&TrafficCapture>,
    ) -> Result<reqwest::Response, GrokError> {
        let started = Instant::now();
        let mut builder = self
            .client
            .post(&self.url)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {access}"))
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-grok-client-identifier", "claude-code-proxy")
            .header("x-grok-client-version", &self.client_version)
            .header("x-grok-agent-id", "claude-code-proxy");

        if let Some(req_id) = meta.req_id.as_deref().filter(|v| !v.is_empty()) {
            builder = builder.header("x-grok-req-id", req_id);
        }
        if let Some(session_id) = meta.session_id.as_deref().filter(|v| !v.is_empty()) {
            builder = builder
                .header("x-grok-session-id", session_id)
                .header("x-grok-conv-id", session_id);
        }
        if let Some(seq) = meta.session_seq {
            builder = builder.header("x-grok-turn-idx", seq.to_string());
        }

        let response = builder.json(body).send().await.map_err(|_| {
            capture_failure(traffic, "transport", "transport", attempt);
            GrokError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                message: "Grok upstream request failed".into(),
            }
        })?;
        let status = response.status();
        if let Some(capture) = traffic {
            capture.write_json("022-upstream-attempt", &serde_json::json!({"attempt":attempt,"status":status.as_u16(),"elapsed_ms":started.elapsed().as_millis(),"headers":safe_headers(response.headers())}));
        }
        if !status.is_success() && status != StatusCode::UNAUTHORIZED {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            if let Some(capture) = traffic {
                let (body, truncated) = read_rejected_body(response, 64 * 1024).await;
                let detail = serde_json::from_slice::<serde_json::Value>(&body)
                    .unwrap_or_else(|_| serde_json::json!({"body_bytes": body.len()}));
                capture.write_json(
                    "031-upstream-error-body",
                    &serde_json::json!({"attempt":attempt,"status":status.as_u16(),"truncated":truncated,"body":detail}),
                );
            }
            return Err(GrokError {
                status,
                retry_after,
                message: "Grok upstream rejected the request".into(),
            });
        }
        Ok(response)
    }
}

async fn read_rejected_body(response: reqwest::Response, limit: usize) -> (Vec<u8>, bool) {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return (body, true);
        }
        body.extend_from_slice(&chunk);
    }
    (body, false)
}

pub(super) fn capture_failure(
    traffic: Option<&TrafficCapture>,
    stage: &str,
    kind: &str,
    attempt: u8,
) {
    if let Some(capture) = traffic {
        capture.write_json(
            "060-grok-stream-error",
            &serde_json::json!({"stage":stage,"kind":kind,"attempt":attempt}),
        );
    }
}

fn safe_headers(headers: &reqwest::header::HeaderMap) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    for name in [
        "content-type",
        "content-length",
        "retry-after",
        "x-request-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            result.insert(
                name.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    serde_json::Value::Object(result)
}

fn safe_url(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "[invalid-url]".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.to_string()
}

fn url_for(base_url: String) -> anyhow::Result<String> {
    responses_url(&base_url)
}
fn responses_url(base_url: &str) -> anyhow::Result<String> {
    let base_url = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url.trim()
    };
    let mut url = reqwest::Url::parse(base_url)?;
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        url.set_path(&format!("{path}/responses"));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn auth_error(_: anyhow::Error) -> GrokError {
    GrokError {
        status: StatusCode::UNAUTHORIZED,
        retry_after: None,
        message: "Grok authentication requires official CLI login and proxy import".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::responses_url;
    #[test]
    fn responses_url_appends_responses_to_base_path() {
        assert_eq!(
            responses_url("http://127.0.0.1:8080/v1").unwrap(),
            "http://127.0.0.1:8080/v1/responses"
        );
    }
    #[test]
    fn responses_url_preserves_responses_endpoint() {
        assert_eq!(
            responses_url("https://example.com/custom/responses/").unwrap(),
            "https://example.com/custom/responses"
        );
    }
    #[test]
    fn responses_url_rejects_invalid_url() {
        assert!(responses_url(":invalid").is_err());
    }
}
