use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::anthropic::sse::parse_sse_events;
use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::retry::{compute_backoff_delay, should_retry_status, sleep};
use crate::traffic::TrafficCapture;

use super::auth::constants::{CODEX_API_ENDPOINT, ORIGINATOR, RESPONSES_LITE_ORIGINATOR};
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{DefaultCodexAuthStore, StoredAuth, file_store};
use super::translate::request::ResponsesRequest;

const MAX_BUFFERED_TRANSPORT_RETRIES: u32 = 3;
const MAX_BUFFERED_TRANSPORT_ATTEMPTS: u32 = MAX_BUFFERED_TRANSPORT_RETRIES + 1;
const BUFFERED_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const WEBSOCKET_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
    pub origin: CodexErrorOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexErrorOrigin {
    Http,
    WebSocket,
    Auth,
}

impl CodexError {
    pub fn new(status: u16, message: String) -> Self {
        Self {
            status,
            message,
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        }
    }
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex error {}: {}", self.status, self.message)
    }
}

#[derive(Debug)]
pub struct CodexHeaderTimeoutError {
    pub timeout_ms: u64,
}

impl std::fmt::Display for CodexHeaderTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Timed out waiting {}ms for Codex response headers",
            self.timeout_ms
        )
    }
}

#[derive(Debug)]
pub struct CodexTransportError {
    pub message: String,
}

impl std::fmt::Display for CodexTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex transport error: {}", self.message)
    }
}

// ---------------------------------------------------------------------------
// Header builder
// ---------------------------------------------------------------------------

