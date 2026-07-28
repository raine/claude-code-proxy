use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message, handshake::client::generate_key},
};

use crate::provider::RequestContext;
use crate::traffic::TrafficCapture;

use super::client::{ActualTransport, CodexError, CodexErrorOrigin, CodexResponse};
use super::continuation::ContinuationCandidate;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const WEBSOCKET_PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";
pub const WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub const WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL: &str = "websocket_response_start_timeout";
pub const WEBSOCKET_MISSING_TERMINAL_DETAIL: &str = "websocket_missing_terminal";

const POOL_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
const MAX_POOL_ENTRIES: usize = 10_000;

// Terminal WebSocket event types that signal the request is done
const TERMINAL_EVENTS: &[&str] = &[
    "response.completed",
    "response.incomplete",
    "response.failed",
    "error",
];

pub type CodexWebSocketEventReceiver = mpsc::Receiver<Result<serde_json::Value, CodexError>>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CodexWebSocketError {
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<String>,
    pub retry_after: Option<String>,
    pub request_sent: bool,
}

impl CodexWebSocketError {
    pub fn new(message: String) -> Self {
        Self {
            message,
            status: None,
            code: None,
            retry_after: None,
            request_sent: false,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_code(mut self, code: String) -> Self {
        self.code = Some(code);
        self
    }
}

impl std::fmt::Display for CodexWebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex WebSocket error: {}", self.message)
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

struct PoolEntry {
    ws: Arc<AsyncMutex<WebSocketStream<MaybeTlsStream<TcpStream>>>>,
    created_at: u64,
}

static WS_POOL: once_cell::sync::Lazy<Mutex<HashMap<String, Arc<PoolEntry>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn clear_codex_websocket_pool_for_tests() {
    let mut guard = WS_POOL.lock().unwrap();
    guard.clear();
}

pub fn invalidate_codex_websocket_pool_key(session_id: &str) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.remove(session_id);
}

pub fn invalidate_codex_websocket_pool_turn(session_id: &str, turn_id: Option<u64>) {
    super::continuation::with_current_turn(Some(session_id), turn_id, || {
        invalidate_codex_websocket_pool_key(session_id)
    });
}

fn invalidate_pool_entry(session_id: &str, entry: &Arc<PoolEntry>) {
    let mut guard = WS_POOL.lock().unwrap();
    if guard
        .get(session_id)
        .is_some_and(|pooled| Arc::ptr_eq(pooled, entry))
    {
        guard.remove(session_id);
    }
}

fn invalidate_pool_owner(pool_key: Option<&str>, entry: Option<&Arc<PoolEntry>>) {
    let Some(session_id) = pool_key else {
        return;
    };
    match entry {
        Some(entry) => invalidate_pool_entry(session_id, entry),
        None => invalidate_codex_websocket_pool_key(session_id),
    }
}

fn pool_get_for_turn(key: &str, turn_id: Option<u64>) -> Option<Arc<PoolEntry>> {
    super::continuation::if_current_turn(Some(key), turn_id, || {
        WS_POOL.lock().ok()?.get(key).cloned()
    })
    .flatten()
}

fn pool_take_for_turn(key: &str, turn_id: Option<u64>) -> Option<Arc<PoolEntry>> {
    super::continuation::if_current_turn(Some(key), turn_id, || WS_POOL.lock().ok()?.remove(key))
        .flatten()
}

fn pool_insert_for_turn(key: String, entry: Arc<PoolEntry>, turn_id: Option<u64>) {
    let session_id = key.clone();
    super::continuation::with_current_turn(Some(&session_id), turn_id, || pool_insert(key, entry));
}

fn pool_remove_entry(key: &str, entry: &Arc<PoolEntry>) {
    invalidate_pool_entry(key, entry);
}

fn pool_insert(key: String, entry: Arc<PoolEntry>) {
    let mut guard = WS_POOL.lock().unwrap();
    // Evict oldest if at capacity
    if guard.len() >= MAX_POOL_ENTRIES
        && let Some(oldest_key) = guard.keys().next().cloned()
    {
        guard.remove(&oldest_key);
    }
    // Evict expired entries
    let now = now_ms();
    guard.retain(|_, e| now.saturating_sub(e.created_at) < POOL_IDLE_TTL_MS);
    guard.insert(key, entry);
}

// ---------------------------------------------------------------------------
// URL conversion
// ---------------------------------------------------------------------------

pub fn to_websocket_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    match parsed.scheme() {
        "http" => parsed.set_scheme("ws").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "https" => parsed.set_scheme("wss").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "ws" | "wss" => { /* already a ws scheme */ }
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Unsupported Codex WebSocket URL scheme: {other}"
            )));
        }
    }
    Ok(parsed.to_string())
}

