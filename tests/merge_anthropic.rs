//! HTTP-level tests for the generic Anthropic-compatible (Merge) upstream.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use claude_code_proxy::{registry::Registry, server::app};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::util::ServiceExt;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    let m = ENV_LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn write_merge_auth(config_dir: &std::path::Path) {
    let dir = config_dir.join("merge");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&json!({"access":"test-merge-token"})).unwrap(),
    )
    .unwrap();
}

async fn spawn_json_upstream(
    captured: Arc<Mutex<Option<(String, Option<String>, Value)>>>,
    response_body: Value,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");
    let response_body = Arc::new(response_body);

    let app = axum::Router::new().fallback({
        let captured = captured.clone();
        let response_body = response_body.clone();
        move |req: Request<Body>| {
            let captured = captured.clone();
            let response_body = response_body.clone();
            async move {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let path = req.uri().path().to_string();
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                let json: Value = serde_json::from_slice(&bytes).unwrap_or_default();
                let _ = captured.lock().map(|mut g| *g = Some((path, auth, json)));
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(response_body.to_string()))
                    .unwrap()
            }
        }
    });

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    addr_str
}

async fn call_messages(model: &str, stream: bool) -> axum::response::Response {
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "merge-smoke")
                .body(Body::from(
                    json!({
                        "model": model,
                        "max_tokens": 64,
                        "stream": stream,
                        "messages": [{"role":"user","content":"hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn merge_prefixed_model_forwards_to_anthropic_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_merge_auth(config.path());

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_json_upstream(
        captured.clone(),
        json!({
            "id": "msg_upstream",
            "type": "message",
            "role": "assistant",
            "content": [{"type":"text","text":"merge ok"}],
            "model": "anthropic/claude-sonnet-5",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2}
        }),
    )
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_MERGE_BASE_URL", &upstream);
    let _token_env = EnvGuard::set("CCP_MERGE_AUTH_TOKEN", "env-merge-token");

    let response = call_messages("merge:anthropic/claude-sonnet-5", false).await;
    assert_eq!(response.status(), StatusCode::OK);

    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "merge ok");

    let (path, auth, sent) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(path, "/v1/messages");
    assert_eq!(auth.as_deref(), Some("Bearer env-merge-token"));
    assert_eq!(sent["model"], "anthropic/claude-sonnet-5");
    assert_eq!(sent["stream"], false);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn merge_unknown_catalog_model_returns_400() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_merge_auth(config.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _token_env = EnvGuard::set("CCP_MERGE_AUTH_TOKEN", "env-merge-token");

    // OpenAI-on-Merge is intentionally out of v1 scope.
    let response = call_messages("merge:openai/gpt-5", false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not in the Anthropic-compatible catalog")
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn completely_unknown_model_still_lists_supported() {
    let _guard = env_lock();
    let response = call_messages("totally-unknown-model", false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let message = value["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("Supported:"));
    assert!(message.contains("merge:"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn merge_stream_passthrough_returns_anthropic_sse() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_merge_auth(config.path());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = format!("http://{addr}");
    let captured_model = Arc::new(Mutex::new(None));

    {
        let captured_model = captured_model.clone();
        let app = axum::Router::new().fallback(move |body: String| {
            let captured_model = captured_model.clone();
            async move {
                let json: Value = serde_json::from_str(&body).unwrap_or_default();
                let _ = captured_model
                    .lock()
                    .map(|mut g| *g = json.get("model").and_then(|v| v.as_str()).map(str::to_string));
                let sse = concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"anthropic/claude-haiku-4-5-20251001\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"stream ok\"}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                );
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
    }

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_MERGE_BASE_URL", &upstream);
    let _token_env = EnvGuard::set("CCP_MERGE_AUTH_TOKEN", "env-merge-token");

    let response = call_messages("merge:anthropic/claude-haiku-4-5-20251001", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("stream ok"));
    assert!(text.contains("message_stop"));
    assert_eq!(
        captured_model.lock().unwrap().as_deref(),
        Some("anthropic/claude-haiku-4-5-20251001")
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn merge_defaults_cover_claude_mg_slugs() {
    let registry = Registry::with_default_alias();
    let models = registry.supported_models_for("merge");
    for expected in [
        "merge:anthropic/claude-opus-4-8",
        "merge:anthropic/fable-5",
        "merge:anthropic/claude-sonnet-5",
        "merge:anthropic/claude-haiku-4-5-20251001",
    ] {
        assert!(
            models.iter().any(|m| m == expected),
            "missing catalog entry {expected}; have {models:?}"
        );
    }
}