pub fn build_codex_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    use_responses_lite: bool,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        header_value("content-type", "application/json")?,
    );
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "text/event-stream")?,
    );
    let bearer = format!("Bearer {}", auth.access);
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &bearer)?,
    );
    let originator = if use_responses_lite {
        RESPONSES_LITE_ORIGINATOR.to_string()
    } else {
        config::codex_originator(ORIGINATOR)
    };
    headers.insert("originator", header_value("originator", &originator)?);
    headers.insert(
        "openai-beta",
        header_value("openai-beta", "responses=experimental")?,
    );
    if use_responses_lite {
        headers.insert(
            "x-openai-internal-codex-responses-lite",
            header_value("x-openai-internal-codex-responses-lite", "true")?,
        );
    }
    if let Some(ref account_id) = auth.account_id {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(ref session_id) = ctx.session_id {
        headers.insert("session_id", header_value("session_id", session_id)?);
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
        let window_id = format!("{session_id}:0");
        headers.insert(
            "x-codex-window-id",
            header_value("x-codex-window-id", &window_id)?,
        );
    }
    let user_agent =
        config::codex_user_agent(&format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION")));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

fn header_value(name: &str, value: &str) -> Result<http::HeaderValue, CodexError> {
    http::HeaderValue::from_str(value).map_err(|e| CodexError {
        status: 500,
        message: format!("Failed to parse {name} header"),
        detail: Some(e.to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::Http,
    })
}

// ---------------------------------------------------------------------------
// WebSocket request shaping
// ---------------------------------------------------------------------------

pub fn build_websocket_request(
    body: &ResponsesRequest,
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> serde_json::Value {
    let mut payload = serde_json::to_value(body).unwrap_or_default();
    let obj = payload.as_object_mut().expect("request must be an object");

    // Omit the stream field for WebSocket transport
    obj.remove("stream");
    obj.insert("type".to_string(), serde_json::json!("response.create"));

    // Apply continuation if available
    if let Some(candidate) = continuation {
        if let Some(ref prev_id) = candidate.previous_response_id {
            obj.insert(
                "previous_response_id".to_string(),
                serde_json::json!(prev_id),
            );
        }
        if let Some(ref delta) = candidate.input_delta {
            obj.insert(
                "input".to_string(),
                serde_json::to_value(delta).unwrap_or_default(),
            );
        }
    }

    payload
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

pub struct CodexResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct CodexHttpClient {
    client: reqwest::Client,
    auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
    base_url: String,
    header_timeout_ms: u64,
    buffered_timeout: Duration,
    websocket_cooldown_until: Mutex<Option<Instant>>,
    #[allow(dead_code)]
    header_timeout_retries: u32,
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHttpClient {
    pub fn new() -> Self {
        let timeout_ms = 60_000;
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .expect("failed to create HTTP client"),
            auth_manager: CodexAuthManager::new(file_store()),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            buffered_timeout: BUFFERED_REQUEST_TIMEOUT,
            websocket_cooldown_until: Mutex::new(None),
            header_timeout_retries: 1,
        }
    }

    pub fn new_with_client(
        client: reqwest::Client,
        auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
        base_url: String,
    ) -> Self {
        Self {
            client,
            auth_manager,
            base_url,
            header_timeout_ms: 60_000,
            buffered_timeout: BUFFERED_REQUEST_TIMEOUT,
            websocket_cooldown_until: Mutex::new(None),
            header_timeout_retries: 1,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        client: reqwest::Client,
        base_url: String,
        header_timeout_ms: u64,
        header_timeout_retries: u32,
        buffered_timeout: Duration,
    ) -> Self {
        Self {
            client,
            auth_manager: CodexAuthManager::new(file_store()),
            base_url,
            header_timeout_ms,
            buffered_timeout,
            websocket_cooldown_until: Mutex::new(None),
            header_timeout_retries,
        }
    }

    pub fn auth_manager(&self) -> &CodexAuthManager<DefaultCodexAuthStore> {
        &self.auth_manager
    }

    fn resolve_transport(
        &self,
        transport: crate::config::CodexTransport,
    ) -> crate::config::CodexTransport {
        use crate::config::CodexTransport;

        if transport != CodexTransport::Auto {
            return transport;
        }
        match *self
            .websocket_cooldown_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            Some(until) if until > Instant::now() => CodexTransport::Http,
            _ => CodexTransport::WebSocket,
        }
    }

    fn trip_websocket_cooldown(&self) {
        *self
            .websocket_cooldown_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Instant::now() + WEBSOCKET_FAILURE_COOLDOWN);
    }

    pub async fn post_codex(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<CodexResponse, CodexError> {
        self.post_codex_with_transport(body, ctx, continuation, crate::config::codex_transport())
            .await
    }

    async fn post_codex_with_transport(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        transport: crate::config::CodexTransport,
    ) -> Result<CodexResponse, CodexError> {
        use super::continuation::clear_continuation;
        use crate::config::CodexTransport;

        let mut auth = self.auth_manager.get_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::Auth,
        })?;

        let initial_pool_key = websocket_pool_key(ctx, continuation);
        if should_reset_websocket_pool(continuation)
            && let Some(key) = initial_pool_key
        {
            super::websocket::invalidate_codex_websocket_pool_key(key);
        }

        let mut active_continuation = continuation;
        let mut active_transport = self.resolve_transport(transport);
        let mut auth_refresh_attempted = false;
        let mut transport_failures = 0u32;
        let deadline = Instant::now() + self.buffered_timeout;
        loop {
            let pool_key = websocket_pool_key(ctx, active_continuation);
            let remaining = deadline.saturating_duration_since(Instant::now());
            let result = match tokio::time::timeout(remaining, async {
                match active_transport {
                    CodexTransport::Http => {
                        let body_json = serde_json::to_string(body).map_err(|e| CodexError {
                            status: 500,
                            message: "Failed to serialize request".to_string(),
                            detail: Some(e.to_string()),
                            retry_after: None,
                            origin: CodexErrorOrigin::Http,
                        })?;
                        self.attempt_post_http(
                            &auth,
                            &body_json,
                            ctx,
                            body.client_metadata.is_some(),
                        )
                        .await
                    }
                    CodexTransport::WebSocket => {
                        let ws_headers =
                            build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
                        let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                        let ws_body = build_websocket_request(body, active_continuation);

                        super::websocket::codex_websocket_request(
                            &self.base_url,
                            &ws_headers,
                            &ws_body,
                            ctx,
                            ctx.traffic.as_deref(),
                            pool_key,
                            super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                            super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                            active_continuation,
                        )
                        .await
                    }
                    CodexTransport::Auto => unreachable!("auto transport must be resolved"),
                }
            })
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(CodexError {
                        status: 504,
                        message: format!(
                            "Buffered Codex request exceeded {}ms",
                            self.buffered_timeout.as_millis()
                        ),
                        detail: Some("buffered_request_timeout".to_string()),
                        retry_after: None,
                        origin: match active_transport {
                            CodexTransport::WebSocket => CodexErrorOrigin::WebSocket,
                            _ => CodexErrorOrigin::Http,
                        },
                    });
                }
            };

            if should_refresh_after_unauthorized(&result, auth_refresh_attempted) {
                auth_refresh_attempted = true;
                match self.auth_manager.force_refresh(&auth.access).await {
                    Ok(new_auth) => {
                        auth = new_auth;
                        if let Some(key) = pool_key {
                            super::websocket::invalidate_codex_websocket_pool_key(key);
                        }
                        continue;
                    }
                    Err(e) => {
                        return Err(CodexError {
                            status: 401,
                            message: "Unauthorized".to_string(),
                            detail: Some(e.to_string()),
                            retry_after: None,
                            origin: CodexErrorOrigin::Http,
                        });
                    }
                }
            }

            if let Ok(response) = &result
                && let Some(failure) = retryable_buffered_upstream_failure(&response.body)
            {
                if failure.status != 429 && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                    let delay =
                        compute_backoff_delay(transport_failures, failure.retry_after.as_deref());
                    log_buffered_retry(
                        ctx,
                        active_transport,
                        transport_failures + 1,
                        delay.wait_ms,
                        failure.status,
                        "upstream_event",
                        &failure.message,
                    );
                    transport_failures += 1;
                    sleep(delay.wait_ms).await;
                    continue;
                }

                if failure.status != 429 {
                    log_buffered_retry_exhausted(
                        ctx,
                        active_transport,
                        failure.status,
                        "upstream_event",
                        &failure.message,
                    );
                }
                return Err(CodexError {
                    status: failure.status,
                    message: failure.message,
                    detail: None,
                    retry_after: failure.retry_after,
                    origin: CodexErrorOrigin::Http,
                });
            }

            match result {
                Ok(response) if response.status == 401 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 401,
                        message: "Unauthorized".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 403 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 403,
                        message: "Forbidden".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = response
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == "retry-after")
                        .map(|(_, v)| v.clone());
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 429,
                        message: "Rate limited".to_string(),
                        detail: Some(detail),
                        retry_after,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if should_retry_status(response.status) => {
                    if transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let retry_after = response
                            .headers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                            .map(|(_, value)| value.as_str());
                        let delay = compute_backoff_delay(transport_failures, retry_after);
                        log_buffered_retry(
                            ctx,
                            active_transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "retryable upstream status",
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    log_buffered_retry_exhausted(
                        ctx,
                        active_transport,
                        response.status,
                        "upstream",
                        "retryable upstream status",
                    );
                    let retry_after = response
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                        .map(|(_, value)| value.clone());
                    return Err(CodexError {
                        status: response.status,
                        message: format!(
                            "Upstream returned retryable status {} after buffered retries",
                            response.status
                        ),
                        detail: None,
                        retry_after,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) => return Ok(response),
                Err(err) if should_retry_without_continuation(&err, active_continuation) => {
                    clear_continuation(ctx.session_id.as_deref());
                    if let Some(key) = pool_key {
                        super::websocket::invalidate_codex_websocket_pool_key(key);
                    }
                    active_continuation = None;
                    continue;
                }
                Err(err) => {
                    // Determine if retryable
                    let retryable = is_retryable_transport_error(&err);
                    if retryable
                        && transport == CodexTransport::Auto
                        && active_transport == CodexTransport::WebSocket
                        && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES
                    {
                        log_buffered_retry(
                            ctx,
                            active_transport,
                            transport_failures + 1,
                            0,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                        transport_failures += 1;
                        if let Some(key) = pool_key {
                            super::websocket::invalidate_codex_websocket_pool_key(key);
                        }
                        self.trip_websocket_cooldown();
                        active_transport = CodexTransport::Http;
                        continue;
                    }
                    if retryable && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let delay =
                            compute_backoff_delay(transport_failures, err.retry_after.as_deref());
                        log_buffered_retry(
                            ctx,
                            active_transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    if retryable {
                        log_buffered_retry_exhausted(
                            ctx,
                            active_transport,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                    }
                    return Err(err);
                }
            }
        }
    }

    pub async fn stream_codex_websocket_events(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<super::websocket::CodexWebSocketEventReceiver, CodexError> {
        let auth = self.auth_manager.get_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::Auth,
        })?;

        let pool_key = websocket_pool_key(ctx, continuation);
        if should_reset_websocket_pool(continuation)
            && let Some(key) = pool_key
        {
            super::websocket::invalidate_codex_websocket_pool_key(key);
        }

        let ws_headers = build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
        let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
        let ws_body = build_websocket_request(body, continuation);

        let first_stream = super::websocket::codex_websocket_event_stream(
            &self.base_url,
            &ws_headers,
            &ws_body,
            ctx,
            ctx.traffic.clone(),
            pool_key,
            super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
            super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
            continuation,
        )
        .await?;

        let can_retry_without_continuation = continuation
            .and_then(|c| c.previous_response_id.as_deref())
            .is_some();
        if !can_retry_without_continuation {
            return Ok(first_stream);
        }

        let retry_body = build_websocket_request(body, None);
        let base_url = self.base_url.clone();
        let ctx = ctx.clone();
        let pool_key = pool_key.map(str::to_string);
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut stream = first_stream;
            let mut retry_available = true;
            loop {
                match stream.recv().await {
                    Some(Err(err)) if retry_available && is_continuation_retry_error(&err) => {
                        retry_available = false;
                        super::continuation::clear_continuation(ctx.session_id.as_deref());
                        if let Some(key) = pool_key.as_deref() {
                            super::websocket::invalidate_codex_websocket_pool_key(key);
                        }
                        match super::websocket::codex_websocket_event_stream(
                            &base_url,
                            &ws_headers,
                            &retry_body,
                            &ctx,
                            ctx.traffic.clone(),
                            pool_key.as_deref(),
                            super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                            super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                            None,
                        )
                        .await
                        {
                            Ok(retry_stream) => {
                                stream = retry_stream;
                                continue;
                            }
                            Err(retry_err) => {
                                let _ = tx.send(Err(retry_err)).await;
                                return;
                            }
                        }
                    }
                    Some(item) => {
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
        });

        Ok(rx)
    }

    async fn attempt_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &str,
        ctx: &RequestContext,
        use_responses_lite: bool,
    ) -> Result<CodexResponse, CodexError> {
        let url = &self.base_url;
        let headers = build_codex_headers(auth, ctx, use_responses_lite)?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_codex_http_request_capture(traffic, url, &headers, body_json);
        }

        // Build headers
        let mut req_builder = self.client.post(url);
        for (key, value) in headers.iter() {
            req_builder = req_builder.header(key.as_str(), value.as_bytes());
        }

        // Apply header timeout
        let started_at = Instant::now();
        let send_fut = req_builder.body(body_json.to_string()).send();
        let header_timeout_dur = Duration::from_millis(self.header_timeout_ms);

        let mut resp = tokio::time::timeout(header_timeout_dur, send_fut)
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex response headers",
                    self.header_timeout_ms
                ),
                detail: None,
                retry_after: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| {
                if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {e}"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    }
                }
            })?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let mut body_bytes = Vec::new();
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS),
                resp.chunk(),
            )
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for the next Codex response body chunk",
                    super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| CodexError {
                status: 0,
                message: format!("Transport error reading Codex response body: {e}"),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::Http,
            })?;

            let Some(chunk) = chunk else {
                break;
            };
            body_bytes.extend_from_slice(&chunk);
        }

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(
                traffic,
                status,
                started_at.elapsed(),
                &headers,
                &body_bytes,
            );
        }

        Ok(CodexResponse {
            body: body_bytes,
            status,
            headers,
        })
    }
}