// ---------------------------------------------------------------------------
// Header rewriting
// ---------------------------------------------------------------------------

pub fn codex_websocket_headers(http_headers: &HeaderMap) -> HeaderMap {
    let mut ws = HeaderMap::new();
    for (key, value) in http_headers.iter() {
        let key_str = key.as_str().to_lowercase();
        // Skip hop-by-hop headers
        if matches!(
            key_str.as_str(),
            "content-length" | "content-type" | "accept" | "connection" | "upgrade"
        ) {
            continue;
        }
        ws.insert(key.clone(), value.clone());
    }
    // Rewrite openai-beta for WebSocket protocol
    ws.insert("openai-beta", WEBSOCKET_PROTOCOL_HEADER.parse().unwrap());
    // Ensure WebSocket key is present
    if !ws.contains_key("sec-websocket-key") {
        ws.insert("sec-websocket-key", generate_key().parse().unwrap());
    }
    ws
}

// ---------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------

fn encode_sse(text: &str) -> Vec<u8> {
    let mut out = String::new();
    for line in text.lines() {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Terminal event detection
// ---------------------------------------------------------------------------

fn is_terminal_event(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some(t) => TERMINAL_EVENTS.contains(&t),
        None => false,
    }
}

fn is_response_event(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("error") => true,
        Some(t) => t.starts_with("response."),
        None => false,
    }
}

fn is_previous_response_missing(payload: &serde_json::Value) -> bool {
    if let Some(code) = payload
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        && code == "previous_response_not_found"
    {
        return true;
    }
    // Case-insensitive message check
    if let Some(msg) = payload
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
    {
        let lower = msg.to_lowercase();
        if lower.contains("previous response") && lower.contains("not found") {
            return true;
        }
    }
    false
}

pub(super) fn event_error_status(payload: &serde_json::Value) -> Option<u16> {
    super::events::classify_event_failure(payload).and_then(|failure| failure.explicit_status)
}

