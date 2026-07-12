// End-to-end tests for local server health, provider routing, Kimi, Codex HTTP,
// and Codex WebSocket through in-process mock upstreams with isolated auth.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use claude_code_proxy::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_proxy::providers::codex::websocket::clear_codex_websocket_pool_for_tests;
use claude_code_proxy::{registry::Registry, server::app};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::util::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serialize all env-var-mutating tests so they never run concurrently.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    // Recover from a poisoned mutex so a failing test doesn't cascade
    let m = ENV_LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Write a valid auth.json for `provider` under `config_dir`.
fn write_auth(config_dir: &std::path::Path, provider: &str) {
    let dir = config_dir.join(provider);
    std::fs::create_dir_all(&dir).unwrap();
    let expires: i64 = 4102444800000;
    let auth = if provider == "codex" {
        json!({"access":"test-access","refresh":"test-refresh","expires":expires,"account_id":"acct_test"})
    } else {
        json!({"access":"test-access","refresh":"test-refresh","expires":expires,"scope":"openid","userId":"user_test"})
    };
    std::fs::write(dir.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
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

/// Send a minimal `POST /v1/messages` through the in-process app.
async fn call_messages(model: &str) -> Response {
    call_messages_body(json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await
}

async fn call_messages_body(body: Value) -> Response {
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "smoke-session")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn traffic_files(state_dir: &Path) -> Vec<PathBuf> {
    collect_files(
        &state_dir
            .join("claude-code-proxy")
            .join("traffic")
            .join("smoke-session"),
    )
}

fn traffic_file<'a>(files: &'a [PathBuf], suffix: &str) -> &'a Path {
    files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        })
        .map(PathBuf::as_path)
        .unwrap_or_else(|| panic!("missing traffic artifact ending in {suffix}; files={files:?}"))
}

fn traffic_json(files: &[PathBuf], suffix: &str) -> Value {
    serde_json::from_slice(&std::fs::read(traffic_file(files, suffix)).unwrap()).unwrap()
}

/// Spawn a mock axum HTTP server that accepts requests at any path, calls
/// `handler(request_json)` and returns the handler's response body as a 200
/// with `content-type: text/event-stream`.
async fn spawn_http_upstream<F>(handler: F) -> String
where
    F: Fn(Value) -> Vec<u8> + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    let app = axum::Router::new().fallback({
        let handler = handler.clone();
        move |body: String| {
            let handler = handler.clone();
            async move {
                let json: Value = serde_json::from_str(&body).unwrap_or_default();
                let response_bytes = handler(json);
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(response_bytes))
                    .unwrap()
            }
        }
    });

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    addr_str
}

