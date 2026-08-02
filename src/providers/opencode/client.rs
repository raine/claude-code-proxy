use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::StatusCode;
use serde::Serialize;

use super::model::EndpointKind;
use crate::traffic::TrafficCapture;

const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct OpenCodeClient {
    client: Arc<reqwest::Client>,
    base_url: reqwest::Url,
    api_key: Option<String>,
}

pub struct OpenCodeResponse {
    response: reqwest::Response,
}

#[derive(Debug)]
pub struct OpenCodeError {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub message: String,
}

impl OpenCodeResponse {
    pub fn into_stream(
        self,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, OpenCodeError>> + Send {
        self.response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| OpenCodeError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                message: "OpenCode Go upstream stream failed".to_string(),
            })
        })
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, OpenCodeError> {
        let mut stream = self.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(OpenCodeError {
                    status: StatusCode::BAD_GATEWAY,
                    retry_after: None,
                    message: "OpenCode Go upstream response exceeds the size limit".to_string(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl OpenCodeClient {
    pub fn new(base_url: String, api_key: Option<String>) -> anyhow::Result<Self> {
        let base_url = reqwest::Url::parse(base_url.trim_end_matches('/'))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client: Arc::new(client),
            base_url,
            api_key,
        })
    }

    pub async fn post<T: Serialize + ?Sized>(
        &self,
        endpoint: EndpointKind,
        body: &T,
        stream: bool,
        traffic: Option<Arc<TrafficCapture>>,
    ) -> Result<OpenCodeResponse, OpenCodeError> {
        let Some(api_key) = self.api_key.as_deref().filter(|key| !key.is_empty()) else {
            return Err(OpenCodeError {
                status: StatusCode::UNAUTHORIZED,
                retry_after: None,
                message: "OpenCode Go API key is not configured; set CCP_OPENCODE_API_KEY, OPENCODE_API_KEY, or opencode.apiKey in config.json".to_string(),
            });
        };
        let url = self.endpoint_url(endpoint);
        let accept = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };

        if let Some(capture) = traffic.as_ref() {
            let value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
            capture.write_json("020-upstream-request", &value);
            let auth_header = match endpoint {
                EndpointKind::ChatCompletions | EndpointKind::Responses => "authorization",
                EndpointKind::Messages => "x-api-key",
            };
            capture.write_json(
                "021-upstream-request-metadata",
                &serde_json::json!({
                    "method": "POST",
                    "url": url.as_str(),
                    "provider": "opencode",
                    "transport": "http",
                    "headers": {
                        "accept": accept,
                        auth_header: "[redacted]",
                        "content-type": "application/json"
                    }
                }),
            );
        }

        let mut request = self
            .client
            .post(url)
            .header(http::header::ACCEPT, accept)
            .header(http::header::CONTENT_TYPE, "application/json")
            .json(body);
        match endpoint {
            EndpointKind::ChatCompletions | EndpointKind::Responses => {
                request = request.header(http::header::AUTHORIZATION, format!("Bearer {api_key}"));
            }
            EndpointKind::Messages => {
                request = request
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01");
            }
        }
        let response = request.send().await.map_err(|_| OpenCodeError {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            message: "OpenCode Go upstream request failed".to_string(),
        })?;

        if let Some(capture) = traffic.as_ref() {
            capture.write_json(
                "030-upstream-response-headers",
                &serde_json::json!({
                    "status": response.status().as_u16(),
                    "headers": safe_headers(response.headers())
                }),
            );
        }

        if !response.status().is_success() {
            return Err(rejected_response(response).await);
        }
        Ok(OpenCodeResponse { response })
    }

    fn endpoint_url(&self, endpoint: EndpointKind) -> reqwest::Url {
        let mut url = self.base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        let suffix = match endpoint {
            EndpointKind::ChatCompletions => "chat/completions",
            EndpointKind::Messages => "messages",
            EndpointKind::Responses => "responses",
        };
        url.set_path(&format!("{base_path}/{suffix}"));
        url
    }
}

async fn rejected_response(response: reqwest::Response) -> OpenCodeError {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while body.len() < 64 * 1024 {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = 64 * 1024 - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("OpenCode Go upstream returned HTTP {status}"));
    OpenCodeError {
        status,
        retry_after,
        message,
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{OriginalUri, State},
        http::HeaderMap,
        routing::post,
    };
    use std::sync::Mutex;

    #[derive(Debug)]
    struct SeenRequest {
        path: String,
        authorization: String,
        x_api_key: String,
        anthropic_version: String,
        body: serde_json::Value,
    }

    type Seen = Arc<Mutex<Vec<SeenRequest>>>;

    async fn capture_request(
        State(seen): State<Seen>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        seen.lock().unwrap().push(SeenRequest {
            path: uri.path().to_string(),
            authorization: headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            x_api_key: headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            anthropic_version: headers
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            body,
        });
        Json(serde_json::json!({"ok": true}))
    }

    #[test]
    fn endpoint_urls_preserve_go_base_path() {
        let client = OpenCodeClient::new(
            "https://opencode.ai/zen/go/v1/".to_string(),
            Some("test".to_string()),
        )
        .unwrap();
        assert_eq!(
            client.endpoint_url(EndpointKind::ChatCompletions).as_str(),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
        assert_eq!(
            client.endpoint_url(EndpointKind::Messages).as_str(),
            "https://opencode.ai/zen/go/v1/messages"
        );
        assert_eq!(
            client.endpoint_url(EndpointKind::Responses).as_str(),
            "https://opencode.ai/zen/go/v1/responses"
        );
    }

    #[tokio::test]
    async fn endpoints_use_protocol_native_auth_and_wire_model_ids() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/chat/completions", post(capture_request))
            .route("/v1/messages", post(capture_request))
            .route("/v1/responses", post(capture_request))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();
        for (endpoint, model) in [
            (EndpointKind::ChatCompletions, "glm-5.2"),
            (EndpointKind::Messages, "minimax-m3"),
            (EndpointKind::Responses, "gpt-5.6-luna"),
        ] {
            client
                .post(
                    endpoint,
                    &serde_json::json!({"model": model, "messages": []}),
                    false,
                    None,
                )
                .await
                .unwrap()
                .into_bytes()
                .await
                .unwrap();
        }
        server.abort();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].path, "/v1/chat/completions");
        assert_eq!(seen[1].path, "/v1/messages");
        assert_eq!(seen[2].path, "/v1/responses");
        assert_eq!(seen[0].authorization, "Bearer test-key");
        assert!(seen[0].x_api_key.is_empty());
        assert!(seen[1].authorization.is_empty());
        assert_eq!(seen[1].x_api_key, "test-key");
        assert_eq!(seen[2].authorization, "Bearer test-key");
        assert!(seen[2].x_api_key.is_empty());
        assert!(seen[0].anthropic_version.is_empty());
        assert_eq!(seen[1].anthropic_version, "2023-06-01");
        assert!(seen[2].anthropic_version.is_empty());
        assert_eq!(seen[0].body["model"], "glm-5.2");
        assert_eq!(seen[1].body["model"], "minimax-m3");
        assert_eq!(seen[2].body["model"], "gpt-5.6-luna");
    }
}