#[allow(dead_code)]
fn extract_retry_after(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|e| e.get("retry_after"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Main request function
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn codex_websocket_request(
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    _ctx: &RequestContext,
    traffic: Option<&TrafficCapture>,
    pool_key: Option<&str>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    continuation: Option<&ContinuationCandidate>,
) -> Result<CodexResponse, CodexError> {
    let ws_url = to_websocket_url(url).map_err(|e| CodexError {
        status: 0,
        message: e.message,
        detail: None,
        retry_after: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let body_json = serde_json::to_string(body_value).unwrap_or_default();
    if let Some(tc) = traffic {
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "url": ws_url,
                "method": "GET",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
                "continuation": {
                    "previousResponseId": continuation
                        .and_then(|c| c.previous_response_id.as_deref()),
                    "inputDeltaCount": continuation
                        .and_then(|c| c.input_delta.as_ref())
                        .map(|items| items.len()),
                    "disabledReason": continuation
                        .and_then(|c| c.disabled_reason.as_deref()),
                },
            }),
        );
    }
    let started_at = Instant::now();

    // Check pool for existing connection
    let pooled = pool_key.and_then(|key| {
        pool_get_for_turn(key, continuation.and_then(|candidate| candidate.turn_id))
    });

    let (ws_stream, _response) = if let Some(entry) = pooled {
        // Use pooled connection
        let mut ws_guard = entry.ws.lock().await;
        // Check if connection is still alive by sending a ping
        if ws_guard.send(Message::Ping(vec![])).await.is_err() {
            invalidate_pool_entry(pool_key.unwrap(), &entry);
            // Fall through to new connection
            connect_with_timeout(&ws_url, headers, connect_timeout_ms).await?
        } else {
            // Connection is alive, send the request through it
            let ws_msg = Message::Text(body_json.clone());
            ws_guard.send(ws_msg).await.map_err(|e| {
                if let Some(key) = pool_key {
                    invalidate_pool_entry(key, &entry);
                }
                CodexError {
                    status: 0,
                    message: format!("WebSocket send error: {e}"),
                    detail: None,
                    retry_after: None,
                    origin: CodexErrorOrigin::WebSocket,
                }
            })?;

            // Collect events
            let (sse_body, terminal_event) = collect_ws_events(
                &mut ws_guard,
                idle_timeout_ms,
                pool_key,
                Some(&entry),
                traffic,
            )
            .await?;
            let Some(terminal_event) = terminal_event else {
                return Err(missing_terminal_error());
            };

            // Handle previous response missing
            if is_previous_response_missing(&terminal_event.payload) {
                return Err(CodexError {
                    status: 0,
                    message: "Previous response not found".to_string(),
                    detail: Some("previous_response_not_found".to_string()),
                    retry_after: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }

            // Extract status from error events
            let status = if terminal_event.event_type == "error" {
                event_error_status(&terminal_event.payload).unwrap_or(500)
            } else {
                200
            };

            // Write traffic metadata
            if let Some(tc) = traffic {
                write_websocket_metadata_capture(tc, &ws_url, pool_key, continuation, true);
                write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
            }

            return Ok(CodexResponse {
                body: sse_body,
                status,
                headers: vec![],
                transport: ActualTransport::WebSocket,
            });
        }
    } else {
        connect_with_timeout(&ws_url, headers, connect_timeout_ms).await?
    };

    // New connection path (not pooled or pool miss)
    let entry = Arc::new(PoolEntry {
        ws: Arc::new(AsyncMutex::new(ws_stream)),
        created_at: now_ms(),
    });

    // Send the request
    let msg = Message::Text(body_json);
    {
        let mut ws_guard = entry.ws.lock().await;
        ws_guard.send(msg).await.map_err(|e| CodexError {
            status: 0,
            message: format!("WebSocket send error: {e}"),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        })?;

        let (sse_body, terminal_event) = collect_ws_events(
            &mut ws_guard,
            idle_timeout_ms,
            pool_key,
            Some(&entry),
            traffic,
        )
        .await?;
        let Some(terminal_event) = terminal_event else {
            return Err(missing_terminal_error());
        };

        if is_previous_response_missing(&terminal_event.payload) {
            if let Some(key) = pool_key {
                invalidate_pool_entry(key, &entry);
            }
            return Err(CodexError {
                status: 0,
                message: "Previous response not found".to_string(),
                detail: Some("previous_response_not_found".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::WebSocket,
            });
        }

        // Pool the connection if we have a key and it was successful
        if let Some(key) = pool_key {
            let should_pool = terminal_event.event_type == "response.completed";
            if should_pool {
                pool_insert_for_turn(
                    key.to_string(),
                    entry.clone(),
                    continuation.and_then(|candidate| candidate.turn_id),
                );
            }
        }

        let status = if terminal_event.event_type == "error" {
            event_error_status(&terminal_event.payload).unwrap_or(500)
        } else {
            200
        };

        // Write traffic metadata
        if let Some(tc) = traffic {
            write_websocket_metadata_capture(tc, &ws_url, pool_key, continuation, false);
            write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
        }

        Ok(CodexResponse {
            body: sse_body,
            status,
            headers: vec![],
            transport: ActualTransport::WebSocket,
        })
    }
}

pub(super) struct ReadyWebSocket {
    ws_url: String,
    guard: OwnedMutexGuard<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    entry: Arc<PoolEntry>,
    used_pooled: bool,
    pool_key: Option<String>,
    turn_id: Option<u64>,
    traffic: Option<Arc<TrafficCapture>>,
    idle_timeout_ms: u64,
}

pub(super) async fn prepare_codex_websocket(
    url: &str,
    headers: &HeaderMap,
    traffic: Option<Arc<TrafficCapture>>,
    pool_key: Option<&str>,
    turn_id: Option<u64>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
) -> Result<ReadyWebSocket, CodexError> {
    let ws_url = to_websocket_url(url).map_err(|error| CodexError {
        status: 0,
        message: error.message,
        detail: None,
        retry_after: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let pooled = pool_key.and_then(|key| pool_take_for_turn(key, turn_id));
    let used_pooled = pooled.is_some();
    let entry = if let Some(entry) = pooled {
        entry
    } else {
        let (stream, _) = connect_with_timeout(&ws_url, headers, connect_timeout_ms).await?;
        Arc::new(PoolEntry {
            ws: Arc::new(AsyncMutex::new(stream)),
            created_at: now_ms(),
        })
    };
    let mut guard = entry.ws.clone().lock_owned().await;
    if used_pooled {
        let nonce = b"codex-ready".to_vec();
        guard
            .send(Message::Ping(nonce.clone()))
            .await
            .map_err(|error| pooled_validation_error(error.to_string()))?;
        let validation = tokio::time::timeout(Duration::from_millis(connect_timeout_ms), async {
            loop {
                match guard.next().await {
                    Some(Ok(Message::Pong(payload))) if payload == nonce => return Ok(()),
                    Some(Ok(Message::Ping(payload))) => guard
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| error.to_string())?,
                    Some(Ok(_)) => {
                        return Err("unexpected frame during pooled validation".to_string());
                    }
                    Some(Err(error)) => return Err(error.to_string()),
                    None => return Err("connection closed during pooled validation".to_string()),
                }
            }
        })
        .await
        .map_err(|_| pooled_validation_error("validation timeout".to_string()))?;
        validation.map_err(pooled_validation_error)?;
    }
    Ok(ReadyWebSocket {
        ws_url,
        guard,
        entry,
        used_pooled,
        pool_key: pool_key.map(str::to_string),
        turn_id,
        traffic,
        idle_timeout_ms,
    })
}

fn pooled_validation_error(detail: String) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket pooled connection validation failed: {detail}"),
        detail: None,
        retry_after: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn start_codex_websocket_events(
    ready: ReadyWebSocket,
    body_value: &serde_json::Value,
    body_json: String,
    headers: &HeaderMap,
    continuation: Option<&ContinuationCandidate>,
) -> CodexWebSocketEventReceiver {
    if let Some(tc) = ready.traffic.as_deref() {
        write_websocket_metadata_capture(
            tc,
            &ready.ws_url,
            ready.pool_key.as_deref(),
            continuation,
            ready.used_pooled,
        );
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
            }),
        );
    }
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let ReadyWebSocket {
            ws_url: _,
            mut guard,
            entry,
            used_pooled: _,
            pool_key,
            turn_id,
            traffic,
            idle_timeout_ms,
        } = ready;
        if let Err(error) = guard.send(Message::Text(body_json)).await {
            if let Some(key) = pool_key.as_deref() {
                pool_remove_entry(key, &entry);
            }
            let _ = tx
                .send(Err(CodexError {
                    status: 0,
                    message: format!("WebSocket send error: {error}"),
                    detail: None,
                    retry_after: None,
                    origin: CodexErrorOrigin::WebSocket,
                }))
                .await;
            return;
        }
        let reusable = stream_ws_events(
            &mut guard,
            idle_timeout_ms,
            pool_key.as_deref(),
            Some(&entry),
            traffic,
            tx,
        )
        .await;
        if let Some(key) = pool_key.as_deref() {
            if reusable {
                pool_insert_for_turn(key.to_string(), entry, turn_id);
            } else {
                pool_remove_entry(key, &entry);
            }
        }
    });
    rx
}

