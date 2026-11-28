use super::*;
use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, State},
    http::HeaderMap,
    routing::post,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

fn context() -> RequestContext {
    RequestContext {
        req_id: "anthropic-test".into(),
        session_id: None,
        session_seq: None,
        provider: "anthropic".into(),
        traffic: None,
        monitor: None,
    }
}

fn request(stream: bool) -> MessagesRequest {
    serde_json::from_value(json!({
        "model": "anthropic/claude-opus-5-5[1m]", "max_tokens": 4096, "stream": stream,
        "system": [{"type": "text", "text": "system", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": "hello"}],
        "thinking": {"type": "adaptive"},
        "tools": [{"name": "test", "input_schema": {"type": "object"}}]
    }))
    .unwrap()
}

async fn upstream(
    State(seen): State<Seen>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    seen.lock()
        .unwrap()
        .push((uri.path().into(), headers, body.clone()));
    if uri.path().ends_with("count_tokens") {
        return Json(json!({"input_tokens": 42})).into_response();
    }
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let chunks = vec![
            Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42}}}\n\n"),
            Bytes::from_static(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"unchanged\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        ];
        let stream = futures_util::stream::unfold(chunks.into_iter(), |mut chunks| async move {
            let chunk = chunks.next()?;
            if chunk.starts_with(b"event: content_block_delta") {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Some((Ok::<_, std::convert::Infallible>(chunk), chunks))
        });
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response();
    }
    Json(json!({
        "id": "msg_native", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "thinking", "thinking": "", "signature": "unchanged"},
            {"type": "tool_use", "id": "toolu_native", "name": "test", "input": {}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 42, "output_tokens": 7}
    }))
    .into_response()
}

async fn mock_provider() -> (AnthropicProvider, Seen, tokio::task::JoinHandle<()>) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/v1/messages", post(upstream))
        .route("/v1/messages/count_tokens", post(upstream))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider = AnthropicProvider {
        client: Some(AnthropicClient::new(url, Some("test-key".into())).unwrap()),
    };
    (provider, seen, task)
}

#[test]
fn preparation_preserves_native_parameters() {
    let body = request(false);
    let (value, model) = prepare_request(&body, false).unwrap();
    assert_eq!(model, "claude-opus-5-5");
    assert_eq!(value["model"], model);
    assert_eq!(value["thinking"], body.extra["thinking"]);
    assert_eq!(value["system"], body.extra["system"]);
    assert_eq!(value["tools"], body.extra["tools"]);
    assert_eq!(value["max_tokens"], 4096);
    assert!(value.get("fallbacks").is_none());
    let (count, _) = prepare_request(&body, true).unwrap();
    assert!(count.get("stream").is_none());
    assert!(count.get("max_tokens").is_none());
    assert_eq!(count["tools"], value["tools"]);
}

#[test]
fn resolves_aliases_and_future_explicit_models_without_fallback() {
    assert_eq!(resolve_model("opus").unwrap(), "claude-opus-5-5");
    assert_eq!(resolve_model("haiku").unwrap(), "claude-haiku-5-5");
    assert_eq!(resolve_model("claude-future-1").unwrap(), "claude-future-1");
    assert!(resolve_model("gpt-6-sol").is_err());
}

#[tokio::test]
async fn native_json_and_token_count_use_real_endpoints() {
    let (provider, seen, task) = mock_provider().await;
    let response = provider.handle_messages(request(false), context()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["content"][0]["signature"], "unchanged");
    assert_eq!(body["content"][1]["type"], "tool_use");
    let response = provider
        .handle_count_tokens(request(false), context())
        .await;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["input_tokens"],
        42
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, "/v1/messages");
    assert_eq!(seen[1].0, "/v1/messages/count_tokens");
    assert_eq!(seen[0].1["x-api-key"], "test-key");
    assert_eq!(seen[0].1["anthropic-version"], client::API_VERSION);
    assert!(seen[0].1.get("authorization").is_none());
    assert_eq!(seen[0].2["model"], "claude-opus-5-5");
    task.abort();
}

#[tokio::test]
async fn streaming_is_live_and_preserves_signatures() {
    let (provider, _, task) = mock_provider().await;
    let generation = provider
        .generate_anthropic_stream(request(true), context())
        .await
        .unwrap();
    let GenerationBody::LiveSse(mut body) = generation.body else {
        panic!("expected live SSE");
    };
    let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
        .await
        .expect("first chunk must arrive before upstream completion")
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(first.data_ref().unwrap()).contains("message_start"));
    let rest = body.collect().await.unwrap().to_bytes();
    let rest = String::from_utf8_lossy(&rest);
    assert!(rest.contains("\"signature\":\"unchanged\""));
    assert!(rest.contains("event: message_stop"));
    task.abort();
}

#[tokio::test]
async fn forwards_only_anthropic_protocol_headers() {
    let (provider, seen, task) = mock_provider().await;
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    headers.insert("anthropic-beta", "context-1m-2025-08-07".parse().unwrap());
    headers.insert("authorization", "Bearer downstream-secret".parse().unwrap());
    headers.insert("x-api-key", "downstream-key".parse().unwrap());
    headers.insert("cookie", "secret-cookie".parse().unwrap());
    let response = provider
        .handle_messages_with_request_headers(request(false), context(), None, headers)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].1["anthropic-beta"], "context-1m-2025-08-07");
    assert_eq!(seen[0].1["x-api-key"], "test-key");
    assert!(seen[0].1.get("authorization").is_none());
    assert!(seen[0].1.get("cookie").is_none());
    task.abort();
}

#[tokio::test]
async fn missing_key_fails_before_network_access() {
    let provider = AnthropicProvider {
        client: Some(AnthropicClient::new("http://127.0.0.1:1".into(), None).unwrap()),
    };
    let response = provider.handle_messages(request(false), context()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn upstream_rate_limit_preserves_retry_after_without_leaking_key() {
    let app = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "9")],
                Json(json!({"error": {"message": "rate limit test-key"}})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider = AnthropicProvider {
        client: Some(AnthropicClient::new(url, Some("test-key".into())).unwrap()),
    };
    let response = provider.handle_messages(request(false), context()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "9");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&bytes).contains("test-key"));
    task.abort();
}
