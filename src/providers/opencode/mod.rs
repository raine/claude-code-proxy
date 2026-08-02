pub mod chat;
pub mod client;
pub mod messages;
pub mod model;
pub mod responses;

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::{
    codex::translate::accumulate::accumulate_response as accumulate_responses_response,
    kimi::count_tokens,
};

use self::client::{OpenCodeClient, OpenCodeError};
use self::model::EndpointKind;

enum ClientState {
    Ready(Arc<OpenCodeClient>),
    Invalid(String),
}

pub struct OpenCodeProvider {
    client: ClientState,
}

impl OpenCodeProvider {
    pub fn new() -> Self {
        let client = OpenCodeClient::new(
            crate::config::opencode_base_url(),
            crate::config::opencode_api_key(),
        )
        .map(Arc::new)
        .map(ClientState::Ready)
        .unwrap_or_else(|error| ClientState::Invalid(error.to_string()));
        Self { client }
    }

    #[cfg(test)]
    fn with_client(client: OpenCodeClient) -> Self {
        Self {
            client: ClientState::Ready(Arc::new(client)),
        }
    }

    fn client(&self) -> Result<Arc<OpenCodeClient>, String> {
        match &self.client {
            ClientState::Ready(client) => Ok(client.clone()),
            ClientState::Invalid(error) => Err(error.clone()),
        }
    }

    async fn buffered_messages_response(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
    ) -> Response {
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(spec) = model::resolve(requested) else {
            return unsupported_model(requested);
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, spec.id);
        }
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return invalid_configuration_response(error),
        };
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

        let value = match spec.endpoint {
            EndpointKind::ChatCompletions => {
                let translated = match chat::prepare_request(&body, spec.id) {
                    Ok(translated) => translated,
                    Err(error) => return invalid_request_response(error),
                };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post(spec.endpoint, &translated, true, ctx.traffic.clone())
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "sse");
                match chat::accumulate_response(&bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => return invalid_upstream_response(error),
                }
            }
            EndpointKind::Messages => {
                let translated = match messages::prepare_request(&body, spec.id) {
                    Ok(translated) => translated,
                    Err(error) => return invalid_request_response(error),
                };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post(spec.endpoint, &translated, false, ctx.traffic.clone())
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "json");
                match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(value) => value,
                    Err(error) => return invalid_upstream_response(error),
                }
            }
            EndpointKind::Responses => {
                let translated =
                    match responses::prepare_request(&body, spec.id, ctx.session_id.clone()) {
                        Ok(translated) => translated,
                        Err(error) => return invalid_request_response(error),
                    };
                mark_upstream_started(&ctx);
                let upstream = match client
                    .post(spec.endpoint, &translated, true, ctx.traffic.clone())
                    .await
                {
                    Ok(upstream) => upstream,
                    Err(error) => return map_error(error),
                };
                let bytes = match upstream.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return map_error(error),
                };
                capture_buffered_upstream(&ctx, &bytes, "sse");
                match accumulate_responses_response(&bytes, &message_id, requested) {
                    Ok(value) => value,
                    Err(error) => return invalid_upstream_response(error),
                }
            }
        };

        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json("051-downstream-response", &value);
        }
        update_buffered_usage(&ctx, &value);
        (StatusCode::OK, Json(value)).into_response()
    }
}

impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub fn advertised_models() -> Vec<String> {
    model::advertised_models()
}