#[allow(clippy::too_many_arguments)]
pub async fn codex_websocket_event_stream(
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    _ctx: &RequestContext,
    traffic: Option<Arc<TrafficCapture>>,
    pool_key: Option<&str>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    continuation: Option<&ContinuationCandidate>,
) -> Result<CodexWebSocketEventReceiver, CodexError> {
    let body_json = serde_json::to_string(body_value).map_err(|error| CodexError {
        status: 500,
        message: "Failed to serialize WebSocket request".to_string(),
        detail: Some(error.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let ready = prepare_codex_websocket(
        url,
        headers,
        traffic,
        pool_key,
        continuation.and_then(|candidate| candidate.turn_id),
        connect_timeout_ms,
        idle_timeout_ms,
    )
    .await?;
    Ok(start_codex_websocket_events(
        ready,
        body_value,
        body_json,
        headers,
        continuation,
    ))
}

fn missing_terminal_error() -> CodexError {
    CodexError {
        status: 0,
        message: "WebSocket connection closed before terminal Codex response event".to_string(),
        detail: Some(WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn response_start_timeout_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket response start timeout after {timeout_ms}ms"),
        detail: Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn write_websocket_metadata_capture(
    traffic: &TrafficCapture,
    ws_url: &str,
    pool_key: Option<&str>,
    continuation: Option<&ContinuationCandidate>,
    pooled: bool,
) {
    traffic.write_json(
        "022-upstream-websocket-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "websocket",
            "url": ws_url,
            "poolKey": pool_key,
            "pooled": pooled,
            "continuation": {
                "previousResponseId": continuation
                    .and_then(|c| c.previous_response_id.as_deref()),
                "inputDeltaCount": continuation
                    .and_then(|c| c.input_delta.as_ref())
                    .map(|items| items.len()),
                "disabledReason": continuation
                    .and_then(|c| c.disabled_reason.as_deref()),
            },
        }),
    );
}

fn write_websocket_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    sse_body: &[u8],
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": {
                "content-type": "text/event-stream",
            },
        }),
    );
    if status >= 400 {
        traffic.write_text(
            "031-upstream-error-body",
            &String::from_utf8_lossy(sse_body),
        );
    } else {
        traffic.write_bytes("032-upstream-response-body.sse", sse_body);
    }
}

// ---------------------------------------------------------------------------
// Connection helper
// ---------------------------------------------------------------------------

const MAX_HANDSHAKE_ERROR_DETAIL_BYTES: usize = 1024;
const GENERIC_HANDSHAKE_ERROR_DETAIL: &str = "WebSocket upgrade was rejected";

fn handshake_error_detail(body: Option<&Vec<u8>>) -> String {
    let Some(value) = body.and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
    else {
        return GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    };
    let Some(message) = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(|value| value.as_str())
    else {
        return GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    };
    let sanitized: String = message
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, ' '))
        .collect();
    let mut end = sanitized.len().min(MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_string()
}

