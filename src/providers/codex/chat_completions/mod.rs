pub mod request;
pub mod response;
pub mod stream;

use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use http::StatusCode;
use serde_json::{Value, json};

use crate::provider::RequestContext;

use super::client::{CodexError, CodexHttpClient};
use request::TranslatedRequest;

pub struct ChatCompletionsBackend {
    client: Arc<CodexHttpClient>,
}

impl Default for ChatCompletionsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatCompletionsBackend {
    pub fn new() -> Self {
        Self {
            client: Arc::new(CodexHttpClient::new()),
        }
    }

    #[cfg(test)]
    fn with_client(client: CodexHttpClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    pub async fn handle(&self, request: TranslatedRequest, ctx: RequestContext) -> Response {
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.execution_resolved(
                &ctx.req_id,
                &request.model,
                super::wire_service_tier_mode(request.upstream.get("service_tier")),
            );
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match self
            .client
            .post_native_responses(&request.upstream, &ctx, request.use_responses_lite, true)
            .await
        {
            Ok(upstream) => upstream,
            Err(error) => return codex_error_response(error),
        };

        if !upstream.status().is_success() {
            return upstream_error_response(upstream, self.client.body_idle_timeout_ms()).await;
        }
        if request.stream {
            return stream::streaming_response(
                upstream,
                ctx,
                request.model,
                request.include_usage,
                self.client.body_idle_timeout_ms(),
            );
        }

        let headers = stream::response_headers(upstream.headers());
        let bytes =
            match collect_body(upstream, self.client.body_idle_timeout_ms(), Some(&ctx)).await {
                Ok(bytes) => bytes,
                Err(error) => return error.response(),
            };
        if let Some(traffic) = ctx.traffic.as_deref() {
            traffic.write_bytes("032-upstream-response-body.sse", &bytes);
        }
        let completion = match response::aggregate_sse(&bytes, &request.model) {
            Ok(completion) => completion,
            Err(error) => return error.response(),
        };
        if let Some(usage) = completion.get("usage")
            && let Some(monitor) = ctx.monitor.as_ref()
        {
            monitor.usage_updated(
                &ctx.req_id,
                usage.get("prompt_tokens").and_then(Value::as_u64),
                usage.get("completion_tokens").and_then(Value::as_u64),
            );
        }
        if let Some(traffic) = ctx.traffic.as_deref() {
            traffic.write_json("050-openai-chat-completion-response", &completion);
        }
        let mut downstream = Json(completion).into_response();
        *downstream.headers_mut() = headers;
        downstream.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        downstream
    }
}

async fn collect_body(
    upstream: reqwest::Response,
    idle_timeout_ms: u64,
    ctx: Option<&RequestContext>,
) -> Result<Vec<u8>, ChatError> {
    let mut stream = upstream.bytes_stream();
    let mut bytes = Vec::new();
    let mut started = false;
    loop {
        match tokio::time::timeout(Duration::from_millis(idle_timeout_ms), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                if !started {
                    if let Some(ctx) = ctx
                        && let Some(monitor) = ctx.monitor.as_ref()
                    {
                        monitor.generation_started(&ctx.req_id);
                    }
                    started = true;
                }
                bytes.extend_from_slice(&chunk);
                if let Some(ctx) = ctx
                    && let Some(monitor) = ctx.monitor.as_ref()
                {
                    monitor.stream_progress(&ctx.req_id, chunk.len() as u64, 0, None, None);
                }
            }
            Ok(Some(Err(error))) => {
                return Err(ChatError::upstream(format!(
                    "Codex response body read failed: {error}"
                )));
            }
            Ok(None) => return Ok(bytes),
            Err(_) => {
                return Err(ChatError::timeout(format!(
                    "Timed out waiting {idle_timeout_ms}ms for the next Codex response body chunk"
                )));
            }
        }
    }
}

fn codex_error_response(error: CodexError) -> Response {
    let retry_after = error.retry_after.clone();
    let response = ChatError::from_codex(error).response();
    if let Some(retry_after) = retry_after
        && let Ok(value) = http::HeaderValue::from_str(&retry_after)
    {
        let (mut parts, body) = response.into_parts();
        parts.headers.insert(http::header::RETRY_AFTER, value);
        Response::from_parts(parts, body)
    } else {
        response
    }
}

async fn upstream_error_response(upstream: reqwest::Response, idle_timeout_ms: u64) -> Response {
    let status = upstream.status();
    let retry_after = upstream.headers().get(http::header::RETRY_AFTER).cloned();
    let bytes = match collect_body(upstream, idle_timeout_ms, None).await {
        Ok(bytes) => bytes,
        Err(error) => return error.response(),
    };
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| format!("Codex request failed with status {}", status.as_u16()));
    let kind = match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        _ => "api_error",
    };
    let response = ChatError::new(status, kind, message, None, None).response();
    if let Some(retry_after) = retry_after {
        let (mut parts, body) = response.into_parts();
        parts.headers.insert(http::header::RETRY_AFTER, retry_after);
        Response::from_parts(parts, body)
    } else {
        response
    }
}

#[derive(Debug, Clone)]
pub struct ChatError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

