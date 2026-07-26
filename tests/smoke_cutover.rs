// End-to-end tests for local server health, Codex/Grok routing, Codex HTTP,
// and Codex WebSocket through in-process mock upstreams with isolated auth.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use claude_code_proxy::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_proxy::providers::codex::websocket::clear_codex_websocket_pool_for_tests;
use claude_code_proxy::providers::grok::auth::login::{CANONICAL_ISSUER, CLIENT_ID};
use claude_code_proxy::{registry::Registry, server::app};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::util::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// Serialize all env-var-mutating tests so they never run concurrently.
async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Write a valid auth.json for `provider` under `config_dir`.
fn write_auth(config_dir: &std::path::Path, provider: &str) {
    let dir = config_dir.join(provider);
    std::fs::create_dir_all(&dir).unwrap();
    let expires: i64 = 4102444800000;
    let auth = match provider {
        "codex" => {
            json!({"access":"test-access","refresh":"test-refresh","expires":expires,"account_id":"acct_test"})
        }
        "grok" => json!({
            "access": "test-access",
            "refresh": "test-refresh",
            "expires_at_ms": expires,
            "issuer": CANONICAL_ISSUER,
            "client_id": CLIENT_ID
        }),
        other => panic!("unsupported auth fixture provider: {other}"),
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
        .oneshot(messages_request(body))
        .await
        .unwrap()
}

fn messages_request(body: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "smoke-session")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn structured_messages_body(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role":"user","content":"return status"}],
        "output_config": {"format": {
            "type": "json_schema",
            "schema": {
                "type": "object",
                "properties": {
                    "status": {"type":"string","const":"ok"},
                    "note": {"type":"string"}
                },
                "required": ["status"],
                "additionalProperties": false
            }
        }}
    })
}

fn incomplete_tool_messages_body(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"inspect"}],
        "tools": [{
            "name": "Workflow",
            "description": "Run a workflow",
            "input_schema": {
                "type":"object",
                "properties":{"script":{"type":"string"}},
                "required":["script"],
                "additionalProperties":false
            }
        }]
    })
}

async fn answer_terminal_barrier<S, R>(sender: &mut S, receiver: &mut R)
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    R: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match receiver.next().await {
                Some(Ok(Message::Ping(payload))) => {
                    sender.send(Message::Pong(payload)).await.unwrap();
                    return;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(other)) => {
                    panic!("expected terminal ordering-barrier Ping, got {other:?}")
                }
                Some(Err(error)) => {
                    panic!("terminal ordering-barrier receive failed: {error}")
                }
                None => panic!("connection closed before terminal ordering-barrier Ping"),
            }
        }
    })
    .await
    .expect("terminal ordering-barrier Ping should arrive");
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

/// Return SSE headers but never a first body byte, then keep accepting long
/// enough to detect whether the proxy unsafely replays the completed POST.
async fn spawn_http_first_byte_stall_upstream() -> (String, tokio::task::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 16 * 1024];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();

        let mut dispatches = 1;
        if let Ok(Ok((_stream, _))) =
            tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
        {
            dispatches += 1;
        }
        dispatches
    });

    (addr_str, task)
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
                r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_up","delta":"codex websocket ok"}"#,
                r#"{"type":"response.output_text.done","output_index":0,"item_id":"msg_up","text":"codex websocket ok"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];

            for event in &events {
                let _ = sender.send(Message::Text(event.to_string())).await;
            }
            answer_terminal_barrier(&mut sender, &mut receiver).await;
        }
    });

    addr_str
}

