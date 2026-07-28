use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{self, Message, handshake::client::generate_key, protocol::WebSocketConfig},
};

use crate::provider::RequestContext;
use crate::timeutil::now_ms;
use crate::traffic::TrafficCapture;

use super::client::{CodexError, CodexErrorOrigin, CodexResponse};
use super::continuation::ContinuationCandidate;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const WEBSOCKET_PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";
pub const WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub const WEBSOCKET_RESPONSE_START_TIMEOUT_MS: u64 = 60_000;
pub const WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const WEBSOCKET_HEARTBEAT_INTERVAL_MS: u64 = 30_000;
pub const WEBSOCKET_PONG_TIMEOUT_MS: u64 = 10_000;
pub const WEBSOCKET_POOL_PROBE_TIMEOUT_MS: u64 = 3_000;
pub const WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL: &str = "websocket_response_start_timeout";
pub const WEBSOCKET_IDLE_TIMEOUT_DETAIL: &str = "websocket_idle_timeout";
pub const WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL: &str = "websocket_heartbeat_timeout";
pub const WEBSOCKET_POOL_HEALTHCHECK_DETAIL: &str = "websocket_pool_healthcheck";
pub const WEBSOCKET_POOL_BUSY_DETAIL: &str = "websocket_pool_busy";
pub const WEBSOCKET_CONNECTION_ERROR_DETAIL: &str = "websocket_connection_error";
pub const WEBSOCKET_MISSING_TERMINAL_DETAIL: &str = "websocket_missing_terminal";
pub const WEBSOCKET_MALFORMED_EVENT_DETAIL: &str = "websocket_malformed_event";
pub const WEBSOCKET_TERMINAL_BARRIER_DETAIL: &str = "websocket_terminal_barrier";

pub(super) fn is_retryable_transport_detail(detail: Option<&str>) -> bool {
    matches!(
        detail,
        Some(
            WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL
                | WEBSOCKET_IDLE_TIMEOUT_DETAIL
                | WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL
                | WEBSOCKET_POOL_HEALTHCHECK_DETAIL
                | WEBSOCKET_POOL_BUSY_DETAIL
                | WEBSOCKET_CONNECTION_ERROR_DETAIL
                | WEBSOCKET_MISSING_TERMINAL_DETAIL
        )
    )
}

const DEFAULT_POOL_IDLE_TTL_MS: u64 = 5 * 60 * 1000;
const DEFAULT_MAX_IDLE_POOL_ENTRIES: usize = 128;
const MAX_CONFIGURED_IDLE_POOL_ENTRIES: usize = 4_096;
const MAX_CONFIGURED_POOL_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
const MIN_POOL_REAPER_INTERVAL_MS: u64 = 1_000;
const MAX_POOL_REAPER_INTERVAL_MS: u64 = 30_000;
pub(super) const WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
pub(super) const WEBSOCKET_CIRCUIT_COOLDOWN_MS: u64 = 30_000;
const MAX_CIRCUIT_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, Copy)]
pub struct CodexWebSocketTimeouts {
    pub connect_ms: u64,
    pub response_start_ms: u64,
    pub idle_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub pong_ms: u64,
    pub pool_probe_ms: u64,
}

impl CodexWebSocketTimeouts {
    pub fn configured() -> Self {
        Self {
            response_start_ms: crate::config::codex_websocket_response_start_timeout_ms(
                WEBSOCKET_RESPONSE_START_TIMEOUT_MS,
            ),
            idle_ms: crate::config::codex_websocket_idle_timeout_ms(WEBSOCKET_IDLE_TIMEOUT_MS),
            ..Self::default()
        }
    }
}

impl Default for CodexWebSocketTimeouts {
    fn default() -> Self {
        Self {
            connect_ms: WEBSOCKET_CONNECT_TIMEOUT_MS,
            response_start_ms: WEBSOCKET_RESPONSE_START_TIMEOUT_MS,
            idle_ms: WEBSOCKET_IDLE_TIMEOUT_MS,
            heartbeat_interval_ms: WEBSOCKET_HEARTBEAT_INTERVAL_MS,
            pong_ms: WEBSOCKET_PONG_TIMEOUT_MS,
            pool_probe_ms: WEBSOCKET_POOL_PROBE_TIMEOUT_MS,
        }
    }
}

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
    valid: AtomicBool,
    fresh_probe: FreshProbeCredit,
}

impl PoolEntry {
    fn new(ws: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
        Self {
            ws: Arc::new(AsyncMutex::new(ws)),
            valid: AtomicBool::new(true),
            fresh_probe: FreshProbeCredit::default(),
        }
    }

    fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    fn invalidate(&self) {
        self.valid.store(false, Ordering::Release);
    }

    fn mark_terminal_probe_success(&self) {
        self.fresh_probe.mark(Instant::now());
    }

    fn consume_fresh_probe(&self, max_age: Duration) -> bool {
        self.fresh_probe.consume(Instant::now(), max_age)
    }
}

#[derive(Default)]
struct FreshProbeCredit {
    validated_at: Mutex<Option<Instant>>,
}

impl FreshProbeCredit {
    fn mark(&self, validated_at: Instant) {
        *self
            .validated_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(validated_at);
    }

    fn consume(&self, now: Instant, max_age: Duration) -> bool {
        let validated_at = self
            .validated_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        validated_at.is_some_and(|validated_at| {
            now.checked_duration_since(validated_at)
                .is_some_and(|age| age <= max_age)
        })
    }
}

struct IdlePoolEntry {
    connection: Arc<PoolEntry>,
    idle_since_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct WebSocketPoolConfig {
    max_idle_entries: usize,
    idle_ttl_ms: u64,
}

impl WebSocketPoolConfig {
    fn configured() -> Self {
        let max_idle_entries =
            crate::config::codex_max_idle_websockets(DEFAULT_MAX_IDLE_POOL_ENTRIES as u64)
                .min(MAX_CONFIGURED_IDLE_POOL_ENTRIES as u64) as usize;
        let idle_ttl_ms = crate::config::codex_idle_websocket_ttl_ms(DEFAULT_POOL_IDLE_TTL_MS)
            .min(MAX_CONFIGURED_POOL_IDLE_TTL_MS);
        Self {
            max_idle_entries,
            idle_ttl_ms,
        }
    }
}

static WS_POOL: once_cell::sync::Lazy<Mutex<HashMap<String, IdlePoolEntry>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
static WS_POOL_REAPER_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
struct CircuitEntry {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    updated_at: Instant,
}

#[derive(Default)]
struct WebSocketCircuitBreaker {
    entries: HashMap<String, CircuitEntry>,
}

impl WebSocketCircuitBreaker {
    fn should_route_http(&mut self, key: &str, now: Instant, probe_lease: Duration) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        match entry.open_until {
            Some(open_until) if now < open_until => true,
            Some(_) => {
                // Reserve one half-open probe. Concurrent requests keep using
                // HTTP until the probe succeeds or this lease expires.
                entry.open_until = Some(now + probe_lease);
                entry.consecutive_failures = WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD - 1;
                entry.updated_at = now;
                false
            }
            None => false,
        }
    }