fn write_codex_http_request_capture(
    traffic: &TrafficCapture,
    url: &str,
    headers: &http::HeaderMap,
    body_json: &str,
) {
    let body = serde_json::from_str(body_json).unwrap_or_else(|_| {
        serde_json::json!({
            "unparseable": true,
            "bytes": body_json.len(),
        })
    });
    traffic.write_json("020-upstream-request", &body);
    traffic.write_json(
        "021-upstream-request-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "http",
            "url": url,
            "method": "POST",
            "headers": headers_to_json(headers),
            "size": summarize_json_request_size(&body, body_json),
        }),
    );
}

fn write_upstream_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    headers: &[(String, String)],
    body: &[u8],
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": headers_to_json_from_pairs(headers),
        }),
    );
    if status >= 400 {
        traffic.write_text("031-upstream-error-body", &String::from_utf8_lossy(body));
    } else {
        traffic.write_bytes("032-upstream-response-body.sse", body);
        write_codex_sse_event_capture(traffic, body);
    }
}

fn write_codex_sse_event_capture(traffic: &TrafficCapture, body: &[u8]) {
    for event in parse_sse_events(body) {
        if event.data == "[DONE]" {
            traffic.write_json_event(
                "040-upstream-event",
                &serde_json::json!({
                    "event": event.event,
                    "data": "[DONE]",
                }),
            );
            continue;
        }

        match serde_json::from_str::<serde_json::Value>(&event.data) {
            Ok(mut value) => {
                if let Some(name) = event.event
                    && let Some(obj) = value.as_object_mut()
                {
                    obj.entry("_sse_event").or_insert(serde_json::json!(name));
                }
                traffic.write_json_event("040-upstream-event", &value);
            }
            Err(_) => {
                traffic.write_json_event(
                    "040-upstream-event",
                    &serde_json::json!({
                        "event": event.event,
                        "unparseable": true,
                        "data": event.data,
                    }),
                );
            }
        }
    }
}