async fn connect_with_timeout(
    url: &str,
    headers: &HeaderMap,
    connect_timeout_ms: u64,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tungstenite::handshake::client::Response,
    ),
    CodexError,
> {
    // Build an http::Request with the given headers for the WebSocket upgrade
    let host = websocket_host_header(url);
    let mut req_builder = http::Request::builder()
        .uri(url)
        .method("GET")
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key());

    // Copy over the codex headers
    for (key, value) in headers.iter() {
        let key_str = key.as_str().to_lowercase();
        // Skip headers already set for WebSocket upgrade
        if matches!(
            key_str.as_str(),
            "connection" | "upgrade" | "sec-websocket-key" | "sec-websocket-version" | "host"
        ) {
            continue;
        }
        req_builder = req_builder.header(key.as_str(), value.as_bytes());
    }

    let request = req_builder.body(()).map_err(|e| CodexError {
        status: 0,
        message: format!("Failed to build WebSocket request: {e}"),
        detail: None,
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    })?;

    let connect_fut = connect_async(request);
    tokio::time::timeout(Duration::from_millis(connect_timeout_ms), connect_fut)
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!("WebSocket connect timeout after {connect_timeout_ms}ms"),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?
        .map_err(|e| {
            let (status, retry_after, detail) = match &e {
                tungstenite::Error::Http(response) => {
                    let detail = Some(handshake_error_detail(response.body().as_ref()));
                    (
                        Some(response.status().as_u16()),
                        response
                            .headers()
                            .get(http::header::RETRY_AFTER)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                        detail,
                    )
                }
                _ => (None, None, None),
            };
            CodexError {
                status: status.unwrap_or(0),
                message: format!("WebSocket connect error: {e}"),
                detail,
                retry_after,
                origin: CodexErrorOrigin::WebSocketHandshake,
            }
        })
}

fn websocket_host_header(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return String::new();
    };
    parsed[url::Position::BeforeHost..url::Position::AfterPort].to_string()
}

// ---------------------------------------------------------------------------
// Event collection
// ---------------------------------------------------------------------------

struct WsEvent {
    event_type: String,
    payload: serde_json::Value,
}

