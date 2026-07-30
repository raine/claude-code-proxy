use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use claude_code_proxy::{
    monitor::{MonitorHandle, RequestStatus},
    registry::Registry,
    server::{
        ServerLimits, app, app_with_limits, app_with_monitor, bind_proxy_listener,
        bind_proxy_listener_with_ack,
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use tower::util::ServiceExt;

fn body_string(json: &str) -> Body {
    Body::from(json.to_string())
}

fn test_limits() -> ServerLimits {
    ServerLimits {
        max_request_body_bytes: 1024,
        max_buffered_request_bytes: 8 * 1024,
        max_concurrent_requests: 8,
        max_concurrent_per_provider: 8,
        max_concurrent_per_session: 8,
        request_body_idle_timeout: Duration::from_millis(100),
        request_body_total_timeout: Duration::from_millis(500),
    }
}

fn count_tokens_request(session_id: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json");
    if let Some(session_id) = session_id {
        request = request.header("x-claude-code-session-id", session_id);
    }
    request
        .body(body_string(
            r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .unwrap()
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
async fn non_loopback_bind_requires_explicit_unsafe_ack() {
    let error = bind_proxy_listener("0.0.0.0", 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("refusing unauthenticated non-loopback bind address"));
    assert!(error.contains("--allow-remote-unauthenticated"));

    let listener = bind_proxy_listener_with_ack("0.0.0.0", 0, true)
        .await
        .unwrap();
    assert_eq!(listener.local_addr().unwrap().ip().to_string(), "0.0.0.0");
    assert_ne!(listener.local_addr().unwrap().port(), 0);
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
async fn version_reports_build_and_runtime_identity() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/version")
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
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["gitSha"].as_str().is_some_and(|sha| !sha.is_empty()));
    assert_eq!(body["pid"], std::process::id());
    assert!(body["configGeneration"].as_u64().is_some());
    assert_eq!(
        body["providerConstructionConfigGeneration"],
        body["configGeneration"]
    );
    assert_eq!(
        body["providerConstructionConfigGenerationEnd"],
        body["providerConstructionConfigGeneration"]
    );
    assert_eq!(body["providerConstructionSnapshotStable"], true);
    assert_eq!(
        body["configGenerationChangedSinceProviderConstruction"],
        false
    );
    assert_eq!(
        body["configReload"]["status"],
        "provider_generation_current"
    );
    assert!(
        body["configReload"]["restartRequired"]
            .as_array()
            .is_some_and(|fields| !fields.is_empty())
    );
    assert!(
        body["binarySha256"]
            .as_str()
            .is_some_and(|sha| sha.len() == 64)
    );
    let mut executable = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = executable.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    assert_eq!(body["binarySha256"], hex::encode(digest.finalize()));
    assert_eq!(
        body["capabilities"]["codexOutputBudget"],
        "unsupported_by_private_gateway"
    );
    assert_eq!(
        body["capabilities"]["codexStructuredOutput"],
        "validated_against_original_schema"
    );
    assert_eq!(
        body["capabilities"]["codexUnconfirmedToolCall"],
        "fail_closed_by_default"
    );
}

#[tokio::test]
async fn models_returns_anthropic_catalog_contract() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
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
    assert_eq!(body["has_more"], false);
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());

    let ids: Vec<&str> = data
        .iter()
        .map(|model| {
            assert_eq!(model["type"], "model");
            assert_eq!(model["display_name"], model["id"]);
            let created_at = model["created_at"].as_str().unwrap();
            time::OffsetDateTime::parse(created_at, &time::format_description::well_known::Rfc3339)
                .unwrap();
            model["id"].as_str().unwrap()
        })
        .collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(ids.contains(&"gpt-5.6-sol"));
    assert!(ids.contains(&"grok-4.5"));
    assert_eq!(body["first_id"].as_str(), ids.first().copied());
    assert_eq!(body["last_id"].as_str(), ids.last().copied());
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
    let request_id = response
        .headers()
        .get("request-id")
        .and_then(|value| value.to_str().ok())
        .expect("messages responses should expose their proxy request id");
    uuid::Uuid::parse_str(request_id).expect("request-id should be a UUID");
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
async fn request_body_limit_rejects_content_length_and_streamed_overflow() {
    let mut limits = test_limits();
    limits.max_request_body_bytes = 64;
    let app = app_with_limits(Arc::new(Registry::with_default_alias()), None, limits);

    let declared = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-length", "65")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(declared.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let chunks = futures_util::stream::iter([
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from(vec![b' '; 40])),
        Ok(bytes::Bytes::from(vec![b' '; 25])),
    ]);
    let streamed = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .body(Body::from_stream(chunks))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(streamed.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn request_body_limit_accepts_the_exact_boundary() {
    let mut body = r#"{"model":"not-a-model","messages":[]}"#.to_string();
    body.push_str(&" ".repeat(128 - body.len()));
    let mut limits = test_limits();
    limits.max_request_body_bytes = body.len();
    let app = app_with_limits(Arc::new(Registry::with_default_alias()), None, limits);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn request_body_timeout_releases_admission_capacity() {
    let mut limits = test_limits();
    limits.max_concurrent_requests = 1;
    limits.request_body_idle_timeout = Duration::from_millis(10);
    limits.request_body_total_timeout = Duration::from_millis(50);
    let app = app_with_limits(Arc::new(Registry::with_default_alias()), None, limits);
    let stalled = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"{}"))
    });

    let timed_out = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(Body::from_stream(stalled))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(timed_out.status(), StatusCode::REQUEST_TIMEOUT);

    let recovered = app.oneshot(count_tokens_request(None)).await.unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
}