#[async_trait]
impl Provider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn supported_models(&self) -> Vec<String> {
        advertised_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &OPENCODE_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        if !body.stream {
            return self.buffered_messages_response(body, ctx).await;
        }
        match self.generate_anthropic_stream(body, ctx).await {
            Ok(generation) => sse_response(generation.body),
            Err(error) => map_provider_error(error),
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.as_deref().unwrap_or_default();
        let Some(spec) = model::resolve(requested) else {
            return unsupported_model(requested);
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, spec.id);
        }
        let tokens = count_tokens::count_tokens(&body);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let requested = body.model.as_deref().unwrap_or_default();
        let spec = model::resolve(requested).ok_or_else(|| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Unsupported OpenCode Go model: {requested}"),
            )
        })?;
        let client = self.client().map_err(|error| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                format!("Invalid OpenCode Go configuration: {error}"),
            )
        })?;

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, spec.id);
            monitor.upstream_started(&ctx.req_id);
        }
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let body = match spec.endpoint {
            EndpointKind::ChatCompletions => {
                let translated = chat::prepare_request(&body, spec.id)
                    .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post(spec.endpoint, &translated, true, ctx.traffic.clone())
                    .await
                    .map_err(opencode_provider_error)?;
                chat::stream_body(
                    upstream,
                    message_id,
                    requested.to_string(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Messages => {
                let translated = messages::prepare_request(&body, spec.id)
                    .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post(spec.endpoint, &translated, true, ctx.traffic.clone())
                    .await
                    .map_err(opencode_provider_error)?;
                messages::stream_body(
                    upstream,
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
            EndpointKind::Responses => {
                let translated = responses::prepare_request(&body, spec.id, ctx.session_id.clone())
                    .map_err(invalid_request_provider_error)?;
                let upstream = client
                    .post(spec.endpoint, &translated, true, ctx.traffic.clone())
                    .await
                    .map_err(opencode_provider_error)?;
                responses::stream_body(
                    upstream,
                    message_id,
                    requested.to_string(),
                    ctx.monitor.clone(),
                    ctx.req_id.clone(),
                    ctx.traffic.clone(),
                )
            }
        };

        Ok(Generation {
            body: GenerationBody::LiveSse(body),
            resolved_model: spec.id.to_string(),
        })
    }
}

fn invalid_request_provider_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        StatusCode::BAD_REQUEST,
        ProviderErrorKind::InvalidRequest,
        error.to_string(),
    )
}

fn opencode_provider_error(error: OpenCodeError) -> ProviderError {
    let (status, kind) = match error.status {
        StatusCode::UNAUTHORIZED => (StatusCode::UNAUTHORIZED, ProviderErrorKind::Authentication),
        StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN => {
            (error.status, ProviderErrorKind::Permission)
        }
        StatusCode::TOO_MANY_REQUESTS => {
            (StatusCode::TOO_MANY_REQUESTS, ProviderErrorKind::RateLimit)
        }
        status if status.is_client_error() => (status, ProviderErrorKind::InvalidRequest),
        _ => (StatusCode::BAD_GATEWAY, ProviderErrorKind::Api),
    };
    let mut mapped = ProviderError::new(status, kind, error.message);
    mapped.retry_after = error.retry_after;
    mapped
}

fn map_error(error: OpenCodeError) -> Response {
    map_provider_error(opencode_provider_error(error))
}

fn map_provider_error(error: ProviderError) -> Response {
    let response = json_error(error.status, error.error_type(), error.message);
    if let Some(retry_after) = error.retry_after {
        ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
    } else {
        response
    }
}

fn unsupported_model(requested: &str) -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        format!("Unsupported OpenCode Go model: {requested}"),
    )
}

fn invalid_configuration_response(error: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        format!("Invalid OpenCode Go configuration: {error}"),
    )
}

fn invalid_request_response(error: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        error.to_string(),
    )
}

fn invalid_upstream_response(error: impl std::fmt::Display) -> Response {
    json_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("OpenCode Go response translation failed: {error}"),
    )
}

fn mark_upstream_started(ctx: &RequestContext) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.upstream_started(&ctx.req_id);
    }
}

fn capture_buffered_upstream(ctx: &RequestContext, bytes: &[u8], extension: &str) {
    if let Some(traffic) = ctx.traffic.as_ref() {
        traffic.write_bytes(&format!("032-upstream-response-body.{extension}"), bytes);
    }
}

fn update_buffered_usage(ctx: &RequestContext, value: &serde_json::Value) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.usage_updated(
            &ctx.req_id,
            value
                .pointer("/usage/input_tokens")
                .and_then(serde_json::Value::as_u64),
            value
                .pointer("/usage/output_tokens")
                .and_then(serde_json::Value::as_u64),
        );
    }
}

fn sse_response(body: GenerationBody) -> Response {
    let body = match body {
        GenerationBody::BufferedSse(bytes) => Body::from(bytes),
        GenerationBody::LiveSse(body) => body,
    };
    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
            (http::header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

struct OpenCodeCli;

impl CliHandlers for OpenCodeCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "OpenCode Go uses an API key; set CCP_OPENCODE_API_KEY, OPENCODE_API_KEY, or opencode.apiKey in config.json"
        )
    }

    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }

    fn status(&self) -> anyhow::Result<()> {
        let Some(source) = crate::config::opencode_api_key_source() else {
            anyhow::bail!("Not authenticated");
        };
        println!("API key configured: true");
        println!("Source: {source}");
        println!("Base URL: {}", crate::config::opencode_base_url());
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "OpenCode Go credentials are managed through environment variables or config.json"
        )
    }
}

static OPENCODE_CLI: OpenCodeCli = OpenCodeCli;

#[cfg(test)]
mod tests {
    use axum::{
        Json, Router,
        body::Body,
        extract::OriginalUri,
        http::HeaderMap,
        response::{IntoResponse, Response},
        routing::post,
    };
    use bytes::Bytes;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::convert::Infallible;

    use super::*;

    fn context() -> RequestContext {
        RequestContext {
            req_id: "req_test".to_string(),
            provider: "opencode".to_string(),
            session_id: None,
            session_seq: None,
            monitor: None,
            traffic: None,
        }
    }

    #[tokio::test]
    async fn missing_key_is_actionable() {
        let client = OpenCodeClient::new("https://example.com/v1".into(), None).unwrap();
        let provider = OpenCodeProvider::with_client(client);
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let response = provider.handle_messages(body, context()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("OPENCODE_API_KEY")
        );
    }