async fn spawn_websocket_structured_upstream(captured: Arc<Mutex<Option<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(ws) = tokio_tungstenite::accept_async(stream).await
        {
            let (mut sender, mut receiver) = ws.split();
            if let Some(Ok(Message::Text(text))) = receiver.next().await
                && let Ok(json) = serde_json::from_str::<Value>(&text)
            {
                let _ = captured.lock().map(|mut guard| *guard = Some(json));
            }

            let output = r#"{"status":"ok","note":null}"#;
            for event in [
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"message","id":"msg_structured"}
                }),
                json!({
                    "type":"response.output_text.delta",
                    "output_index":0,
                    "item_id":"msg_structured",
                    "delta":output
                }),
                json!({
                    "type":"response.output_text.done",
                    "output_index":0,
                    "item_id":"msg_structured",
                    "text":output
                }),
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{"type":"message","id":"msg_structured"}
                }),
                json!({
                    "type":"response.completed",
                    "response":{"id":"resp_structured","usage":{"input_tokens":5,"output_tokens":2}}
                }),
            ] {
                sender.send(Message::Text(event.to_string())).await.unwrap();
            }
            answer_terminal_barrier(&mut sender, &mut receiver).await;
        }
    });

    addr_str
}

async fn spawn_websocket_incomplete_tool_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = ws.next().await;
        for event in [
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"function_call","call_id":"call_1","name":"Workflow"}
            }),
            json!({
                "type":"response.function_call_arguments.delta",
                "output_index":0,
                "delta":"{\"script\":\"inspect\"}"
            }),
        ] {
            ws.send(Message::Text(event.to_string())).await.unwrap();
        }
        drop(ws);
    });

    addr_str
}

/// Emit one semantic text delta, then a malformed JSON frame and a nominal
/// success terminal. Keep listening long enough to detect an unsafe replay.
async fn spawn_websocket_malformed_after_text_upstream() -> (String, tokio::task::JoinHandle<usize>)
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = ws.next().await;
        for event in [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_up","delta":"partial before malformed"}"#,
        ] {
            ws.send(Message::Text(event.to_string())).await.unwrap();
        }
        ws.send(Message::Text(
            r#"{"type":"response.output_text.delta","delta":"unterminated""#.to_string(),
        ))
        .await
        .unwrap();
        let _ = ws
            .send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"must_not_complete","usage":{}}}"#
                    .to_string(),
            ))
            .await;
        drop(ws);

        let mut dispatches = 1;
        if let Ok(Ok((_stream, _))) =
            tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
        {
            dispatches += 1;
        }
        dispatches
    });

    (addr_str, task)
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
                r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_up","delta":"early chunk"}"#,
            ];
            for event in &early_events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }

            tokio::time::sleep(Duration::from_secs(2)).await;

            let terminal_events = [
                r#"{"type":"response.output_text.done","output_index":0,"item_id":"msg_up","text":"early chunk"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];
            for event in &terminal_events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }
            let (mut sender, mut receiver) = ws.split();
            answer_terminal_barrier(&mut sender, &mut receiver).await;
        }
    });

    addr_str
}

async fn spawn_websocket_rejection_then_http_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let (mut websocket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 16 * 1024];
        let read = websocket.read(&mut request).await.unwrap();
        assert!(read > 0);
        assert!(
            String::from_utf8_lossy(&request[..read])
                .to_ascii_lowercase()
                .contains("upgrade: websocket")
        );
        websocket
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        drop(websocket);

        let (mut http, _) = listener.accept().await.unwrap();
        let read = http.read(&mut request).await.unwrap();
        assert!(read > 0);
        assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"auto http fallback\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"auto http fallback\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        http.write_all(response.as_bytes()).await.unwrap();
        http.write_all(body.as_bytes()).await.unwrap();
    });

    addr_str
}

async fn spawn_non_retryable_websocket_rejection_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let (mut websocket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 16 * 1024];
        assert!(websocket.read(&mut request).await.unwrap() > 0);
        websocket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 11\r\nconnection: close\r\n\r\nunsupported",
            )
            .await
            .unwrap();
    });

    addr_str
}

