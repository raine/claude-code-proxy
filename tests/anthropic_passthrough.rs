use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::post,
};
use claude_code_proxy::{
    config::AliasProvider,
    monitor::{MonitorHandle, RequestStatus},
    providers::anthropic::AnthropicProvider,
    registry::Registry,
    server::app_with_monitor,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use tower::ServiceExt;

struct Upstream {
    url: reqwest::Url,
    task: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn upstream(app: Router) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream { url, task }
}

fn proxy(upstream: &Upstream, monitor: &MonitorHandle) -> Router {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let provider: Arc<dyn claude_code_proxy::provider::Provider> =
        Arc::new(AnthropicProvider::with_client(client, upstream.url.clone()));
    let registry = Arc::new(Registry::from_providers(
        AliasProvider::Anthropic,
        [provider],
    ));
    app_with_monitor(registry, Some(monitor.clone()))
}

fn request(path: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .header("authorization", "Bearer claude-subscription")
        .header(
            "anthropic-beta",
            "oauth-2025-04-20,prompt-caching-2024-07-31",
        )
        .header("anthropic-version", "2023-06-01")
        .body(body.into())
        .unwrap()
}

#[tokio::test]
async fn relays_native_body_credentials_query_and_json_usage() {
    let captured = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    let upstream = upstream(Router::new().route("/v1/messages", post(move |req: Request<Body>| {
        let sink = sink.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            *sink.lock().await = Some((parts, body));
            ([("request-id", "req_anthropic"), ("content-type", "application/json")],
             r#"{"type":"message","usage":{"input_tokens":17,"output_tokens":8},"content":[{"type":"thinking","signature":"native","thinking":"summary"}]}"#)
        }
    }))).await;
    let monitor = MonitorHandle::default();
    let raw = r#"{ "model":"claude-opus-5", "messages":[], "future_field":{"enabled":true} }"#;
    let response = proxy(&upstream, &monitor)
        .oneshot(request("/v1/messages?beta=true", raw))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["request-id"], "req_anthropic");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        std::str::from_utf8(&bytes)
            .unwrap()
            .contains("\"signature\":\"native\"")
    );
    let captured = captured.lock().await;
    let (parts, body) = captured.as_ref().unwrap();
    assert_eq!(body, raw.as_bytes());
    assert_eq!(parts.uri.query(), Some("beta=true"));
    assert_eq!(parts.headers["authorization"], "Bearer claude-subscription");
    assert_eq!(parts.headers["anthropic-version"], "2023-06-01");
    assert!(
        parts.headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("oauth")
    );
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].provider.as_deref(), Some("anthropic"));
    assert_eq!(snapshot.recent[0].input_tokens, Some(17));
    assert_eq!(snapshot.recent[0].output_tokens, Some(8));
    assert_eq!(snapshot.recent[0].status, RequestStatus::Completed);
}

#[tokio::test]
async fn streams_before_completion_and_observes_fragmented_usage() {
    let release = Arc::new(Notify::new());
    let released = release.clone();
    let upstream = upstream(Router::new().route("/v1/messages", post(move || {
        let released = released.clone();
        async move {
            let chunks = futures_util::stream::unfold(0, move |stage| {
                let released = released.clone();
                async move {
                    let data = match stage {
                        0 => "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\nevent: message_delta\nda",
                        1 => { released.notified().await; "ta: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n" },
                        _ => return None,
                    };
                    Some((Ok::<_, Infallible>(Bytes::from_static(data.as_bytes())), stage + 1))
                }
            });
            ([("content-type", "text/event-stream")], Body::from_stream(chunks))
        }
    }))).await;
    let monitor = MonitorHandle::default();
    let response = proxy(&upstream, &monitor)
        .oneshot(request(
            "/v1/messages",
            r#"{"model":"claude-opus-5","stream":true,"messages":[]}"#,
        ))
        .await
        .unwrap();
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert!(first.starts_with(b"event: message_start"));
    assert_eq!(monitor.snapshot().active[0].input_tokens, Some(7));
    release.notify_one();
    let rest = body.collect().await.unwrap().to_bytes();
    assert!(rest.ends_with(b"data: {\"type\":\"message_stop\"}\n\n"));
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].output_tokens, Some(9));
    assert_eq!(snapshot.recent[0].status, RequestStatus::Completed);
}

#[tokio::test]
async fn token_counting_applies_the_same_model_and_thinking_conversion() {
    let upstream = upstream(Router::new().route("/v1/messages/count_tokens", post(|headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
        assert_eq!(headers["authorization"], "Bearer claude-subscription");
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["messages"][0]["content"][0], json!({"type":"text","text":"<previous_reasoning>\nGPT summary\n</previous_reasoning>"}));
        axum::Json(json!({"input_tokens":42}))
    }))).await;
    let monitor = MonitorHandle::default();
    let body = json!({"model":"opus[1m]","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"GPT summary","signature":"ccp:codex:v1:cnNfMQ:ciphertext"}]}]});
    let response = proxy(&upstream, &monitor)
        .oneshot(request(
            "/v1/messages/count_tokens?beta=true",
            body.to_string(),
        ))
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"input_tokens":42})
    );
    assert_eq!(monitor.snapshot().recent[0].input_tokens, Some(42));
}

#[tokio::test]
async fn preserves_upstream_errors_and_does_not_follow_redirects() {
    for (status, body) in [
        (
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"quota"}}"#,
        ),
        (StatusCode::TEMPORARY_REDIRECT, "redirect"),
    ] {
        let upstream = upstream(Router::new().route(
            "/v1/messages",
            post(move || async move {
                (
                    status,
                    [
                        ("retry-after", "13"),
                        ("location", "http://127.0.0.1:1/never"),
                        ("request-id", "real-id"),
                    ],
                    body,
                )
                    .into_response()
            }),
        ))
        .await;
        let monitor = MonitorHandle::default();
        let response = proxy(&upstream, &monitor)
            .oneshot(request(
                "/v1/messages",
                r#"{"model":"claude-opus-5","messages":[]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["retry-after"], "13");
        assert_eq!(response.headers()["request-id"], "real-id");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body.as_bytes()
        );
    }
}

#[tokio::test]
async fn streamed_errors_remain_verbatim_and_fail_the_dashboard_request() {
    let raw = "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n";
    let upstream = upstream(Router::new().route(
        "/v1/messages",
        post(move || async move { ([("content-type", "text/event-stream")], raw) }),
    ))
    .await;
    let monitor = MonitorHandle::default();
    let response = proxy(&upstream, &monitor)
        .oneshot(request(
            "/v1/messages",
            r#"{"model":"claude-opus-5","messages":[],"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        raw.as_bytes()
    );
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].status, RequestStatus::Failed);
    assert_eq!(snapshot.recent[0].error.as_deref(), Some("overloaded"));
}