/// Spawn a mock WebSocket server that accepts one connection, captures the
/// first text message, and responds with Codex WebSocket events that
/// accumulate to `"codex websocket ok"`.
async fn spawn_websocket_upstream(captured: Arc<Mutex<Option<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(ws) = tokio_tungstenite::accept_async(stream).await
        {
            let (mut sender, mut receiver) = ws.split();

            // Read the incoming response.create message
            if let Some(Ok(Message::Text(text))) = receiver.next().await
                && let Ok(json) = serde_json::from_str::<Value>(&text)
            {
                let _ = captured.lock().map(|mut g| *g = Some(json));
            }

            // Send Codex Responses events as WebSocket text messages
            let events = [
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.output_text.delta","output_index":0,"delta":"codex websocket ok"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];

            for event in &events {
                let _ = sender.send(Message::Text(event.to_string())).await;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_delayed_terminal_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await
        {
            let _ = ws.next().await;
            let early_events = [
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.output_text.delta","output_index":0,"delta":"early chunk"}"#,
            ];
            for event in &early_events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }

            tokio::time::sleep(Duration::from_secs(2)).await;

            let terminal_events = [
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];
            for event in &terminal_events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_error_upstream(message: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await
        {
            let _ = ws.next().await;
            let event = json!({
                "type": "error",
                "status": 400,
                "error": {
                    "type": "invalid_request_error",
                    "param": "input",
                    "message": message
                }
            });
            let _ = ws.send(Message::Text(event.to_string())).await;
        }
    });

    addr_str
}

async fn spawn_websocket_sequence_upstream(captured: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let texts = ["first", "second", "third"];
        let mut handled = 0usize;
        while handled < texts.len() {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < texts.len() {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                let idx = handled;
                let response_text = texts[idx];
                let response_id = format!("resp_{}", idx + 1);
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_up_{idx}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_previous_missing_then_retry_upstream(
    captured: Arc<Mutex<Vec<Value>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let mut handled = 0usize;
        while handled < 3 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < 3 {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                if handled == 1 {
                    let event = json!({
                        "type": "error",
                        "error": {
                            "code": "previous_response_not_found",
                            "message": "previous response not found",
                            "status": 400
                        }
                    });
                    let _ = sender.send(Message::Text(event.to_string())).await;
                    handled += 1;
                    break;
                }

                let response_text = if handled == 0 { "first" } else { "retry" };
                let response_id = if handled == 0 { "resp_1" } else { "resp_retry" };
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_up_{handled}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_close_then_retry_upstream(captured: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let mut handled = 0usize;
        while handled < 3 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < 3 {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                if handled == 1 {
                    handled += 1;
                    let _ = sender.close().await;
                    break;
                }

                let response_text = if handled == 0 { "first" } else { "retry" };
                let response_id = if handled == 0 { "resp_1" } else { "resp_retry" };
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_close_{handled}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

// ---------------------------------------------------------------------------
// Health and routing smoke tests (no env var mutation needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_healthz_returns_ok() {
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
async fn smoke_codex_model_routes_to_real_provider() {
    let _guard = env_lock();
    let response = call_messages("gpt-5.5").await;
    // Should attempt auth (not return 501 placeholder)
    assert!(
        response.status() != StatusCode::NOT_IMPLEMENTED,
        "codex models must resolve to the real provider, not a placeholder"
    );
}

#[test]
fn smoke_kimi_model_is_registered() {
    // Kimi uses reqwest::blocking::Client internally, which panics when
    // dropped from an async context (it joins a dedicated runtime thread).
    // Test routing at the Registry level instead of through the HTTP stack.
    let registry = Registry::with_default_alias();
    let provider = registry.provider_for_model("kimi-for-coding", None);
    assert!(
        provider.is_some(),
        "kimi-for-coding must resolve to a registered provider"
    );
    assert_eq!(
        provider.unwrap().name(),
        "kimi",
        "kimi-for-coding must route to the kimi provider"
    );
}

// ---------------------------------------------------------------------------
// Kimi smoke: mock upstream verifies request shape and returns a valid
// streaming response. Uses multi-thread runtime because KimiHttpClient uses
// reqwest::blocking::Client internally.
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_kimi_messages_uses_mock_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "kimi");

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut g| *g = Some(body));
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"kimi ok\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_KIMI_BASE_URL", &upstream);
    let response = call_messages("kimi-for-coding").await;

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "kimi ok");

    let sent = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "kimi-for-coding");
    assert_eq!(sent["stream"], true);
}

// ---------------------------------------------------------------------------
// Codex HTTP smoke: mock upstream verifies request shape and returns
// Responses SSE events.
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_messages_uses_mock_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut g| *g = Some(body));
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"codex http ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "codex http ok");

    let sent = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "gpt-5.5");
    assert_eq!(sent["stream"], true);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_context_window_error_requests_compaction() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"input exceeds context window\"}}}\n\n"
            .as_bytes()
            .to_vec()
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "request_too_large");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("input exceeds context window")),
        "response body: {}",
        String::from_utf8_lossy(&body)
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_traffic_capture_writes_upstream_artifacts() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"codex http ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
        )
        .as_bytes()
        .to_vec()
    })
    .await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::OK);
    let files = traffic_files(state.path());
    let request = traffic_json(&files, "020-upstream-request.json");
    assert_eq!(request["model"], "gpt-5.5");

    let metadata = traffic_json(&files, "021-upstream-request-metadata.json");
    assert_eq!(metadata["transport"], "http");
    assert!(
        metadata["headers"]["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    assert_eq!(
        traffic_json(&files, "030-upstream-response-headers.json")["status"],
        200
    );
    traffic_file(&files, "032-upstream-response-body.sse");
    traffic_file(&files, "040-upstream-event.json");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_stream_traffic_captures_downstream_events() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"codex stream ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
        )
        .as_bytes()
        .to_vec()
    })
    .await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("message_stop"), "stream body: {text}");

    let files = traffic_files(state.path());
    let downstream = traffic_json(&files, "050-downstream-event.json");
    assert!(downstream.get("event").is_some());
    assert!(downstream.get("data").is_some());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_truncated_upstream_writes_reducer_diagnostic() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n"
        )
        .as_bytes()
        .to_vec()
    })
    .await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let files = traffic_files(state.path());
    let diagnostic = traffic_json(&files, "060-codex-reducer-error.json");
    assert_eq!(diagnostic["kind"], "Transient");
    assert_eq!(
        diagnostic["diagnostics"]["saw_terminal_event"],
        Value::Bool(false)
    );
}

// ---------------------------------------------------------------------------
// Codex WebSocket smoke: mock upstream verifies request shape and returns
// Responses events over WebSocket.
// ---------------------------------------------------------------------------