async fn spawn_websocket_error_upstream(message: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(ws) = tokio_tungstenite::accept_async(stream).await
        {
            let (mut sender, mut receiver) = ws.split();
            let _ = receiver.next().await;
            let event = json!({
                "type": "error",
                "status": 400,
                "error": {
                    "type": "invalid_request_error",
                    "param": "input",
                    "message": message
                }
            });
            let _ = sender.send(Message::Text(event.to_string())).await;
            answer_terminal_barrier(&mut sender, &mut receiver).await;
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
                let message_id = format!("msg_up_{idx}");
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":message_id}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "item_id":message_id,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_text.done",
                        "output_index":0,
                        "item_id":message_id,
                        "text":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message","id":message_id}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                answer_terminal_barrier(&mut sender, &mut receiver).await;
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
                    let rate_limits = json!({
                        "type": "codex.rate_limits",
                        "rate_limits": {"limit_reached": false}
                    });
                    let _ = sender.send(Message::Text(rate_limits.to_string())).await;
                    let event = json!({
                        "type": "error",
                        "error": {
                            "code": "previous_response_not_found",
                            "message": "previous response not found",
                            "status": 400
                        }
                    });
                    let _ = sender.send(Message::Text(event.to_string())).await;
                    // `previous_response_not_found` is intercepted before the generic
                    // terminal barrier so the request can be retried with full input.
                    handled += 1;
                    break;
                }

                let response_text = if handled == 0 { "first" } else { "retry" };
                let response_id = if handled == 0 { "resp_1" } else { "resp_retry" };
                let message_id = format!("msg_up_{handled}");
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":message_id}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "item_id":message_id,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_text.done",
                        "output_index":0,
                        "item_id":message_id,
                        "text":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message","id":message_id}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                answer_terminal_barrier(&mut sender, &mut receiver).await;
                handled += 1;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_close_after_second_request_upstream(
    captured: Arc<Mutex<Vec<Value>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let mut handled = 0usize;
        while handled < 2 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < 2 {
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
                let message_id = format!("msg_close_{handled}");
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":message_id}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "item_id":message_id,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_text.done",
                        "output_index":0,
                        "item_id":message_id,
                        "text":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message","id":message_id}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                answer_terminal_barrier(&mut sender, &mut receiver).await;
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
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", "http://127.0.0.1:9/backend-api/codex");
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;
    // Should attempt auth (not return 501 placeholder)
    assert!(
        response.status() != StatusCode::NOT_IMPLEMENTED,
        "codex models must resolve to the real provider, not a placeholder"
    );
}

// ---------------------------------------------------------------------------
// Codex HTTP smoke: mock upstream verifies request shape and returns
// Responses SSE events.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_codex_http_messages_uses_mock_upstream() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut g| *g = Some(body));
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"codex http ok\"}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"codex http ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
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
    assert!(sent.get("max_output_tokens").is_none());
    assert!(sent.get("max_tokens").is_none());
    assert!(sent.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn smoke_codex_http_structured_output_projects_wire_schema_and_elides_synthetic_null() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut guard| *guard = Some(body));
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_structured\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_structured\",\"delta\":\"{\\\"status\\\":\\\"ok\\\",\\\"note\\\":null}\"}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_structured\",\"text\":\"{\\\"status\\\":\\\"ok\\\",\\\"note\\\":null}\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_structured\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_structured\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(structured_messages_body("gpt-5.6-sol")).await;

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], r#"{"status":"ok"}"#);

    let sent = captured.lock().unwrap().clone().unwrap();
    let schema = &sent["text"]["format"]["schema"];
    assert_eq!(schema["required"], json!(["note", "status"]));
    assert_eq!(
        schema["properties"]["note"]["anyOf"][1],
        json!({"type":"null"})
    );
}