async fn collect_ws_events(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    idle_timeout_ms: u64,
    pool_key: Option<&str>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError> {
    let mut sse_body: Vec<u8> = Vec::new();
    let mut terminal_event: Option<WsEvent> = None;
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout = if response_started {
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_key, pool_entry);
                    return Err(CodexError {
                        status: 0,
                        message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::WebSocket,
                    });
                }
            }
        } else {
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_key, pool_entry);
                    return Err(response_start_timeout_error(idle_timeout_ms));
                }
            }
        };

        let timeout = tokio::time::timeout(read_timeout, ws.next());

        let frame = timeout.await.map_err(|_| {
            invalidate_pool_owner(pool_key, pool_entry);
            if response_started {
                CodexError {
                    status: 0,
                    message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                    detail: None,
                    retry_after: None,
                    origin: CodexErrorOrigin::WebSocket,
                }
            } else {
                response_start_timeout_error(idle_timeout_ms)
            }
        })?;

        match frame {
            Some(Ok(Message::Text(text))) => {
                // Parse JSON
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Some(tc) = traffic {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        // Write invalid JSON as-is
                        sse_body.extend_from_slice(&encode_sse(&text));
                        continue;
                    }
                };

                // Convert to SSE bytes
                sse_body.extend_from_slice(&encode_sse(&text));
                if let Some(tc) = traffic {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = Instant::now();
                }

                // Check for terminal events
                if is_terminal_event(&parsed) {
                    terminal_event = Some(WsEvent {
                        event_type: parsed
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        payload: parsed,
                    });
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                // Reject binary frames
                invalidate_pool_owner(pool_key, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: "WebSocket binary frames not supported".to_string(),
                    detail: None,
                    retry_after: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
            Some(Ok(Message::Ping(data))) => {
                // Respond to ping automatically, continue
                let _ = ws.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Pong(_))) => {
                continue;
            }
            Some(Ok(Message::Frame(_))) => {
                // Raw frame passthrough - continue
                continue;
            }
            Some(Ok(Message::Close(_))) => {
                // Connection closed - invalidate pool
                invalidate_pool_owner(pool_key, pool_entry);
                break;
            }
            Some(Err(e)) => {
                // Stream error - invalidate pool
                invalidate_pool_owner(pool_key, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: format!("WebSocket stream error: {e}"),
                    detail: None,
                    retry_after: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
            None => {
                // Stream ended - invalidate pool
                invalidate_pool_owner(pool_key, pool_entry);
                break;
            }
        }
    }

    Ok((sse_body, terminal_event))
}

async fn stream_ws_events(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    idle_timeout_ms: u64,
    pool_key: Option<&str>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<Arc<TrafficCapture>>,
    tx: mpsc::Sender<Result<serde_json::Value, CodexError>>,
) -> bool {
    let started_at = Instant::now();
    let mut sse_body: Vec<u8> = Vec::new();
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;
    let mut status = 200u16;
    let mut reusable = false;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout =
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_key, pool_entry);
                    let err = if response_started {
                        CodexError {
                            status: 0,
                            message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                            detail: None,
                            retry_after: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }
                    } else {
                        response_start_timeout_error(idle_timeout_ms)
                    };
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            };

        let frame = match tokio::time::timeout(read_timeout, ws.next()).await {
            Ok(frame) => frame,
            Err(_) => {
                invalidate_pool_owner(pool_key, pool_entry);
                let err = if response_started {
                    CodexError {
                        status: 0,
                        message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }
                } else {
                    response_start_timeout_error(idle_timeout_ms)
                };
                let _ = tx.send(Err(err)).await;
                break;
            }
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Some(tc) = traffic.as_deref() {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        sse_body.extend_from_slice(&encode_sse(&text));
                        continue;
                    }
                };

                sse_body.extend_from_slice(&encode_sse(&text));
                if let Some(tc) = traffic.as_deref() {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = Instant::now();
                }

                if parsed.get("type").and_then(|v| v.as_str()) == Some("error") {
                    status = event_error_status(&parsed).unwrap_or(500);
                }
                let terminal = is_terminal_event(&parsed);
                if terminal && is_previous_response_missing(&parsed) {
                    invalidate_pool_owner(pool_key, pool_entry);
                    let _ = tx
                        .send(Err(CodexError {
                            status: 0,
                            message: "Previous response not found".to_string(),
                            detail: Some("previous_response_not_found".to_string()),
                            retry_after: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }))
                        .await;
                    break;
                }
                let event_type = parsed
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if tx.send(Ok(parsed)).await.is_err() {
                    invalidate_pool_owner(pool_key, pool_entry);
                    break;
                }
                if terminal {
                    reusable = event_type == "response.completed";
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                invalidate_pool_owner(pool_key, pool_entry);
                let _ = tx
                    .send(Err(CodexError {
                        status: 0,
                        message: "WebSocket binary frames not supported".to_string(),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }))
                    .await;
                break;
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws.send(Message::Pong(data)).await;
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) | None => {
                invalidate_pool_owner(pool_key, pool_entry);
                let _ = tx.send(Err(missing_terminal_error())).await;
                break;
            }
            Some(Err(e)) => {
                invalidate_pool_owner(pool_key, pool_entry);
                let _ = tx
                    .send(Err(CodexError {
                        status: 0,
                        message: format!("WebSocket stream error: {e}"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }))
                    .await;
                break;
            }
        }
    }

    if let Some(tc) = traffic.as_deref() {
        write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
    }
    reusable
}

fn headers_to_json(headers: &HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers.iter() {
        out.insert(
            key.to_string(),
            serde_json::Value::String(value.to_str().unwrap_or("").to_string()),
        );
    }
    serde_json::Value::Object(out)
}