#[tokio::test]
async fn global_request_byte_budget_load_sheds_before_aggregate_growth() {
    let mut limits = test_limits();
    limits.max_buffered_request_bytes = 4;
    let app = app_with_limits(Arc::new(Registry::with_default_alias()), None, limits);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(Body::from("12345"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "1");
}

#[tokio::test]
async fn global_admission_permit_is_held_until_response_body_drop() {
    let mut limits = test_limits();
    limits.max_concurrent_requests = 1;
    let app = app_with_limits(Arc::new(Registry::with_default_alias()), None, limits);

    let first = app
        .clone()
        .oneshot(count_tokens_request(None))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let saturated = app
        .clone()
        .oneshot(count_tokens_request(None))
        .await
        .unwrap();
    assert_eq!(saturated.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(saturated.headers()["retry-after"], "1");

    drop(first);
    let recovered = app.oneshot(count_tokens_request(None)).await.unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
}

#[tokio::test]
async fn provider_and_session_admission_limits_are_independent() {
    let mut provider_limits = test_limits();
    provider_limits.max_concurrent_per_provider = 1;
    let provider_app = app_with_limits(
        Arc::new(Registry::with_default_alias()),
        None,
        provider_limits,
    );
    let first = provider_app
        .clone()
        .oneshot(count_tokens_request(Some("provider-a")))
        .await
        .unwrap();
    let saturated = provider_app
        .clone()
        .oneshot(count_tokens_request(Some("provider-b")))
        .await
        .unwrap();
    assert_eq!(saturated.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(first);

    let mut session_limits = test_limits();
    session_limits.max_concurrent_per_session = 1;
    let session_app = app_with_limits(
        Arc::new(Registry::with_default_alias()),
        None,
        session_limits,
    );
    let first = session_app
        .clone()
        .oneshot(count_tokens_request(Some("same-session")))
        .await
        .unwrap();
    let saturated = session_app
        .clone()
        .oneshot(count_tokens_request(Some("same-session")))
        .await
        .unwrap();
    assert_eq!(saturated.status(), StatusCode::TOO_MANY_REQUESTS);
    let other_session = session_app
        .clone()
        .oneshot(count_tokens_request(Some("other-session")))
        .await
        .unwrap();
    assert_eq!(other_session.status(), StatusCode::OK);
    drop(first);
    drop(other_session);
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

#[test]
fn known_model_resolves_to_codex_provider_without_loading_credentials() {
    let registry = Registry::with_default_alias();
    let provider = registry
        .provider_for_model("gpt-5.4", None)
        .expect("known Codex model");

    assert_eq!(provider.name(), "codex");
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
async fn count_tokens_endpoint_includes_structured_output_schema_for_each_provider() {
    let schema = json!({
        "type": "object",
        "properties": {
            "result": {
                "type": "string",
                "description": "schema-token-evidence-".repeat(512)
            }
        },
        "required": ["result"],
        "additionalProperties": false
    });
    assert!(serde_json::to_vec(&schema).unwrap().len() >= 10 * 1024);

    for model in ["gpt-5.6-sol", "grok-4.5-high"] {
        let app = app(Arc::new(Registry::with_default_alias()));
        let mut counts = Vec::new();
        for format in [
            None,
            Some(json!({
                "type": "json_schema",
                "name": "large_result",
                "schema": schema.clone()
            })),
        ] {
            let mut payload = json!({
                "model": model,
                "messages": [{"role": "user", "content": "return a result"}]
            });
            if let Some(format) = format {
                payload["output_config"] = json!({"format": format});
            }
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/v1/messages/count_tokens")
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{model} count response: {}",
                String::from_utf8_lossy(&body)
            );
            let body: Value = serde_json::from_slice(&body).unwrap();
            counts.push(
                body["input_tokens"]
                    .as_u64()
                    .expect("count endpoint returns input_tokens"),
            );
        }
        assert!(
            counts[1] > counts[0],
            "{model} endpoint omitted the translated structured schema: {counts:?}"
        );
    }
}

#[tokio::test]
async fn count_tokens_opus_5_uses_grok_session_affinity() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );

    for model in ["grok-4.5", "claude-opus-5"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/messages/count_tokens")
                    .header("content-type", "application/json")
                    .header("x-claude-code-session-id", "server-opus-5-grok-affinity")
                    .body(Body::from(
                        json!({
                            "model": model,
                            "messages": [{"role": "user", "content": "hello"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{model} response body: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent.len(), 2);
    assert!(
        snapshot
            .recent
            .iter()
            .all(|request| request.provider.as_deref() == Some("grok"))
    );
    assert!(
        snapshot
            .recent
            .iter()
            .any(|request| request.model.as_deref() == Some("claude-opus-5"))
    );
}

#[tokio::test]
async fn count_tokens_accepts_tool_reference_results_for_grok_and_codex() {
    for model in ["grok-4.5-high", "gpt-5.6-sol"] {
        let app = app(Arc::new(Registry::with_default_alias()));
        let payload = json!({
            "model": model,
            "tools": [
                {"name": "ToolSearch", "input_schema": {"type": "object"}},
                {"name": "WebFetch", "defer_loading": true, "input_schema": {"type": "object"}}
            ],
            "messages": [
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "call_tool_search_1",
                        "name": "ToolSearch",
                        "input": {"query": "select:WebFetch"}
                    }]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call_tool_search_1",
                        "content": [{
                            "type": "tool_reference",
                            "tool_name": "WebFetch"
                        }]
                    }]
                }
            ]
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/messages/count_tokens")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            status,
            StatusCode::OK,
            "{model} rejected ToolSearch result: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            parsed["input_tokens"]
                .as_u64()
                .is_some_and(|tokens| tokens > 0),
            "{model} returned an invalid token count: {parsed}"
        );
    }
}

#[tokio::test]
async fn count_tokens_accepts_all_anthropic_tool_result_shapes_for_grok_and_codex() {
    let result_contents = [
        None,
        Some(json!([])),
        Some(json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ])),
        Some(json!([{
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "iVBORw0KGgoAAAANSUhEUgAAACAAAAAQCAYAAAB3AH1ZAAAAI0lEQVR4nO3OIQEAAAwEoetfeovxBoGnq1tKQEBAQEBAYB142QD8LgetqmMAAAAASUVORK5CYII="
            }
        }])),
        Some(json!([{
            "type": "search_result",
            "source": "https://example.com/result",
            "title": "Example result",
            "content": [{"type": "text", "text": "search body"}]
        }])),
        Some(json!([{
            "type": "document",
            "source": {
                "type": "text",
                "media_type": "text/plain",
                "data": "document body"
            },
            "title": "Example document"
        }])),
        Some(json!([{
            "type": "tool_reference",
            "tool_name": "WebFetch"
        }])),
    ];

    for model in ["grok-4.5-high", "gpt-5.6-sol"] {
        for result_content in &result_contents {
            let app = app(Arc::new(Registry::with_default_alias()));
            let mut result = json!({
                "type": "tool_result",
                "tool_use_id": "call_shape_1"
            });
            if let Some(content) = result_content {
                result["content"] = content.clone();
            }
            let payload = json!({
                "model": model,
                "tools": [
                    {"name": "ToolSearch", "input_schema": {"type": "object"}},
                    {"name": "WebFetch", "defer_loading": true, "input_schema": {"type": "object"}}
                ],
                "messages": [
                    {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "call_shape_1",
                            "name": "ToolSearch",
                            "input": {},
                            "caller": {"type": "direct"}
                        }]
                    },
                    {"role": "user", "content": [result]}
                ]
            });
            let response = app
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/v1/messages/count_tokens")
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{model} rejected tool_result content {result_content:?}: {}",
                String::from_utf8_lossy(&body)
            );
        }
    }
}