#[tokio::test]
async fn smoke_codex_http_incomplete_tool_call_fails_closed_unless_unsafe_opt_in() {
    let _guard = env_lock().await;
    for unsafe_salvage in [false, true] {
        let config = TempDir::new().unwrap();
        write_auth(config.path(), "codex");
        let upstream = spawn_http_upstream(|_body: Value| {
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"Workflow\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"script\\\":\\\"inspect\\\"}\"}\n\n"
            )
            .as_bytes()
            .to_vec()
        })
        .await;

        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
        let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
        let _unsafe_env = EnvGuard::set(
            "CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE",
            if unsafe_salvage { "true" } else { "false" },
        );
        let response = call_messages_body(incomplete_tool_messages_body("gpt-5.6-sol")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        if unsafe_salvage {
            assert!(body.contains("input_json_delta"), "{body}");
            assert!(body.contains(r#""stop_reason":"tool_use""#), "{body}");
            assert!(body.contains("event: message_stop"), "{body}");
            assert!(!body.contains("event: error"), "{body}");
        } else {
            assert!(body.contains("event: error"), "{body}");
            assert!(!body.contains("input_json_delta"), "{body}");
            assert!(!body.contains("event: message_stop"), "{body}");
        }
    }
}

#[tokio::test]
async fn smoke_codex_zero_max_tokens_is_rejected_without_upstream_dispatch() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let upstream_calls = upstream_calls.clone();
        move |_body: Value| {
            upstream_calls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 0,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("cache-only"),
        "unexpected error body: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        upstream_calls.load(Ordering::SeqCst),
        0,
        "max_tokens=0 must fail before Codex dispatch"
    );
}

#[tokio::test]
async fn smoke_explicit_wss_transport_fails_fast_when_environment_proxy_would_intercept() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set(
        "CCP_CODEX_BASE_URL",
        "wss://proxy-preflight.invalid/backend-api/codex/responses",
    );
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _proxy_env = EnvGuard::set("HTTPS_PROXY", "http://127.0.0.1:18080");
    let _proxy_lower_env = EnvGuard::set("https_proxy", "http://127.0.0.1:18080");
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "");
    let _no_proxy_lower_env = EnvGuard::set("no_proxy", "");

    let response = call_messages("gpt-5.6-sol").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("environment proxy"), "{body}");
    assert!(body.contains("NO_PROXY"), "{body}");
}

#[tokio::test]
async fn smoke_codex_incompatible_explicit_strict_tool_never_dispatches() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let upstream_calls = upstream_calls.clone();
        move |_body: Value| {
            upstream_calls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}],
        "tools": [{
            "name": "Lookup",
            "description": "Look up a value",
            "strict": true,
            "input_schema": {
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }
        }]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("strict=true"),
        "unexpected error body: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        upstream_calls.load(Ordering::SeqCst),
        0,
        "an incompatible explicit strict schema must fail before Codex dispatch"
    );
}

#[tokio::test]
async fn smoke_grok_zero_max_tokens_is_rejected_without_upstream_dispatch() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "grok");

    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let upstream_calls = upstream_calls.clone();
        move |_body: Value| {
            upstream_calls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_GROK_BASE_URL", &upstream);
    let response = call_messages_body(json!({
        "model": "grok-4.5",
        "max_tokens": 0,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("cache-only"),
        "unexpected error body: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        upstream_calls.load(Ordering::SeqCst),
        0,
        "max_tokens=0 must fail before Grok dispatch"
    );
}

#[tokio::test]
async fn smoke_opus_5_uses_grok_session_affinity() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "grok");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            captured.lock().unwrap().push(body);
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"grok affinity ok\"}\n\n",
                "data: {\"type\":\"response.output_text.done\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_GROK_BASE_URL", &upstream);
    let app = app(Arc::new(Registry::with_default_alias()));
    for model in ["grok-4.5", "claude-opus-5"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .header("x-claude-code-session-id", "smoke-opus-5-grok-affinity")
                    .body(Body::from(
                        json!({
                            "model": model,
                            "max_tokens": 64,
                            "messages": [{"role":"user","content":"hello"}]
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

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["model"], "grok-4.5");
    assert_eq!(captured[1]["model"], "grok-4.5");
}

#[tokio::test]
async fn smoke_co_marker_promotes_codex_xhigh_to_wire_max() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut guard| *guard = Some(body));
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"wire max ok\"}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"wire max ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
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
    let response = app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-ccproxy-codex-xhigh-as-max", "1")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.6-sol",
                        "max_tokens": 64,
                        "messages": [{"role": "user", "content": "hello"}],
                        "output_config": {"effort": "xhigh"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "wire max ok");

    let sent = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "gpt-5.6-sol");
    assert_eq!(sent["reasoning"]["effort"], "max");
}