fn summarize_json_request_size(body: &serde_json::Value, body_json: &str) -> serde_json::Value {
    serde_json::json!({
        "bytes": body_json.len(),
        "inputCount": body
            .get("input")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
        "toolCount": body
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_error_status_requires_error_event_and_checks_numeric_fallbacks() {
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "response.failed",
                "status": "failed",
                "status_code": 401
            })),
            Some(401)
        );
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "response.completed",
                "status_code": 401
            })),
            None
        );
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "error",
                "error": {"status": 401}
            })),
            Some(401)
        );
    }

    #[test]
    fn websocket_url_conversion() {
        assert_eq!(
            to_websocket_url("https://example.test/codex").unwrap(),
            "wss://example.test/codex"
        );
        assert_eq!(
            to_websocket_url("http://example.test/codex").unwrap(),
            "ws://example.test/codex"
        );
        assert_eq!(
            to_websocket_url("wss://example.test/codex").unwrap(),
            "wss://example.test/codex"
        );
        assert!(to_websocket_url("ftp://example.test/codex").is_err());
    }

    #[test]
    fn websocket_host_header_preserves_explicit_port() {
        assert_eq!(
            websocket_host_header("wss://chatgpt.com/backend-api/codex/responses"),
            "chatgpt.com"
        );
        assert_eq!(
            websocket_host_header("ws://127.0.0.1:4141/backend-api/codex/responses"),
            "127.0.0.1:4141"
        );
        assert_eq!(websocket_host_header("ws://[::1]:4141/path"), "[::1]:4141");
    }

    #[test]
    fn websocket_headers_rewrite_beta() {
        let mut headers = http::HeaderMap::new();
        headers.insert("openai-beta", "responses=experimental".parse().unwrap());
        headers.insert("content-length", "10".parse().unwrap());
        headers.insert("authorization", "Bearer tok".parse().unwrap());
        let ws = codex_websocket_headers(&headers);
        assert_eq!(ws.get("openai-beta").unwrap(), WEBSOCKET_PROTOCOL_HEADER);
        assert!(!ws.contains_key("content-length"));
        assert_eq!(ws.get("authorization").unwrap(), "Bearer tok");
    }

    #[test]
    fn websocket_headers_strips_accept() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::ACCEPT, "text/event-stream".parse().unwrap());
        let ws = codex_websocket_headers(&headers);
        assert!(!ws.contains_key(http::header::ACCEPT.as_str()));
    }

    #[test]
    fn websocket_headers_adds_sec_key() {
        let headers = http::HeaderMap::new();
        let ws = codex_websocket_headers(&headers);
        assert!(ws.contains_key("sec-websocket-key"));
    }

    #[test]
    fn encode_sse_single_line() {
        let result = encode_sse(r#"{"type":"test","data":"hello"}"#);
        let expected = b"data: {\"type\":\"test\",\"data\":\"hello\"}\n\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn encode_sse_multi_line() {
        let result = encode_sse("line1\nline2");
        assert_eq!(
            String::from_utf8(result).unwrap(),
            "data: line1\ndata: line2\n\n"
        );
    }

    #[test]
    fn is_terminal_event_detection() {
        let completed = serde_json::json!({"type": "response.completed"});
        assert!(is_terminal_event(&completed));

        let delta = serde_json::json!({"type": "response.output_text.delta"});
        assert!(!is_terminal_event(&delta));

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(is_terminal_event(&error));
    }

    #[test]
    fn is_response_event_detection() {
        let rate_limits = serde_json::json!({"type": "codex.rate_limits"});
        assert!(!is_response_event(&rate_limits));

        let output = serde_json::json!({"type": "response.output_text.delta"});
        assert!(is_response_event(&output));

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(is_response_event(&error));
    }

    #[test]
    fn is_previous_response_missing_detection() {
        let by_code = serde_json::json!({
            "type": "error",
            "error": {"code": "previous_response_not_found", "message": "not found"}
        });
        assert!(is_previous_response_missing(&by_code));

        let by_msg = serde_json::json!({
            "type": "error",
            "error": {"message": "The previous response was not found"}
        });
        assert!(is_previous_response_missing(&by_msg));

        let unrelated = serde_json::json!({"type": "error", "error": {"message": "rate limited"}});
        assert!(!is_previous_response_missing(&unrelated));
    }

    #[tokio::test]
    async fn pool_checkout_is_exclusive_and_removal_is_identity_safe() {
        clear_codex_websocket_pool_for_tests();
        let first = Arc::new(PoolEntry {
            ws: Arc::new(AsyncMutex::new(create_dummy_stream_async().await)),
            created_at: now_ms(),
        });
        pool_insert("exclusive".to_string(), first.clone());
        assert!(Arc::ptr_eq(
            &WS_POOL.lock().unwrap().remove("exclusive").unwrap(),
            &first
        ));
        assert!(WS_POOL.lock().unwrap().remove("exclusive").is_none());

        let replacement = Arc::new(PoolEntry {
            ws: Arc::new(AsyncMutex::new(create_dummy_stream_async().await)),
            created_at: now_ms(),
        });
        pool_insert("exclusive".to_string(), replacement.clone());
        pool_remove_entry("exclusive", &first);
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get("exclusive").unwrap(),
            &replacement
        ));
        clear_codex_websocket_pool_for_tests();
    }

    #[test]
    fn pool_invalidation() {
        clear_codex_websocket_pool_for_tests();
        // Verify pool operations work through the public API
        // We insert an entry directly into the pool, then invalidate it
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "test-session".to_string(),
                Arc::new(PoolEntry {
                    ws: Arc::new(AsyncMutex::new(create_dummy_stream())),
                    created_at: now_ms(),
                }),
            );
        }
        assert!(WS_POOL.lock().unwrap().contains_key("test-session"));

        invalidate_codex_websocket_pool_key("test-session");
        assert!(!WS_POOL.lock().unwrap().contains_key("test-session"));
    }

    #[tokio::test]
    async fn websocket_connect_401_is_pre_request_handshake_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\npolicy denied")
                .await
                .unwrap();
        });

        let err = match connect_with_timeout(
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
        )
        .await
        {
            Ok(_) => panic!("expected unauthorized websocket handshake to fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 401);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    #[tokio::test]
    async fn websocket_connect_502_preserves_retry_metadata() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nRetry-After: 3\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let err = match connect_with_timeout(
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
        )
        .await
        {
            Ok(_) => panic!("expected websocket handshake to fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 502);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        assert_eq!(err.retry_after.as_deref(), Some("3"));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    #[tokio::test]
    async fn binary_frame_invalidates_pool_key() {
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "binary-session".to_string(),
                Arc::new(PoolEntry {
                    ws: Arc::new(AsyncMutex::new(pooled_stream)),
                    created_at: now_ms(),
                }),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Binary(vec![1, 2, 3])).await.unwrap();
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err = match collect_ws_events(&mut ws, 1_000, Some("binary-session"), None, None).await
        {
            Ok(_) => panic!("expected binary frame to fail"),
            Err(err) => err,
        };

        assert!(err.message.contains("binary frames"));
        assert!(!WS_POOL.lock().unwrap().contains_key("binary-session"));
    }

    #[tokio::test]
    async fn response_start_timeout_ignores_rate_limits_and_pings() {
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "start-timeout-session".to_string(),
                Arc::new(PoolEntry {
                    ws: Arc::new(AsyncMutex::new(pooled_stream)),
                    created_at: now_ms(),
                }),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"codex.rate_limits","rate_limits":{"allowed":true}}"#.into(),
            ))
            .await
            .unwrap();
            loop {
                if ws.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err =
            match collect_ws_events(&mut ws, 50, Some("start-timeout-session"), None, None).await {
                Ok(_) => panic!("expected response start timeout"),
                Err(err) => err,
            };

        assert_eq!(
            err.detail.as_deref(),
            Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
        );
        assert!(
            !WS_POOL
                .lock()
                .unwrap()
                .contains_key("start-timeout-session")
        );
    }

    #[tokio::test]
    async fn response_idle_timeout_ignores_pings_after_response_event() {
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "response-idle-session".to_string(),
                Arc::new(PoolEntry {
                    ws: Arc::new(AsyncMutex::new(pooled_stream)),
                    created_at: now_ms(),
                }),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#
                    .into(),
            ))
            .await
            .unwrap();
            loop {
                if ws.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err =
            match collect_ws_events(&mut ws, 50, Some("response-idle-session"), None, None).await {
                Ok(_) => panic!("expected response idle timeout"),
                Err(err) => err,
            };

        assert!(err.message.contains("idle timeout"));
        assert_eq!(err.detail, None);
        assert!(
            !WS_POOL
                .lock()
                .unwrap()
                .contains_key("response-idle-session")
        );
    }

    async fn create_dummy_stream_async() -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = tokio_tungstenite::accept_async(socket).await;
            futures_util::future::pending::<()>().await;
        });
        let url = format!("ws://{addr}/");
        let (ws, _) = tokio::time::timeout(
            Duration::from_millis(1000),
            tokio_tungstenite::connect_async(&url),
        )
        .await
        .unwrap()
        .unwrap();
        ws
    }

    #[test]
    fn handshake_details_are_structured_sanitized_and_bounded() {
        let message = format!("safe\n{}", "x".repeat(MAX_HANDSHAKE_ERROR_DETAIL_BYTES * 2));
        let body = serde_json::to_vec(&serde_json::json!({
            "error": { "message": message }
        }))
        .unwrap();
        let detail = handshake_error_detail(Some(&body));
        assert!(!detail.contains('\n'));
        assert!(detail.len() <= MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
        assert!(detail.starts_with("safe"));
    }

    #[test]
    fn handshake_details_reject_unstructured_and_binary_bodies() {
        for body in [
            b"<html>denied</html>".to_vec(),
            vec![0xff, 0xfe],
            b"{".to_vec(),
        ] {
            assert_eq!(
                handshake_error_detail(Some(&body)),
                GENERIC_HANDSHAKE_ERROR_DETAIL
            );
        }
    }

    fn create_dummy_stream() -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        // Use a connected TcpStream pair with connect_async which returns
        // WebSocketStream<MaybeTlsStream<TcpStream>>
        use tokio::net::TcpListener;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let _conn = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                // Accept WebSocket handshake
                let _ = tokio_tungstenite::accept_async(socket).await;
                // Keep alive
                futures_util::future::pending::<()>().await;
            });
            // Use connect_async to get MaybeTlsStream
            let url = format!("ws://{}/", addr);
            let (ws, _) = tokio::time::timeout(
                Duration::from_millis(1000),
                tokio_tungstenite::connect_async(&url),
            )
            .await
            .unwrap()
            .unwrap();
            ws
        })
    }
}