// Multi-threaded runtime so the spawned accept task runs independently and
// the listener is registered with the I/O driver before connect_async starts.
// A single-threaded runtime risks the root task (connect_async) outpacing the
// spawned accept task, causing connection-refused races.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_messages_uses_mock_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_websocket_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let response = call_messages("gpt-5.5").await;

    let ws_status = response.status();
    let ws_body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if ws_status != StatusCode::OK {
        panic!(
            "WS: expected 200, got {}: {}",
            ws_status,
            String::from_utf8_lossy(&ws_body_bytes)
        );
    }
    let value: Value = serde_json::from_slice(&ws_body_bytes).unwrap();
    assert_eq!(value["content"][0]["text"], "codex websocket ok");

    let guard = captured.lock().unwrap();
    let sent = guard.clone().unwrap_or_else(|| {
        panic!(
            "WS mock did not capture a request. Response body: {}",
            String::from_utf8_lossy(&ws_body_bytes)
        );
    });
    assert_eq!(sent["type"], "response.create");
    assert_eq!(sent["model"], "gpt-5.5");
    assert!(sent.get("max_output_tokens").is_none());
    assert!(sent.get("stream").is_none());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_returns_delta_before_terminal() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();

    let upstream = spawn_websocket_delayed_terminal_upstream().await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = tokio::time::timeout(
        Duration::from_millis(500),
        call_messages_body(json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        })),
    )
    .await
    .expect("streaming response should start before terminal upstream event");
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    let mut collected = Vec::new();
    let read = tokio::time::timeout(Duration::from_millis(500), async {
        while !String::from_utf8_lossy(&collected).contains("text_delta") {
            let Some(frame) = body.frame().await else {
                break;
            };
            let frame = frame.unwrap();
            if let Ok(data) = frame.into_data() {
                collected.extend_from_slice(&data);
            }
        }
    })
    .await;
    assert!(read.is_ok(), "stream did not yield an early text delta");
    let text = String::from_utf8_lossy(&collected);
    assert!(text.contains("early chunk"), "stream body: {text}");
    assert!(
        !text.contains("message_stop"),
        "stream finished too early: {text}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_context_window_error_requests_compaction() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();

    let upstream = spawn_websocket_error_upstream("input exceeds context window").await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "response body: {}",
        String::from_utf8_lossy(&body)
    );
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "request_too_large");
    assert_eq!(value["error"]["message"], "input exceeds context window");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_uses_previous_response_id() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_sequence_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("second"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 2, "expected two upstream websocket requests");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert_eq!(
        guard[1]["input"].as_array().map(Vec::len),
        Some(1),
        "second request should send only the appended input delta"
    );
    assert_eq!(guard[1]["input"][0]["role"], "user");
    assert_eq!(guard[1]["input"][0]["content"][0]["text"], "two");

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_retries_missing_previous_response_id() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_previous_missing_then_retry_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("retry"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected retry websocket request");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert!(guard[2].get("previous_response_id").is_none());
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(3),
        "retry request should send the full input"
    );

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_retries_empty_close_with_full_context() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_close_then_retry_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("retry"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected full-context retry request");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert!(guard[2].get("previous_response_id").is_none());
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(3),
        "retry request should send the full input"
    );

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_previous_response_id_sends_delta_on_second_turn() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_sequence_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    let second_status = second.status();
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        second_status,
        StatusCode::OK,
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );
    let value: Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(value["content"][0]["text"], "second");

    let third = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"},
            {"role":"assistant","content":"second"},
            {"role":"user","content":"three"}
        ]
    }))
    .await;
    let third_status = third.status();
    let third_body = axum::body::to_bytes(third.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        third_status,
        StatusCode::OK,
        "third response body: {}",
        String::from_utf8_lossy(&third_body)
    );
    let value: Value = serde_json::from_slice(&third_body).unwrap();
    assert_eq!(value["content"][0]["text"], "third");

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected three upstream websocket requests");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert_eq!(
        guard[1]["input"].as_array().map(Vec::len),
        Some(1),
        "second request should send only the appended input delta"
    );
    assert_eq!(guard[1]["input"][0]["role"], "user");
    assert_eq!(guard[1]["input"][0]["content"][0]["text"], "two");
    assert_eq!(guard[2]["previous_response_id"], "resp_2");
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(1),
        "third request should keep reusing the pooled websocket continuation"
    );
    assert_eq!(guard[2]["input"][0]["role"], "user");
    assert_eq!(guard[2]["input"][0]["content"][0]["text"], "three");

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_traffic_capture_writes_upstream_artifacts() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_websocket_upstream(captured.clone()).await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::OK);
    let files = traffic_files(state.path());
    let request = traffic_json(&files, "020-upstream-request.json");
    assert_eq!(request["type"], "response.create");
    assert!(request.get("stream").is_none());

    let metadata = traffic_json(&files, "021-upstream-request-metadata.json");
    assert_eq!(metadata["transport"], "websocket");
    assert!(
        metadata["headers"]["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    traffic_file(&files, "022-upstream-websocket-metadata.json");
    assert_eq!(
        traffic_json(&files, "030-upstream-response-headers.json")["status"],
        200
    );
    traffic_file(&files, "032-upstream-response-body.sse");
    traffic_file(&files, "040-upstream-event.json");
}