    fn record_failure(&mut self, key: &str, now: Instant) -> bool {
        if !self.entries.contains_key(key)
            && self.entries.len() >= MAX_CIRCUIT_ENTRIES
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.updated_at)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }

        let entry = self.entries.entry(key.to_string()).or_insert(CircuitEntry {
            consecutive_failures: 0,
            open_until: None,
            updated_at: now,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.updated_at = now;
        if entry.consecutive_failures >= WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD {
            entry.consecutive_failures = WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD;
            entry.open_until = Some(now + Duration::from_millis(WEBSOCKET_CIRCUIT_COOLDOWN_MS));
            return true;
        }
        false
    }

    fn record_success(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

static WS_CIRCUIT_BREAKER: once_cell::sync::Lazy<Mutex<WebSocketCircuitBreaker>> =
    once_cell::sync::Lazy::new(|| Mutex::new(WebSocketCircuitBreaker::default()));

pub fn clear_codex_websocket_pool_for_tests() {
    let mut guard = WS_POOL.lock().unwrap();
    for idle in guard.values() {
        idle.connection.invalidate();
    }
    guard.clear();
    drop(guard);
    WS_CIRCUIT_BREAKER.lock().unwrap().entries.clear();
}

#[cfg(test)]
pub(super) fn codex_websocket_pool_contains_for_tests(session_id: &str) -> bool {
    WS_POOL.lock().unwrap().contains_key(session_id)
}

#[cfg(test)]
fn codex_websocket_pool_len_for_tests() -> usize {
    WS_POOL.lock().unwrap().len()
}

pub(super) fn codex_websocket_circuit_open(key: &str) -> bool {
    let timeouts = CodexWebSocketTimeouts::configured();
    let probe_lease = Duration::from_millis(
        timeouts
            .connect_ms
            .saturating_add(timeouts.pool_probe_ms)
            .saturating_add(timeouts.response_start_ms)
            .saturating_add(1_000),
    );
    WS_CIRCUIT_BREAKER
        .lock()
        .unwrap()
        .should_route_http(key, Instant::now(), probe_lease)
}

pub(super) fn record_codex_websocket_failure(key: &str) -> bool {
    WS_CIRCUIT_BREAKER
        .lock()
        .unwrap()
        .record_failure(key, Instant::now())
}

pub(super) fn record_codex_websocket_success(key: &str) {
    WS_CIRCUIT_BREAKER.lock().unwrap().record_success(key);
}

pub fn invalidate_codex_websocket_pool_key(session_id: &str) {
    let mut guard = WS_POOL.lock().unwrap();
    if let Some(idle) = guard.remove(session_id) {
        idle.connection.invalidate();
    }
}

pub fn invalidate_codex_websocket_pool_turn(session_id: &str, turn_id: Option<u64>) {
    super::continuation::with_current_turn(Some(session_id), turn_id, || {
        invalidate_codex_websocket_pool_key(session_id)
    });
}

fn invalidate_pool_entry(session_id: &str, entry: &Arc<PoolEntry>) {
    // The entry may already have been checked out by the next turn while this turn's
    // post-terminal barrier is still running. Mark it invalid before touching the map so the
    // next owner cannot send a continuation request after it acquires the socket lock.
    entry.invalidate();
    let mut guard = WS_POOL.lock().unwrap();
    if guard
        .get(session_id)
        .is_some_and(|pooled| Arc::ptr_eq(&pooled.connection, entry))
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

fn pool_take_for_turn(key: &str, turn_id: Option<u64>) -> Option<Arc<PoolEntry>> {
    let idle_ttl_ms = WebSocketPoolConfig::configured().idle_ttl_ms;
    super::continuation::if_current_turn(Some(key), turn_id, || {
        let mut guard = WS_POOL.lock().ok()?;
        let idle = guard.remove(key)?;
        if idle_pool_entry_expired(&idle, now_ms(), idle_ttl_ms) || !idle.connection.is_valid() {
            idle.connection.invalidate();
            return None;
        }
        Some(idle.connection)
    })
    .flatten()
}

fn pool_insert_for_turn(key: String, entry: Arc<PoolEntry>, turn_id: Option<u64>) {
    let session_id = key.clone();
    super::continuation::with_current_turn(Some(&session_id), turn_id, || pool_insert(key, entry));
}

fn pool_insert(key: String, entry: Arc<PoolEntry>) {
    if !entry.is_valid() {
        return;
    }
    let config = WebSocketPoolConfig::configured();
    let now = now_ms();
    {
        let mut guard = WS_POOL.lock().unwrap();
        guard.insert(
            key,
            IdlePoolEntry {
                connection: entry,
                idle_since_ms: now,
            },
        );
        reap_idle_pool(&mut guard, now, config);
    }
    ensure_pool_reaper_started();
}

fn idle_pool_entry_expired(entry: &IdlePoolEntry, now_ms: u64, idle_ttl_ms: u64) -> bool {
    now_ms.saturating_sub(entry.idle_since_ms) >= idle_ttl_ms
}

fn reap_idle_pool(
    pool: &mut HashMap<String, IdlePoolEntry>,
    now_ms: u64,
    config: WebSocketPoolConfig,
) {
    pool.retain(|_, entry| {
        let retain = entry.connection.is_valid()
            && !idle_pool_entry_expired(entry, now_ms, config.idle_ttl_ms);
        if !retain {
            entry.connection.invalidate();
        }
        retain
    });
    while pool.len() > config.max_idle_entries {
        let Some(oldest_key) = pool
            .iter()
            .min_by(|(left_key, left), (right_key, right)| {
                left.idle_since_ms
                    .cmp(&right.idle_since_ms)
                    .then_with(|| left_key.cmp(right_key))
            })
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(entry) = pool.remove(&oldest_key) {
            entry.connection.invalidate();
        }
    }
}

struct PoolReaperRunningGuard;

impl Drop for PoolReaperRunningGuard {
    fn drop(&mut self) {
        WS_POOL_REAPER_RUNNING.store(false, Ordering::Release);
    }
}

fn ensure_pool_reaper_started() {
    if WS_POOL_REAPER_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        WS_POOL_REAPER_RUNNING.store(false, Ordering::Release);
        return;
    };
    let running = PoolReaperRunningGuard;
    runtime.spawn(async move {
        // Capture the guard before the task is first polled so dropping an
        // unstarted task during runtime shutdown still clears the flag.
        let _running = running;
        loop {
            let configured = WebSocketPoolConfig::configured();
            let interval_ms = (configured.idle_ttl_ms / 2)
                .clamp(MIN_POOL_REAPER_INTERVAL_MS, MAX_POOL_REAPER_INTERVAL_MS);
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;

            let configured = WebSocketPoolConfig::configured();
            let mut guard = WS_POOL.lock().unwrap();
            reap_idle_pool(&mut guard, now_ms(), configured);
        }
    });
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

// ---------------------------------------------------------------------------
// Main request function
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn codex_websocket_request(
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<&TrafficCapture>,
    pool_key: Option<&str>,
    timeouts: CodexWebSocketTimeouts,
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
        pool_take_for_turn(key, continuation.and_then(|candidate| candidate.turn_id))
    });

    if let Some(entry) = pooled {
        let lock = tokio::time::timeout(
            Duration::from_millis(timeouts.pool_probe_ms),
            entry.ws.lock(),
        )
        .await;
        let mut ws_guard = match lock {
            Ok(guard) => guard,
            Err(_) => {
                // A cancelled or superseded turn can retain this lock. Detach
                // its entry so the coordinator can retry on a fresh socket.
                invalidate_pool_owner(pool_key, Some(&entry));
                let err = pool_busy_error(timeouts.pool_probe_ms);
                log_websocket_pool_refresh(ctx, &err.message);
                return Err(err);
            }
        };
        if !entry.is_valid() {
            return Err(pool_healthcheck_error(
                "connection was invalidated while waiting for its previous turn".to_string(),
            ));
        }
        let freshly_probed =
            entry.consume_fresh_probe(Duration::from_millis(timeouts.pool_probe_ms));
        if !freshly_probed
            && let Err(err) = probe_pooled_connection(&mut ws_guard, timeouts.pool_probe_ms).await
        {
            invalidate_pool_owner(pool_key, Some(&entry));
            log_websocket_pool_refresh(ctx, &err.message);
            return Err(err);
        }

        if let Err(err) = send_ws_frame(
            &mut ws_guard,
            Message::Text(body_json.clone()),
            timeouts.pong_ms,
            "request",
            WEBSOCKET_CONNECTION_ERROR_DETAIL,
        )
        .await
        {
            invalidate_pool_owner(pool_key, Some(&entry));
            return Err(err);
        }

        let (sse_body, terminal_event) =
            collect_ws_events(&mut ws_guard, timeouts, pool_key, Some(&entry), traffic).await?;
        let Some(terminal_event) = terminal_event else {
            return Err(missing_terminal_error());
        };

        if is_previous_response_missing(&terminal_event.payload) {
            return Err(CodexError {
                status: 0,
                message: "Previous response not found".to_string(),
                detail: Some("previous_response_not_found".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::WebSocket,
            });
        }

        let status = if terminal_event.kind.is_failure() {
            event_error_status(&terminal_event.payload).unwrap_or(500)
        } else {
            200
        };
        let reusable = terminal_event.connection_reusable && terminal_event.kind.is_reusable();
        drop(ws_guard);
        if reusable && let Some(key) = pool_key {
            pool_insert_for_turn(
                key.to_string(),
                entry.clone(),
                continuation.and_then(|candidate| candidate.turn_id),
            );
        }

        if let Some(tc) = traffic {
            write_websocket_metadata_capture(tc, &ws_url, pool_key, continuation, true);
            write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
        }

        return Ok(CodexResponse {
            body: sse_body,
            status,
            headers: vec![],
        });
    }

    let (ws_stream, _response) =
        connect_with_timeout(&ws_url, headers, timeouts.connect_ms).await?;

    // New connection path (not pooled or pool miss)
    let entry = Arc::new(PoolEntry::new(ws_stream));

    // Send the request
    let msg = Message::Text(body_json);
    {
        let mut ws_guard = entry.ws.lock().await;
        send_ws_frame(
            &mut ws_guard,
            msg,
            timeouts.pong_ms,
            "request",
            WEBSOCKET_CONNECTION_ERROR_DETAIL,
        )
        .await?;

        let (sse_body, terminal_event) =
            collect_ws_events(&mut ws_guard, timeouts, pool_key, Some(&entry), traffic).await?;
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

        let status = if terminal_event.kind.is_failure() {
            event_error_status(&terminal_event.payload).unwrap_or(500)
        } else {
            200
        };
        let reusable = terminal_event.connection_reusable && terminal_event.kind.is_reusable();
        drop(ws_guard);
        if reusable && let Some(key) = pool_key {
            pool_insert_for_turn(
                key.to_string(),
                entry.clone(),
                continuation.and_then(|candidate| candidate.turn_id),
            );
        }

        // Write traffic metadata
        if let Some(tc) = traffic {
            write_websocket_metadata_capture(tc, &ws_url, pool_key, continuation, false);
            write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
        }

        Ok(CodexResponse {
            body: sse_body,
            status,
            headers: vec![],
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn codex_websocket_event_stream(
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<Arc<TrafficCapture>>,
    pool_key: Option<&str>,
    timeouts: CodexWebSocketTimeouts,
    continuation: Option<&ContinuationCandidate>,
) -> Result<CodexWebSocketEventReceiver, CodexError> {
    let ws_url = to_websocket_url(url).map_err(|e| CodexError {
        status: 0,
        message: e.message,
        detail: None,
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    })?;
    let body_json = serde_json::to_string(body_value).unwrap_or_default();
    if let Some(tc) = traffic.as_deref() {
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

    let pooled = pool_key.and_then(|key| {
        pool_take_for_turn(key, continuation.and_then(|candidate| candidate.turn_id))
    });
    let used_pooled = pooled.is_some();
    let entry = if let Some(entry) = pooled {
        entry
    } else {
        let (ws_stream, _) = connect_with_timeout(&ws_url, headers, timeouts.connect_ms).await?;
        Arc::new(PoolEntry::new(ws_stream))
    };

    if let Some(tc) = traffic.as_deref() {
        write_websocket_metadata_capture(tc, &ws_url, pool_key, continuation, used_pooled);
    }

    let (tx, rx) = mpsc::channel(super::LIVE_EVENT_CHANNEL_CAPACITY);
    let turn_id = continuation.and_then(|candidate| candidate.turn_id);
    let pool_key = pool_key.map(str::to_string);
    let ws = entry.ws.clone();
    let ctx = ctx.clone();
    let cancel_tx = tx.clone();
    let cancel_pool_key = pool_key.clone();
    let cancel_entry = entry.clone();
    tokio::spawn(async move {
        let work = async move {
            let lock_timeout_ms = if used_pooled {
                timeouts.pool_probe_ms
            } else {
                timeouts.pong_ms
            };
            let lock =
                tokio::time::timeout(Duration::from_millis(lock_timeout_ms), ws.lock_owned()).await;
            let mut ws_guard = match lock {
                Ok(guard) => guard,
                Err(_) => {
                    // Do not let an abandoned reader block the next turn until the
                    // business idle timeout; its Arc remains valid while detached.
                    invalidate_pool_owner(pool_key.as_deref(), Some(&entry));
                    let err = pool_busy_error(lock_timeout_ms);
                    log_websocket_pool_refresh(&ctx, &err.message);
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            };
            if !entry.is_valid() {
                let err = pool_healthcheck_error(
                    "connection was invalidated while waiting for its previous turn".to_string(),
                );
                log_websocket_pool_refresh(&ctx, &err.message);
                let _ = tx.send(Err(err)).await;
                return;
            }
            let freshly_probed = used_pooled
                && entry.consume_fresh_probe(Duration::from_millis(timeouts.pool_probe_ms));
            if used_pooled
                && !freshly_probed
                && let Err(err) =
                    probe_pooled_connection(&mut ws_guard, timeouts.pool_probe_ms).await
            {
                invalidate_pool_owner(pool_key.as_deref(), Some(&entry));
                log_websocket_pool_refresh(&ctx, &err.message);
                let _ = tx.send(Err(err)).await;
                return;
            }
            if let Err(err) = send_ws_frame(
                &mut ws_guard,
                Message::Text(body_json),
                timeouts.pong_ms,
                "request",
                WEBSOCKET_CONNECTION_ERROR_DETAIL,
            )
            .await
            {
                invalidate_pool_owner(pool_key.as_deref(), Some(&entry));
                let _ = tx.send(Err(err)).await;
                return;
            }

            let reusable = stream_ws_events(
                &mut ws_guard,
                timeouts,
                pool_key.as_deref(),
                Some(&entry),
                turn_id,
                traffic,
                tx,
            )
            .await;

            // A reusable connection is published exactly once, at the terminal
            // event before it is forwarded downstream. Publishing it again here
            // can race with an immediate next turn that has already checked out
            // the same Arc, falsely marking an active socket as idle.
            if !reusable && let Some(key) = pool_key.as_deref() {
                invalidate_pool_entry(key, &entry);
            }
        };
        tokio::select! {
            _ = cancel_tx.closed() => {
                invalidate_pool_owner(cancel_pool_key.as_deref(), Some(&cancel_entry));
            }
            _ = work => {}
        }
    });
    Ok(rx)
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

fn malformed_event_error(error: &serde_json::Error) -> CodexError {
    CodexError {
        status: 0,
        message: format!("Codex WebSocket event contained malformed JSON: {error}"),
        detail: Some(WEBSOCKET_MALFORMED_EVENT_DETAIL.to_string()),
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

fn response_idle_timeout_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket idle timeout after {timeout_ms}ms"),
        detail: Some(WEBSOCKET_IDLE_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn heartbeat_timeout_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket heartbeat timed out after {timeout_ms}ms without a Pong"),
        detail: Some(WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn pool_healthcheck_error(reason: String) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket pooled connection health check failed: {reason}"),
        detail: Some(WEBSOCKET_POOL_HEALTHCHECK_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn terminal_barrier_error(reason: String) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket terminal ordering barrier failed: {reason}"),
        detail: Some(WEBSOCKET_TERMINAL_BARRIER_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn pool_busy_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket pooled connection remained busy for {timeout_ms}ms"),
        detail: Some(WEBSOCKET_POOL_BUSY_DETAIL.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn connection_error(message: String, detail: &str) -> CodexError {
    CodexError {
        status: 0,
        message,
        detail: Some(detail.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

async fn send_ws_frame(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    frame: Message,
    timeout_ms: u64,
    description: &str,
    detail: &str,
) -> Result<(), CodexError> {
    match tokio::time::timeout(Duration::from_millis(timeout_ms), ws.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(connection_error(
            format!("WebSocket {description} send error: {err}"),
            detail,
        )),
        Err(_) => Err(connection_error(
            format!("WebSocket {description} send timeout after {timeout_ms}ms"),
            detail,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketPingBarrierOutcome {
    MatchingPong,
    OrderedClose,
}

async fn websocket_ping_barrier<F, E>(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeout_ms: u64,
    description: &str,
    mut inspect_text: F,
    make_error: E,
) -> Result<WebSocketPingBarrierOutcome, CodexError>
where
    F: FnMut(&str) -> Result<(), CodexError>,
    E: Fn(String) -> CodexError,
{
    let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
    send_ws_frame(
        ws,
        Message::Ping(nonce.clone()),
        timeout_ms,
        &format!("{description} Ping"),
        WEBSOCKET_POOL_HEALTHCHECK_DETAIL,
    )
    .await
    .map_err(|error| make_error(error.message))?;

    let started_at = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    loop {
        let Some(remaining) = timeout.checked_sub(started_at.elapsed()) else {
            return Err(make_error(format!(
                "no matching Pong within {timeout_ms}ms"
            )));
        };
        if remaining.is_zero() {
            return Err(make_error(format!(
                "no matching Pong within {timeout_ms}ms"
            )));
        }

        let frame = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| make_error(format!("no matching Pong within {timeout_ms}ms")))?;
        match frame {
            Some(Ok(Message::Pong(payload))) if payload == nonce => {
                return Ok(WebSocketPingBarrierOutcome::MatchingPong);
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Ok(Message::Ping(payload))) => {
                send_ws_frame(
                    ws,
                    Message::Pong(payload),
                    timeout_ms,
                    &format!("{description} Pong"),
                    WEBSOCKET_POOL_HEALTHCHECK_DETAIL,
                )
                .await
                .map_err(|error| make_error(error.message))?;
            }
            Some(Ok(Message::Text(text))) => inspect_text(&text)?,
            Some(Ok(Message::Binary(_))) => {
                return Err(make_error("unexpected binary frame".to_string()));
            }
            // A successfully decoded Close frame or clean stream exhaustion
            // is itself an ordering fence: every preceding server frame has
            // already been delivered. Terminal callers may commit their
            // staged response, but the connection must not return to the pool.
            Some(Ok(Message::Close(_))) | None => {
                return Ok(WebSocketPingBarrierOutcome::OrderedClose);
            }
            Some(Err(err)) => {
                return Err(make_error(format!("stream error: {err}")));
            }
        }
    }
}

async fn probe_pooled_connection(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeout_ms: u64,
) -> Result<(), CodexError> {
    let outcome = websocket_ping_barrier(
        ws,
        timeout_ms,
        "pooled health-check",
        |text| {
            let informational = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|payload| {
                    payload
                        .get("type")
                        .and_then(|value| value.as_str())
                        .map(|event_type| matches!(event_type, "codex.rate_limits" | "keepalive"))
                })
                .unwrap_or(false);
            if informational {
                Ok(())
            } else {
                Err(pool_healthcheck_error(
                    "unexpected data arrived before the next request".to_string(),
                ))
            }
        },
        pool_healthcheck_error,
    )
    .await?;
    match outcome {
        WebSocketPingBarrierOutcome::MatchingPong => Ok(()),
        WebSocketPingBarrierOutcome::OrderedClose => Err(pool_healthcheck_error(
            "connection closed during health check".to_string(),
        )),
    }
}

async fn confirm_terminal_ordering_barrier<F>(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeout_ms: u64,
    max_event_bytes: usize,
    traffic: Option<&TrafficCapture>,
    mut capture_text: F,
) -> Result<WebSocketPingBarrierOutcome, CodexError>
where
    F: FnMut(&str) -> Result<(), CodexError>,
{
    websocket_ping_barrier(
        ws,
        timeout_ms,
        "terminal ordering-barrier",
        |text| {
            if text.len() > max_event_bytes {
                return Err(terminal_barrier_error(format!(
                    "post-terminal event exceeded the {max_event_bytes}-byte limit"
                )));
            }
            capture_text(text)?;
            let parsed = serde_json::from_str::<serde_json::Value>(text).map_err(|error| {
                if let Some(traffic) = traffic {
                    traffic.write_json_event(
                        "040-upstream-event",
                        &serde_json::json!({
                            "unparseable": true,
                            "data": text,
                        }),
                    );
                }
                terminal_barrier_error(format!(
                    "post-terminal event contained malformed JSON: {error}"
                ))
            })?;
            if let Some(traffic) = traffic {
                traffic.write_json_event("040-upstream-event", &parsed);
            }
            let event_type = parsed.get("type").and_then(serde_json::Value::as_str);
            if matches!(event_type, Some("codex.rate_limits" | "keepalive")) {
                Ok(())
            } else {
                Err(terminal_barrier_error(format!(
                    "unexpected post-terminal event {event_type:?}"
                )))
            }
        },
        terminal_barrier_error,
    )
    .await
}

fn log_websocket_pool_refresh(ctx: &RequestContext, reason: &str) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("reason".into(), serde_json::json!(reason));
    crate::logging::create_logger("codex").info("websocket_pool_refresh", Some(fields));
}

fn log_websocket_terminal_probe_failure(error: &CodexError) {
    let fields = serde_json::Map::from_iter([
        ("reason".into(), serde_json::json!(error.message)),
        ("detail".into(), serde_json::json!(error.detail)),
    ]);
    crate::logging::create_logger("codex")
        .warn("websocket_terminal_pool_probe_failed", Some(fields));
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

    let websocket_config = WebSocketConfig {
        max_message_size: Some(super::MAX_LIVE_EVENT_BYTES),
        max_frame_size: Some(super::MAX_LIVE_EVENT_BYTES),
        ..WebSocketConfig::default()
    };
    // Model request and Ping/Pong frames are small and latency-sensitive. Disable
    // Nagle so a direct Linux/WSL connection does not wait for a delayed ACK.
    let connect_fut = connect_async_with_config(request, Some(websocket_config), true);
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
                    let detail = response
                        .body()
                        .as_ref()
                        .and_then(|body| String::from_utf8(body.clone()).ok())
                        .filter(|body| !body.trim().is_empty());
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

struct WebSocketWatchdog {
    response_start_timeout: Duration,
    idle_timeout: Duration,
    heartbeat_interval: Duration,
    pong_timeout: Duration,
    response_wait_started: Instant,
    last_response_event_at: Instant,
    response_started: bool,
    next_ping_at: Instant,
    pending_pong: Option<(Vec<u8>, Instant)>,
}

enum WatchdogWake {
    SendPing(Vec<u8>),
    Timeout(CodexError),
    Wait,
}

impl WebSocketWatchdog {
    fn new(timeouts: CodexWebSocketTimeouts) -> Self {
        let now = Instant::now();
        Self {
            response_start_timeout: Duration::from_millis(timeouts.response_start_ms),
            idle_timeout: Duration::from_millis(timeouts.idle_ms),
            heartbeat_interval: Duration::from_millis(timeouts.heartbeat_interval_ms),
            pong_timeout: Duration::from_millis(timeouts.pong_ms),
            response_wait_started: now,
            last_response_event_at: now,
            response_started: false,
            next_ping_at: now
                .checked_add(Duration::from_millis(timeouts.heartbeat_interval_ms))
                .unwrap_or(now),
            pending_pong: None,
        }
    }

    fn response_deadline(&self) -> Instant {
        let (started_at, timeout) = if self.response_started {
            (self.last_response_event_at, self.idle_timeout)
        } else {
            (self.response_wait_started, self.response_start_timeout)
        };
        started_at.checked_add(timeout).unwrap_or(started_at)
    }

    fn next_wait(&self) -> Duration {
        let now = Instant::now();
        let mut deadline = self.response_deadline();
        let liveness_deadline = self
            .pending_pong
            .as_ref()
            .map(|(_, deadline)| *deadline)
            .unwrap_or(self.next_ping_at);
        if liveness_deadline < deadline {
            deadline = liveness_deadline;
        }
        deadline.saturating_duration_since(now)
    }

    fn on_timer(&mut self) -> WatchdogWake {
        let now = Instant::now();
        if now >= self.response_deadline() {
            return WatchdogWake::Timeout(if self.response_started {
                response_idle_timeout_error(self.idle_timeout.as_millis() as u64)
            } else {
                response_start_timeout_error(self.response_start_timeout.as_millis() as u64)
            });
        }
        if let Some((_, deadline)) = self.pending_pong.as_ref()
            && now >= *deadline
        {
            return WatchdogWake::Timeout(heartbeat_timeout_error(
                self.pong_timeout.as_millis() as u64
            ));
        }
        if self.pending_pong.is_none() && now >= self.next_ping_at {
            let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
            return WatchdogWake::SendPing(nonce);
        }
        WatchdogWake::Wait
    }

    fn note_ping_sent(&mut self, nonce: Vec<u8>) {
        let now = Instant::now();
        let deadline = now.checked_add(self.pong_timeout).unwrap_or(now);
        self.pending_pong = Some((nonce, deadline));
    }

    fn note_inbound_activity(&mut self) {
        let now = Instant::now();
        self.pending_pong = None;
        self.next_ping_at = now.checked_add(self.heartbeat_interval).unwrap_or(now);
    }

    fn note_pong(&mut self, payload: &[u8]) {
        if self
            .pending_pong
            .as_ref()
            .is_some_and(|(expected, _)| expected == payload)
        {
            self.note_inbound_activity();
        }
    }

    fn note_response_event(&mut self) {
        self.response_started = true;
        self.last_response_event_at = Instant::now();
    }

    async fn next_frame(
        &mut self,
        ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<Option<Message>, CodexError> {
        loop {
            let frame = tokio::time::timeout(self.next_wait(), ws.next()).await;
            match frame {
                Err(_) => match self.on_timer() {
                    WatchdogWake::SendPing(nonce) => {
                        send_ws_frame(
                            ws,
                            Message::Ping(nonce.clone()),
                            self.pong_timeout.as_millis() as u64,
                            "heartbeat Ping",
                            WEBSOCKET_CONNECTION_ERROR_DETAIL,
                        )
                        .await?;
                        self.note_ping_sent(nonce);
                    }
                    WatchdogWake::Timeout(err) => return Err(err),
                    WatchdogWake::Wait => {}
                },
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    self.note_inbound_activity();
                    send_ws_frame(
                        ws,
                        Message::Pong(payload),
                        self.pong_timeout.as_millis() as u64,
                        "Pong",
                        WEBSOCKET_CONNECTION_ERROR_DETAIL,
                    )
                    .await?;
                }
                Ok(Some(Ok(Message::Pong(payload)))) => self.note_pong(&payload),
                Ok(Some(Ok(Message::Frame(_)))) => self.note_inbound_activity(),
                Ok(Some(Ok(Message::Text(text)))) => {
                    self.note_inbound_activity();
                    return Ok(Some(Message::Text(text)));
                }
                Ok(Some(Ok(Message::Binary(data)))) => {
                    self.note_inbound_activity();
                    return Ok(Some(Message::Binary(data)));
                }
                Ok(Some(Ok(Message::Close(frame)))) => return Ok(Some(Message::Close(frame))),
                Ok(Some(Err(err))) => {
                    return Err(connection_error(
                        format!("WebSocket stream error: {err}"),
                        WEBSOCKET_CONNECTION_ERROR_DETAIL,
                    ));
                }
                Ok(None) => return Ok(None),
            }
        }
    }
}

#[derive(Debug)]
struct WsEvent {
    kind: super::events::CodexTerminalKind,
    payload: serde_json::Value,
    connection_reusable: bool,
}

async fn collect_ws_events(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeouts: CodexWebSocketTimeouts,
    pool_key: Option<&str>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError> {
    let mut sse_body: Vec<u8> = Vec::new();
    let mut terminal_event: Option<WsEvent> = None;
    let mut watchdog = WebSocketWatchdog::new(timeouts);

    loop {
        let frame = watchdog.next_frame(ws).await.inspect_err(|_| {
            invalidate_pool_owner(pool_key, pool_entry);
        })?;

        match frame {
            Some(Message::Text(text)) => {
                if text.len() > crate::traffic::MAX_STREAM_CAPTURE_EVENT_BYTES {
                    invalidate_pool_owner(pool_key, pool_entry);
                    return Err(CodexError {
                        status: 0,
                        message: "Codex WebSocket event exceeded the buffered response limit"
                            .to_string(),
                        detail: Some("websocket_event_size_limit".to_string()),
                        retry_after: None,
                        origin: CodexErrorOrigin::WebSocket,
                    });
                }
                // Parse JSON
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(error) => {
                        if let Some(tc) = traffic {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        invalidate_pool_owner(pool_key, pool_entry);
                        return Err(malformed_event_error(&error));
                    }
                };

                // Convert to SSE bytes
                append_buffered_sse(&mut sse_body, &text).inspect_err(|_| {
                    invalidate_pool_owner(pool_key, pool_entry);
                })?;
                if let Some(tc) = traffic {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    watchdog.note_response_event();
                }

                // Check for terminal events
                if let Some(kind) = super::events::CodexTerminalKind::from_payload(&parsed) {
                    let barrier = confirm_terminal_ordering_barrier(
                        ws,
                        timeouts.pool_probe_ms,
                        crate::traffic::MAX_STREAM_CAPTURE_EVENT_BYTES,
                        traffic,
                        |_| Ok(()),
                    )
                    .await;
                    // The terminal payload is authoritative for the response. A failed
                    // post-terminal probe only means this socket is unsafe to reuse; it must not
                    // turn an otherwise complete response into a transport error.
                    let connection_reusable =
                        matches!(&barrier, Ok(WebSocketPingBarrierOutcome::MatchingPong));
                    if connection_reusable && let Some(entry) = pool_entry {
                        entry.mark_terminal_probe_success();
                    }
                    if let Err(error) = &barrier {
                        log_websocket_terminal_probe_failure(error);
                    }
                    if !connection_reusable {
                        invalidate_pool_owner(pool_key, pool_entry);
                    }
                    terminal_event = Some(WsEvent {
                        kind,
                        payload: parsed,
                        connection_reusable,
                    });
                    break;
                }
            }
            Some(Message::Binary(_)) => {
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
            Some(Message::Close(_)) => {
                // Connection closed - invalidate pool
                invalidate_pool_owner(pool_key, pool_entry);
                break;
            }
            None => {
                // Stream ended - invalidate pool
                invalidate_pool_owner(pool_key, pool_entry);
                break;
            }
            Some(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
        }
    }

    Ok((sse_body, terminal_event))
}

fn append_buffered_sse(buffer: &mut Vec<u8>, text: &str) -> Result<(), CodexError> {
    let encoded = encode_sse(text);
    if buffer.len().saturating_add(encoded.len()) > crate::traffic::MAX_SSE_CAPTURE_BYTES {
        return Err(CodexError {
            status: 0,
            message: "Codex buffered WebSocket response exceeded the size limit".to_string(),
            detail: Some("websocket_response_size_limit".to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        });
    }
    buffer.extend_from_slice(&encoded);
    Ok(())
}

async fn stream_ws_events(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeouts: CodexWebSocketTimeouts,
    pool_key: Option<&str>,
    pool_entry: Option<&Arc<PoolEntry>>,
    turn_id: Option<u64>,
    traffic: Option<Arc<TrafficCapture>>,
    tx: mpsc::Sender<Result<serde_json::Value, CodexError>>,
) -> bool {
    let started_at = Instant::now();
    let mut sse_body = traffic.as_ref().map(|_| Vec::new());
    let mut watchdog = WebSocketWatchdog::new(timeouts);
    let mut status = 200u16;
    let mut reusable = false;

    loop {
        let frame = tokio::select! {
            _ = tx.closed() => {
                invalidate_pool_owner(pool_key, pool_entry);
                break;
            }
            result = watchdog.next_frame(ws) => match result {
                Ok(frame) => frame,
                Err(err) => {
                    invalidate_pool_owner(pool_key, pool_entry);
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        };

        match frame {
            Some(Message::Text(text)) => {
                if text.len() > super::MAX_LIVE_EVENT_BYTES {
                    invalidate_pool_owner(pool_key, pool_entry);
                    let _ = tx
                        .send(Err(CodexError {
                            status: 0,
                            message: "Codex WebSocket event exceeded the live event size limit"
                                .to_string(),
                            detail: Some("websocket_event_size_limit".to_string()),
                            retry_after: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }))
                        .await;
                    break;
                }
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(error) => {
                        if let Some(tc) = traffic.as_deref() {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        append_live_capture_sse(&mut sse_body, &text);
                        invalidate_pool_owner(pool_key, pool_entry);
                        let _ = tx.send(Err(malformed_event_error(&error))).await;
                        break;
                    }
                };

                append_live_capture_sse(&mut sse_body, &text);
                if let Some(tc) = traffic.as_deref() {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    watchdog.note_response_event();
                }

                let terminal_kind = super::events::CodexTerminalKind::from_payload(&parsed);
                if terminal_kind.is_some_and(super::events::CodexTerminalKind::is_failure) {
                    status = event_error_status(&parsed).unwrap_or(500);
                }
                let terminal = terminal_kind.is_some();
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
                let terminal_kind_reusable =
                    terminal_kind.is_some_and(super::events::CodexTerminalKind::is_reusable);
                if terminal_kind_reusable && let (Some(key), Some(entry)) = (pool_key, pool_entry) {
                    // Publish the still-locked connection as a busy reservation before Claude
                    // sees message_stop. An immediate next turn can check it out and wait for this
                    // turn's bounded ordering probe instead of racing onto a fresh socket.
                    pool_insert_for_turn(key.to_string(), entry.clone(), turn_id);
                }
                // Forward the authoritative terminal event before probing the connection. Claude
                // can finish the response immediately even if a weak network drops the socket
                // during the subsequent pool-reuse check.
                if tx.send(Ok(parsed)).await.is_err() {
                    invalidate_pool_owner(pool_key, pool_entry);
                    break;
                }
                if terminal {
                    let barrier = confirm_terminal_ordering_barrier(
                        ws,
                        timeouts.pool_probe_ms,
                        super::MAX_LIVE_EVENT_BYTES,
                        traffic.as_deref(),
                        |text| {
                            append_live_capture_sse(&mut sse_body, text);
                            Ok(())
                        },
                    )
                    .await;
                    let connection_reusable =
                        matches!(&barrier, Ok(WebSocketPingBarrierOutcome::MatchingPong));
                    if connection_reusable && let Some(entry) = pool_entry {
                        entry.mark_terminal_probe_success();
                    }
                    if let Err(error) = &barrier {
                        log_websocket_terminal_probe_failure(error);
                    }
                    if !connection_reusable {
                        invalidate_pool_owner(pool_key, pool_entry);
                    }
                    reusable = connection_reusable && terminal_kind_reusable;
                    break;
                }
            }
            Some(Message::Binary(_)) => {
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
            Some(Message::Close(_)) | None => {
                invalidate_pool_owner(pool_key, pool_entry);
                let _ = tx.send(Err(missing_terminal_error())).await;
                break;
            }
            Some(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
        }
    }

    if let (Some(tc), Some(sse_body)) = (traffic.as_deref(), sse_body.as_deref()) {
        write_websocket_response_capture(tc, status, started_at.elapsed(), sse_body);
    }
    reusable
}

fn append_live_capture_sse(capture: &mut Option<Vec<u8>>, text: &str) {
    let Some(capture) = capture.as_mut() else {
        return;
    };
    let encoded = encode_sse(text);
    if capture.len().saturating_add(encoded.len()) <= crate::traffic::MAX_SSE_CAPTURE_BYTES {
        capture.extend_from_slice(&encoded);
    }
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

    fn idle_pool_entry(
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        idle_since_ms: u64,
    ) -> IdlePoolEntry {
        IdlePoolEntry {
            connection: Arc::new(PoolEntry::new(stream)),
            idle_since_ms,
        }
    }

    fn test_context() -> RequestContext {
        RequestContext {
            req_id: "codex-websocket-test".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        }
    }

    fn continuation_request(
        messages: &[&str],
    ) -> super::super::translate::request::ResponsesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-sol",
            "input": messages.iter().map(|text| serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}]
            })).collect::<Vec<_>>(),
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {}
        }))
        .unwrap()
    }

    async fn buffered_terminal_while_peer_stays_open(event: &'static str) -> CodexResponse {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(event.into())).await.unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                if let Message::Ping(payload) = frame {
                    ws.send(Message::Pong(payload)).await.unwrap();
                    futures_util::future::pending::<()>().await;
                }
            }
        });
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            codex_websocket_request(
                &format!("http://{addr}/responses"),
                &HeaderMap::new(),
                &serde_json::json!({"type":"response.create","input":[]}),
                &test_context(),
                None,
                None,
                test_timeouts(2_000, 2_000),
                None,
            ),
        )
        .await;
        server.abort();
        result
            .expect("a terminal event must finish before the peer closes")
            .unwrap()
    }

    fn test_timeouts(response_start_ms: u64, idle_ms: u64) -> CodexWebSocketTimeouts {
        CodexWebSocketTimeouts {
            connect_ms: 1_000,
            response_start_ms,
            idle_ms,
            heartbeat_interval_ms: 1_000,
            pong_ms: 100,
            pool_probe_ms: 100,
        }
    }

    #[test]
    fn websocket_circuit_opens_cools_down_and_resets() {
        let mut breaker = WebSocketCircuitBreaker::default();
        let now = Instant::now();
        let probe_lease = Duration::from_secs(90);

        assert!(!breaker.should_route_http("wss://example.test", now, probe_lease));
        assert!(!breaker.record_failure("wss://example.test", now));
        assert!(!breaker.record_failure("wss://example.test", now));
        assert!(breaker.record_failure("wss://example.test", now));
        assert!(breaker.should_route_http(
            "wss://example.test",
            now + Duration::from_millis(WEBSOCKET_CIRCUIT_COOLDOWN_MS - 1),
            probe_lease,
        ));

        let probe_at = now + Duration::from_millis(WEBSOCKET_CIRCUIT_COOLDOWN_MS);
        assert!(!breaker.should_route_http("wss://example.test", probe_at, probe_lease));
        assert!(breaker.should_route_http("wss://example.test", probe_at, probe_lease));

        let next_probe_at = probe_at + probe_lease;
        assert!(!breaker.should_route_http("wss://example.test", next_probe_at, probe_lease));
        assert!(breaker.record_failure("wss://example.test", next_probe_at));
        breaker.record_success("wss://example.test");
        assert!(!breaker.should_route_http("wss://example.test", next_probe_at, probe_lease));
    }

    #[test]
    fn websocket_circuit_isolates_keys_and_bounds_entries() {
        let mut breaker = WebSocketCircuitBreaker::default();
        let now = Instant::now();
        for _ in 0..WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD {
            breaker.record_failure("wss://a.example", now);
        }
        let probe_lease = Duration::from_secs(90);
        assert!(breaker.should_route_http("wss://a.example", now, probe_lease));
        assert!(!breaker.should_route_http("wss://b.example", now, probe_lease));

        for index in 0..=MAX_CIRCUIT_ENTRIES {
            breaker.record_failure(
                &format!("wss://endpoint-{index}.example"),
                now + Duration::from_nanos(index as u64),
            );
        }
        assert!(breaker.entries.len() <= MAX_CIRCUIT_ENTRIES);
    }

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

    #[tokio::test]
    async fn websocket_wire_config_rejects_a_message_over_one_mebibyte() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let oversized = "x".repeat(super::super::MAX_LIVE_EVENT_BYTES + 1);
            let _ = websocket.send(Message::Text(oversized)).await;
        });

        let (mut websocket, _) =
            connect_with_timeout(&format!("ws://{addr}/"), &HeaderMap::new(), 1_000)
                .await
                .unwrap();
        let error = websocket.next().await.unwrap().unwrap_err();
        drop(websocket);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("oversized WebSocket sender should stop after the client closes")
            .unwrap();

        assert!(matches!(error, tungstenite::Error::Capacity(_)));
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
    fn websocket_terminal_detection_uses_shared_classifier() {
        let completed = serde_json::json!({"type": "response.completed"});
        assert!(super::super::events::CodexTerminalKind::from_payload(&completed).is_some());

        let delta = serde_json::json!({"type": "response.output_text.delta"});
        assert!(super::super::events::CodexTerminalKind::from_payload(&delta).is_none());

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(super::super::events::CodexTerminalKind::from_payload(&error).is_some());
    }

    #[tokio::test]
    async fn buffered_response_done_finishes_while_peer_stays_open() {
        let response = buffered_terminal_while_peer_stays_open(
            r#"{"type":"response.done","response":{"id":"resp_done"}}"#,
        )
        .await;

        assert_eq!(response.status, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("response.done"));
    }

    #[tokio::test]
    async fn buffered_response_error_finishes_while_peer_stays_open() {
        let response = buffered_terminal_while_peer_stays_open(
            r#"{"type":"response.error","response":{"error":{"status":503,"message":"failed"}}}"#,
        )
        .await;

        assert_eq!(response.status, 503);
        assert!(String::from_utf8_lossy(&response.body).contains("response.error"));
    }

    #[tokio::test]
    async fn buffered_terminal_survives_pool_probe_timeout() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let pool_key = "buffered-terminal-probe-timeout";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.done","response":{"id":"resp_probe_timeout"}}"#.into(),
            ))
            .await
            .unwrap();
            futures_util::future::pending::<()>().await;
        });
        let mut timeouts = test_timeouts(1_000, 1_000);
        timeouts.pool_probe_ms = 50;

        let response = tokio::time::timeout(
            Duration::from_millis(250),
            codex_websocket_request(
                &format!("http://{addr}/responses"),
                &HeaderMap::new(),
                &serde_json::json!({"type":"response.create","input":[]}),
                &test_context(),
                None,
                Some(pool_key),
                timeouts,
                None,
            ),
        )
        .await
        .expect("a pool probe timeout must not stall the completed response")
        .expect("a pool probe timeout must not replace the terminal response");

        assert_eq!(response.status, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("response.done"));
        assert!(!codex_websocket_pool_contains_for_tests(pool_key));
        server.abort();
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn buffered_terminal_error_survives_clean_close_and_connection_is_not_pooled() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let pool_key = "terminal-clean-close";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.error","response":{"error":{"status":503,"message":"upstream overloaded"}}}"#
                    .into(),
            ))
            .await
            .unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                if matches!(frame, Message::Ping(_)) {
                    ws.close(None).await.unwrap();
                    return;
                }
            }
        });

        let response = codex_websocket_request(
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &serde_json::json!({"type":"response.create","input":[]}),
            &test_context(),
            None,
            Some(pool_key),
            test_timeouts(1_000, 1_000),
            None,
        )
        .await
        .expect("a clean close after terminal must preserve the upstream response");

        assert_eq!(response.status, 503);
        assert!(String::from_utf8_lossy(&response.body).contains("response.error"));
        assert!(!WS_POOL.lock().unwrap().contains_key(pool_key));
        server.await.unwrap();
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
    async fn pool_invalidation() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let stream = create_dummy_stream_async().await;
        // Verify pool operations work through the public API
        // We insert an entry directly into the pool, then invalidate it
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "test-session".to_string(),
                idle_pool_entry(stream, now_ms()),
            );
        }
        assert!(WS_POOL.lock().unwrap().contains_key("test-session"));

        invalidate_codex_websocket_pool_key("test-session");
        assert!(!WS_POOL.lock().unwrap().contains_key("test-session"));
    }

    #[tokio::test]
    async fn expired_idle_pool_entry_is_reaped_without_lookup() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let idle_ttl_ms = 50;
        let now = 1_000;
        let entry = idle_pool_entry(create_dummy_stream_async().await, now - idle_ttl_ms - 1);
        WS_POOL
            .lock()
            .unwrap()
            .insert("expired-session".to_string(), entry);

        {
            let mut guard = WS_POOL.lock().unwrap();
            reap_idle_pool(
                &mut guard,
                now,
                WebSocketPoolConfig {
                    max_idle_entries: 128,
                    idle_ttl_ms,
                },
            );
        }
        assert!(!WS_POOL.lock().unwrap().contains_key("expired-session"));
    }

    #[tokio::test]
    async fn idle_pool_capacity_evicts_oldest_entries_deterministically() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let first = idle_pool_entry(create_dummy_stream_async().await, 10);
        let second = idle_pool_entry(create_dummy_stream_async().await, 20);
        let third = idle_pool_entry(create_dummy_stream_async().await, 30);
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert("first".to_string(), first);
            guard.insert("second".to_string(), second);
            guard.insert("third".to_string(), third);
            reap_idle_pool(
                &mut guard,
                30,
                WebSocketPoolConfig {
                    max_idle_entries: 2,
                    idle_ttl_ms: 1_000,
                },
            );
        }

        assert_eq!(codex_websocket_pool_len_for_tests(), 2);
        assert!(!codex_websocket_pool_contains_for_tests("first"));
        assert!(codex_websocket_pool_contains_for_tests("second"));
        assert!(codex_websocket_pool_contains_for_tests("third"));
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn pooled_probe_requires_matching_pong() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                if let Message::Ping(payload) = frame {
                    ws.send(Message::Pong(payload)).await.unwrap();
                    return;
                }
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        probe_pooled_connection(&mut ws, 100).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pooled_probe_times_out_when_peer_stops_reading() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            futures_util::future::pending::<()>().await;
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err = probe_pooled_connection(&mut ws, 30)
            .await
            .expect_err("a write-only half-open connection must fail its probe");
        assert_eq!(
            err.detail.as_deref(),
            Some(WEBSOCKET_POOL_HEALTHCHECK_DETAIL)
        );
        server.abort();
    }

    #[derive(Clone, Copy)]
    enum TerminalBarrierPeer {
        Clean,
        Telemetry,
        SemanticTail,
        Silent,
        Close,
    }

    async fn run_live_terminal_barrier(
        peer: TerminalBarrierPeer,
    ) -> (bool, Vec<Result<serde_json::Value, CodexError>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{}}}"#.into(),
            ))
            .await
            .unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                let Message::Ping(payload) = frame else {
                    continue;
                };
                match peer {
                    TerminalBarrierPeer::Clean => {
                        ws.send(Message::Pong(payload)).await.unwrap();
                    }
                    TerminalBarrierPeer::Telemetry => {
                        ws.send(Message::Text(
                            r#"{"type":"codex.rate_limits","remaining":42}"#.into(),
                        ))
                        .await
                        .unwrap();
                        ws.send(Message::Text(r#"{"type":"keepalive"}"#.into()))
                            .await
                            .unwrap();
                        ws.send(Message::Pong(payload)).await.unwrap();
                    }
                    TerminalBarrierPeer::SemanticTail => {
                        ws.send(Message::Text(
                            r#"{"type":"response.output_text.delta","delta":"late"}"#.into(),
                        ))
                        .await
                        .unwrap();
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    TerminalBarrierPeer::Silent => {
                        futures_util::future::pending::<()>().await;
                    }
                    TerminalBarrierPeer::Close => {
                        let _ = ws.close(None).await;
                    }
                }
                return;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(super::super::LIVE_EVENT_CHANNEL_CAPACITY);
        let reusable = stream_ws_events(
            &mut ws,
            test_timeouts(1_000, 1_000),
            None,
            None,
            None,
            None,
            tx,
        )
        .await;
        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            items.push(item);
        }
        server.abort();
        let _ = server.await;
        (reusable, items)
    }

    #[tokio::test]
    async fn matching_terminal_barrier_keeps_connection_reusable() {
        let (reusable, items) = run_live_terminal_barrier(TerminalBarrierPeer::Clean).await;
        assert!(reusable);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]
                .as_ref()
                .unwrap()
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
    }

    #[tokio::test]
    async fn terminal_barrier_allows_only_explicit_post_terminal_telemetry() {
        let (reusable, items) = run_live_terminal_barrier(TerminalBarrierPeer::Telemetry).await;
        assert!(reusable);
        assert_eq!(
            items.len(),
            1,
            "telemetry must not be forwarded after terminal"
        );
        assert!(items[0].is_ok());
    }

    #[tokio::test]
    async fn post_terminal_semantic_tail_invalidates_connection_but_preserves_terminal() {
        let (reusable, items) = run_live_terminal_barrier(TerminalBarrierPeer::SemanticTail).await;
        assert!(!reusable);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]
                .as_ref()
                .unwrap()
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
    }

    #[tokio::test]
    async fn terminal_barrier_timeout_invalidates_connection_but_preserves_terminal() {
        let (reusable, items) = run_live_terminal_barrier(TerminalBarrierPeer::Silent).await;
        assert!(!reusable);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]
                .as_ref()
                .unwrap()
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
    }

    #[tokio::test]
    async fn live_terminal_is_forwarded_before_the_pool_probe_finishes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp_early","usage":{}}}"#.into(),
            ))
            .await
            .unwrap();
            futures_util::future::pending::<()>().await;
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(super::super::LIVE_EVENT_CHANNEL_CAPACITY);
        let mut timeouts = test_timeouts(1_000, 1_000);
        timeouts.pool_probe_ms = 500;
        let producer = tokio::spawn(async move {
            stream_ws_events(&mut ws, timeouts, None, None, None, None, tx).await
        });

        let terminal = tokio::time::timeout(Duration::from_millis(250), rx.recv())
            .await
            .expect("terminal must be forwarded before the 500ms pool probe timeout")
            .expect("terminal event was missing")
            .expect("terminal event became a transport error");
        assert_eq!(
            terminal.get("type").and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
        assert!(
            !producer.is_finished(),
            "the pool probe should still be pending when terminal is forwarded"
        );
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), producer)
                .await
                .expect("pool probe did not finish within its bounded timeout")
                .unwrap()
        );
        assert!(rx.recv().await.is_none(), "no error may follow a terminal");
        server.abort();
    }

    #[tokio::test]
    async fn immediate_next_full_context_turn_keeps_checked_out_socket_out_of_idle_pool() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        super::super::continuation::clear_all_continuations_for_tests();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_probe_tx, release_probe_rx) = tokio::sync::oneshot::channel();
        let (second_request_tx, second_request_rx) = tokio::sync::oneshot::channel();
        let (release_second_terminal_tx, release_second_terminal_rx) =
            tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut requests = 0_u8;
            let mut pings = 0_u8;
            let mut first_terminal_probe = Some(release_probe_rx);
            let mut second_request_tx = Some(second_request_tx);
            let mut release_second_terminal_rx = Some(release_second_terminal_rx);
            while let Some(frame) = ws.next().await {
                match frame.unwrap() {
                    Message::Ping(payload) => {
                        pings += 1;
                        if let Some(release) = first_terminal_probe.take() {
                            release.await.unwrap();
                        }
                        ws.send(Message::Pong(payload)).await.unwrap();
                        if requests == 2 {
                            return (requests, pings);
                        }
                    }
                    Message::Text(text) => {
                        requests += 1;
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match requests {
                            1 => {
                                assert!(request.get("previous_response_id").is_none());
                                ws.send(Message::Text(
                                    r#"{"type":"response.completed","response":{"id":"resp_busy_1","usage":{}}}"#.into(),
                                ))
                                .await
                                .unwrap();
                            }
                            2 => {
                                assert!(request.get("previous_response_id").is_none());
                                let _ = second_request_tx.take().unwrap().send(());
                                release_second_terminal_rx.take().unwrap().await.unwrap();
                                ws.send(Message::Text(
                                    r#"{"type":"response.completed","response":{"id":"resp_busy_2","usage":{}}}"#.into(),
                                ))
                                .await
                                .unwrap();
                            }
                            _ => panic!("unexpected full-context retry on the same turn"),
                        }
                    }
                    Message::Close(_) => return (requests, pings),
                    Message::Pong(_) | Message::Frame(_) => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
            (requests, pings)
        });

        let session_id = "immediate-terminal-busy-reservation";
        let first_request = continuation_request(&["first"]);
        let first = super::super::continuation::continuation_candidate(
            Some(session_id),
            &first_request,
            false,
        );
        let mut ctx = test_context();
        ctx.session_id = Some(session_id.to_string());
        let mut timeouts = test_timeouts(1_000, 1_000);
        timeouts.pool_probe_ms = 500;
        let first_wire =
            super::super::client::build_websocket_request(&first_request, Some(&first));
        let mut first_events = codex_websocket_event_stream(
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &first_wire,
            &ctx,
            None,
            Some(session_id),
            timeouts,
            Some(&first),
        )
        .await
        .unwrap();
        let terminal = tokio::time::timeout(Duration::from_millis(250), first_events.recv())
            .await
            .expect("first terminal must be forwarded before its probe")
            .unwrap()
            .unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert!(codex_websocket_pool_contains_for_tests(session_id));

        let second_request = continuation_request(&["first", "second"]);
        let second = super::super::continuation::continuation_candidate(
            Some(session_id),
            &second_request,
            false,
        );
        assert!(second.previous_response_id.is_none());
        let second_wire =
            super::super::client::build_websocket_request(&second_request, Some(&second));
        let mut second_events = codex_websocket_event_stream(
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &second_wire,
            &ctx,
            None,
            Some(session_id),
            timeouts,
            Some(&second),
        )
        .await
        .expect("the next turn must check out the busy reservation");
        assert!(!codex_websocket_pool_contains_for_tests(session_id));
        release_probe_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(500), second_request_rx)
            .await
            .expect("the next turn did not reuse the released socket")
            .unwrap();
        assert!(
            !codex_websocket_pool_contains_for_tests(session_id),
            "the previous worker must not re-publish a socket already checked out by this turn"
        );
        release_second_terminal_tx.send(()).unwrap();
        let terminal = tokio::time::timeout(Duration::from_millis(500), second_events.recv())
            .await
            .expect("second terminal timed out")
            .unwrap()
            .unwrap();
        assert_eq!(terminal["response"]["id"], "resp_busy_2");
        assert!(codex_websocket_pool_contains_for_tests(session_id));
        assert_eq!(
            server.await.unwrap(),
            (2, 2),
            "each completed turn needs one ordering Ping, but the fresh first-turn Pong must avoid a redundant checkout Ping"
        );

        drop(first_events);
        drop(second_events);
        clear_codex_websocket_pool_for_tests();
        super::super::continuation::clear_all_continuations_for_tests();
    }

    #[test]
    fn terminal_probe_credit_is_one_shot_and_expires() {
        let credit = FreshProbeCredit::default();
        let now = Instant::now();

        credit.mark(now);
        assert!(credit.consume(now, Duration::from_secs(1)));
        assert!(!credit.consume(now, Duration::from_secs(1)));

        credit.mark(now - Duration::from_secs(2));
        assert!(!credit.consume(now, Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn checked_out_busy_reservation_is_rejected_when_terminal_probe_fails() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        super::super::continuation::clear_all_continuations_for_tests();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_probe_tx, release_probe_rx) = tokio::sync::oneshot::channel();
        let (unexpected_request_tx, unexpected_request_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut first_request = true;
            let mut release_probe_rx = Some(release_probe_rx);
            let mut unexpected_request_tx = Some(unexpected_request_tx);
            while let Some(frame) = ws.next().await {
                let Ok(frame) = frame else {
                    break;
                };
                match frame {
                    Message::Text(_) if first_request => {
                        first_request = false;
                        ws.send(Message::Text(
                            r#"{"type":"response.completed","response":{"id":"resp_bad_probe","usage":{}}}"#.into(),
                        ))
                        .await
                        .unwrap();
                    }
                    Message::Text(_) => {
                        let _ = unexpected_request_tx.take().unwrap().send(());
                    }
                    Message::Ping(_) if release_probe_rx.is_some() => {
                        release_probe_rx.take().unwrap().await.unwrap();
                        ws.send(Message::Text(
                            r#"{"type":"response.output_text.delta","delta":"late"}"#.into(),
                        ))
                        .await
                        .unwrap();
                    }
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Close(_) => break,
                    Message::Pong(_) | Message::Frame(_) => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        });

        let session_id = "failed-terminal-busy-reservation";
        let first_request = continuation_request(&["first"]);
        let first = super::super::continuation::continuation_candidate(
            Some(session_id),
            &first_request,
            true,
        );
        let mut ctx = test_context();
        ctx.session_id = Some(session_id.to_string());
        let mut timeouts = test_timeouts(1_000, 1_000);
        timeouts.pool_probe_ms = 500;
        let first_wire =
            super::super::client::build_websocket_request(&first_request, Some(&first));
        let mut first_events = codex_websocket_event_stream(
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &first_wire,
            &ctx,
            None,
            Some(session_id),
            timeouts,
            Some(&first),
        )
        .await
        .unwrap();
        let terminal = first_events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");

        super::super::continuation::record_continuation(
            Some(session_id),
            first.turn_id,
            &first_request,
            Some("resp_bad_probe"),
            &[],
        );
        let second_request = continuation_request(&["first", "second"]);
        let second = super::super::continuation::continuation_candidate(
            Some(session_id),
            &second_request,
            true,
        );
        let second_wire =
            super::super::client::build_websocket_request(&second_request, Some(&second));
        let mut second_events = codex_websocket_event_stream(
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &second_wire,
            &ctx,
            None,
            Some(session_id),
            timeouts,
            Some(&second),
        )
        .await
        .expect("the next turn must take the busy reservation before probe failure");
        release_probe_tx.send(()).unwrap();

        let error = tokio::time::timeout(Duration::from_millis(500), second_events.recv())
            .await
            .expect("invalidated reservation did not wake its next owner")
            .unwrap()
            .expect_err("an invalidated reservation must not carry a continuation request");
        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_POOL_HEALTHCHECK_DETAIL)
        );
        let unexpected_request =
            tokio::time::timeout(Duration::from_millis(100), unexpected_request_rx).await;
        assert!(
            !matches!(unexpected_request, Ok(Ok(()))),
            "the next continuation request reached the invalid socket"
        );

        drop(first_events);
        drop(second_events);
        server.abort();
        clear_codex_websocket_pool_for_tests();
        super::super::continuation::clear_all_continuations_for_tests();
    }

    #[tokio::test]
    async fn terminal_barrier_clean_close_forwards_terminal_without_reusing_connection() {
        let (reusable, items) = run_live_terminal_barrier(TerminalBarrierPeer::Close).await;
        assert!(!reusable);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]
                .as_ref()
                .unwrap()
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
    }

    #[tokio::test]
    async fn heartbeat_detects_silent_half_open_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            futures_util::future::pending::<()>().await;
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let mut timeouts = test_timeouts(1_000, 1_000);
        timeouts.heartbeat_interval_ms = 20;
        timeouts.pong_ms = 20;
        let err = tokio::time::timeout(
            Duration::from_millis(250),
            collect_ws_events(&mut ws, timeouts, None, None, None),
        )
        .await
        .expect("heartbeat watchdog should finish promptly")
        .expect_err("silent peer must fail the heartbeat");
        assert_eq!(
            err.detail.as_deref(),
            Some(WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL)
        );
        server.abort();
    }

    #[tokio::test]
    async fn matching_heartbeat_pong_keeps_stream_alive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut terminal_sent = false;
            while let Some(Ok(frame)) = ws.next().await {
                if let Message::Ping(payload) = frame {
                    ws.send(Message::Pong(payload)).await.unwrap();
                    if terminal_sent {
                        return;
                    }
                    ws.send(Message::Text(
                        r#"{"type":"response.completed","response":{"id":"resp_1"}}"#.into(),
                    ))
                    .await
                    .unwrap();
                    terminal_sent = true;
                }
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let mut timeouts = test_timeouts(500, 500);
        timeouts.heartbeat_interval_ms = 20;
        timeouts.pong_ms = 100;
        let (_, terminal) = tokio::time::timeout(
            Duration::from_millis(250),
            collect_ws_events(&mut ws, timeouts, None, None, None),
        )
        .await
        .expect("matching Pong should keep the stream alive")
        .unwrap();
        assert_eq!(
            terminal.as_ref().map(|event| event.kind),
            Some(super::super::events::CodexTerminalKind::Completed)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_event_receiver_releases_pooled_connection() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut request_seen_tx = Some(request_seen_tx);
            while let Some(Ok(frame)) = ws.next().await {
                match frame {
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Text(_) => {
                        if let Some(tx) = request_seen_tx.take() {
                            let _ = tx.send(());
                        }
                        futures_util::future::pending::<()>().await;
                    }
                    _ => {}
                }
            }
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let entry = Arc::new(PoolEntry::new(ws));
        WS_POOL.lock().unwrap().insert(
            "cancel-session".to_string(),
            IdlePoolEntry {
                connection: entry.clone(),
                idle_since_ms: now_ms(),
            },
        );

        let ctx = RequestContext {
            req_id: "cancel-request".to_string(),
            session_id: Some("cancel-session".to_string()),
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        };
        let continuation = ContinuationCandidate {
            turn_id: None,
            previous_response_id: Some("resp_previous".to_string()),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: None,
        };
        let receiver = codex_websocket_event_stream(
            &format!("http://{addr}/"),
            &HeaderMap::new(),
            &serde_json::json!({"type":"response.create","input":[]}),
            &ctx,
            None,
            Some("cancel-session"),
            test_timeouts(1_000, 1_000),
            Some(&continuation),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_millis(250), request_seen_rx)
            .await
            .expect("request should reach the pooled connection")
            .unwrap();
        drop(receiver);

        let _guard = tokio::time::timeout(Duration::from_millis(250), entry.ws.lock())
            .await
            .expect("cancelled receiver must release the pooled socket lock");
        assert!(!WS_POOL.lock().unwrap().contains_key("cancel-session"));
        server.abort();
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
        assert_eq!(err.detail.as_deref(), Some("policy denied"));
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
        assert_eq!(err.detail, None);
        assert_eq!(err.retry_after.as_deref(), Some("3"));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    #[tokio::test]
    async fn binary_frame_invalidates_pool_key() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "binary-session".to_string(),
                idle_pool_entry(pooled_stream, now_ms()),
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
        let err = match collect_ws_events(
            &mut ws,
            test_timeouts(1_000, 1_000),
            Some("binary-session"),
            None,
            None,
        )
        .await
        {
            Ok(_) => panic!("expected binary frame to fail"),
            Err(err) => err,
        };

        assert!(err.message.contains("binary frames"));
        assert!(!WS_POOL.lock().unwrap().contains_key("binary-session"));
    }

    #[tokio::test]
    async fn malformed_live_json_fails_closed_and_invalidates_pool() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#
                    .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.output_text.delta","delta":"truncated""#.into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp_1"}}"#.into(),
            ))
            .await
            .unwrap();
        });

        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let entry = Arc::new(PoolEntry::new(ws));
        WS_POOL.lock().unwrap().insert(
            "malformed-session".to_string(),
            IdlePoolEntry {
                connection: entry.clone(),
                idle_since_ms: now_ms(),
            },
        );

        let (tx, mut rx) = mpsc::channel(super::super::LIVE_EVENT_CHANNEL_CAPACITY);
        let reusable = {
            let mut ws = entry.ws.lock().await;
            stream_ws_events(
                &mut ws,
                test_timeouts(1_000, 1_000),
                Some("malformed-session"),
                Some(&entry),
                None,
                None,
                tx,
            )
            .await
        };

        assert!(!reusable);
        let first = rx
            .recv()
            .await
            .expect("the valid event should be forwarded before the protocol error")
            .expect("the first event should remain valid");
        assert_eq!(
            first.get("type").and_then(serde_json::Value::as_str),
            Some("response.output_item.added")
        );

        let error = rx
            .recv()
            .await
            .expect("malformed JSON must become a terminal stream error")
            .expect_err("a later success event must not replace the protocol error");
        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_MALFORMED_EVENT_DETAIL)
        );
        assert_eq!(error.origin, CodexErrorOrigin::WebSocket);
        assert!(!is_retryable_transport_detail(error.detail.as_deref()));
        assert!(!super::super::client::is_retryable_transport_error(&error));
        assert!(!super::super::client::should_fallback_to_http(&error));
        assert!(
            rx.recv().await.is_none(),
            "response.completed after malformed JSON must not be forwarded"
        );
        assert!(!codex_websocket_pool_contains_for_tests(
            "malformed-session"
        ));

        server.await.unwrap();
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn malformed_buffered_json_fails_closed_and_invalidates_pool() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#
                    .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.output_text.delta","delta":"truncated""#.into(),
            ))
            .await
            .unwrap();
            let _ = ws
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"resp_1"}}"#.into(),
                ))
                .await;
        });

        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let entry = Arc::new(PoolEntry::new(ws));
        WS_POOL.lock().unwrap().insert(
            "malformed-buffered-session".to_string(),
            IdlePoolEntry {
                connection: entry.clone(),
                idle_since_ms: now_ms(),
            },
        );

        let error = {
            let mut ws = entry.ws.lock().await;
            collect_ws_events(
                &mut ws,
                test_timeouts(1_000, 1_000),
                Some("malformed-buffered-session"),
                Some(&entry),
                None,
            )
            .await
            .expect_err("malformed buffered JSON must fail before a success terminal")
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_MALFORMED_EVENT_DETAIL)
        );
        assert!(!is_retryable_transport_detail(error.detail.as_deref()));
        assert!(!super::super::client::is_retryable_transport_error(&error));
        assert!(!super::super::client::should_fallback_to_http(&error));
        assert!(!codex_websocket_pool_contains_for_tests(
            "malformed-buffered-session"
        ));

        server.await.unwrap();
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn response_start_timeout_ignores_rate_limits_and_pings() {
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "start-timeout-session".to_string(),
                idle_pool_entry(pooled_stream, now_ms()),
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
        let err = match collect_ws_events(
            &mut ws,
            test_timeouts(50, 1_000),
            Some("start-timeout-session"),
            None,
            None,
        )
        .await
        {
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
        let _pool_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                "response-idle-session".to_string(),
                idle_pool_entry(pooled_stream, now_ms()),
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
        let err = match collect_ws_events(
            &mut ws,
            test_timeouts(1_000, 50),
            Some("response-idle-session"),
            None,
            None,
        )
        .await
        {
            Ok(_) => panic!("expected response idle timeout"),
            Err(err) => err,
        };

        assert!(err.message.contains("idle timeout"));
        assert_eq!(err.detail.as_deref(), Some(WEBSOCKET_IDLE_TIMEOUT_DETAIL));
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
}