impl ChatError {
    pub fn new(
        status: StatusCode,
        kind: &'static str,
        message: impl Into<String>,
        param: Option<&str>,
        code: Option<&str>,
    ) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            param: param.map(str::to_string),
            code: code.map(str::to_string),
        }
    }

    pub fn invalid(message: impl Into<String>, param: Option<&str>, code: Option<&str>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message,
            param,
            code,
        )
    }

    pub fn unsupported(param: impl Into<String>) -> Self {
        let param = param.into();
        Self::invalid(
            format!("Unsupported parameter: {param}"),
            Some(&param),
            Some("unsupported_parameter"),
        )
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "api_error", message, None, None)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            message,
            None,
            None,
        )
    }

    fn from_codex(error: CodexError) -> Self {
        let status = match error.status {
            401 => StatusCode::UNAUTHORIZED,
            403 => StatusCode::FORBIDDEN,
            429 => StatusCode::TOO_MANY_REQUESTS,
            _ if error.message.contains("Timed out waiting") => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        };
        let kind = match status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            _ => "api_error",
        };
        Self::new(
            status,
            kind,
            error.detail.unwrap_or(error.message),
            None,
            None,
        )
    }

    pub fn value(&self) -> Value {
        json!({"error":{"message":self.message,"type":self.kind,"param":self.param,"code":self.code}})
    }

    pub fn response(self) -> Response {
        (self.status, Json(self.value())).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        monitor::{EndpointKind, MonitorHandle},
        providers::codex::auth::token_store::StoredAuth,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn context(monitor: MonitorHandle) -> RequestContext {
        monitor.request_started(
            "chat-test",
            Some("session".into()),
            None,
            EndpointKind::ChatCompletions,
        );
        RequestContext {
            req_id: "chat-test".into(),
            session_id: Some("session".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: Some(monitor),
        }
    }

    async fn mock_backend(
        sse_body: &'static [u8],
    ) -> (ChatCompletionsBackend, tokio::task::JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")?
                            .trim()
                            .parse::<usize>()
                            .ok()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + length {
                    break;
                }
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nx-request-id: upstream-1\r\nconnection: close\r\n\r\n",
                sse_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(sse_body).await.unwrap();
            body
        });
        let client = CodexHttpClient::new_for_test(
            reqwest::Client::new(),
            format!("http://{address}/v1/responses"),
            1_000,
            1_000,
            0,
        );
        client.auth_manager().set_test_auth(StoredAuth {
            access: "test-token".into(),
            refresh: String::new(),
            account_id: Some("account".into()),
            expires: u64::MAX,
        });
        (ChatCompletionsBackend::with_client(client), server)
    }

    #[tokio::test]
    async fn buffered_request_translates_upstream_and_downstream() {
        const SSE: &[u8] = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"answer\\\":\\\"yes\\\"}\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_buffered\",\"model\":\"gpt-5.6-sol\",\"status\":\"completed\",\"usage\":{\"input_tokens\":8,\"output_tokens\":4}}}\n\n";
        let (backend, server) = mock_backend(SSE).await;
        let request = request::translate_request(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"system","content":"JSON only"},{"role":"user","content":"answer"}],
            "reasoning_effort":"low",
            "response_format":{"type":"json_schema","json_schema":{"name":"answer","strict":true,"schema":{"type":"object"}}}
        })).unwrap();
        let monitor = MonitorHandle::new(10);
        let response = backend.handle(request, context(monitor.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-request-id"], "upstream-1");
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(
            value["choices"][0]["message"]["content"],
            r#"{"answer":"yes"}"#
        );
        assert_eq!(value["usage"]["total_tokens"], 12);

        let upstream = server.await.unwrap();
        assert_eq!(upstream["store"], false);
        assert_eq!(upstream["stream"], true);
        assert_eq!(upstream["input"][0]["role"], "developer");
        assert_eq!(upstream["reasoning"]["effort"], "low");
        assert_eq!(upstream["reasoning"]["context"], "all_turns");
        assert_eq!(upstream["text"]["format"]["name"], "answer");
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.active[0].input_tokens, Some(8));
        assert_eq!(snapshot.active[0].output_tokens, Some(4));
    }

    #[tokio::test]
    async fn streaming_request_emits_chat_chunks_usage_and_done() {
        const SSE: &[u8] = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream\",\"model\":\"gpt-5.6-sol\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream\",\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n";
        let (backend, server) = mock_backend(SSE).await;
        let request = request::translate_request(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user","content":"hello"}],
            "stream":true,
            "stream_options":{"include_usage":true}
        }))
        .unwrap();
        let response = backend
            .handle(request, context(MonitorHandle::new(10)))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let body = String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains(r#""delta":{"role":"assistant"}"#));
        assert!(body.contains(r#""delta":{"content":"hello"}"#));
        assert!(body.contains(r#""finish_reason":"stop""#));
        assert!(body.contains(r#""prompt_tokens":3"#));
        assert!(body.ends_with("data: [DONE]\n\n"));
        server.await.unwrap();
    }

    #[test]
    fn codex_errors_map_status_and_preserve_retry_metadata() {
        let response = codex_error_response(CodexError {
            status: 429,
            message: "Rate limited".into(),
            detail: Some("Try later".into()),
            retry_after: Some("7".into()),
            origin: super::super::client::CodexErrorOrigin::Http,
        });
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[http::header::RETRY_AFTER], "7");

        let auth = ChatError::from_codex(CodexError {
            status: 401,
            message: "Auth error".into(),
            detail: None,
            retry_after: None,
            origin: super::super::client::CodexErrorOrigin::Auth,
        });
        assert_eq!(auth.status, StatusCode::UNAUTHORIZED);
        assert_eq!(auth.kind, "authentication_error");
    }
}
