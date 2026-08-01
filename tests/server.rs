use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::IntoResponse;
use claude_code_proxy::{
    MessagesRequest,
    config::AliasProvider,
    monitor::{MonitorHandle, RequestStatus},
    provider::{CliHandlers, Generation, GenerationBody, Provider, ProviderError, RequestContext},
    registry::Registry,
    server::{
        AppFeatures, app, app_with_features, app_with_monitor, app_with_options,
        bind_proxy_listener,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::util::ServiceExt;

fn body_string(json: &str) -> Body {
    Body::from(json.to_string())
}

struct FakeCli;

impl CliHandlers for FakeCli {
    fn login(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn device(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn status(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

static FAKE_CLI: FakeCli = FakeCli;

struct FakeProvider {
    name: &'static str,
    models: Vec<String>,
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        let model = body.model.unwrap_or_default();
        let sse = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_fake\",\"model\":{model:?},\"usage\":{{\"input_tokens\":2}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{name}\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            name = self.name,
        );
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: model,
        })
    }
}

fn routed_registry() -> Arc<Registry> {
    Arc::new(Registry::from_providers(
        AliasProvider::Kimi,
        vec![
            Arc::new(FakeProvider {
                name: "kimi",
                models: vec!["kimi-k2.6".to_string()],
            }) as Arc<dyn Provider>,
            Arc::new(FakeProvider {
                name: "grok",
                models: vec!["grok-4.5".to_string()],
            }),
            Arc::new(FakeProvider {
                name: "cursor",
                models: vec!["cursor".to_string()],
            }),
        ],
    ))
}

#[tokio::test]
async fn bind_error_names_address_and_port() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();

    let err = bind_proxy_listener("127.0.0.1", port)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains(&format!("127.0.0.1:{port}")));
    assert!(err.contains("failed to bind proxy listener"));
}

#[tokio::test]
async fn configurable_bind_address_accepts_all_interfaces() {
    let listener = bind_proxy_listener("0.0.0.0", 0).await.unwrap();
    assert_eq!(listener.local_addr().unwrap().ip().to_string(), "0.0.0.0");
}

#[tokio::test]
async fn invalid_bind_address_is_actionable() {
    let err = bind_proxy_listener("not-an-ip", 18765)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid proxy bind address"));
    assert!(err.contains("not-an-ip"));
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(body, json!({"ok": true}));
}

#[tokio::test]
async fn invalid_json_request_is_json_error() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let error_type = value["error"]["type"].as_str().unwrap_or("");
    assert_eq!(error_type, "invalid_request_error");
}

#[tokio::test]
async fn empty_body_is_invalid_json() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_model_returns_400_with_summary() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}],"model":"not-a-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("Unknown model \"not-a-model\""));
    assert!(message.contains("Supported:"));
}

#[tokio::test]
async fn missing_model_returns_400() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let error_type = body["error"]["type"].as_str().unwrap_or("");
    assert_eq!(error_type, "invalid_request_error");
}

#[tokio::test]
async fn known_model_reaches_codex_provider() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Codex provider is now concrete, so it should attempt auth before returning 501
    let status = response.status();
    assert!(
        status != StatusCode::NOT_IMPLEMENTED,
        "codex should no longer be a placeholder provider"
    );
}

#[tokio::test]
async fn count_tokens_routes_to_provider() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Codex provider is now concrete, so count_tokens should succeed
    let status = response.status();
    assert!(
        status != StatusCode::NOT_IMPLEMENTED,
        "count_tokens should no longer return 501 for codex models"
    );
}