#[tokio::test]
async fn smoke_codex_http_nonstream_keeps_completed_response_after_rate_limit_telemetry() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"completed answer\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"completed answer\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
            "data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":60}}}\n\n"
        )
        .as_bytes()
        .to_vec()
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
    assert_eq!(value["content"][0]["text"], "completed answer");
    assert_eq!(value["stop_reason"], "end_turn");
    assert_eq!(value["usage"]["input_tokens"], 5);
    assert_eq!(value["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn smoke_codex_http_context_window_error_requests_compaction() {
    let _guard = env_lock().await;
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

#[tokio::test]
async fn smoke_codex_http_traffic_capture_writes_upstream_artifacts() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"codex http ok\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"codex http ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
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

#[tokio::test]
async fn smoke_codex_http_stream_traffic_captures_downstream_events() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"codex stream ok\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"codex stream ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
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

#[tokio::test]
async fn smoke_codex_http_truncated_upstream_writes_reducer_diagnostic() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_up\",\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_up\",\"text\":\"partial\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n"
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
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_messages_uses_mock_upstream() {
    let _guard = env_lock().await;
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
    assert!(sent.get("max_tokens").is_none());
    assert!(sent.get("max_completion_tokens").is_none());
    assert!(sent.get("stream").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_structured_output_projects_wire_schema_and_elides_synthetic_null() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_websocket_structured_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let response = call_messages_body(structured_messages_body("gpt-5.6-sol")).await;

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["content"][0]["text"], r#"{"status":"ok"}"#);

    let sent = captured.lock().unwrap().clone().unwrap();
    let schema = &sent["text"]["format"]["schema"];
    assert_eq!(schema["required"], json!(["note", "status"]));
    assert_eq!(
        schema["properties"]["note"]["anyOf"][1],
        json!({"type":"null"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_incomplete_tool_call_fails_closed_unless_unsafe_opt_in() {
    let _guard = env_lock().await;
    for unsafe_salvage in [false, true] {
        let config = TempDir::new().unwrap();
        write_auth(config.path(), "codex");
        clear_codex_websocket_pool_for_tests();
        let upstream = spawn_websocket_incomplete_tool_upstream().await;

        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
        let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
        let _unsafe_env = EnvGuard::set(
            "CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE",
            if unsafe_salvage { "true" } else { "false" },
        );
        let response = call_messages_body(incomplete_tool_messages_body("gpt-5.6-sol")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        if unsafe_salvage {
            assert!(body.contains("input_json_delta"), "{body}");
            assert!(body.contains(r#""stop_reason":"tool_use""#), "{body}");
            assert!(body.contains("event: message_stop"), "{body}");
            assert!(!body.contains("event: error"), "{body}");
        } else {
            assert!(body.contains("event: error"), "{body}");
            assert!(!body.contains("input_json_delta"), "{body}");
            assert!(!body.contains("event: message_stop"), "{body}");
        }
        clear_codex_websocket_pool_for_tests();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_malformed_json_fails_in_band_without_replay() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();

    let (upstream, server) = spawn_websocket_malformed_after_text_upstream().await;
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
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("malformed upstream JSON should terminate the downstream stream")
    .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();

    assert!(body.contains("partial before malformed"), "{body}");
    assert!(body.contains("event: error"), "{body}");
    assert!(body.contains("api_error"), "{body}");
    assert!(body.contains("websocket_malformed_event"), "{body}");
    assert!(body.contains("event: content_block_stop"), "{body}");
    assert!(
        body.rfind("event: content_block_stop").unwrap() < body.find("event: error").unwrap(),
        "{body}"
    );
    assert!(!body.contains("event: message_stop"), "{body}");
    assert_eq!(server.await.unwrap(), 1, "malformed JSON must not replay");

    clear_codex_websocket_pool_for_tests();
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_http_first_byte_timeout_does_not_replay_post() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");

    let (upstream, server) = spawn_http_first_byte_stall_upstream().await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _first_byte_env = EnvGuard::set("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS", "50");

    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;
    let status = response.status();
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("first-byte timeout should terminate the downstream stream")
    .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("api_error"), "{body}");
    assert!(body.contains("http_response_first_byte"), "{body}");
    assert!(!body.contains("event: message_stop"), "{body}");
    assert_eq!(server.await.unwrap(), 1, "a timed-out POST must not replay");
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_and_auto_stream_return_delta_before_terminal() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

    for transport in ["websocket", "auto"] {
        clear_codex_websocket_pool_for_tests();
        let upstream = spawn_websocket_delayed_terminal_upstream().await;
        let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
        let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", transport);

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
        .unwrap_or_else(|_| panic!("{transport} stream should start before terminal event"));
        assert_eq!(response.status(), StatusCode::OK, "transport={transport}");

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
        assert!(
            read.is_ok(),
            "{transport} stream did not yield an early text delta"
        );
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("early chunk"),
            "{transport} stream body: {text}"
        );
        assert!(
            !text.contains("message_stop"),
            "{transport} stream finished too early: {text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_auto_stream_falls_back_after_websocket_handshake_rejection() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    let upstream = spawn_websocket_rejection_then_http_upstream().await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "auto");
    let proxy = app(Arc::new(Registry::with_default_alias()));
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        proxy.oneshot(messages_request(json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }))),
    )
    .await
    .expect("auto should fall back without waiting for WebSocket retry backoff")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("auto http fallback"),
        "stream body: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_does_not_retry_permanent_handshake_rejection() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    let upstream = spawn_non_retryable_websocket_rejection_upstream().await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        call_messages_body(json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        })),
    )
    .await
    .expect("permanent WebSocket rejection should fail without retry backoff");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_context_window_error_requests_compaction() {
    let _guard = env_lock().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_uses_previous_response_id() {
    let _guard = env_lock().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_reuses_connection_with_full_context_when_continuation_is_off() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_sequence_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "0");

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

    let second = tokio::time::timeout(
        Duration::from_secs(2),
        call_messages_body(json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [
                {"role":"user","content":"one"},
                {"role":"assistant","content":"first"},
                {"role":"user","content":"two"}
            ]
        })),
    )
    .await
    .expect("second full-context turn should reuse the existing WebSocket");
    assert_eq!(second.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 2, "expected two requests on one WebSocket");
    assert!(
        guard
            .iter()
            .all(|request| request.get("previous_response_id").is_none())
    );
    assert_eq!(guard[1]["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(guard[1]["input"][2]["content"][0]["text"], "two");

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_retries_missing_previous_response_id() {
    let _guard = env_lock().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_does_not_replay_post_dispatch_close() {
    let _guard = env_lock().await;
    let config = TempDir::new().unwrap();
    write_auth(config.path(), "codex");
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_close_after_second_request_upstream(captured.clone()).await;

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
    assert_eq!(second.status(), StatusCode::BAD_GATEWAY);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&second_body).contains("terminal"));

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 2, "the uncertain request must not be replayed");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_previous_response_id_sends_delta_on_second_turn() {
    let _guard = env_lock().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_traffic_capture_writes_upstream_artifacts() {
    let _guard = env_lock().await;
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