fn headers_to_json(headers: &http::HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers.iter() {
        out.insert(
            key.to_string(),
            serde_json::Value::String(value.to_str().unwrap_or("").to_string()),
        );
    }
    serde_json::Value::Object(out)
}

fn headers_to_json_from_pairs(headers: &[(String, String)]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers {
        out.insert(key.clone(), serde_json::Value::String(value.clone()));
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

fn is_retryable_transport_error(err: &CodexError) -> bool {
    if err.detail.as_deref() == Some("websocket_pre_request") {
        return err.status == 0 || should_retry_status(err.status);
    }
    if err.status != 0 {
        return false;
    }

    let message = err.message.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "transport error",
        "websocket send error",
        "websocket stream error",
        "websocket connection closed",
        "failed to ping pooled connection",
        "connection reset",
        "connection closed",
        "econnreset",
        "etimedout",
        "broken pipe",
        "epipe",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

struct BufferedUpstreamFailure {
    status: u16,
    message: String,
    retry_after: Option<String>,
}

fn retryable_buffered_upstream_failure(body: &[u8]) -> Option<BufferedUpstreamFailure> {
    for event in parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        let event_type = payload.get("type").and_then(|value| value.as_str());
        if !matches!(
            event_type,
            Some("response.failed" | "response.error" | "error" | "codex.rate_limits")
        ) {
            continue;
        }

        let message = payload
            .get("response")
            .and_then(|response| response.get("error"))
            .and_then(|error| error.get("message"))
            .and_then(|value| value.as_str())
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(|value| value.as_str())
            })
            .unwrap_or("Retryable upstream failure")
            .to_string();
        if !super::retryable_live_start_payload(&payload, &message) {
            continue;
        }

        let status = payload
            .get("status")
            .or_else(|| payload.get("status_code"))
            .and_then(|value| value.as_u64())
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(|error| error.get("status"))
                    .and_then(|value| value.as_u64())
            })
            .or_else(|| {
                payload
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("status"))
                    .and_then(|value| value.as_u64())
            })
            .and_then(|status| u16::try_from(status).ok())
            .unwrap_or_else(|| {
                if event_type == Some("codex.rate_limits") {
                    429
                } else if message.to_ascii_lowercase().contains("overloaded") {
                    529
                } else {
                    503
                }
            });

        return Some(BufferedUpstreamFailure {
            status,
            message,
            retry_after: super::retry_after_from_live_payload(&payload),
        });
    }

    None
}