#[tokio::test]
async fn count_tokens_accepts_current_web_search_versions_for_grok_and_codex() {
    for model in ["grok-4.5-high", "gpt-5.6-sol"] {
        for version in [
            "web_search_20250305",
            "web_search_20260209",
            "web_search_20260318",
        ] {
            let app = app(Arc::new(Registry::with_default_alias()));
            let payload = json!({
                "model": model,
                "messages": [{"role": "user", "content": "search rust"}],
                "tools": [{
                    "type": version,
                    "name": "web_search",
                    "max_uses": 3,
                    "allowed_domains": ["rust-lang.org"],
                    "user_location": {
                        "type": "approximate",
                        "city": "Shanghai",
                        "country": "CN",
                        "timezone": "Asia/Shanghai"
                    },
                    "allowed_callers": ["direct"]
                }]
            });
            let response = app
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/v1/messages/count_tokens")
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{model} rejected {version}: {}",
                String::from_utf8_lossy(&body)
            );
        }
    }
}

#[tokio::test]
async fn count_tokens_rejects_unknown_forced_tools_locally_for_grok_and_codex() {
    for model in ["grok-4.5-high", "gpt-5.6-sol"] {
        let app = app(Arc::new(Registry::with_default_alias()));
        let payload = json!({
            "model": model,
            "messages": [{"role": "user", "content": "run it"}],
            "tools": [{
                "name": "KnownTool",
                "description": "A known tool",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": {"type": "tool", "name": "MissingTool"}
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/messages/count_tokens")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{model} should reject an unknown forced tool before dispatch"
        );
    }
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
async fn compaction_header_routes_only_compaction_requests_to_grok() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let compaction = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-ccproxy-compaction-model", "grok-4.5-high")
                .header(
                    "x-claude-code-session-id",
                    "compaction-without-prior-affinity",
                )
                .body(body_string(
                    r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far."}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(compaction.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(compaction.into_body(), usize::MAX)
        .await
        .unwrap();

    let ordinary = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-ccproxy-compaction-model", "grok-4.5-high")
                .header(
                    "x-claude-code-session-id",
                    "compaction-without-prior-affinity",
                )
                .body(body_string(
                    r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"ordinary request"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ordinary.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(ordinary.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 2);
    let compact = state
        .recent
        .iter()
        .find(|request| request.session_seq == Some(1))
        .expect("compaction route must be recorded");
    assert_eq!(compact.model.as_deref(), Some("grok-4.5-high"));
    assert_eq!(compact.provider.as_deref(), Some("grok"));
    let normal = state
        .recent
        .iter()
        .find(|request| request.session_seq == Some(2))
        .expect("ordinary route must be recorded");
    assert!(
        normal
            .model
            .as_deref()
            .is_some_and(|model| model.starts_with("claude-opus-5"))
    );
    assert_eq!(normal.provider.as_deref(), Some("codex"));
}

#[tokio::test]
async fn codex_compaction_preserves_existing_grok_session_affinity() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let session_id = "codex-compaction-with-grok-affinity";
    let requests = [
        ("grok-4.5", None, "establish explicit Grok affinity"),
        (
            "claude-opus-5",
            Some("gpt-5.6-terra"),
            "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far.",
        ),
        ("claude-opus-5", None, "ordinary alias request"),
    ];

    for (model, compaction_model, content) in requests {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", session_id);
        if let Some(compaction_model) = compaction_model {
            request = request.header("x-ccproxy-compaction-model", compaction_model);
        }
        let response = app
            .clone()
            .oneshot(
                request
                    .body(Body::from(
                        json!({
                            "model": model,
                            "messages": [{"role": "user", "content": content}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{model} response body: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 3);
    let expected = [
        (1, "grok-4.5", "grok"),
        (2, "gpt-5.6-terra", "codex"),
        (3, "claude-opus-5", "grok"),
    ];
    for (session_seq, model, provider) in expected {
        let request = state
            .recent
            .iter()
            .find(|request| request.session_seq == Some(session_seq))
            .unwrap_or_else(|| panic!("session sequence {session_seq} must be recorded"));
        assert_eq!(request.model.as_deref(), Some(model));
        assert_eq!(request.provider.as_deref(), Some(provider));
    }
}

#[tokio::test]
async fn invalid_compaction_override_model_is_rejected() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-ccproxy-compaction-model", "not-a-model")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far."}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