#[tokio::test]
async fn context_window_hint_is_removed_before_provider_dispatch() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-luna[1m]","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn opus_5_alias_routes_to_provider() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn image_routes_reject_variations_wrong_media_and_oversized_generation() {
    let features = AppFeatures {
        responses_api: false,
        images_api: true,
        transcriptions_api: false,
    };
    let variation = app_with_features(Arc::new(Registry::with_default_alias()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/variations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(variation.status(), StatusCode::NOT_FOUND);

    let wrong_media = app_with_features(Arc::new(Registry::with_default_alias()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::from("--x--\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_media.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let oversized = app_with_features(Arc::new(Registry::with_default_alias()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(vec![b'x'; 256 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn monitor_tracks_image_endpoint_without_session_affinity() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_features(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.recent[0].endpoint.label(), "images");
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert!(state.recent[0].session_seq.is_none());
    assert!(state.recent[0].traffic_capture_path.is_none());
}

#[tokio::test]
async fn image_edit_accepts_multipart_and_validates_fields() {
    let app = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let boundary = "ccp-image-test";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"input.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/edits")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "prompt");
}

#[tokio::test]
async fn image_routes_are_independently_opt_in() {
    let disabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    );
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn transcription_route_is_independently_opt_in_and_validates_multipart() {
    let disabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    );
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: true,
        },
    );
    let boundary = "ccp-transcription-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-4o-mini-transcribe\r\n--{boundary}--\r\n"
    );
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "file");
}