fn codex_error_origin_name(origin: CodexErrorOrigin) -> &'static str {
    match origin {
        CodexErrorOrigin::Http => "http",
        CodexErrorOrigin::WebSocket => "websocket",
        CodexErrorOrigin::Auth => "auth",
    }
}

fn log_buffered_retry(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    failed_attempt: u32,
    delay_ms: u64,
    status: u16,
    origin: &str,
    reason: &str,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("transport".into(), serde_json::json!(transport.as_str()));
    fields.insert("failedAttempt".into(), serde_json::json!(failed_attempt));
    fields.insert("nextAttempt".into(), serde_json::json!(failed_attempt + 1));
    fields.insert(
        "maxAttempts".into(),
        serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
    );
    fields.insert("delayMs".into(), serde_json::json!(delay_ms));
    fields.insert("status".into(), serde_json::json!(status));
    fields.insert("origin".into(), serde_json::json!(origin));
    fields.insert("reason".into(), serde_json::json!(reason));
    create_logger("codex").warn("buffered_transport_retry", Some(fields));
}

fn log_buffered_retry_exhausted(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    status: u16,
    origin: &str,
    reason: &str,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("transport".into(), serde_json::json!(transport.as_str()));
    fields.insert(
        "attempts".into(),
        serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
    );
    fields.insert("status".into(), serde_json::json!(status));
    fields.insert("origin".into(), serde_json::json!(origin));
    fields.insert("reason".into(), serde_json::json!(reason));
    create_logger("codex").warn("buffered_transport_retry_exhausted", Some(fields));
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("econnreset")
        || msg.contains("etimedout")
        || msg.contains("epipe")
}

fn should_refresh_after_unauthorized(
    result: &Result<CodexResponse, CodexError>,
    auth_refresh_attempted: bool,
) -> bool {
    if auth_refresh_attempted {
        return false;
    }
    match result {
        Ok(response) => response.status == 401,
        Err(err) => err.status == 401,
    }
}

fn should_retry_without_continuation(
    err: &CodexError,
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> bool {
    if continuation
        .and_then(|c| c.previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    is_continuation_retry_error(err)
}

fn is_continuation_retry_error(err: &CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
            | Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
    )
}

fn websocket_pool_key<'a>(
    ctx: &'a RequestContext,
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> Option<&'a str> {
    let session_id = ctx.session_id.as_deref()?;
    let continuation = continuation?;
    if continuation.disabled_reason.as_deref() == Some("disabled") {
        return None;
    }
    Some(session_id)
}