    #[tokio::test]
    async fn count_tokens_is_local_for_all_protocol_families() {
        let provider = OpenCodeProvider::with_client(
            OpenCodeClient::new("https://example.com/v1".into(), None).unwrap(),
        );
        for model in ["glm-5.2", "minimax-m3", "opencode-go/gpt-5.6-luna"] {
            let body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello world"}]
            }))
            .unwrap();
            let response = provider.handle_count_tokens(body, context()).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    async fn mock_go_upstream(
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        if uri.path().ends_with("/messages") {
            assert_eq!(
                headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok()),
                Some("test-key")
            );
        } else {
            assert_eq!(
                headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer test-key")
            );
        }
        if body["model"] == "qwen3.7-max" {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(http::header::RETRY_AFTER, "17")],
                Json(json!({"error":{"message":"Go limit reached"}})),
            )
                .into_response();
        }
        if uri.path().ends_with("/chat/completions") {
            return (
                [(http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello from chat\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
                .into_response();
        }
        if uri.path().ends_with("/responses") {
            return (
                [(http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello from responses\"}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":6,\"output_tokens\":3}}}\n\n"
                ),
            )
                .into_response();
        }
        if body["stream"] == true {
            return (
                [(http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_native\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"minimax-m3\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                ),
            )
                .into_response();
        }
        Json(json!({
            "id": "msg_native",
            "type": "message",
            "role": "assistant",
            "model": body["model"],
            "content": [{"type":"text","text":"hello from messages"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens":4,"output_tokens":3}
        }))
        .into_response()
    }

    async fn mock_provider() -> (OpenCodeProvider, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_go_upstream))
            .route("/v1/messages", post(mock_go_upstream))
            .route("/v1/responses", post(mock_go_upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap();
        (OpenCodeProvider::with_client(client), server)
    }

    async fn delayed_chat_upstream() -> Response {
        let stream = futures_util::stream::unfold(0u8, |step| async move {
            match step {
                0 => Some((
                    Ok::<Bytes, Infallible>(Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"early\"}}]}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    Some((
                        Ok(Bytes::from_static(
                            b"data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n",
                        )),
                        2,
                    ))
                }
                _ => None,
            }
        });
        (
            [(http::header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    #[tokio::test]
    async fn generate_contract_streams_before_upstream_completion() {
        let app = Router::new().route("/v1/chat/completions", post(delayed_chat_upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = OpenCodeProvider::with_client(
            OpenCodeClient::new(format!("http://{address}/v1"), Some("test-key".to_string()))
                .unwrap(),
        );
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let generation = provider
            .generate_anthropic_stream(body, context())
            .await
            .unwrap();
        let GenerationBody::LiveSse(body) = generation.body else {
            panic!("OpenCode Go generation must remain live");
        };
        let mut stream = body.into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("the proxy buffered the stream until upstream completion")
            .expect("stream ended before the first translated event")
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("early"));
        server.abort();
    }

    #[tokio::test]
    async fn buffered_messages_cover_all_three_upstream_protocols() {
        let (provider, server) = mock_provider().await;
        for (model, expected) in [
            ("glm-5.2", "hello from chat"),
            ("opencode-go/minimax-m3", "hello from messages"),
            ("opencode-go/gpt-5.6-luna", "hello from responses"),
        ] {
            let body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "stream": false,
                "messages": [{"role":"user","content":"hello"}]
            }))
            .unwrap();
            let response = provider.handle_messages(body, context()).await;
            assert_eq!(response.status(), StatusCode::OK, "model {model}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["content"][0]["text"], expected, "model {model}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn streaming_contract_covers_all_three_upstream_protocols() {
        let (provider, server) = mock_provider().await;
        for model in [
            "glm-5.2",
            "opencode-go/minimax-m3",
            "opencode-go/gpt-5.6-luna",
        ] {
            let body: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "messages": [{"role":"user","content":"hello"}]
            }))
            .unwrap();
            let generation = provider
                .generate_anthropic_stream(body, context())
                .await
                .unwrap();
            assert_eq!(generation.resolved_model, model::resolve(model).unwrap().id);
            let GenerationBody::LiveSse(body) = generation.body else {
                panic!("OpenCode Go generation must remain live");
            };
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            assert!(
                String::from_utf8_lossy(&bytes).contains("message_stop"),
                "model {model}"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn rate_limit_and_retry_after_are_preserved() {
        let (provider, server) = mock_provider().await;
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "qwen3.7-max",
            "messages": [{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let response = provider.handle_messages(body, context()).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("17")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["type"], "rate_limit_error");
        assert_eq!(value["error"]["message"], "Go limit reached");
        server.abort();
    }

    #[test]
    fn permission_errors_are_not_reported_as_authentication_failures() {
        for status in [StatusCode::PAYMENT_REQUIRED, StatusCode::FORBIDDEN] {
            let error = opencode_provider_error(OpenCodeError {
                status,
                retry_after: None,
                message: "denied".into(),
            });
            assert_eq!(error.status, status);
            assert_eq!(error.kind, ProviderErrorKind::Permission);
        }
    }
}