#[tokio::test]
async fn transcription_route_rejects_non_audio_uploads() {
    let app = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: true,
        },
    );
    let boundary = "ccp-transcription-type-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nnot audio\r\n--{boundary}--\r\n"
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn native_responses_route_is_disabled_by_default_option() {
    let app = app_with_options(Arc::new(Registry::with_default_alias()), None, false);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(r#"{"model":"gpt-5.4","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn enabled_native_responses_route_uses_openai_errors() {
    let app = app_with_options(Arc::new(Registry::with_default_alias()), None, true);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert!(body.get("type").is_none());
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn chat_completions_route_uses_responses_api_gate() {
    let disabled = app_with_options(Arc::new(Registry::with_default_alias()), None, false);
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_options(Arc::new(Registry::with_default_alias()), None, true);
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn openai_routes_select_non_codex_providers_and_aliases() {
    for (uri, request, expected) in [
        (
            "/v1/chat/completions",
            json!({"model":"kimi-k2.6","messages":[{"role":"user","content":"hello"}]}),
            "kimi",
        ),
        (
            "/v1/responses",
            json!({"model":"grok-4.5","input":"hello"}),
            "grok",
        ),
        (
            "/v1/chat/completions",
            json!({"model":"cursor:gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
            "cursor",
        ),
        (
            "/v1/responses",
            json!({"model":"sonnet","input":"hello"}),
            "kimi",
        ),
    ] {
        let response = app_with_options(routed_registry(), None, true)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(body_string(&request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri} {request}");
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let text = if uri.ends_with("responses") {
            value["output"][0]["content"][0]["text"].as_str()
        } else {
            value["choices"][0]["message"]["content"].as_str()
        };
        assert_eq!(text, Some(expected));
    }
}

#[tokio::test]
async fn routed_openai_streams_use_surface_specific_events() {
    let chat = app_with_options(routed_registry(), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"kimi-k2.6","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let chat = String::from_utf8(
        axum::body::to_bytes(chat.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(chat.contains("chat.completion.chunk"));
    assert!(chat.contains("\"total_tokens\":3"));
    assert!(chat.ends_with("data: [DONE]\n\n"));

    let responses = app_with_options(routed_registry(), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"grok-4.5","stream":true,"input":"hello"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let responses = String::from_utf8(
        axum::body::to_bytes(responses.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(responses.contains("event: response.created"));
    assert!(responses.contains("event: response.completed"));
    assert!(responses.contains("\"sequence_number\":0"));
}

#[tokio::test]
async fn non_codex_validation_uses_openai_errors_before_generation() {
    let monitor = MonitorHandle::new(10);
    let response = app_with_options(routed_registry(), Some(monitor.clone()), true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-client-request-id", "invalid-routed-request")
                .body(body_string(
                    r#"{"model":"kimi-k2.6","messages":[{"role":"user","content":"hello"}],"temperature":0.5}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["error"]["param"], "temperature");
    assert_eq!(value["error"]["code"], "unsupported_parameter");
    assert!(
        claude_code_proxy::session::existing_session_now(Some("invalid-routed-request")).is_none()
    );
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].session_seq, None);
    assert_eq!(snapshot.recent[0].provider, None);
}

#[tokio::test]
async fn chat_completions_validation_returns_openai_parameter_errors() {
    let app = app_with_options(Arc::new(Registry::with_default_alias()), None, true);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}],"max_tokens":100}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "max_tokens");
    assert_eq!(body["error"]["code"], "unsupported_parameter");
}

#[tokio::test]
async fn unknown_routes_use_anthropic_not_found_error() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(body["type"].as_str().unwrap_or(""), "error");
}

#[tokio::test]
async fn monitor_records_successful_request_events() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "project-session")
                .body(body_string(
                    r##"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.177.45c"},{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK.","cache_control":{"type":"ephemeral"}},{"type":"text","text":"\nYou are an interactive agent.\n\n# Environment\nYou have been invoked in the following environment: \n - Primary working directory: /projects/example\n - Is a git repository: true","cache_control":{"type":"ephemeral"}}],"output_config":{"effort":"high"}}"##,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let state = monitor.snapshot();
    assert_eq!(state.active.len(), 1);
    assert!(state.recent.is_empty());

    let _body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.recent[0].status, RequestStatus::Completed);
    assert_eq!(state.recent[0].http_status, Some(200));
    assert_eq!(
        state.recent[0].session_id.as_deref(),
        Some("project-session")
    );
    assert!(state.recent[0].session_seq.is_some());
    assert_eq!(state.recent[0].project.as_deref(), Some("example"));
    assert_eq!(state.sessions[0].project.as_deref(), Some("example"));
    assert_eq!(state.recent[0].provider.as_deref(), Some("codex"));
    assert_eq!(state.recent[0].model.as_deref(), Some("gpt-5.4"));
    assert_eq!(state.recent[0].effort.as_deref(), Some("high"));
    assert!(state.recent[0].input_tokens.is_some());
}

#[tokio::test]
async fn monitor_records_invalid_json_failure() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert_eq!(state.recent[0].http_status, Some(400));
    let error = state.recent[0].error.as_deref().unwrap_or("");
    assert!(error.starts_with("Invalid JSON:"));
}

#[tokio::test]
async fn monitor_records_unknown_model_failure() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}],"model":"not-a-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert_eq!(state.recent[0].http_status, Some(400));
    let error = state.recent[0].error.as_deref().unwrap_or("");
    assert!(error.starts_with("Unknown model \"not-a-model\""));
    assert!(error.contains("Supported:"));
}

async fn get_models(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

#[tokio::test]
async fn models_endpoint_lists_supported_models() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert!(!data.is_empty());
    let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"gpt-5.6-sol"));
    for entry in data {
        assert_eq!(entry["type"], "model");
        assert!(entry["display_name"].as_str().is_some());
    }
    assert_eq!(value["has_more"], json!(false));
    assert_eq!(value["first_id"], data[0]["id"]);
    assert_eq!(value["last_id"], data[data.len() - 1]["id"]);
}

#[tokio::test]
async fn models_endpoint_includes_claude_prefixed_aliases_for_discovery() {
    // Claude Code's gateway model discovery ignores ids that don't start with
    // "claude" or "anthropic", so the alias entries are what make
    // CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 useful at all.
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?limit=1000").await;

    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = value["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.iter().any(|id| id.starts_with("claude-")));
    assert!(ids.contains(&"claude-opus-5"));
}

#[tokio::test]
async fn models_endpoint_respects_limit() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(value["has_more"], json!(true));
    assert_eq!(value["last_id"], data[1]["id"]);
}

#[tokio::test]
async fn models_endpoint_tolerates_unknown_query_params() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, _) = get_models(app, "/v1/models?limit=1000&after_id=x").await;
    assert_eq!(status, StatusCode::OK);
}