fn should_reset_websocket_pool(
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> bool {
    let Some(reason) = continuation.and_then(|c| c.disabled_reason.as_deref()) else {
        return false;
    };
    !matches!(reason, "missing_state" | "disabled")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    fn buffered_test_client(base_url: String) -> CodexHttpClient {
        let client = CodexHttpClient::new_for_test(
            reqwest::Client::new(),
            base_url,
            1_000,
            1,
            Duration::from_secs(5),
        );
        client.auth_manager().set_test_auth(StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: u64::MAX,
            account_id: Some("test_account".into()),
        });
        client
    }

    fn buffered_test_context(req_id: &str) -> RequestContext {
        RequestContext {
            req_id: req_id.into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        }
    }

    fn buffered_test_request() -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-5.6-sol".into(),
            instructions: None,
            input: vec![],
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: None,
                format: None,
            },
            reasoning: None,
        }
    }

    #[test]
    fn codex_error_display() {
        let err = CodexError {
            status: 429,
            message: "Rate limited".to_string(),
            detail: Some("body".to_string()),
            retry_after: Some("5".to_string()),
            origin: CodexErrorOrigin::Http,
        };
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
    }

    #[test]
    fn websocket_pre_request_502_is_retryable() {
        let err = CodexError {
            status: 502,
            message: "WebSocket connect error".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: Some("3".to_string()),
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn websocket_pre_request_400_is_not_retryable() {
        let err = CodexError {
            status: 400,
            message: "WebSocket connect error".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(!is_retryable_transport_error(&err));
    }

    #[test]
    fn statusless_websocket_failures_are_retryable() {
        for message in [
            "WebSocket stream error: IO error: Connection reset by peer",
            "WebSocket connection closed before terminal Codex response event",
            "WebSocket idle timeout after 300000ms",
            "WebSocket response start timeout after 300000ms",
            "WebSocket protocol error: Connection reset without closing handshake",
        ] {
            let err = CodexError {
                status: 0,
                message: message.to_string(),
                detail: None,
                retry_after: None,
                origin: CodexErrorOrigin::WebSocket,
            };

            assert!(
                is_retryable_transport_error(&err),
                "expected retryable WebSocket error: {message}"
            );
        }
    }

    #[test]
    fn statusless_http_transport_matching_is_case_insensitive() {
        let err = CodexError {
            status: 0,
            message: "Transport error: Connection reset by peer".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn statusful_websocket_failures_are_not_replayed_as_transport_errors() {
        let err = CodexError {
            status: 400,
            message: "Connection reset by peer".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(!is_retryable_transport_error(&err));
    }

    #[test]
    fn statusless_websocket_semantic_and_config_errors_are_not_retryable() {
        for message in [
            "Previous response not found",
            "WebSocket binary frames not supported",
            "Unsupported WebSocket URL scheme: ftp",
            "Failed to build WebSocket request: invalid header",
        ] {
            let err = CodexError {
                status: 0,
                message: message.to_string(),
                detail: None,
                retry_after: None,
                origin: CodexErrorOrigin::WebSocket,
            };

            assert!(
                !is_retryable_transport_error(&err),
                "expected non-retryable WebSocket error: {message}"
            );
        }
    }

    #[test]
    fn buffered_rate_limit_event_preserves_429_and_retry_after() {
        let body = b"data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":7}}}\n\n";
        let failure = retryable_buffered_upstream_failure(body).unwrap();
        assert_eq!(failure.status, 429);
        assert_eq!(failure.retry_after.as_deref(), Some("7"));
    }

    #[tokio::test]
    async fn buffered_auto_falls_back_to_http_before_releasing_partial_output() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            {
                let mut websocket = accept_async(stream).await.unwrap();
                let request = websocket.next().await.unwrap().unwrap();
                assert!(request.is_text());
                server_attempts.fetch_add(1, Ordering::SeqCst);
                websocket
                    .send(Message::Text(
                        r#"{"type":"response.output_text.delta","delta":"discard me"}"#.into(),
                    ))
                    .await
                    .unwrap();
            }

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 16 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            assert!(request[..read].starts_with(b"POST "));
            server_attempts.fetch_add(1, Ordering::SeqCst);
            let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"keep me\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\",\"usage\":{}}}\n\n";
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let client = buffered_test_client(format!("http://{addr}/backend-api/codex/responses"));
        let ctx = buffered_test_context("buffered-retry");
        let request = buffered_test_request();

        let response = tokio::time::timeout(
            Duration::from_secs(6),
            client.post_codex_with_transport(
                &request,
                &ctx,
                None,
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("buffered retry timed out")
        .expect("buffered retry failed");
        server.await.unwrap();

        let body = String::from_utf8(response.body).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            client.resolve_transport(crate::config::CodexTransport::Auto),
            crate::config::CodexTransport::Http
        );
        assert!(body.contains("keep me"));
        assert!(body.contains("response.completed"));
        assert!(!body.contains("discard me"));
    }

    #[tokio::test]
    async fn buffered_http_retries_body_reset_after_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();

        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 16 * 1024];
                let read = stream.read(&mut request).await.unwrap();
                assert!(read > 0);
                assert!(request[..read].starts_with(b"POST "));
                server_attempts.fetch_add(1, Ordering::SeqCst);

                if attempt == 0 {
                    let partial = b"data: discard me\n\n";
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        partial.len() + 128
                    );
                    stream.write_all(headers.as_bytes()).await.unwrap();
                    stream.write_all(partial).await.unwrap();
                    stream.shutdown().await.unwrap();
                    continue;
                }

                let body = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http_ok\",\"usage\":{}}}\n\n";
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });

        let client = buffered_test_client(format!("http://{addr}/responses"));
        let response = tokio::time::timeout(
            Duration::from_secs(8),
            client.post_codex_with_transport(
                &buffered_test_request(),
                &buffered_test_context("http-body-reset"),
                None,
                crate::config::CodexTransport::Http,
            ),
        )
        .await
        .expect("buffered HTTP retry timed out")
        .expect("buffered HTTP retry failed");
        server.await.unwrap();

        let body = String::from_utf8(response.body).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(body.contains("resp_http_ok"));
        assert!(!body.contains("discard me"));
    }

    #[tokio::test]
    async fn buffered_transport_retries_503_before_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();

        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 16 * 1024];
                let read = stream.read(&mut request).await.unwrap();
                assert!(read > 0);
                server_attempts.fetch_add(1, Ordering::SeqCst);

                let (status, retry_after, body): (&str, &str, &[u8]) = if attempt == 0 {
                    ("503 Service Unavailable", "retry-after: 0\r\n", b"retry me")
                } else {
                    ("200 OK", "", b"data: keep me\n\n")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\n{retry_after}connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });

        let client = buffered_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &buffered_test_context("retry-503"),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .expect("503 retry failed");
        server.await.unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep me\n\n");
    }

    #[tokio::test]
    async fn buffered_rate_limit_fails_fast() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 7\r\nretry-after: 60\r\nconnection: close\r\n\r\nlimited",
                )
                .await
                .unwrap();
        });

        let client = buffered_test_client(format!("http://{addr}/responses"));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.post_codex_with_transport(
                &buffered_test_request(),
                &buffered_test_context("rate-limit"),
                None,
                crate::config::CodexTransport::Http,
            ),
        )
        .await
        .expect("rate limit was retried");
        let Err(error) = result else {
            panic!("rate limit should fail");
        };
        server.await.unwrap();

        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after.as_deref(), Some("60"));
    }

    #[tokio::test]
    async fn buffered_request_has_wall_clock_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let _ = websocket.next().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let client = CodexHttpClient::new_for_test(
            reqwest::Client::new(),
            format!("http://{addr}/responses"),
            1_000,
            1,
            Duration::from_millis(50),
        );
        client.auth_manager().set_test_auth(StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: u64::MAX,
            account_id: Some("test_account".into()),
        });

        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &buffered_test_context("deadline"),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await;
        let Err(error) = result else {
            panic!("deadline should fail");
        };
        server.abort();

        assert_eq!(error.status, 504);
        assert_eq!(error.detail.as_deref(), Some("buffered_request_timeout"));
    }

    #[tokio::test]
    async fn exhausted_retry_budget_does_not_add_http_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..=MAX_BUFFERED_TRANSPORT_RETRIES {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                let _ = websocket.next().await;
                server_attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < MAX_BUFFERED_TRANSPORT_RETRIES {
                    websocket
                        .send(Message::Text(
                            r#"{"type":"response.failed","response":{"error":{"type":"overloaded_error","message":"overloaded","status":529,"retry_after_seconds":0}}}"#.into(),
                        ))
                        .await
                        .unwrap();
                }
            }
        });
        let client = buffered_test_client(format!("http://{addr}/responses"));

        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &buffered_test_context("retry-boundary"),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await;
        let Err(error) = result else {
            panic!("exhausted retry budget should fail");
        };
        server.await.unwrap();

        assert_eq!(error.status, 0);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            MAX_BUFFERED_TRANSPORT_ATTEMPTS as usize
        );
    }

    #[tokio::test]
    async fn buffered_auto_retries_overloaded_response_failed_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();

        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                let request = websocket.next().await.unwrap().unwrap();
                assert!(request.is_text());
                server_attempts.fetch_add(1, Ordering::SeqCst);

                if attempt == 0 {
                    websocket
                        .send(Message::Text(
                            r#"{"type":"response.failed","response":{"error":{"type":"overloaded_error","message":"overloaded","status":529,"retry_after_seconds":0}}}"#
                                .into(),
                        ))
                        .await
                        .unwrap();
                    continue;
                }

                websocket
                    .send(Message::Text(
                        r#"{"type":"response.output_text.delta","delta":"keep me"}"#.into(),
                    ))
                    .await
                    .unwrap();
                websocket
                    .send(Message::Text(
                        r#"{"type":"response.completed","response":{"id":"resp_after_overload","usage":{}}}"#
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        });

        let client = buffered_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &buffered_test_context("response-failed-overload"),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await
            .expect("overloaded response.failed retry failed");
        server.await.unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            String::from_utf8(response.body)
                .unwrap()
                .contains("keep me")
        );
    }

    #[tokio::test]
    async fn buffered_continuation_fallback_keeps_transport_retry_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = requests.clone();

        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                let request = websocket.next().await.unwrap().unwrap();
                let request: serde_json::Value =
                    serde_json::from_str(request.to_text().unwrap()).unwrap();
                server_requests.lock().unwrap().push(request);

                if attempt == 0 {
                    websocket
                        .send(Message::Text(
                            r#"{"type":"error","error":{"code":"previous_response_not_found","message":"previous response not found","status":400}}"#
                                .into(),
                        ))
                        .await
                        .unwrap();
                    continue;
                }
                websocket
                    .send(Message::Text(
                        r#"{"type":"response.output_text.delta","delta":"discard me"}"#.into(),
                    ))
                    .await
                    .unwrap();
            }

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 16 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            assert!(request[..read].starts_with(b"POST "));
            let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"keep me\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_full_context\",\"usage\":{}}}\n\n";
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let client = buffered_test_client(format!("http://{addr}/responses"));
        let mut ctx = buffered_test_context("continuation-then-reset");
        ctx.session_id = Some("session-continuation".into());
        let continuation = super::super::continuation::ContinuationCandidate {
            previous_response_id: Some("resp_previous".into()),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: None,
        };
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            client.post_codex_with_transport(
                &buffered_test_request(),
                &ctx,
                Some(&continuation),
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("continuation fallback retry timed out")
        .expect("continuation fallback retry failed");
        server.await.unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0]["previous_response_id"],
            serde_json::json!("resp_previous")
        );
        assert!(requests[1].get("previous_response_id").is_none());
        assert!(
            String::from_utf8(response.body)
                .unwrap()
                .contains("keep me")
        );
    }

    #[test]
    fn codex_headers_include_session_and_beta() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: Some("acct".into()),
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("s".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        };
        let headers = build_codex_headers(&auth, &ctx, false).unwrap();
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        assert_eq!(headers.get("session_id").unwrap(), "s");
    }

    #[test]
    fn codex_headers_include_responses_lite_when_requested() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        };
        let headers = build_codex_headers(&auth, &ctx, true).unwrap();
        assert_eq!(
            headers
                .get("x-openai-internal-codex-responses-lite")
                .unwrap(),
            "true"
        );
        assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
    }

    #[test]
    fn codex_headers_omit_session_when_missing() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        };
        let headers = build_codex_headers(&auth, &ctx, false).unwrap();
        assert!(headers.get("session_id").is_none());
        assert!(headers.get("x-client-request-id").is_none());
    }

    #[test]
    fn codex_headers_return_error_for_invalid_session_header() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("bad\nsession".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        };
        let err = build_codex_headers(&auth, &ctx, false).unwrap_err();
        assert_eq!(err.status, 500);
        assert!(err.message.contains("session_id"));
    }

    #[test]
    fn build_websocket_request_removes_stream() {
        let input = vec![
            super::super::translate::request::ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "hello".to_string(),
                    },
                ],
            },
        ];
        let req = ResponsesRequest {
            model: "gpt-5.5".to_string(),
            instructions: None,
            input,
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: Some("low".to_string()),
                format: None,
            },
            reasoning: None,
        };
        let payload = build_websocket_request(&req, None);
        assert_eq!(
            payload.get("type").and_then(|v| v.as_str()),
            Some("response.create")
        );
        assert!(payload.get("stream").is_none());
        assert!(payload.get("previous_response_id").is_none());
    }

    #[test]
    fn websocket_pool_key_tracks_continuation_opt_in() {
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("session".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        };
        let disabled = super::super::continuation::ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("disabled".into()),
        };
        let first_enabled = super::super::continuation::ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("missing_state".into()),
        };
        let append = super::super::continuation::ContinuationCandidate {
            previous_response_id: Some("resp_1".into()),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: None,
        };

        assert_eq!(websocket_pool_key(&ctx, Some(&disabled)), None);
        assert_eq!(
            websocket_pool_key(&ctx, Some(&first_enabled)),
            Some("session")
        );
        assert_eq!(websocket_pool_key(&ctx, Some(&append)), Some("session"));
    }

    #[test]
    fn websocket_pool_reset_ignores_initial_and_disabled_states() {
        let missing_state = super::super::continuation::ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("missing_state".into()),
        };
        let disabled = super::super::continuation::ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("disabled".into()),
        };
        let prompt_changed = super::super::continuation::ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("prompt_changed".into()),
        };

        assert!(!should_reset_websocket_pool(Some(&missing_state)));
        assert!(!should_reset_websocket_pool(Some(&disabled)));
        assert!(should_reset_websocket_pool(Some(&prompt_changed)));
    }

    #[test]
    fn build_codex_headers_error_on_empty_access() {
        let auth = StoredAuth {
            access: "".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
        };
        let result = build_codex_headers(&auth, &ctx, false);
        assert!(
            result.is_ok(),
            "empty access should still produce valid Bearer header"
        );
    }

    #[test]
    fn codex_header_timeout_error_display() {
        let err = CodexHeaderTimeoutError { timeout_ms: 60000 };
        let display = format!("{err}");
        assert!(display.contains("60000"));
    }

    #[test]
    fn codex_transport_error_display() {
        let err = CodexTransportError {
            message: "connection reset".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("connection reset"));
    }

    #[test]
    fn unauthorized_retry_check_covers_http_and_websocket_results() {
        let http_unauthorized = Ok(CodexResponse {
            body: Vec::new(),
            status: 401,
            headers: Vec::new(),
        });
        let websocket_unauthorized = Err(CodexError {
            status: 401,
            message: "WebSocket connect error".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        });
        let forbidden = Err(CodexError {
            status: 403,
            message: "Forbidden".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        });

        assert!(should_refresh_after_unauthorized(&http_unauthorized, false));
        assert!(should_refresh_after_unauthorized(
            &websocket_unauthorized,
            false
        ));
        assert!(!should_refresh_after_unauthorized(&forbidden, false));
        assert!(!should_refresh_after_unauthorized(&http_unauthorized, true));
    }

    #[test]
    fn continuation_retry_requires_previous_response_id() {
        let append = super::super::continuation::ContinuationCandidate {
            previous_response_id: Some("resp_1".into()),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: None,
        };
        let initial = super::super::continuation::ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("missing_state".into()),
        };
        let timeout = CodexError {
            status: 0,
            message: "WebSocket response start timeout after 60000ms".to_string(),
            detail: Some(
                super::super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string(),
            ),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let missing = CodexError {
            status: 0,
            message: "Previous response not found".to_string(),
            detail: Some("previous_response_not_found".to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let idle = CodexError {
            status: 0,
            message: "WebSocket idle timeout after 60000ms".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(should_retry_without_continuation(&timeout, Some(&append)));
        assert!(should_retry_without_continuation(&missing, Some(&append)));
        assert!(!should_retry_without_continuation(&idle, Some(&append)));
        assert!(!should_retry_without_continuation(&timeout, Some(&initial)));
        assert!(!should_retry_without_continuation(&timeout, None));
    }
}
