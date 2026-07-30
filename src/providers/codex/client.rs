use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

use crate::anthropic::sse::{parse_sse_events, try_parse_sse_events};
use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::retry::{BackoffOutcome, ModelRetryBackoff, ReplaySafety, should_retry_status, sleep};
use crate::traffic::TrafficCapture;

use super::auth::constants::{CODEX_API_ENDPOINT, ORIGINATOR, RESPONSES_LITE_ORIGINATOR};
use super::auth::manager::{CodexAuthErrorKind, CodexAuthManager, codex_auth_error_kind};
use super::auth::token_store::{DefaultCodexAuthStore, StoredAuth, file_store};
use super::dispatch_budget::{
    CODEX_DISPATCH_BUDGET_DETAIL, CodexDispatchBudget, CodexDispatchBudgetExceeded,
    CodexModelDispatchReservation, MAX_CODEX_MODEL_DISPATCHES,
};
use super::translate::request::ResponsesRequest;

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
    UpstreamHttp,
    WebSocket,
    WebSocketHandshake,
    Auth,
    BufferedHttp,
    BufferedWebSocket,
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

    /// Classify model replay safety from structured error provenance.
    ///
    /// Keep this conservative: only constructors that prove the model request
    /// was not dispatched, or an explicit upstream retry response, may opt in.
    pub fn replay_safety(&self) -> ReplaySafety {
        if self.origin == CodexErrorOrigin::WebSocketHandshake
            || matches!(
                self.detail.as_deref(),
                Some(
                    CODEX_HTTP_CONNECT_ERROR_DETAIL
                        | "websocket_pre_request"
                        | super::websocket::WEBSOCKET_POOL_HEALTHCHECK_DETAIL
                        | super::websocket::WEBSOCKET_POOL_BUSY_DETAIL
                )
            )
        {
            return ReplaySafety::DefinitelyNotDispatched;
        }

        if self.detail.as_deref() == Some("previous_response_not_found") {
            return ReplaySafety::ExplicitlyRetryableResponse;
        }

        let explicit_retry_status = matches!(self.status, 401 | 429 | 500 | 502 | 503 | 504 | 529);
        let explicit_upstream_origin = matches!(
            self.origin,
            CodexErrorOrigin::UpstreamHttp
                | CodexErrorOrigin::BufferedHttp
                | CodexErrorOrigin::BufferedWebSocket
                | CodexErrorOrigin::WebSocket
        );
        let local_failure = matches!(
            self.detail.as_deref(),
            Some(CODEX_TOTAL_TIMEOUT_DETAIL | CODEX_DISPATCH_BUDGET_DETAIL)
        );
        if explicit_retry_status && explicit_upstream_origin && !local_failure {
            return ReplaySafety::ExplicitlyRetryableResponse;
        }

        ReplaySafety::OutcomeUnknown
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

fn default_user_agent(use_responses_lite: bool) -> String {
    if use_responses_lite {
        RESPONSES_LITE_ORIGINATOR.to_string()
    } else {
        format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"))
    }
}

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
    let user_agent = config::codex_user_agent(&default_user_agent(use_responses_lite));
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

const MAX_BUFFERED_TRANSPORT_ATTEMPTS: u32 = MAX_CODEX_MODEL_DISPATCHES;
const CODEX_HTTP_CONNECT_ERROR_DETAIL: &str = "http_connect_error";
pub(super) const CODEX_WEBSOCKET_ENV_PROXY_UNSUPPORTED_DETAIL: &str =
    "websocket_environment_proxy_unsupported";
const HTTP_CONNECT_TIMEOUT_MS: u64 = 15_000;
const HTTP_RESPONSE_HEADER_TIMEOUT_MS: u64 = 60_000;
const HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS: u64 = 300_000;
const LIVE_HTTP_ERROR_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LIVE_HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_BUFFERED_HTTP_SUCCESS_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_BUFFERED_HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;
pub(super) const DEFAULT_CODEX_TOTAL_TIMEOUT_MS: u64 = 540_000;
pub(super) const CODEX_TOTAL_TIMEOUT_DETAIL: &str = "codex_total_timeout";
const MAX_CODEX_TOTAL_TIMEOUT_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy)]
pub(super) struct CodexRequestDeadline {
    at: tokio::time::Instant,
    timeout_ms: u64,
}

impl CodexRequestDeadline {
    pub(super) fn configured_from(started_at: Instant) -> Self {
        Self::from_started_at(
            started_at,
            config::codex_total_timeout_ms(DEFAULT_CODEX_TOTAL_TIMEOUT_MS),
        )
    }

    pub(super) fn from_timeout_ms(timeout_ms: u64) -> Self {
        let timeout_ms = timeout_ms.min(MAX_CODEX_TOTAL_TIMEOUT_MS);
        let now = tokio::time::Instant::now();
        Self {
            at: now
                .checked_add(Duration::from_millis(timeout_ms))
                .unwrap_or(now),
            timeout_ms,
        }
    }

    fn from_started_at(started_at: Instant, timeout_ms: u64) -> Self {
        let timeout_ms = timeout_ms.min(MAX_CODEX_TOTAL_TIMEOUT_MS);
        let remaining = Duration::from_millis(timeout_ms).saturating_sub(started_at.elapsed());
        let now = tokio::time::Instant::now();
        Self {
            at: now.checked_add(remaining).unwrap_or(now),
            timeout_ms,
        }
    }

    pub(super) fn at(self) -> tokio::time::Instant {
        self.at
    }

    pub(super) fn timeout_ms(self) -> u64 {
        self.timeout_ms
    }
}

struct CodexRequestGuard {
    session_id: Option<String>,
    turn_id: Option<u64>,
    pool_key: Option<String>,
    armed: bool,
}

impl CodexRequestGuard {
    fn new(
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        transport: crate::config::CodexTransport,
    ) -> Self {
        Self {
            session_id: ctx.session_id.clone(),
            turn_id: continuation.and_then(|candidate| candidate.turn_id),
            pool_key: if matches!(transport, crate::config::CodexTransport::Http) {
                None
            } else {
                websocket_pool_key(ctx, continuation).map(str::to_string)
            },
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CodexRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Invalidation is turn-gated, so it must run while the continuation
        // still owns the turn. Aborting first would make this a no-op and could
        // leave a partially consumed pooled socket available for reuse.
        if let Some(key) = self.pool_key.as_deref() {
            super::websocket::invalidate_codex_websocket_pool_turn(key, self.turn_id);
        }
        super::continuation::abort_continuation(self.session_id.as_deref(), self.turn_id);
    }
}

pub struct CodexHttpClient {
    client: reqwest::Client,
    auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
    base_url: String,
    websocket_circuit_key: String,
    transport_decision: config::CodexTransportDecision,
    header_timeout_ms: u64,
    http_first_byte_timeout_ms: u64,
    body_idle_timeout_ms: u64,
    total_timeout_ms: u64,
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

fn build_codex_http_client(base_url: &str, connect_timeout_ms: u64) -> reqwest::Client {
    let mut builder =
        crate::upstream_http::model_client_builder(Duration::from_millis(connect_timeout_ms));
    if crate::oauth_http::is_loopback_url(base_url) {
        builder = builder.no_proxy();
    }
    builder.build().expect("failed to create HTTP client")
}

impl CodexHttpClient {
    pub fn new() -> Self {
        let base_url = config::codex_base_url(CODEX_API_ENDPOINT);
        let connect_timeout_ms = config::codex_connect_timeout_ms(HTTP_CONNECT_TIMEOUT_MS);
        let body_idle_timeout_ms =
            config::codex_body_idle_timeout_ms(HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS);
        // Preserve the historical behavior unless an independent first-byte
        // timeout is configured: the resolved body-idle timeout remains the
        // first-chunk limit as well.
        let http_first_byte_timeout_ms =
            config::codex_http_first_byte_timeout_ms(body_idle_timeout_ms);
        // Reqwest snapshots its system proxy matcher when the client is built.
        // Capture the Auto transport decision in the same construction window
        // and keep both immutable for this provider lifetime.
        let transport_decision = config::codex_transport_decision_for_url(&base_url);
        Self {
            client: build_codex_http_client(&base_url, connect_timeout_ms),
            auth_manager: CodexAuthManager::new(file_store()),
            websocket_circuit_key: websocket_circuit_key_for_url(&base_url),
            base_url,
            transport_decision,
            header_timeout_ms: config::codex_header_timeout_ms(HTTP_RESPONSE_HEADER_TIMEOUT_MS),
            http_first_byte_timeout_ms,
            body_idle_timeout_ms,
            total_timeout_ms: config::codex_total_timeout_ms(DEFAULT_CODEX_TOTAL_TIMEOUT_MS),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_client(
        client: reqwest::Client,
        auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
        base_url: String,
    ) -> Self {
        let websocket_circuit_key = websocket_circuit_key_for_url(&base_url);
        let transport_decision = config::codex_transport_decision_for_url(&base_url);
        Self {
            client,
            auth_manager,
            base_url,
            websocket_circuit_key,
            transport_decision,
            header_timeout_ms: HTTP_RESPONSE_HEADER_TIMEOUT_MS,
            http_first_byte_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            total_timeout_ms: config::codex_total_timeout_ms(DEFAULT_CODEX_TOTAL_TIMEOUT_MS),
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        base_url: String,
        header_timeout_ms: u64,
        body_idle_timeout_ms: u64,
        total_timeout_ms: u64,
    ) -> Self {
        let websocket_circuit_key = websocket_circuit_key_for_url(&base_url);
        let transport_decision = config::codex_transport_decision_for_url(&base_url);
        Self {
            client: build_codex_http_client(&base_url, HTTP_CONNECT_TIMEOUT_MS),
            auth_manager: CodexAuthManager::new(file_store()),
            base_url,
            websocket_circuit_key,
            transport_decision,
            header_timeout_ms,
            http_first_byte_timeout_ms: body_idle_timeout_ms,
            body_idle_timeout_ms,
            total_timeout_ms,
        }
    }

    pub fn auth_manager(&self) -> &CodexAuthManager<DefaultCodexAuthStore> {
        &self.auth_manager
    }

    pub(super) fn websocket_circuit_key(&self) -> &str {
        &self.websocket_circuit_key
    }

    pub(super) fn transport_decision(&self) -> config::CodexTransportDecision {
        self.transport_decision
    }

    pub(super) fn transport(&self) -> config::CodexTransport {
        self.transport_decision.effective()
    }

    fn validate_transport_preflight(
        &self,
        transport: config::CodexTransport,
    ) -> Result<(), CodexError> {
        if transport == config::CodexTransport::WebSocket
            && self
                .transport_decision
                .explicit_websocket_environment_proxy_unsupported()
        {
            return Err(CodexError {
                status: 400,
                message: "Explicit Codex WebSocket transport cannot use the configured HTTP(S) environment proxy. Set CCP_CODEX_TRANSPORT=auto or http, or add the Codex upstream host to NO_PROXY to allow a direct WebSocket connection".to_string(),
                detail: Some(CODEX_WEBSOCKET_ENV_PROXY_UNSUPPORTED_DETAIL.to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::WebSocketHandshake,
            });
        }
        Ok(())
    }

    pub async fn post_codex(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<CodexResponse, CodexError> {
        self.post_codex_with_transport(body, ctx, continuation, self.transport())
            .await
    }

    pub(super) async fn post_codex_before(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        deadline: CodexRequestDeadline,
    ) -> Result<CodexResponse, CodexError> {
        self.post_codex_with_transport_before(body, ctx, continuation, self.transport(), deadline)
            .await
    }

    async fn post_codex_with_transport(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        transport: crate::config::CodexTransport,
    ) -> Result<CodexResponse, CodexError> {
        self.post_codex_with_transport_before(
            body,
            ctx,
            continuation,
            transport,
            CodexRequestDeadline::from_timeout_ms(self.total_timeout_ms),
        )
        .await
    }

    async fn post_codex_with_transport_before(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        transport: crate::config::CodexTransport,
        deadline: CodexRequestDeadline,
    ) -> Result<CodexResponse, CodexError> {
        let mut guard = CodexRequestGuard::new(ctx, continuation, transport);
        let dispatch_budget = CodexDispatchBudget::new();
        let result = match tokio::time::timeout_at(
            deadline.at(),
            self.post_codex_with_transport_inner(
                body,
                ctx,
                continuation,
                transport,
                &dispatch_budget,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(codex_total_timeout_error(transport, deadline.timeout_ms())),
        };
        if result.is_ok() {
            guard.disarm();
        }
        result
    }

    async fn post_codex_with_transport_inner(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        transport: crate::config::CodexTransport,
        dispatch_budget: &CodexDispatchBudget,
    ) -> Result<CodexResponse, CodexError> {
        use crate::config::CodexTransport;

        self.validate_transport_preflight(transport)?;

        let mut auth = self
            .auth_manager
            .get_auth_with_budget(dispatch_budget)
            .await
            .map_err(auth_refresh_error)?;

        let initial_pool_key = websocket_pool_key(ctx, continuation);
        if should_reset_websocket_pool(continuation)
            && let Some(key) = initial_pool_key
        {
            super::websocket::invalidate_codex_websocket_pool_turn(
                key,
                continuation.and_then(|candidate| candidate.turn_id),
            );
        }

        let turn_id = continuation.and_then(|candidate| candidate.turn_id);
        let mut active_continuation = continuation.cloned();
        // Auto may degrade to HTTP, but never switches back within this logical request.
        let mut active_transport = transport;
        let mut auth_refresh_attempted = false;
        let mut reserved_model_replay: Option<CodexModelDispatchReservation> = None;
        let mut model_retry_backoff = ModelRetryBackoff::default();
        let mut serialized_http_body = None;
        let websocket_timeouts = super::websocket::CodexWebSocketTimeouts::configured();
        loop {
            let reservation = match reserved_model_replay.take() {
                Some(reservation) => reservation,
                None => dispatch_budget
                    .reserve_model()
                    .map_err(|error| model_dispatch_budget_error(error, active_transport))?,
            };
            let mut physical_attempts = reservation.attempt;
            let pool_key = websocket_pool_key(ctx, active_continuation.as_ref());
            let (result, result_transport) = match active_transport {
                CodexTransport::Http => {
                    let body_json = cached_serialized_request(&mut serialized_http_body, body)?;
                    (
                        self.attempt_post_http(
                            &auth,
                            &body_json,
                            ctx,
                            body.client_metadata.is_some(),
                        )
                        .await,
                        CodexTransport::Http,
                    )
                }
                CodexTransport::WebSocket => {
                    let ws_headers =
                        build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let ws_body = build_websocket_request(body, active_continuation.as_ref());

                    (
                        super::websocket::codex_websocket_request(
                            &self.base_url,
                            &ws_headers,
                            &ws_body,
                            ctx,
                            ctx.traffic.as_deref(),
                            pool_key,
                            websocket_timeouts,
                            active_continuation.as_ref(),
                        )
                        .await,
                        CodexTransport::WebSocket,
                    )
                }
                CodexTransport::Auto => {
                    let circuit_key = self.websocket_circuit_key();
                    if super::websocket::codex_websocket_circuit_open(circuit_key) {
                        log_websocket_circuit_fallback(ctx);
                        active_transport = CodexTransport::Http;
                        let body_json = cached_serialized_request(&mut serialized_http_body, body)?;
                        (
                            self.attempt_post_http(
                                &auth,
                                &body_json,
                                ctx,
                                body.client_metadata.is_some(),
                            )
                            .await,
                            CodexTransport::Http,
                        )
                    } else {
                        let ws_headers =
                            build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
                        let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                        let ws_body = build_websocket_request(body, active_continuation.as_ref());

                        // Try WebSocket first.
                        let ws_result = super::websocket::codex_websocket_request(
                            &self.base_url,
                            &ws_headers,
                            &ws_body,
                            ctx,
                            ctx.traffic.as_deref(),
                            pool_key,
                            websocket_timeouts,
                            active_continuation.as_ref(),
                        )
                        .await;

                        match ws_result {
                            Ok(response) => {
                                super::websocket::record_codex_websocket_success(circuit_key);
                                (Ok(response), CodexTransport::WebSocket)
                            }
                            Err(err) if should_fallback_to_http(&err) => {
                                record_auto_websocket_failure(ctx, circuit_key, &err);
                                if dispatch_budget.can_reserve_model() {
                                    log_auto_http_fallback(ctx, &err);
                                    let Some(delay) =
                                        auto_http_fallback_delay(&err, &mut model_retry_backoff)
                                    else {
                                        return Err(err);
                                    };
                                    if delay.exceeds_budget {
                                        return Err(err);
                                    }
                                    if let Some(key) = pool_key {
                                        super::websocket::invalidate_codex_websocket_pool_turn(
                                            key, turn_id,
                                        );
                                    }
                                    active_continuation =
                                        full_context_continuation(active_continuation.as_ref());
                                    active_transport = CodexTransport::Http;
                                    if delay.wait_ms > 0 {
                                        sleep(delay.wait_ms).await;
                                    }
                                    let body_json =
                                        cached_serialized_request(&mut serialized_http_body, body)?;
                                    let fallback =
                                        dispatch_budget.reserve_model().map_err(|error| {
                                            model_dispatch_budget_error(error, CodexTransport::Http)
                                        })?;
                                    physical_attempts = fallback.attempt;
                                    (
                                        self.attempt_post_http(
                                            &auth,
                                            &body_json,
                                            ctx,
                                            body.client_metadata.is_some(),
                                        )
                                        .await,
                                        CodexTransport::Http,
                                    )
                                } else {
                                    (Err(err), CodexTransport::WebSocket)
                                }
                            }
                            Err(err) => {
                                record_auto_websocket_failure(ctx, circuit_key, &err);
                                (Err(err), CodexTransport::WebSocket)
                            }
                        }
                    }
                }
            };

            if should_refresh_after_unauthorized(&result, auth_refresh_attempted)
                && let Ok(replay) = dispatch_budget.reserve_model()
            {
                auth_refresh_attempted = true;
                match self
                    .auth_manager
                    .force_refresh_with_budget(&auth.access, dispatch_budget)
                    .await
                {
                    Ok(new_auth) => {
                        auth = new_auth;
                        reserved_model_replay = Some(replay);
                        if let Some(key) = pool_key {
                            super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                        }
                        active_continuation =
                            full_context_continuation(active_continuation.as_ref());
                        continue;
                    }
                    Err(error) => return Err(auth_refresh_error(error)),
                }
            }

            if let Ok(response) = &result
                && (200..300).contains(&response.status)
                && let Some(failure) = super::events::first_retryable_failure(&response.body)
            {
                if dispatch_budget.can_reserve_model() {
                    let delay = model_retry_backoff
                        .next_delay(
                            ReplaySafety::ExplicitlyRetryableResponse,
                            failure.retry_after.as_deref(),
                        )
                        .expect("explicit retry response must permit model replay");
                    if delay.exceeds_budget {
                        return Err(CodexError {
                            status: failure.status,
                            message: failure.message.clone(),
                            detail: Some(failure.message),
                            retry_after: failure.retry_after,
                            origin: buffered_origin(result_transport),
                        });
                    }
                    log_buffered_retry(
                        ctx,
                        result_transport,
                        physical_attempts,
                        delay.wait_ms,
                        failure.status,
                        "upstream_event",
                        &failure.message,
                    );
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    sleep(delay.wait_ms).await;
                    continue;
                }

                log_buffered_retry_exhausted(
                    ctx,
                    result_transport,
                    failure.status,
                    "upstream_event",
                    &failure.message,
                );
                return Err(CodexError {
                    status: failure.status,
                    message: failure.message.clone(),
                    detail: Some(failure.message),
                    retry_after: failure.retry_after,
                    origin: buffered_origin(result_transport),
                });
            }

            match result {
                Ok(response)
                    if codex_error_body_is_context_overflow(
                        &response.body,
                        Some(response.status),
                    ) =>
                {
                    // A structured context code is terminal even when an
                    // upstream gateway wraps it in an otherwise retryable
                    // outer status such as 429/5xx. Replaying the same
                    // oversized prompt cannot succeed and only burns model
                    // dispatch budget.
                    return Err(codex_status_error(response, result_transport));
                }
                Ok(response) if response.status == 401 => {
                    let detail = codex_status_error_message(&response.body)
                        .unwrap_or_else(|| "Authentication failed".to_string());
                    return Err(CodexError {
                        status: 401,
                        message: "Unauthorized".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 403 => {
                    let detail = codex_status_error_message(&response.body)
                        .unwrap_or_else(|| "Authentication failed".to_string());
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
                    if dispatch_budget.can_reserve_model() {
                        let delay = model_retry_backoff
                            .next_delay(
                                ReplaySafety::ExplicitlyRetryableResponse,
                                retry_after.as_deref(),
                            )
                            .expect("explicit retry response must permit model replay");
                        if delay.exceeds_budget {
                            let detail = codex_status_error_message(&response.body)
                                .unwrap_or_else(|| "Rate limited".to_string());
                            return Err(CodexError {
                                status: 429,
                                message: "Rate limited".to_string(),
                                detail: Some(detail),
                                retry_after,
                                origin: CodexErrorOrigin::Http,
                            });
                        }
                        log_buffered_retry(
                            ctx,
                            result_transport,
                            physical_attempts,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "rate limited",
                        );
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    let detail = codex_status_error_message(&response.body)
                        .unwrap_or_else(|| "Rate limited".to_string());
                    log_buffered_retry_exhausted(
                        ctx,
                        result_transport,
                        response.status,
                        "upstream",
                        "rate limited",
                    );
                    return Err(CodexError {
                        status: 429,
                        message: "Rate limited".to_string(),
                        detail: Some(detail),
                        retry_after,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if should_retry_codex_status(response.status) => {
                    if dispatch_budget.can_reserve_model() {
                        let retry_after = response
                            .headers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                            .map(|(_, value)| value.as_str());
                        let delay = model_retry_backoff
                            .next_delay(ReplaySafety::ExplicitlyRetryableResponse, retry_after)
                            .expect("explicit retry response must permit model replay");
                        if delay.exceeds_budget {
                            return Err(codex_status_error(response, result_transport));
                        }
                        log_buffered_retry(
                            ctx,
                            result_transport,
                            physical_attempts,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "retryable upstream status",
                        );
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    log_buffered_retry_exhausted(
                        ctx,
                        result_transport,
                        response.status,
                        "upstream",
                        "retryable upstream status",
                    );
                    return Err(codex_status_error(response, result_transport));
                }
                Ok(response) if !(200..300).contains(&response.status) => {
                    return Err(codex_status_error(response, result_transport));
                }
                Ok(response) => return Ok(response),
                Err(err)
                    if dispatch_budget.can_reserve_model()
                        && should_retry_without_continuation(
                            &err,
                            active_continuation.as_ref(),
                        ) =>
                {
                    if let Some(key) = pool_key {
                        super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                    }
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    if err.detail.as_deref() != Some("previous_response_not_found") {
                        let delay = model_retry_backoff
                            .next_delay(err.replay_safety(), err.retry_after.as_deref())
                            .expect("replay-safe continuation recovery must have a retry delay");
                        if delay.exceeds_budget {
                            return Err(err);
                        }
                        sleep(delay.wait_ms).await;
                    }
                    continue;
                }
                Err(err) => {
                    // Determine if retryable
                    let retryable = is_retryable_transport_error(&err);
                    let replayable = retryable && err.replay_safety().permits_model_replay();
                    if replayable && dispatch_budget.can_reserve_model() {
                        let delay = model_retry_backoff
                            .next_delay(err.replay_safety(), err.retry_after.as_deref())
                            .expect("replayable transport error must have a model retry delay");
                        if delay.exceeds_budget {
                            return Err(err);
                        }
                        log_buffered_retry(
                            ctx,
                            result_transport,
                            physical_attempts,
                            delay.wait_ms,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    if replayable {
                        log_buffered_retry_exhausted(
                            ctx,
                            result_transport,
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

    pub(super) async fn stream_codex_websocket_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
        dispatch_budget: CodexDispatchBudget,
    ) -> Result<super::websocket::CodexWebSocketEventReceiver, CodexError> {
        self.validate_transport_preflight(config::CodexTransport::WebSocket)?;

        let auth = self
            .auth_manager
            .get_auth_with_budget(&dispatch_budget)
            .await
            .map_err(auth_refresh_error)?;

        let turn_id = continuation.and_then(|candidate| candidate.turn_id);
        let pool_key = websocket_pool_key(ctx, continuation).map(str::to_string);
        if should_reset_websocket_pool(continuation)
            && let Some(key) = pool_key.as_deref()
        {
            super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
        }

        let client = self.clone();
        let body = body.clone();
        let ctx = ctx.clone();
        let continuation = continuation.cloned();
        let (tx, rx) = tokio::sync::mpsc::channel(super::LIVE_EVENT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            client
                .coordinate_live_websocket_events(
                    body,
                    ctx,
                    continuation,
                    auth,
                    pool_key,
                    dispatch_budget,
                    tx,
                )
                .await;
        });

        Ok(rx)
    }

    #[allow(clippy::too_many_arguments)]
    async fn coordinate_live_websocket_events(
        &self,
        body: ResponsesRequest,
        ctx: RequestContext,
        mut continuation: Option<super::continuation::ContinuationCandidate>,
        mut auth: StoredAuth,
        pool_key: Option<String>,
        dispatch_budget: CodexDispatchBudget,
        tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
    ) {
        let turn_id = continuation
            .as_ref()
            .and_then(|candidate| candidate.turn_id);
        let mut auth_refresh_attempted = false;
        let mut continuation_retry_available = continuation
            .as_ref()
            .and_then(|candidate| candidate.previous_response_id.as_deref())
            .is_some();
        let mut replay_window_closed = false;
        let mut reserved_model_replay: Option<CodexModelDispatchReservation> = None;
        let mut model_retry_backoff = ModelRetryBackoff::default();
        let websocket_timeouts = super::websocket::CodexWebSocketTimeouts::configured();

        'attempt: loop {
            let ws_headers = match build_codex_headers(&auth, &ctx, body.client_metadata.is_some())
            {
                Ok(headers) => super::websocket::codex_websocket_headers(&headers),
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            };
            let ws_body = build_websocket_request(&body, continuation.as_ref());
            let reservation = match reserved_model_replay.take() {
                Some(reservation) => reservation,
                None => match dispatch_budget.reserve_model() {
                    Ok(reservation) => reservation,
                    Err(error) => {
                        let _ = tx
                            .send(Err(model_dispatch_budget_error(
                                error,
                                crate::config::CodexTransport::WebSocket,
                            )))
                            .await;
                        return;
                    }
                },
            };
            let _model_attempt = reservation.attempt;
            let start = super::websocket::codex_websocket_event_stream(
                &self.base_url,
                &ws_headers,
                &ws_body,
                &ctx,
                ctx.traffic.clone(),
                pool_key.as_deref(),
                websocket_timeouts,
                continuation.as_ref(),
            );
            let mut stream = tokio::select! {
                _ = tx.closed() => {
                    if let Some(key) = pool_key.as_deref() {
                        super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                    }
                    super::continuation::abort_continuation(ctx.session_id.as_deref(), turn_id);
                    return;
                }
                result = start => match result {
                    Ok(stream) => stream,
                    Err(err)
                        if err.status == 401
                            && !auth_refresh_attempted
                            && !replay_window_closed =>
                    {
                        let replay = match dispatch_budget.reserve_model() {
                            Ok(replay) => replay,
                            Err(_) => {
                                let _ = tx.send(Err(err)).await;
                                return;
                            }
                        };
                        auth_refresh_attempted = true;
                        if let Some(key) = pool_key.as_deref() {
                            super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                        }
                        let refresh = self
                            .auth_manager
                            .force_refresh_with_budget(&auth.access, &dispatch_budget);
                        auth = tokio::select! {
                            _ = tx.closed() => {
                                super::continuation::abort_continuation(
                                    ctx.session_id.as_deref(),
                                    turn_id,
                                );
                                return;
                            }
                            result = refresh => match result {
                                Ok(auth) => auth,
                                Err(refresh_err) => {
                                    let _ = tx.send(Err(auth_refresh_error(refresh_err))).await;
                                    return;
                                }
                            }
                        };
                        reserved_model_replay = Some(replay);
                        continuation_retry_available = false;
                        continuation = full_context_continuation(continuation.as_ref());
                        continue 'attempt;
                    }
                    Err(err) if continuation_retry_available && is_continuation_retry_error(&err) => {
                        continuation_retry_available = false;
                        if let Some(key) = pool_key.as_deref() {
                            super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                        }
                        continuation = full_context_continuation(continuation.as_ref());
                        if err.detail.as_deref() != Some("previous_response_not_found") {
                            let delay = model_retry_backoff
                                .next_delay(err.replay_safety(), err.retry_after.as_deref())
                                .expect("replay-safe continuation recovery must have a retry delay");
                            if delay.exceeds_budget {
                                let _ = tx.send(Err(err)).await;
                                return;
                            }
                            tokio::select! {
                                _ = tx.closed() => {
                                    super::continuation::abort_continuation(
                                        ctx.session_id.as_deref(),
                                        turn_id,
                                    );
                                    return;
                                }
                                _ = sleep(delay.wait_ms) => {}
                            }
                        }
                        continue 'attempt;
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                }
            };

            loop {
                let item = tokio::select! {
                    _ = tx.closed() => {
                        if let Some(key) = pool_key.as_deref() {
                            super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                        }
                        super::continuation::abort_continuation(
                            ctx.session_id.as_deref(),
                            turn_id,
                        );
                        return;
                    }
                    item = stream.recv() => item,
                };
                let Some(item) = item else {
                    return;
                };

                let unauthorized = match &item {
                    Err(err) => err.status == 401,
                    Ok(payload) => super::websocket::event_error_status(payload) == Some(401),
                };
                if unauthorized && !auth_refresh_attempted && !replay_window_closed {
                    let replay = match dispatch_budget.reserve_model() {
                        Ok(replay) => replay,
                        Err(_) => {
                            let _ = tx.send(item).await;
                            return;
                        }
                    };
                    auth_refresh_attempted = true;
                    if let Some(key) = pool_key.as_deref() {
                        super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                    }
                    let refresh = self
                        .auth_manager
                        .force_refresh_with_budget(&auth.access, &dispatch_budget);
                    auth = tokio::select! {
                        _ = tx.closed() => {
                            super::continuation::abort_continuation(
                                ctx.session_id.as_deref(),
                                turn_id,
                            );
                            return;
                        }
                        result = refresh => match result {
                            Ok(auth) => auth,
                            Err(refresh_err) => {
                                let _ = tx.send(Err(auth_refresh_error(refresh_err))).await;
                                return;
                            }
                        }
                    };
                    reserved_model_replay = Some(replay);
                    continuation_retry_available = false;
                    continuation = full_context_continuation(continuation.as_ref());
                    continue 'attempt;
                }

                if let Err(err) = &item
                    && continuation_retry_available
                    && is_continuation_retry_error(err)
                    && !replay_window_closed
                {
                    let delay = if err.detail.as_deref() == Some("previous_response_not_found") {
                        None
                    } else {
                        let delay = model_retry_backoff
                            .next_delay(err.replay_safety(), err.retry_after.as_deref())
                            .expect("replay-safe continuation recovery must have a retry delay");
                        if delay.exceeds_budget {
                            let _ = tx.send(item).await;
                            return;
                        }
                        Some(delay.wait_ms)
                    };
                    continuation_retry_available = false;
                    if let Some(key) = pool_key.as_deref() {
                        super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                    }
                    continuation = full_context_continuation(continuation.as_ref());
                    if let Some(wait_ms) = delay {
                        tokio::select! {
                            _ = tx.closed() => {
                                super::continuation::abort_continuation(
                                    ctx.session_id.as_deref(),
                                    turn_id,
                                );
                                return;
                            }
                            _ = sleep(wait_ms) => {}
                        }
                    }
                    continue 'attempt;
                }

                if item.as_ref().is_ok_and(event_closes_live_retry_window) {
                    replay_window_closed = true;
                }
                let terminal = item.as_ref().is_ok_and(|payload| {
                    super::events::CodexTerminalKind::from_payload(payload).is_some()
                });
                if tx.send(item).await.is_err() {
                    if let Some(key) = pool_key.as_deref() {
                        super::websocket::invalidate_codex_websocket_pool_turn(key, turn_id);
                    }
                    super::continuation::abort_continuation(ctx.session_id.as_deref(), turn_id);
                    return;
                }
                if terminal {
                    // The downstream response is complete, but the inner WebSocket worker may
                    // still be running its bounded pool-reuse probe. Ignore downstream closure
                    // while it finishes so a successfully probed continuation socket is not
                    // invalidated merely because Claude consumed message_stop promptly.
                    while stream.recv().await.is_some() {}
                    return;
                }
            }
        }
    }

    pub(super) async fn stream_codex_http_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        deadline: CodexRequestDeadline,
        dispatch_budget: CodexDispatchBudget,
    ) -> Result<super::websocket::CodexWebSocketEventReceiver, CodexError> {
        let mut auth = self
            .auth_manager
            .get_auth_with_budget(&dispatch_budget)
            .await
            .map_err(auth_refresh_error)?;
        let body_json = serialize_request(body)?;
        let mut auth_refresh_attempted = false;
        let mut reserved_model_replay: Option<CodexModelDispatchReservation> = None;

        loop {
            let headers = build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
            match reserved_model_replay.take() {
                Some(_) => {}
                None => {
                    dispatch_budget.reserve_model().map_err(|error| {
                        model_dispatch_budget_error(error, crate::config::CodexTransport::Http)
                    })?;
                }
            }
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_codex_http_request_capture(traffic, &self.base_url, &headers, &body_json);
            }

            let mut request = self.client.post(&self.base_url);
            for (key, value) in headers.iter() {
                request = request.header(key.as_str(), value.as_bytes());
            }
            let started_at = Instant::now();
            let send = tokio::time::timeout(
                Duration::from_millis(self.header_timeout_ms),
                request.body(body_json.clone()).send(),
            );
            let response = tokio::select! {
                _ = tokio::time::sleep_until(deadline.at()) => {
                    return Err(codex_total_timeout_error(
                        crate::config::CodexTransport::Http,
                        deadline.timeout_ms(),
                    ));
                }
                result = send => match result {
                    Err(_) => {
                        return Err(CodexError {
                            status: 0,
                            message: format!(
                                "Timed out waiting {}ms for Codex response headers",
                                self.header_timeout_ms
                            ),
                            detail: Some("http_response_headers".to_string()),
                            retry_after: None,
                            origin: CodexErrorOrigin::Http,
                        });
                    }
                    Ok(Err(error)) => {
                        let detail = if error.is_connect() {
                            CODEX_HTTP_CONNECT_ERROR_DETAIL
                        } else {
                            "http_response_headers"
                        };
                        return Err(CodexError {
                            status: 0,
                            message: format!("Transport error waiting for Codex response headers: {error}"),
                            detail: Some(detail.to_string()),
                            retry_after: None,
                            origin: CodexErrorOrigin::Http,
                        });
                    }
                    Ok(Ok(response)) => response,
                }
            };

            let status = response.status().as_u16();
            let response_headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(key, value)| {
                    (
                        key.to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            let retry_after = response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            validate_codex_http_success(status, response.headers())?;
            let first_byte_timeout = Duration::from_millis(self.http_first_byte_timeout_ms);
            let body_idle_timeout = Duration::from_millis(self.body_idle_timeout_ms);

            if status == 401
                && !auth_refresh_attempted
                && let Ok(replay) = dispatch_budget.reserve_model()
            {
                auth_refresh_attempted = true;
                auth = self
                    .auth_manager
                    .force_refresh_with_budget(&auth.access, &dispatch_budget)
                    .await
                    .map_err(auth_refresh_error)?;
                reserved_model_replay = Some(replay);
                continue;
            }
            if !(200..300).contains(&status) {
                let error_body = read_live_http_error_body(
                    response,
                    first_byte_timeout,
                    body_idle_timeout,
                    deadline,
                )
                .await?;
                if let Some(traffic) = ctx.traffic.as_deref() {
                    write_upstream_response_capture(
                        traffic,
                        status,
                        started_at.elapsed(),
                        &response_headers,
                        &error_body,
                    );
                }
                let message =
                    codex_status_error_message(&error_body).unwrap_or_else(|| match status {
                        401 => "Unauthorized".to_string(),
                        403 => "Forbidden".to_string(),
                        429 => "Rate limited".to_string(),
                        _ => format!("Upstream Codex request failed with status {status}"),
                    });
                let mapped_status =
                    if codex_error_body_is_context_overflow(&error_body, Some(status)) {
                        413
                    } else {
                        status
                    };
                return Err(CodexError {
                    status: mapped_status,
                    detail: Some(message.clone()),
                    message,
                    retry_after,
                    origin: CodexErrorOrigin::UpstreamHttp,
                });
            }

            let (tx, rx) = tokio::sync::mpsc::channel(super::LIVE_EVENT_CHANNEL_CAPACITY);
            let ctx = ctx.clone();
            tokio::spawn(async move {
                forward_live_http_response(
                    response,
                    status,
                    response_headers,
                    started_at,
                    ctx,
                    first_byte_timeout,
                    body_idle_timeout,
                    deadline,
                    tx,
                )
                .await;
            });
            return Ok(rx);
        }
    }

    async fn attempt_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &Bytes,
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
        let send_fut = req_builder.body(body_json.clone()).send();
        let header_timeout_dur = Duration::from_millis(self.header_timeout_ms);

        let mut resp = tokio::time::timeout(header_timeout_dur, send_fut)
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex response headers",
                    self.header_timeout_ms
                ),
                detail: Some("http_response_headers".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| {
                if e.is_connect() {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: Some(CODEX_HTTP_CONNECT_ERROR_DETAIL.to_string()),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: Some("http_response_headers".to_string()),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {e}"),
                        detail: Some("http_response_headers".to_string()),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    }
                }
            })?;

        let status = resp.status().as_u16();
        validate_codex_http_success(status, resp.headers())?;
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        if !(200..300).contains(&status) {
            // The upstream status is authoritative even if its diagnostic body
            // stalls or resets. Preserve that explicit retry signal and its
            // Retry-After header instead of replacing it with a statusless,
            // outcome-unknown transport error.
            let body_bytes = read_buffered_http_error_body(
                resp,
                Duration::from_millis(self.http_first_byte_timeout_ms),
                Duration::from_millis(self.body_idle_timeout_ms),
            )
            .await;
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_upstream_response_capture(
                    traffic,
                    status,
                    started_at.elapsed(),
                    &headers,
                    &body_bytes,
                );
            }
            return Ok(CodexResponse {
                body: body_bytes,
                status,
                headers,
            });
        }

        let body_limit = MAX_BUFFERED_HTTP_SUCCESS_BODY_BYTES;
        if resp
            .content_length()
            .is_some_and(|length| length > body_limit as u64)
        {
            return Err(CodexError {
                status: 502,
                message: "Codex HTTP response exceeded the buffered body limit".to_string(),
                detail: Some("http_response_size_limit".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::Http,
            });
        }
        let mut body_bytes = Vec::new();
        let mut response_started = false;
        loop {
            let first_chunk = !response_started;
            let chunk_timeout_ms = if first_chunk {
                self.http_first_byte_timeout_ms
            } else {
                self.body_idle_timeout_ms
            };
            let chunk = tokio::time::timeout(Duration::from_millis(chunk_timeout_ms), resp.chunk())
                .await
                .map_err(|_| CodexError {
                    status: 0,
                    message: if first_chunk {
                        format!(
                            "Timed out waiting {}ms for the first Codex HTTP response body byte",
                            self.http_first_byte_timeout_ms
                        )
                    } else {
                        format!(
                            "Timed out waiting {}ms for the next Codex response body chunk",
                            self.body_idle_timeout_ms
                        )
                    },
                    detail: Some(
                        if first_chunk {
                            "http_response_first_byte"
                        } else {
                            "http_response_body"
                        }
                        .to_string(),
                    ),
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
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            if body_bytes.len().saturating_add(chunk.len()) > body_limit {
                return Err(CodexError {
                    status: 502,
                    message: "Codex HTTP response exceeded the buffered body limit".to_string(),
                    detail: Some("http_response_size_limit".to_string()),
                    retry_after: None,
                    origin: CodexErrorOrigin::Http,
                });
            }
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

#[derive(Default)]
struct CodexLiveSseDecoder {
    pending: BytesMut,
    records_seen: usize,
    scan_from: usize,
}

impl CodexLiveSseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<serde_json::Value>, String> {
        self.pending.extend_from_slice(bytes);
        let mut payloads = Vec::new();
        while let Some(end) = next_sse_record_end(&self.pending, self.scan_from) {
            if end > super::MAX_LIVE_EVENT_BYTES {
                return Err("Codex SSE event exceeded the incremental decode limit".to_string());
            }
            let record = self.pending.split_to(end);
            self.scan_from = 0;
            let stream_start = self.records_seen == 0;
            self.records_seen = self.records_seen.saturating_add(1);
            decode_codex_sse_record(&record, stream_start, &mut payloads)?;
        }
        if self.pending.len() > super::MAX_LIVE_EVENT_BYTES {
            return Err("Codex SSE event exceeded the incremental decode limit".to_string());
        }
        // The longest record delimiter is CRLFCRLF (four bytes), so only its
        // last three existing bytes can combine with the next network chunk.
        // Retaining this cursor avoids rescanning a large partial event from
        // byte zero for every small chunk.
        self.scan_from = self.pending.len().saturating_sub(3);
        Ok(payloads)
    }

    fn finish(mut self) -> Result<Vec<serde_json::Value>, String> {
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            return Ok(Vec::new());
        }
        self.pending.extend_from_slice(b"\n\n");
        let mut payloads = Vec::new();
        while let Some(end) = next_sse_record_end(&self.pending, self.scan_from) {
            if end > super::MAX_LIVE_EVENT_BYTES {
                return Err("Codex SSE event exceeded the incremental decode limit".to_string());
            }
            let record = self.pending.split_to(end);
            self.scan_from = 0;
            let stream_start = self.records_seen == 0;
            self.records_seen = self.records_seen.saturating_add(1);
            decode_codex_sse_record(&record, stream_start, &mut payloads)?;
        }
        Ok(payloads)
    }
}

fn next_sse_record_end(bytes: &[u8], scan_from: usize) -> Option<usize> {
    fn line_ending_len(bytes: &[u8], index: usize) -> Option<usize> {
        match bytes.get(index) {
            Some(b'\r') if bytes.get(index + 1) == Some(&b'\n') => Some(2),
            Some(b'\r' | b'\n') => Some(1),
            _ => None,
        }
    }

    for index in scan_from.min(bytes.len())..bytes.len() {
        let Some(first) = line_ending_len(bytes, index) else {
            continue;
        };
        let second_index = index + first;
        if let Some(second) = line_ending_len(bytes, second_index) {
            return Some(second_index + second);
        }
    }
    None
}

fn decode_codex_sse_record(
    record: &[u8],
    stream_start: bool,
    payloads: &mut Vec<serde_json::Value>,
) -> Result<(), String> {
    const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";
    let record = if stream_start {
        record.strip_prefix(UTF8_BOM).unwrap_or(record)
    } else {
        if record.starts_with(UTF8_BOM) {
            return Err("Codex SSE stream contained a UTF-8 BOM after stream start".to_string());
        }
        record
    };
    let events = try_parse_sse_events(record).map_err(|error| {
        format!(
            "Codex SSE event contained invalid UTF-8 at byte {}",
            error.valid_up_to()
        )
    })?;
    for event in events {
        if event.data == "[DONE]" {
            continue;
        }
        let payload = serde_json::from_str(&event.data)
            .map_err(|error| format!("Malformed Codex SSE event: {error}"))?;
        payloads.push(payload);
    }
    Ok(())
}

fn live_http_terminal_event(payload: &serde_json::Value) -> bool {
    super::events::CodexTerminalKind::from_payload(payload).is_some()
}

fn live_http_body_error(message: String) -> CodexError {
    CodexError {
        status: 0,
        message,
        detail: Some("http_response_body".to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::Http,
    }
}

fn live_http_first_byte_error(message: String) -> CodexError {
    CodexError {
        status: 0,
        message,
        detail: Some("http_response_first_byte".to_string()),
        retry_after: None,
        origin: CodexErrorOrigin::Http,
    }
}

async fn send_live_http_item(
    tx: &tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
    item: Result<serde_json::Value, CodexError>,
    deadline: CodexRequestDeadline,
) -> bool {
    tokio::select! {
        biased;
        _ = tx.closed() => false,
        _ = tokio::time::sleep_until(deadline.at()) => false,
        result = tx.send(item) => result.is_ok(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveHttpBatchOutcome {
    Continue,
    Terminal,
    Cancelled,
}

fn validate_live_http_payload_batch(
    payloads: Vec<serde_json::Value>,
) -> Result<(Vec<serde_json::Value>, Option<serde_json::Value>), CodexError> {
    let mut before_terminal = Vec::with_capacity(payloads.len());
    let mut terminal = None;
    for payload in payloads {
        if terminal.is_some() {
            if super::translate::reducer::is_post_terminal_telemetry(&payload) {
                continue;
            }
            return Err(live_http_body_error(format!(
                "unexpected Codex event {:?} after terminal response event",
                payload.get("type").and_then(|value| value.as_str())
            )));
        }
        if live_http_terminal_event(&payload) {
            terminal = Some(payload);
        } else {
            before_terminal.push(payload);
        }
    }
    Ok((before_terminal, terminal))
}

async fn forward_live_http_payload_batch(
    tx: &tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
    payloads: Vec<serde_json::Value>,
    deadline: CodexRequestDeadline,
) -> Result<LiveHttpBatchOutcome, CodexError> {
    // Validate every complete event decoded from this network chunk before exposing its
    // terminal event. Once that batch is valid, the terminal is the protocol commit point and
    // later transport chunks no longer decide whether Claude sees a successful completion.
    let (before_terminal, terminal) = validate_live_http_payload_batch(payloads)?;
    for payload in before_terminal {
        if !send_live_http_item(tx, Ok(payload), deadline).await {
            return Ok(LiveHttpBatchOutcome::Cancelled);
        }
    }
    let Some(terminal) = terminal else {
        return Ok(LiveHttpBatchOutcome::Continue);
    };
    if !send_live_http_item(tx, Ok(terminal), deadline).await {
        return Ok(LiveHttpBatchOutcome::Cancelled);
    }
    Ok(LiveHttpBatchOutcome::Terminal)
}

async fn read_buffered_http_error_body(
    mut response: reqwest::Response,
    first_byte_timeout: Duration,
    body_idle_timeout: Duration,
) -> Vec<u8> {
    let mut body = Vec::new();
    let mut first_chunk = true;
    loop {
        let wait = if first_chunk {
            first_byte_timeout
        } else {
            body_idle_timeout
        }
        .min(LIVE_HTTP_ERROR_BODY_IDLE_TIMEOUT);
        let chunk = match tokio::time::timeout(wait, response.chunk()).await {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(_)) | Err(_) => break,
        };
        let Some(chunk) = chunk else {
            break;
        };
        first_chunk = false;
        let remaining = MAX_BUFFERED_HTTP_ERROR_BODY_BYTES.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    body
}

async fn read_live_http_error_body(
    mut response: reqwest::Response,
    first_byte_timeout: Duration,
    body_idle_timeout: Duration,
    deadline: CodexRequestDeadline,
) -> Result<Vec<u8>, CodexError> {
    let mut body = Vec::new();
    let mut first_chunk = true;
    loop {
        // Error bodies are diagnostic only, so retain the existing five-second
        // cap while honoring any stricter configured first-byte/body-idle
        // timeout.
        let chunk_timeout = if first_chunk {
            first_byte_timeout
        } else {
            body_idle_timeout
        }
        .min(LIVE_HTTP_ERROR_BODY_IDLE_TIMEOUT);
        let chunk = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline.at()) => {
                return Err(codex_total_timeout_error(
                    crate::config::CodexTransport::Http,
                    deadline.timeout_ms(),
                ));
            }
            result = tokio::time::timeout(chunk_timeout, response.chunk()) => {
                match result {
                    Ok(Ok(chunk)) => chunk,
                    Ok(Err(_)) | Err(_) => return Ok(body),
                }
            }
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        first_chunk = false;
        let remaining = MAX_LIVE_HTTP_ERROR_BODY_BYTES.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok(body);
        }
        body.extend_from_slice(&chunk);
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_live_http_response(
    mut response: reqwest::Response,
    status: u16,
    headers: Vec<(String, String)>,
    started_at: Instant,
    ctx: RequestContext,
    first_byte_timeout: Duration,
    body_idle_timeout: Duration,
    deadline: CodexRequestDeadline,
    tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
) {
    let mut decoder = CodexLiveSseDecoder::default();
    // Translation is incremental. Retain raw bytes only when traffic capture
    // is enabled, and cap them so a long reasoning stream cannot grow memory
    // without bound.
    let mut captured_body = ctx.traffic.is_some().then(Vec::new);
    let mut capture_truncated = false;
    let mut terminal = false;
    let mut final_error = None;
    let mut cancelled = false;
    let mut first_chunk = true;

    'body: loop {
        let chunk_timeout = if first_chunk {
            first_byte_timeout
        } else {
            body_idle_timeout
        };
        let chunk = tokio::select! {
            biased;
            _ = tx.closed() => {
                cancelled = true;
                break 'body;
            }
            _ = tokio::time::sleep_until(deadline.at()) => {
                final_error = Some(codex_total_timeout_error(
                    crate::config::CodexTransport::Http,
                    deadline.timeout_ms(),
                ));
                break 'body;
            }
            result = tokio::time::timeout(chunk_timeout, response.chunk()) => match result {
                Err(_) => {
                    final_error = Some(if first_chunk {
                        live_http_first_byte_error(format!(
                            "Timed out waiting {}ms for the first Codex HTTP response body byte",
                            first_byte_timeout.as_millis()
                        ))
                    } else {
                        live_http_body_error(format!(
                            "Timed out waiting {}ms for the next Codex response body chunk",
                            body_idle_timeout.as_millis()
                        ))
                    });
                    break 'body;
                }
                Ok(Err(error)) => {
                    final_error = Some(live_http_body_error(format!(
                        "Transport error reading Codex response body: {error}"
                    )));
                    break 'body;
                }
                Ok(Ok(chunk)) => chunk,
            }
        };

        let Some(chunk) = chunk else {
            match decoder.finish() {
                Ok(payloads) => {
                    match forward_live_http_payload_batch(&tx, payloads, deadline).await {
                        Ok(LiveHttpBatchOutcome::Continue) => {}
                        Ok(LiveHttpBatchOutcome::Terminal) => terminal = true,
                        Ok(LiveHttpBatchOutcome::Cancelled) => cancelled = true,
                        Err(error) => final_error = Some(error),
                    }
                }
                Err(message) => final_error = Some(live_http_body_error(message)),
            }
            if !terminal && final_error.is_none() && !cancelled {
                final_error = Some(live_http_body_error(
                    "HTTP response body ended before terminal Codex response event".to_string(),
                ));
            }
            break;
        };
        first_chunk = false;

        if let Some(body) = captured_body.as_mut() {
            let remaining =
                crate::traffic::MAX_STREAM_CAPTURE_EVENT_BYTES.saturating_sub(body.len());
            if chunk.len() > remaining {
                body.extend_from_slice(&chunk[..remaining]);
                capture_truncated = true;
            } else {
                body.extend_from_slice(&chunk);
            }
        }
        let payloads = match decoder.push(&chunk) {
            Ok(payloads) => payloads,
            Err(message) => {
                final_error = Some(live_http_body_error(message));
                break;
            }
        };
        match forward_live_http_payload_batch(&tx, payloads, deadline).await {
            Ok(LiveHttpBatchOutcome::Continue) => {}
            Ok(LiveHttpBatchOutcome::Terminal) => {
                break 'body;
            }
            Ok(LiveHttpBatchOutcome::Cancelled) => {
                cancelled = true;
                break 'body;
            }
            Err(error) => {
                final_error = Some(error);
                break 'body;
            }
        }
    }

    if let Some(traffic) = ctx.traffic.as_deref() {
        write_upstream_response_capture(
            traffic,
            status,
            started_at.elapsed(),
            &headers,
            captured_body.as_deref().unwrap_or_default(),
        );
        if capture_truncated {
            traffic.write_json(
                "033-upstream-response-truncated",
                &serde_json::json!({
                    "limitBytes": crate::traffic::MAX_STREAM_CAPTURE_EVENT_BYTES,
                }),
            );
        }
    }
    if !cancelled && let Some(error) = final_error {
        if error.detail.as_deref() == Some(CODEX_TOTAL_TIMEOUT_DETAIL) {
            // The deadline has already elapsed, so the ordinary deadline-aware
            // sender would suppress the terminal cause. Never block on a full
            // queue; the downstream task enforces the same deadline itself.
            let _ = tx.try_send(Err(error));
        } else {
            let _ = send_live_http_item(&tx, Err(error), deadline).await;
        }
    }
}

fn write_codex_http_request_capture(
    traffic: &TrafficCapture,
    url: &str,
    headers: &http::HeaderMap,
    body_json: &[u8],
) {
    let body = serde_json::from_slice(body_json).unwrap_or_else(|_| {
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
            "size": summarize_json_request_size(&body, body_json.len()),
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

fn summarize_json_request_size(body: &serde_json::Value, body_bytes: usize) -> serde_json::Value {
    serde_json::json!({
        "bytes": body_bytes,
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

fn auth_refresh_error(err: anyhow::Error) -> CodexError {
    let kind = codex_auth_error_kind(&err);
    let budget_exhausted = err.downcast_ref::<CodexDispatchBudgetExceeded>().is_some();
    CodexError {
        status: match kind {
            CodexAuthErrorKind::CredentialsInvalid => 401,
            CodexAuthErrorKind::Temporary | CodexAuthErrorKind::RefreshOutcomeUnknown => 503,
        },
        message: if budget_exhausted {
            "Codex dispatch budget exhausted"
        } else {
            match kind {
                CodexAuthErrorKind::CredentialsInvalid => "Unauthorized",
                CodexAuthErrorKind::Temporary => "Authentication temporarily unavailable",
                CodexAuthErrorKind::RefreshOutcomeUnknown => "Authentication state is ambiguous",
            }
        }
        .to_string(),
        detail: Some(if budget_exhausted {
            CODEX_DISPATCH_BUDGET_DETAIL.to_string()
        } else {
            err.to_string()
        }),
        retry_after: None,
        origin: CodexErrorOrigin::Auth,
    }
}

fn model_dispatch_budget_error(
    error: CodexDispatchBudgetExceeded,
    transport: crate::config::CodexTransport,
) -> CodexError {
    CodexError {
        status: 503,
        message: error.to_string(),
        detail: Some(CODEX_DISPATCH_BUDGET_DETAIL.to_string()),
        retry_after: None,
        origin: match transport {
            crate::config::CodexTransport::Http => CodexErrorOrigin::Http,
            crate::config::CodexTransport::WebSocket | crate::config::CodexTransport::Auto => {
                CodexErrorOrigin::WebSocket
            }
        },
    }
}

pub(super) fn codex_total_timeout_error(
    transport: crate::config::CodexTransport,
    timeout_ms: u64,
) -> CodexError {
    CodexError {
        status: 504,
        message: format!("Codex request exceeded the total wall-clock budget of {timeout_ms}ms"),
        detail: Some(CODEX_TOTAL_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        origin: buffered_origin(transport),
    }
}

fn codex_status_error(
    response: CodexResponse,
    transport: crate::config::CodexTransport,
) -> CodexError {
    let retry_after = response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.clone());
    let message = codex_status_error_message(&response.body).unwrap_or_else(|| {
        format!(
            "Upstream Codex request failed with status {}",
            response.status
        )
    });
    let status = if codex_error_body_is_context_overflow(&response.body, Some(response.status)) {
        413
    } else {
        response.status
    };
    CodexError {
        status,
        message: message.clone(),
        detail: Some(message),
        retry_after,
        origin: buffered_origin(transport),
    }
}

fn codex_status_error_message(body: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        let has_allowlisted_detail = value.pointer("/error/message").is_some()
            || value.pointer("/error/detail").is_some()
            || value.pointer("/error/code").is_some()
            || value.pointer("/error/type").is_some()
            || value.pointer("/error/param").is_some()
            || value.get("error").is_some_and(serde_json::Value::is_string)
            || value.get("message").is_some()
            || value.get("detail").is_some();
        // A valid but unknown JSON shape must not fall through to the raw-text
        // renderer. Private gateways can place credentials or echoed input in
        // arbitrary fields; only the allowlisted diagnostic surface above is
        // safe to render.
        return has_allowlisted_detail.then(|| super::events::sanitized_error_message(&value));
    }

    parse_sse_events(body)
        .into_iter()
        .find_map(|event| {
            let payload = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
            super::events::classify_event_failure(&payload).map(|failure| failure.message)
        })
        .or_else(|| {
            let value = std::str::from_utf8(body).ok()?;
            (!value.trim_start().starts_with("data:"))
                .then(|| crate::providers::translate_shared::sanitize_external_error_detail(value))
                .flatten()
        })
}

fn codex_error_body_is_context_overflow(body: &[u8], outer_status: Option<u16>) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .is_some_and(|value| {
            super::events::is_context_overflow_error_with_status(&value, outer_status)
        })
        || parse_sse_events(body).into_iter().any(|event| {
            serde_json::from_str::<serde_json::Value>(&event.data)
                .ok()
                .is_some_and(|payload| {
                    super::events::is_context_overflow_error_with_status(&payload, outer_status)
                })
        })
}

fn buffered_origin(transport: crate::config::CodexTransport) -> CodexErrorOrigin {
    match transport {
        crate::config::CodexTransport::Http => CodexErrorOrigin::BufferedHttp,
        crate::config::CodexTransport::WebSocket | crate::config::CodexTransport::Auto => {
            CodexErrorOrigin::BufferedWebSocket
        }
    }
}

fn serialize_request(body: &ResponsesRequest) -> Result<Bytes, CodexError> {
    serde_json::to_vec(body)
        .map(Bytes::from)
        .map_err(|error| CodexError {
            status: 500,
            message: "Failed to serialize request".to_string(),
            detail: Some(error.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        })
}

fn cached_serialized_request(
    cached: &mut Option<Bytes>,
    body: &ResponsesRequest,
) -> Result<Bytes, CodexError> {
    if let Some(bytes) = cached.as_ref() {
        return Ok(bytes.clone());
    }
    let bytes = serialize_request(body)?;
    *cached = Some(bytes.clone());
    Ok(bytes)
}

fn websocket_circuit_key_for_url(base_url: &str) -> String {
    reqwest::Url::parse(base_url)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|_| base_url.trim_end_matches('/').to_string())
}

fn should_retry_codex_status(status: u16) -> bool {
    should_retry_status(status) || status == 529
}

fn codex_error_origin_name(origin: CodexErrorOrigin) -> &'static str {
    match origin {
        CodexErrorOrigin::Http => "http",
        CodexErrorOrigin::UpstreamHttp => "upstream_http",
        CodexErrorOrigin::WebSocket => "websocket",
        CodexErrorOrigin::WebSocketHandshake => "websocket_handshake",
        CodexErrorOrigin::Auth => "auth",
        CodexErrorOrigin::BufferedHttp => "buffered_http",
        CodexErrorOrigin::BufferedWebSocket => "buffered_websocket",
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
    let reason = crate::providers::translate_shared::sanitize_external_error_detail(reason)
        .unwrap_or_else(|| "Upstream error".to_string());
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
    let reason = crate::providers::translate_shared::sanitize_external_error_detail(reason)
        .unwrap_or_else(|| "Upstream error".to_string());
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

pub(super) fn log_live_transport_retry(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    failed_attempt: u32,
    max_attempts: u32,
    delay_ms: u64,
    err: &CodexError,
) {
    let reason = crate::providers::translate_shared::sanitize_external_error_detail(&err.message)
        .unwrap_or_else(|| "Upstream error".to_string());
    create_logger("codex").warn(
        "live_transport_retry",
        Some(serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(ctx.req_id)),
            ("transport".into(), serde_json::json!(transport.as_str())),
            ("failedAttempt".into(), serde_json::json!(failed_attempt)),
            ("nextAttempt".into(), serde_json::json!(failed_attempt + 1)),
            ("maxAttempts".into(), serde_json::json!(max_attempts)),
            ("delayMs".into(), serde_json::json!(delay_ms)),
            ("status".into(), serde_json::json!(err.status)),
            (
                "origin".into(),
                serde_json::json!(codex_error_origin_name(err.origin)),
            ),
            ("reason".into(), serde_json::json!(reason)),
        ])),
    );
}

pub(super) fn log_websocket_circuit_fallback(ctx: &RequestContext) {
    create_logger("codex").warn(
        "websocket_circuit_fallback",
        Some(serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(ctx.req_id)),
            (
                "cooldownMs".into(),
                serde_json::json!(super::websocket::WEBSOCKET_CIRCUIT_COOLDOWN_MS),
            ),
            ("fallbackTransport".into(), serde_json::json!("http")),
        ])),
    );
}

pub(super) fn record_auto_websocket_failure(
    ctx: &RequestContext,
    circuit_key: &str,
    err: &CodexError,
) -> bool {
    if !is_websocket_transport_health_failure(err) {
        // A non-transport response proves the WebSocket path is reachable and
        // breaks a run of consecutive transport failures.
        super::websocket::record_codex_websocket_success(circuit_key);
        return false;
    }

    let opened = super::websocket::record_codex_websocket_failure(circuit_key);
    if opened {
        create_logger("codex").warn(
            "websocket_circuit_opened",
            Some(serde_json::Map::from_iter([
                ("reqId".into(), serde_json::json!(ctx.req_id)),
                (
                    "failures".into(),
                    serde_json::json!(super::websocket::WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD),
                ),
                (
                    "cooldownMs".into(),
                    serde_json::json!(super::websocket::WEBSOCKET_CIRCUIT_COOLDOWN_MS),
                ),
            ])),
        );
    }
    opened
}

fn is_websocket_transport_health_failure(err: &CodexError) -> bool {
    matches!(
        err.origin,
        CodexErrorOrigin::WebSocket | CodexErrorOrigin::WebSocketHandshake
    ) && err.detail.as_deref() != Some("previous_response_not_found")
        && is_retryable_transport_error(err)
}

pub(super) fn is_retryable_transport_error(err: &CodexError) -> bool {
    if err.origin == CodexErrorOrigin::WebSocketHandshake {
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_TERMINAL_BARRIER_DETAIL) {
        // A terminal frame proves the model request was dispatched. Any
        // ordering-barrier failure is protocol corruption after dispatch, not
        // a replay-safe transport startup failure.
        return false;
    }
    if super::websocket::is_retryable_transport_detail(err.detail.as_deref()) {
        return true;
    }
    if err.detail.as_deref() == Some("websocket_pre_request") {
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.status != 0 {
        return false;
    }

    let message = err.message.to_ascii_lowercase();
    message.contains("timed out waiting")
        || message.contains("transport error")
        || message.contains("connection reset")
        || message.contains("connection closed")
        || message.contains("timed out")
        || message.contains("econnreset")
        || message.contains("etimedout")
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

fn validate_codex_http_success(
    status: u16,
    headers: &reqwest::header::HeaderMap,
) -> Result<(), CodexError> {
    if !(200..300).contains(&status) {
        return Ok(());
    }
    if status == 204 {
        return Err(CodexError {
            status: 502,
            message: "Codex upstream returned an empty 204 response".to_string(),
            detail: Some("http_invalid_success_response".to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        });
    }
    if let Some(content_type) = headers.get(reqwest::header::CONTENT_TYPE) {
        let event_stream = content_type
            .to_str()
            .ok()
            .and_then(|value| value.split(';').next())
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"));
        if !event_stream {
            return Err(CodexError {
                status: 502,
                message: format!(
                    "Codex upstream returned a non-SSE success response ({})",
                    content_type.to_str().unwrap_or("<invalid content-type>")
                ),
                detail: Some("http_invalid_success_response".to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::Http,
            });
        }
    }
    Ok(())
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

/// Immediate Auto-mode fallback from WebSocket to buffered HTTP.
///
/// Only replay-safe, retryable WebSocket transport failures qualify. Business
/// 4xx, continuation misses, and service 429/500/502/503/504/529 event errors stay on the
/// original error path and must not be treated as transport fallback.
pub(super) fn should_fallback_to_http(err: &CodexError) -> bool {
    is_websocket_transport_health_failure(err) && err.replay_safety().permits_model_replay()
}

pub(super) fn auto_http_fallback_delay(
    err: &CodexError,
    backoff: &mut ModelRetryBackoff,
) -> Option<BackoffOutcome> {
    // A statusless, definitely pre-dispatch WebSocket transport failure is
    // switching to a different transport, not retrying a model response. Start
    // HTTP immediately; explicit service responses and Retry-After still keep
    // the existing bounded backoff policy.
    if err.status == 0
        && err.retry_after.is_none()
        && err.replay_safety() == ReplaySafety::DefinitelyNotDispatched
    {
        return Some(BackoffOutcome {
            wait_ms: 0,
            exceeds_budget: false,
        });
    }
    backoff.next_delay(err.replay_safety(), err.retry_after.as_deref())
}

pub(super) fn log_auto_http_fallback(ctx: &RequestContext, err: &CodexError) {
    let reason = crate::providers::translate_shared::sanitize_external_error_detail(&err.message)
        .unwrap_or_else(|| "Upstream error".to_string());
    let mut fields = serde_json::Map::from_iter([
        ("reqId".into(), serde_json::json!(ctx.req_id)),
        ("action".into(), serde_json::json!("fallback_http")),
        (
            "origin".into(),
            serde_json::json!(codex_error_origin_name(err.origin)),
        ),
        ("status".into(), serde_json::json!(err.status)),
        ("reason".into(), serde_json::json!(reason)),
    ]);
    if let Some(detail) = err
        .detail
        .as_deref()
        .and_then(crate::providers::translate_shared::sanitize_external_error_detail)
    {
        fields.insert("detail".into(), serde_json::json!(detail));
    }
    create_logger("codex").warn("auto_transport_fallback", Some(fields));
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

fn full_context_continuation(
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> Option<super::continuation::ContinuationCandidate> {
    continuation.map(|candidate| super::continuation::ContinuationCandidate {
        turn_id: candidate.turn_id,
        previous_response_id: None,
        input_delta: None,
        input_delta_count: candidate.input_delta_count,
        disabled_reason: Some("full_context_retry".to_string()),
    })
}

fn event_closes_live_retry_window(payload: &serde_json::Value) -> bool {
    // `response.created` confirms that the upstream generation exists, but the
    // live translator emits no Anthropic bytes for it. Keep the recovery window
    // open until an event can commit semantic output such as text or tool use.
    !matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some("codex.rate_limits" | "codex.response.metadata" | "keepalive" | "response.created")
    )
}

fn is_continuation_retry_error(err: &CodexError) -> bool {
    // store:false continuation state is tied to its WebSocket. Once that
    // connection is lost or detached, the retry must send full context.
    err.detail.as_deref() == Some("previous_response_not_found")
        || (err.replay_safety().permits_model_replay()
            && (super::websocket::is_retryable_transport_detail(err.detail.as_deref())
                || (err.origin == CodexErrorOrigin::WebSocketHandshake
                    && (err.status == 0 || should_retry_codex_status(err.status)))))
}

fn websocket_pool_key<'a>(
    ctx: &'a RequestContext,
    _continuation: Option<&super::continuation::ContinuationCandidate>,
) -> Option<&'a str> {
    // Transport reuse and server-side response continuation are independent.
    // Even with previousResponseId disabled, a session WebSocket can carry a
    // later full-context response.create request without changing its payload.
    ctx.session_id.as_deref()
}

fn should_reset_websocket_pool(
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> bool {
    let Some(reason) = continuation.and_then(|c| c.disabled_reason.as_deref()) else {
        return false;
    };
    reason != "disabled"
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn http_test_auth() -> StoredAuth {
        StoredAuth {
            access: "test".into(),
            refresh: String::new(),
            account_id: Some("acct".into()),
            expires: u64::MAX,
        }
    }

    fn http_test_context() -> RequestContext {
        RequestContext {
            req_id: "http-body-test".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        }
    }

    fn http_test_client(base_url: String, body_idle_timeout_ms: u64) -> CodexHttpClient {
        CodexHttpClient::new_for_test(base_url, 100, body_idle_timeout_ms, 10_000)
    }

    fn http_test_client_with_body_timeouts(
        base_url: String,
        first_byte_timeout_ms: u64,
        body_idle_timeout_ms: u64,
    ) -> CodexHttpClient {
        let mut client = http_test_client(base_url, body_idle_timeout_ms);
        client.http_first_byte_timeout_ms = first_byte_timeout_ms;
        client
    }

    #[test]
    fn codex_http_timeout_defaults_remain_backward_compatible() {
        assert_eq!(HTTP_CONNECT_TIMEOUT_MS, 15_000);
        assert_eq!(HTTP_RESPONSE_HEADER_TIMEOUT_MS, 60_000);
        assert_eq!(HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS, 300_000);
        let client = CodexHttpClient::new_for_test(
            "http://127.0.0.1:1/responses".to_string(),
            60_000,
            300_000,
            DEFAULT_CODEX_TOTAL_TIMEOUT_MS,
        );
        assert_eq!(
            client.http_first_byte_timeout_ms,
            client.body_idle_timeout_ms
        );
    }

    #[tokio::test]
    async fn explicit_websocket_environment_proxy_fails_before_auth_and_model_dispatch() {
        let mut client = CodexHttpClient::new_for_test(
            "http://127.0.0.1:1/responses".to_string(),
            100,
            100,
            1_000,
        );
        client.transport_decision =
            config::CodexTransportDecision::resolve(config::CodexTransport::WebSocket, false, true);
        let request = buffered_test_request();
        let context = http_test_context();

        let buffered = match client
            .post_codex_with_transport(&request, &context, None, config::CodexTransport::WebSocket)
            .await
        {
            Ok(_) => panic!("explicit WebSocket must fail locally before reading auth"),
            Err(error) => error,
        };
        assert_eq!(buffered.status, 400);
        assert_eq!(
            buffered.detail.as_deref(),
            Some(CODEX_WEBSOCKET_ENV_PROXY_UNSUPPORTED_DETAIL)
        );
        assert!(
            buffered
                .message
                .contains("CCP_CODEX_TRANSPORT=auto or http")
        );
        assert!(buffered.message.contains("NO_PROXY"));
        assert_eq!(
            buffered.replay_safety(),
            ReplaySafety::DefinitelyNotDispatched
        );
        assert!(!is_retryable_transport_error(&buffered));
        assert!(!should_fallback_to_http(&buffered));

        let client = Arc::new(client);
        let streamed = match client
            .stream_codex_websocket_events(&request, &context, None, CodexDispatchBudget::new())
            .await
        {
            Ok(_) => panic!("live WebSocket must fail locally before reading auth"),
            Err(error) => error,
        };
        assert_eq!(
            streamed.detail.as_deref(),
            Some(CODEX_WEBSOCKET_ENV_PROXY_UNSUPPORTED_DETAIL)
        );
        assert!(!is_retryable_transport_error(&streamed));
    }

    #[test]
    fn codex_http_success_requires_a_nonempty_sse_response() {
        let mut sse = reqwest::header::HeaderMap::new();
        sse.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(validate_codex_http_success(200, &sse).is_ok());

        let no_content = validate_codex_http_success(204, &sse).unwrap_err();
        assert_eq!(
            no_content.detail.as_deref(),
            Some("http_invalid_success_response")
        );
        assert_eq!(no_content.replay_safety(), ReplaySafety::OutcomeUnknown);

        let mut json = reqwest::header::HeaderMap::new();
        json.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let non_sse = validate_codex_http_success(200, &json).unwrap_err();
        assert!(non_sse.message.contains("non-SSE"));
        assert_eq!(non_sse.replay_safety(), ReplaySafety::OutcomeUnknown);
    }

    #[test]
    fn codex_http_retries_reuse_one_serialized_bytes_allocation() {
        let request = buffered_test_request();
        let mut cached = None;
        let first = cached_serialized_request(&mut cached, &request).unwrap();
        let second = cached_serialized_request(&mut cached, &request).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.as_ptr(), second.as_ptr());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first).unwrap()["model"],
            request.model
        );
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
            schema_bridge: None,
            hosted_web_search_max_uses: None,
        }
    }

    fn authenticated_http_test_client(base_url: String) -> CodexHttpClient {
        let client = http_test_client(base_url, 100);
        client.auth_manager().set_test_auth(http_test_auth());
        client
    }

    fn authenticated_http_test_client_with_body_timeouts(
        base_url: String,
        first_byte_timeout_ms: u64,
        body_idle_timeout_ms: u64,
    ) -> CodexHttpClient {
        let client = http_test_client_with_body_timeouts(
            base_url,
            first_byte_timeout_ms,
            body_idle_timeout_ms,
        );
        client.auth_manager().set_test_auth(http_test_auth());
        client
    }

    fn dispatch_budget_test_client(
        temp: &tempfile::TempDir,
        model_endpoint: String,
        token_endpoint: String,
        auth: StoredAuth,
    ) -> CodexHttpClient {
        use super::super::auth::token_store::{
            CodexTokenStore, KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE,
        };
        use crate::auth::{KeychainFileAuthStore, SystemKeychain};

        let auth_path = temp.path().join("codex-auth.json");
        let legacy_path = temp.path().join("legacy-codex-auth.json");
        let store = CodexTokenStore::new(KeychainFileAuthStore::new(
            auth_path.to_string_lossy().into_owned(),
            legacy_path.to_string_lossy().into_owned(),
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
            false,
            SystemKeychain,
        ));
        store.save_auth(auth).unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(store, token_endpoint);
        CodexHttpClient::new_with_client(
            build_codex_http_client(&model_endpoint, HTTP_CONNECT_TIMEOUT_MS),
            manager,
            model_endpoint,
        )
    }

    async fn read_complete_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "peer closed before completing the HTTP request");
            request.extend_from_slice(&chunk[..read]);
            let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers_end = headers_end + 4;
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= headers_end + content_length {
                return String::from_utf8_lossy(&request).into_owned();
            }
        }
    }

    async fn write_test_http_response(
        stream: &mut tokio::net::TcpStream,
        status: &str,
        content_type: &str,
        extra_headers: &str,
        body: &[u8],
    ) {
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }

    async fn assert_model_post_does_not_follow_redirect(status: &'static str) {
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let target_server = tokio::spawn(async move {
            let accepted =
                tokio::time::timeout(Duration::from_millis(250), target_listener.accept()).await;
            let Ok(Ok((mut stream, _))) = accepted else {
                return None;
            };
            let request = read_complete_http_request(&mut stream).await;
            write_test_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                "",
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"redirected\",\"usage\":{}}}\n\n",
            )
            .await;
            Some(request)
        });

        let origin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin_listener.local_addr().unwrap();
        let location = format!("http://{target_addr}/redirect-target");
        let origin_server = tokio::spawn(async move {
            let (mut stream, _) = origin_listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut stream).await;
            let headers = format!("location: {location}\r\n");
            write_test_http_response(
                &mut stream,
                status,
                "text/plain",
                &headers,
                b"redirects are not accepted",
            )
            .await;
            request
        });

        let client = CodexHttpClient::new_for_test(
            format!("http://{origin_addr}/responses"),
            500,
            500,
            2_000,
        );
        client.auth_manager().set_test_auth(http_test_auth());
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;

        let origin_request = origin_server.await.unwrap();
        let target_request = target_server.await.unwrap();
        assert!(origin_request.starts_with("POST /responses "));
        assert!(origin_request.contains("Bearer test"));
        assert!(
            target_request.is_none(),
            "the redirected endpoint must not receive a replayed model POST"
        );
        let error = match result {
            Ok(_) => panic!("Codex model redirects must remain terminal responses"),
            Err(error) => error,
        };
        assert_eq!(
            error.status,
            status[..3].parse::<u16>().expect("numeric test status")
        );
        assert_eq!(error.origin, CodexErrorOrigin::BufferedHttp);
    }

    #[tokio::test]
    async fn model_post_does_not_follow_307_or_308_redirects() {
        assert_model_post_does_not_follow_redirect("307 Temporary Redirect").await;
        assert_model_post_does_not_follow_redirect("308 Permanent Redirect").await;
    }

    #[test]
    fn auth_error_mapping_only_uses_401_for_invalid_credentials() {
        use super::super::auth::manager::CodexAuthError;

        let invalid = auth_refresh_error(anyhow::Error::new(CodexAuthError::CredentialsInvalid {
            message: "invalid".into(),
        }));
        let temporary = auth_refresh_error(anyhow::Error::new(CodexAuthError::Temporary {
            message: "temporary".into(),
        }));
        let ambiguous =
            auth_refresh_error(anyhow::Error::new(CodexAuthError::RefreshOutcomeUnknown {
                message: "ambiguous".into(),
            }));

        assert_eq!(invalid.status, 401);
        assert_eq!(temporary.status, 503);
        assert_eq!(ambiguous.status, 503);
        assert_eq!(invalid.origin, CodexErrorOrigin::Auth);
        assert_eq!(temporary.origin, CodexErrorOrigin::Auth);
        assert_eq!(ambiguous.origin, CodexErrorOrigin::Auth);
    }

    #[tokio::test]
    async fn buffered_http_retries_retryable_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16 * 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let (status, body): (&str, &[u8]) = if attempt == 0 {
                    ("503 Service Unavailable", b"retry")
                } else {
                    ("200 OK", b"data: keep\n\n")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn buffered_http_context_code_does_not_retry_retryable_outer_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut stream).await;
            assert!(request.starts_with("POST /responses "));
            write_test_http_response(
                &mut stream,
                "503 Service Unavailable",
                "application/json",
                "retry-after: 0\r\n",
                br#"{"error":{"code":"context_length_exceeded","message":"request rejected"}}"#,
            )
            .await;

            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_ok()
        });

        let error = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .err()
            .expect("structured context overflow must be returned as an error");

        assert_eq!(error.status, 413);
        assert_eq!(error.origin, CodexErrorOrigin::BufferedHttp);
        assert!(
            !server.await.unwrap(),
            "a structured context overflow must not replay the same model POST"
        );
    }

    #[tokio::test]
    async fn buffered_http_rate_limit_status_wins_over_context_like_prose_and_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_complete_http_request(&mut stream).await;
                assert!(request.starts_with("POST /responses "));
                if attempt == 0 {
                    write_test_http_response(
                        &mut stream,
                        "429 Too Many Requests",
                        "application/json",
                        "retry-after: 0\r\n",
                        br#"{"error":{"type":"invalid_request_error","message":"Prompt is too long for this minute's token quota"}}"#,
                    )
                    .await;
                } else {
                    write_test_http_response(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        "",
                        b"data: keep\n\n",
                    )
                    .await;
                }
            }
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .expect("an explicit 429 must remain eligible for safe retry");

        server.await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn buffered_http_preserves_retryable_status_when_error_body_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let first_request = read_complete_http_request(&mut first).await;
            assert!(first_request.starts_with("POST /responses "));
            first
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 100\r\nretry-after: 0\r\nconnection: keep-alive\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut second, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .expect("the explicit 503 should remain retryable")
                .unwrap();
            let second_request = read_complete_http_request(&mut second).await;
            assert!(second_request.starts_with("POST /responses "));
            second
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 10\r\nconnection: close\r\n\r\ndata: ok\n\n",
                )
                .await
                .unwrap();
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: ok\n\n");
    }

    #[tokio::test]
    async fn buffered_http_header_timeout_after_complete_post_is_not_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut stream).await;
            assert!(request.starts_with("POST /responses "));
            // Hold the first POST open without response headers. Three seconds
            // covers the maximum 2.4-second first retry jitter.
            let replayed = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .is_ok();
            drop(stream);
            replayed
        });

        let result = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;
        let error = match result {
            Ok(_) => panic!("a header timeout after dispatch must fail without replay"),
            Err(error) => error,
        };

        assert_eq!(error.detail.as_deref(), Some("http_response_headers"));
        assert_eq!(error.replay_safety(), ReplaySafety::OutcomeUnknown);
        assert!(
            !server.await.unwrap(),
            "the uncertain POST must not be replayed"
        );
    }

    #[tokio::test]
    async fn buffered_http_first_byte_timeout_after_complete_post_is_not_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut stream).await;
            assert!(request.starts_with("POST /responses "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            let replayed = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .is_ok();
            drop(stream);
            replayed
        });

        let result = authenticated_http_test_client_with_body_timeouts(
            format!("http://{addr}/responses"),
            30,
            500,
        )
        .post_codex_with_transport(
            &buffered_test_request(),
            &http_test_context(),
            None,
            crate::config::CodexTransport::Http,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("a first-byte timeout after dispatch must fail without replay"),
            Err(error) => error,
        };

        assert_eq!(error.detail.as_deref(), Some("http_response_first_byte"));
        assert_eq!(error.replay_safety(), ReplaySafety::OutcomeUnknown);
        assert!(
            !server.await.unwrap(),
            "the uncertain POST must not be replayed"
        );
    }

    #[tokio::test]
    async fn auto_falls_back_to_http_after_statusful_websocket_handshake_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let read = websocket.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("Upgrade: websocket"));
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
            let body = b"data: keep\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            http.write_all(response.as_bytes()).await.unwrap();
            http.write_all(body).await.unwrap();
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn auto_does_not_replay_after_websocket_request_closes_before_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = futures_util::StreamExt::next(&mut websocket)
                .await
                .expect("the model request frame should arrive")
                .expect("the model request frame should be valid");
            assert!(request.is_text());
            drop(websocket);
            tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .is_ok()
        });

        let result = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await;
        let error = match result {
            Ok(_) => panic!("a post-dispatch close must not fall back to HTTP"),
            Err(error) => error,
        };
        let replayed = server.await.unwrap();

        assert!(matches!(
            error.detail.as_deref(),
            Some(
                super::super::websocket::WEBSOCKET_CONNECTION_ERROR_DETAIL
                    | super::super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL
            )
        ));
        assert_eq!(error.replay_safety(), ReplaySafety::OutcomeUnknown);
        assert!(!should_fallback_to_http(&error));
        assert!(
            !replayed,
            "the uncertain WebSocket request must not be replayed"
        );
    }

    #[tokio::test]
    async fn auto_http_fallback_stays_on_http_and_uses_full_context() {
        use super::super::translate::request::{ResponsesContentPart, ResponsesInputItem};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut handshake = [0_u8; 16 * 1024];
            let read = websocket.read(&mut handshake).await.unwrap();
            assert!(String::from_utf8_lossy(&handshake[..read]).contains("Upgrade: websocket"));
            websocket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(websocket);

            let (mut http, _) = listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut http).await;
            assert!(request.starts_with("POST "));
            assert!(!request.contains("previous_response_id"));
            assert!(request.contains("full context"));
            assert!(!request.contains("delta only"));
            http.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 5\r\nretry-after: 0\r\nconnection: close\r\n\r\nretry",
            )
            .await
            .unwrap();
            drop(http);

            let (mut retried_http, _) = listener.accept().await.unwrap();
            let retried_request = read_complete_http_request(&mut retried_http).await;
            assert!(retried_request.starts_with("POST "));
            assert!(!retried_request.contains("previous_response_id"));
            assert!(retried_request.contains("full context"));
            assert!(!retried_request.contains("delta only"));
            let body = b"data: keep\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            retried_http.write_all(response.as_bytes()).await.unwrap();
            retried_http.write_all(body).await.unwrap();
        });

        let full_input = ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText {
                text: "full context".into(),
            }],
        };
        let delta_input = ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText {
                text: "delta only".into(),
            }],
        };
        let mut request = buffered_test_request();
        request.input = vec![full_input];
        let continuation = super::super::continuation::ContinuationCandidate {
            turn_id: Some(7),
            previous_response_id: Some("resp_prev".into()),
            input_delta: Some(vec![delta_input]),
            input_delta_count: 1,
            disabled_reason: None,
        };

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &request,
                &http_test_context(),
                Some(&continuation),
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn live_websocket_401_refresh_replays_with_full_context() {
        use super::super::translate::request::{ResponsesContentPart, ResponsesInputItem};
        use tokio_tungstenite::tungstenite::Message;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut model_hits = 0_usize;
            let mut oauth_hits = 0_usize;

            let (stream, _) = listener.accept().await.unwrap();
            model_hits += 1;
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let first = futures_util::StreamExt::next(&mut websocket)
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            assert!(first.contains("\"previous_response_id\":\"resp_prev\""));
            assert!(first.contains("delta only"));
            assert!(!first.contains("full context"));
            futures_util::SinkExt::send(
                &mut websocket,
                Message::Text(
                    r#"{"type":"response.created","response":{"id":"resp_started","status":"in_progress"}}"#
                        .into(),
                ),
            )
            .await
            .unwrap();
            futures_util::SinkExt::send(
                &mut websocket,
                Message::Text(
                    r#"{"type":"error","error":{"status":401,"message":"Unauthorized"}}"#.into(),
                ),
            )
            .await
            .unwrap();
            loop {
                let frame = tokio::time::timeout(
                    Duration::from_secs(1),
                    futures_util::StreamExt::next(&mut websocket),
                )
                .await
                .expect("401 terminal ordering barrier Ping should arrive")
                .expect("401 terminal ordering barrier connection should remain open")
                .unwrap();
                match frame {
                    Message::Ping(payload) => {
                        futures_util::SinkExt::send(&mut websocket, Message::Pong(payload))
                            .await
                            .unwrap();
                        break;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    other => panic!("unexpected frame after 401 terminal response: {other:?}"),
                }
            }
            drop(websocket);

            let (mut oauth, _) = listener.accept().await.unwrap();
            oauth_hits += 1;
            let refresh_request = read_complete_http_request(&mut oauth).await;
            assert!(refresh_request.starts_with("POST /oauth/token "));
            assert!(refresh_request.contains("refresh_token=r0"));
            write_test_http_response(
                &mut oauth,
                "200 OK",
                "application/json",
                "",
                br#"{"access_token":"a1","refresh_token":"r1","expires_in":3600}"#,
            )
            .await;

            let (stream, _) = listener.accept().await.unwrap();
            model_hits += 1;
            let mut retried_websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let retried = futures_util::StreamExt::next(&mut retried_websocket)
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            assert!(!retried.contains("previous_response_id"));
            assert!(retried.contains("full context"));
            assert!(!retried.contains("delta only"));
            futures_util::SinkExt::send(
                &mut retried_websocket,
                Message::Text(
                    r#"{"type":"response.completed","response":{"id":"resp_ok","usage":{}}}"#
                        .into(),
                ),
            )
            .await
            .unwrap();
            let barrier_deadline = tokio::time::sleep(Duration::from_secs(4));
            tokio::pin!(barrier_deadline);
            let mut saw_barrier_ping = false;
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        accepted.unwrap();
                        panic!("response.created followed by 401 must not trigger an extra replay");
                    }
                    frame = futures_util::StreamExt::next(&mut retried_websocket) => {
                        match frame {
                            Some(Ok(Message::Ping(payload))) => {
                                futures_util::SinkExt::send(
                                    &mut retried_websocket,
                                    Message::Pong(payload),
                                )
                                .await
                                .unwrap();
                                saw_barrier_ping = true;
                                barrier_deadline.as_mut().reset(
                                    tokio::time::Instant::now() + Duration::from_millis(300),
                                );
                            }
                            Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                            Some(Ok(other)) => {
                                panic!("unexpected frame after terminal response: {other:?}");
                            }
                            Some(Err(_)) | None if saw_barrier_ping => break,
                            Some(Err(error)) => panic!("terminal barrier stream failed: {error}"),
                            None => panic!("terminal barrier connection closed"),
                        }
                    }
                    () = &mut barrier_deadline => {
                        assert!(saw_barrier_ping, "terminal ordering barrier Ping should arrive");
                        break;
                    }
                }
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err(),
                "response.created followed by 401 must not trigger an extra replay"
            );
            (model_hits, oauth_hits)
        });

        let full_input = ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText {
                text: "full context".into(),
            }],
        };
        let delta_input = ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText {
                text: "delta only".into(),
            }],
        };
        let mut request = buffered_test_request();
        request.input = vec![full_input];
        let continuation = super::super::continuation::ContinuationCandidate {
            turn_id: Some(7),
            previous_response_id: Some("resp_prev".into()),
            input_delta: Some(vec![delta_input]),
            input_delta_count: 1,
            disabled_reason: None,
        };
        let temp = tempfile::TempDir::new().unwrap();
        let client = Arc::new(dispatch_budget_test_client(
            &temp,
            format!("http://{addr}/responses"),
            format!("http://{addr}/oauth/token"),
            StoredAuth {
                access: "a0".into(),
                refresh: "r0".into(),
                expires: u64::MAX,
                account_id: Some("acct".into()),
            },
        ));

        let mut events = client
            .stream_codex_websocket_events(
                &request,
                &http_test_context(),
                Some(&continuation),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();
        let created = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("response.created should be forwarded while recovery remains available")
            .unwrap()
            .unwrap();
        assert_eq!(created["type"], "response.created");

        let completed = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("the refreshed full-context replay should complete without forwarding 401")
            .unwrap()
            .unwrap();

        assert_eq!(completed["type"], "response.completed");
        assert_eq!(server.await.unwrap(), (2, 1));
    }

    #[tokio::test]
    async fn live_terminal_receiver_drop_still_allows_pool_probe_and_reuse() {
        let _state_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        use tokio_tungstenite::tungstenite::Message;

        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (allow_pong_tx, allow_pong_rx) = tokio::sync::oneshot::channel();
        let (pong_sent_tx, pong_sent_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = futures_util::StreamExt::next(&mut websocket)
                .await
                .expect("live request frame should arrive")
                .unwrap();
            assert!(matches!(request, Message::Text(_)));
            futures_util::SinkExt::send(
                &mut websocket,
                Message::Text(
                    r#"{"type":"response.completed","response":{"id":"resp_pool","usage":{}}}"#
                        .into(),
                ),
            )
            .await
            .unwrap();

            loop {
                let frame = futures_util::StreamExt::next(&mut websocket)
                    .await
                    .expect("terminal pool probe Ping should arrive")
                    .unwrap();
                match frame {
                    Message::Ping(payload) => {
                        allow_pong_rx
                            .await
                            .expect("test should release the matching Pong after receiver drop");
                        futures_util::SinkExt::send(&mut websocket, Message::Pong(payload))
                            .await
                            .unwrap();
                        let _ = pong_sent_tx.send(());
                        let _ = release_rx.await;
                        break;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    other => panic!("unexpected frame during terminal pool probe: {other:?}"),
                }
            }
        });

        let session_id = "live-terminal-receiver-drop-pool-reuse";
        let request = buffered_test_request();
        let continuation =
            super::super::continuation::continuation_candidate(Some(session_id), &request, true);
        let mut ctx = http_test_context();
        ctx.session_id = Some(session_id.to_string());
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events(
                &request,
                &ctx,
                Some(&continuation),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let terminal = tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("terminal should reach the outer receiver")
            .expect("outer receiver should remain open for the terminal")
            .expect("terminal should not become a transport error");
        assert_eq!(terminal["type"], "response.completed");
        drop(events);
        allow_pong_tx
            .send(())
            .expect("terminal pool probe should still be waiting for its Pong");
        tokio::time::timeout(Duration::from_millis(500), pong_sent_rx)
            .await
            .expect("matching Pong should be sent within the bounded probe window")
            .expect("matching Pong observer should remain available");

        tokio::time::timeout(Duration::from_millis(500), async {
            while !super::super::websocket::codex_websocket_pool_contains_for_tests(session_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the outer receiver must not cancel terminal pool reuse");

        let _ = release_tx.send(());
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        super::super::continuation::clear_continuation(Some(session_id));
    }

    #[tokio::test]
    async fn live_websocket_handshake_401_consumes_continuation_retry() {
        use super::super::translate::request::{ResponsesContentPart, ResponsesInputItem};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut rejected_handshake, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(rejected_handshake.read(&mut request).await.unwrap() > 0);
            rejected_handshake
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut oauth, _) = listener.accept().await.unwrap();
            let refresh_request = read_complete_http_request(&mut oauth).await;
            assert!(refresh_request.starts_with("POST /oauth/token "));
            write_test_http_response(
                &mut oauth,
                "200 OK",
                "application/json",
                "",
                br#"{"access_token":"a1","refresh_token":"r1","expires_in":3600}"#,
            )
            .await;

            let (mut full_context_handshake, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let read = full_context_handshake.read(&mut request).await.unwrap();
            let handshake_request = String::from_utf8_lossy(&request[..read]);
            assert!(handshake_request.starts_with("GET "));
            full_context_handshake
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();

            let Ok(Ok((mut unexpected_retry, _))) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
            else {
                return false;
            };
            let _ = unexpected_retry.read(&mut request).await;
            unexpected_retry
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            true
        });

        let full_input = ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText {
                text: "full context".into(),
            }],
        };
        let delta_input = ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText {
                text: "delta only".into(),
            }],
        };
        let mut request = buffered_test_request();
        request.input = vec![full_input];
        let continuation = super::super::continuation::ContinuationCandidate {
            turn_id: Some(8),
            previous_response_id: Some("resp_prev".into()),
            input_delta: Some(vec![delta_input]),
            input_delta_count: 1,
            disabled_reason: None,
        };
        let temp = tempfile::TempDir::new().unwrap();
        let client = Arc::new(dispatch_budget_test_client(
            &temp,
            format!("http://{addr}/responses"),
            format!("http://{addr}/oauth/token"),
            StoredAuth {
                access: "a0".into(),
                refresh: "r0".into(),
                expires: u64::MAX,
                account_id: Some("acct".into()),
            },
        ));

        let mut events = client
            .stream_codex_websocket_events(
                &request,
                &http_test_context(),
                Some(&continuation),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("the full-context handshake failure should be returned promptly")
            .expect("the coordinator should send the handshake error")
            .unwrap_err();

        assert_eq!(error.status, 503);
        assert_eq!(error.origin, CodexErrorOrigin::WebSocketHandshake);
        assert!(
            !server.await.unwrap(),
            "a 401 full-context replay must consume the continuation retry allowance"
        );
    }

    #[tokio::test]
    async fn auto_fallback_counts_websocket_and_http_against_one_attempt_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut handshake = [0_u8; 16 * 1024];
            let read = websocket.read(&mut handshake).await.unwrap();
            assert!(String::from_utf8_lossy(&handshake[..read]).contains("Upgrade: websocket"));
            websocket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(websocket);

            for _ in 0..3 {
                let (mut http, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16 * 1024];
                let read = http.read(&mut request).await.unwrap();
                assert!(read > 0);
                assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
                http.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 5\r\nretry-after: 0\r\nconnection: close\r\n\r\nretry",
                )
                .await
                .unwrap();
            }
        });

        let result = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await;
        server.await.unwrap();
        let error = match result {
            Ok(_) => panic!("the shared physical-attempt budget should be exhausted"),
            Err(error) => error,
        };

        assert_eq!(error.status, 503);
        assert_eq!(error.origin, CodexErrorOrigin::BufferedHttp);
    }

    #[tokio::test]
    async fn auto_routes_directly_to_http_while_websocket_circuit_is_open() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
            let body = b"data: keep\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let ctx = http_test_context();
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let circuit_key = client.websocket_circuit_key().to_string();
        for _ in 0..super::super::websocket::WEBSOCKET_CIRCUIT_FAILURE_THRESHOLD {
            super::super::websocket::record_codex_websocket_failure(&circuit_key);
        }

        let response = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &ctx,
                None,
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        server.await.unwrap();
        super::super::websocket::record_codex_websocket_success(&circuit_key);

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn over_budget_retry_after_stops_without_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nretry-after: 120\r\nconnection: close\r\n\r\nstop",
                )
                .await
                .unwrap();
        });

        let error = match authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
        {
            Ok(_) => panic!("over-budget Retry-After should propagate"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(error.status, 503);
        assert_eq!(error.retry_after.as_deref(), Some("120"));
    }

    #[tokio::test]
    async fn buffered_completed_response_ignores_trailing_rate_limit_telemetry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let completed_with_tail = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":0}}}\n\n".to_vec();
        let expected_body = completed_with_tail.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut stream).await;
            assert!(request.starts_with("POST /responses "));
            write_test_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                "",
                &completed_with_tail,
            )
            .await;

            let Ok(Ok((mut replay, _))) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
            else {
                return 1;
            };
            let _ = read_complete_http_request(&mut replay).await;
            write_test_http_response(
                &mut replay,
                "200 OK",
                "text/event-stream",
                "",
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"unexpected_replay\",\"usage\":{}}}\n\n",
            )
            .await;
            2
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, expected_body);
        assert_eq!(
            server.await.unwrap(),
            1,
            "completed response must not replay"
        );
    }

    #[tokio::test]
    async fn buffered_incomplete_response_ignores_trailing_rate_limit_telemetry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incomplete_with_tail = b"data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{}}}\n\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":0}}}\n\n".to_vec();
        let expected_body = incomplete_with_tail.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_complete_http_request(&mut stream).await;
            assert!(request.starts_with("POST /responses "));
            write_test_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                "",
                &incomplete_with_tail,
            )
            .await;

            let Ok(Ok((mut replay, _))) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
            else {
                return false;
            };
            let _ = read_complete_http_request(&mut replay).await;
            true
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, expected_body);
        assert!(
            !server.await.unwrap(),
            "incomplete response must not replay after trailing quota telemetry"
        );
    }

    #[tokio::test]
    async fn buffered_http_rejects_non_retryable_error_status_before_sse_parsing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = br#"{"error":{"message":"Model not found gpt-test"}}"#;
            let response = format!(
                "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let result = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;
        server.await.unwrap();
        let error = match result {
            Ok(_) => panic!("non-success HTTP status must not reach the SSE reducer"),
            Err(error) => error,
        };

        assert_eq!(error.status, 404);
        assert_eq!(error.detail.as_deref(), Some("Model not found gpt-test"));
        assert_eq!(error.origin, CodexErrorOrigin::BufferedHttp);
    }

    #[test]
    fn status_error_preserves_buffered_websocket_event_message() {
        let error = codex_status_error(
            CodexResponse {
                body: b"data: {\"type\":\"error\",\"error\":{\"status\":400,\"message\":\"bad request\"}}\n\n"
                    .to_vec(),
                status: 400,
                headers: Vec::new(),
            },
            crate::config::CodexTransport::WebSocket,
        );

        assert_eq!(error.status, 400);
        assert_eq!(error.detail.as_deref(), Some("bad request"));
        assert_eq!(error.origin, CodexErrorOrigin::BufferedWebSocket);
    }

    #[test]
    fn status_error_uses_structured_context_code_and_sanitizes_detail() {
        let error = codex_status_error(
            CodexResponse {
                body: br#"{"error":{"code":"context_length_exceeded","type":"invalid_request_error","param":"input","message":"Authorization: Bearer top-secret"}}"#
                    .to_vec(),
                status: 400,
                headers: Vec::new(),
            },
            crate::config::CodexTransport::Http,
        );

        assert_eq!(error.status, 413);
        let detail = error.detail.unwrap();
        assert!(!detail.contains("top-secret"));
        assert!(detail.contains("[redacted upstream error detail]"));
        assert!(detail.contains("code: context_length_exceeded"));
        assert!(detail.contains("param: input"));
    }

    #[test]
    fn status_error_keeps_structured_non_context_code_authoritative() {
        let error = codex_status_error(
            CodexResponse {
                body: br#"{"error":{"code":"safety_policy_violation","type":"invalid_request_error","message":"Prompt is too long for the safety classifier"}}"#
                    .to_vec(),
                status: 400,
                headers: Vec::new(),
            },
            crate::config::CodexTransport::Http,
        );

        assert_eq!(error.status, 400);
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("safety_policy_violation"))
        );
    }

    #[test]
    fn malformed_error_bytes_are_not_exposed() {
        assert_eq!(codex_status_error_message(b"\xffBearer top-secret"), None);
        assert_eq!(
            codex_status_error_message(br#"{"error":{"secret":"sk-live-123"}}"#),
            None
        );
        let error = codex_status_error(
            CodexResponse {
                body: br#"{"error":{"secret":"sk-live-123"}}"#.to_vec(),
                status: 400,
                headers: Vec::new(),
            },
            crate::config::CodexTransport::Http,
        );
        assert_eq!(
            error.detail.as_deref(),
            Some("Upstream Codex request failed with status 400")
        );
        assert!(!error.detail.unwrap().contains("sk-live-123"));
    }

    #[test]
    fn total_deadline_clamps_extreme_timeout_without_overflowing() {
        let deadline = CodexRequestDeadline::from_started_at(Instant::now(), u64::MAX);

        assert_eq!(deadline.timeout_ms(), MAX_CODEX_TOTAL_TIMEOUT_MS);
        assert!(deadline.at() > tokio::time::Instant::now());
    }

    #[test]
    fn model_dispatch_budget_maps_to_a_terminal_codex_error() {
        let budget = CodexDispatchBudget::new();
        for expected in 1..=MAX_BUFFERED_TRANSPORT_ATTEMPTS {
            assert_eq!(budget.reserve_model().unwrap().attempt, expected);
        }

        let error = model_dispatch_budget_error(
            budget.reserve_model().unwrap_err(),
            crate::config::CodexTransport::WebSocket,
        );
        assert_eq!(error.status, 503);
        assert_eq!(error.detail.as_deref(), Some(CODEX_DISPATCH_BUDGET_DETAIL));
    }

    #[tokio::test]
    async fn expired_refresh_then_401_refresh_then_replay_shares_one_dispatch_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut oauth_hits = 0_usize;
            let mut model_hits = 0_usize;
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_complete_http_request(&mut stream).await;
                if request.starts_with("POST /oauth/token ") {
                    oauth_hits += 1;
                    let (expected_refresh, access, refresh) = if oauth_hits == 1 {
                        ("refresh_token=r0", "a1", "r1")
                    } else {
                        ("refresh_token=r1", "a2", "r2")
                    };
                    assert!(request.contains(expected_refresh));
                    let body = format!(
                        r#"{{"access_token":"{access}","refresh_token":"{refresh}","expires_in":3600}}"#
                    );
                    write_test_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        "",
                        body.as_bytes(),
                    )
                    .await;
                } else {
                    assert!(request.starts_with("POST /responses "));
                    model_hits += 1;
                    if model_hits == 1 {
                        assert!(request.contains("Bearer a1"));
                        write_test_http_response(
                            &mut stream,
                            "401 Unauthorized",
                            "application/json",
                            "",
                            br#"{"error":{"message":"expired access"}}"#,
                        )
                        .await;
                    } else {
                        assert!(request.contains("Bearer a2"));
                        write_test_http_response(
                            &mut stream,
                            "200 OK",
                            "text/event-stream",
                            "",
                            b"data: keep\n\n",
                        )
                        .await;
                    }
                }
            }
            (model_hits, oauth_hits)
        });

        let temp = tempfile::TempDir::new().unwrap();
        let client = dispatch_budget_test_client(
            &temp,
            format!("http://{addr}/responses"),
            format!("http://{addr}/oauth/token"),
            StoredAuth {
                access: "expired".into(),
                refresh: "r0".into(),
                expires: 0,
                account_id: Some("acct".into()),
            },
        );
        let response = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(server.await.unwrap(), (2, 2));
    }

    #[tokio::test]
    async fn fourth_model_401_does_not_refresh_without_replay_capacity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut oauth_hits = 0_usize;
            let mut model_hits = 0_usize;
            loop {
                let accepted = if model_hits >= MAX_CODEX_MODEL_DISPATCHES as usize {
                    match tokio::time::timeout(Duration::from_millis(200), listener.accept()).await
                    {
                        Ok(accepted) => accepted.unwrap(),
                        Err(_) => break,
                    }
                } else {
                    listener.accept().await.unwrap()
                };
                let (mut stream, _) = accepted;
                let request = read_complete_http_request(&mut stream).await;
                if request.starts_with("POST /oauth/token ") {
                    oauth_hits += 1;
                    write_test_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        "",
                        br#"{"access_token":"unexpected","refresh_token":"unexpected-r","expires_in":3600}"#,
                    )
                    .await;
                    continue;
                }

                assert!(request.starts_with("POST /responses "));
                model_hits += 1;
                match model_hits {
                    1..=3 => {
                        write_test_http_response(
                            &mut stream,
                            "503 Service Unavailable",
                            "application/json",
                            "retry-after: 0\r\n",
                            br#"{"error":{"message":"retry"}}"#,
                        )
                        .await;
                    }
                    4 => {
                        write_test_http_response(
                            &mut stream,
                            "401 Unauthorized",
                            "application/json",
                            "",
                            br#"{"error":{"message":"expired access"}}"#,
                        )
                        .await;
                    }
                    _ => {
                        write_test_http_response(
                            &mut stream,
                            "200 OK",
                            "text/event-stream",
                            "",
                            b"data: unexpected replay\n\n",
                        )
                        .await;
                    }
                }
            }
            (model_hits, oauth_hits)
        });

        let temp = tempfile::TempDir::new().unwrap();
        let client = dispatch_budget_test_client(
            &temp,
            format!("http://{addr}/responses"),
            format!("http://{addr}/oauth/token"),
            StoredAuth {
                access: "valid-until-rejected".into(),
                refresh: "r0".into(),
                expires: u64::MAX,
                account_id: Some("acct".into()),
            },
        );
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;
        let error = match result {
            Ok(_) => panic!("a fourth model 401 must be returned without refresh or replay"),
            Err(error) => error,
        };

        assert_eq!(error.status, 401);
        assert_eq!(server.await.unwrap(), (4, 0));
    }

    #[tokio::test]
    async fn live_http_events_are_forwarded_incrementally_before_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream
                .write_all(b"data: {\"type\":\"response.created\"}\n\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            stream
                .write_all(
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"HTTP_ONCE\",\"output_index\":0}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
                )
                .await
                .unwrap();
        });
        let client = Arc::new(authenticated_http_test_client_with_body_timeouts(
            format!("http://{addr}/responses"),
            100,
            300,
        ));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let first = tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .expect("the first HTTP SSE record should not wait for response EOF")
            .unwrap()
            .unwrap();
        assert_eq!(first["type"], "response.created");
        let mut types = vec!["response.created".to_string()];
        while let Some(event) = events.recv().await {
            types.push(event.unwrap()["type"].as_str().unwrap().to_string());
        }
        server.await.unwrap();
        assert_eq!(
            types,
            [
                "response.created",
                "response.output_text.delta",
                "response.completed"
            ]
        );
    }

    #[tokio::test]
    async fn live_http_rejects_same_chunk_terminal_tail_before_forwarding_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"late\"}\n\n",
                )
                .await
                .unwrap();
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let error = events.recv().await.unwrap().unwrap_err();
        assert!(error.message.contains("after terminal response event"));
        assert!(events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_forwards_terminal_before_a_later_semantic_tail() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = stream
                .write_all(
                    b"data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"late\"}\n\n",
                )
                .await;
        });
        let client = Arc::new(authenticated_http_test_client_with_body_timeouts(
            format!("http://{addr}/responses"),
            100,
            300,
        ));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let terminal = tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .expect("the terminal waited for a later HTTP chunk")
            .expect("the terminal event was missing")
            .expect("the terminal event became a transport error");
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_terminal_survives_a_truncated_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            const TERMINAL: &[u8] = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\n";
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let declared_length = TERMINAL.len() + 128;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {declared_length}\r\nconnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(TERMINAL).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Dropping the stream with fewer bytes than Content-Length makes a later body read
            // fail. The already decoded terminal must remain authoritative.
        });
        let client = Arc::new(authenticated_http_test_client_with_body_timeouts(
            format!("http://{addr}/responses"),
            100,
            300,
        ));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let terminal = tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .expect("the terminal waited for the truncated body to close")
            .expect("the terminal event was missing")
            .expect("the terminal event became a body error");
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_terminal_ignores_a_partial_post_terminal_sse_tail() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"truncated",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let client = Arc::new(authenticated_http_test_client_with_body_timeouts(
            format!("http://{addr}/responses"),
            100,
            300,
        ));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let terminal = tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .expect("the terminal waited for an incomplete SSE tail")
            .expect("the terminal event was missing")
            .expect("the incomplete tail replaced the terminal with an error");
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_allows_rate_limit_telemetry_after_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true}}\n\n",
                )
                .await
                .unwrap();
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_stalled_first_byte_uses_independent_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });
        let client = Arc::new(authenticated_http_test_client_with_body_timeouts(
            format!("http://{addr}/responses"),
            50,
            500,
        ));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .expect("the independent first-byte deadline should fire")
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .message
                .contains("first Codex HTTP response body byte")
        );
        assert_eq!(error.detail.as_deref(), Some("http_response_first_byte"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_status_error_preserves_the_provider_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = br#"{"type":"error","error":{"message":"provider overloaded now"}}"#;
            let headers = format!(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let error = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap_err();
        server.await.unwrap();

        assert_eq!(error.status, 503);
        assert_eq!(error.detail.as_deref(), Some("provider overloaded now"));
        assert_eq!(error.retry_after.as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn live_http_total_deadline_preempts_stalled_headers_and_closes_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let mut byte = [0_u8; 1];
            tokio::time::timeout(Duration::from_millis(500), stream.read(&mut byte))
                .await
                .expect("the live header deadline should close the HTTP socket")
                .unwrap()
        });
        let client = Arc::new(CodexHttpClient::new_for_test(
            format!("http://{addr}/responses"),
            5_000,
            5_000,
            5_000,
        ));
        client.auth_manager().set_test_auth(http_test_auth());

        let error = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(80),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, 504);
        assert_eq!(error.detail.as_deref(), Some(CODEX_TOTAL_TIMEOUT_DETAIL));
        assert_eq!(server.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn live_http_active_bytes_cannot_extend_the_total_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for _ in 0..40 {
                if stream
                    .write_all(b"data: {\"type\":\"keepalive\"}\n\n")
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        });
        let client = Arc::new(CodexHttpClient::new_for_test(
            format!("http://{addr}/responses"),
            1_000,
            1_000,
            5_000,
        ));
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(100),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                match events.recv().await {
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => break error,
                    None => panic!("live HTTP deadline closed without its terminal cause"),
                }
            }
        })
        .await
        .expect("active bytes must not extend the total deadline");
        assert_eq!(error.status, 504);
        assert_eq!(error.detail.as_deref(), Some(CODEX_TOTAL_TIMEOUT_DETAIL));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_live_http_receiver_closes_the_upstream_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.created\"}\n\n",
                )
                .await
                .unwrap();
            let mut byte = [0_u8; 1];
            tokio::time::timeout(Duration::from_millis(500), stream.read(&mut byte))
                .await
                .expect("dropping the receiver should close the HTTP response")
                .unwrap()
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                CodexRequestDeadline::from_timeout_ms(1_000),
                CodexDispatchBudget::new(),
            )
            .await
            .unwrap();
        assert!(events.recv().await.unwrap().is_ok());
        drop(events);

        assert_eq!(server.await.unwrap(), 0);
    }

    #[test]
    fn incremental_http_sse_decoder_accepts_split_crlf_records() {
        let mut decoder = CodexLiveSseDecoder::default();
        assert!(
            decoder
                .push(b"event: response.created\r\ndata: {\"type\":\"response.cre")
                .unwrap()
                .is_empty()
        );
        let events = decoder.push(b"ated\"}\r\n\r\n").unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.created");
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn incremental_http_sse_decoder_dispatches_cr_only_record_immediately() {
        let mut decoder = CodexLiveSseDecoder::default();
        let events = decoder
            .push(b"event: response.created\rdata: {\"type\":\"response.created\"}\r\r")
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.created");
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn incremental_http_sse_decoder_accepts_split_stream_start_bom() {
        let mut decoder = CodexLiveSseDecoder::default();
        assert!(decoder.push(b"\xef").unwrap().is_empty());
        assert!(decoder.push(b"\xbb").unwrap().is_empty());
        let events = decoder
            .push(b"\xbfdata: {\"type\":\"response.created\"}\n\n")
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.created");
    }

    #[test]
    fn incremental_http_sse_decoder_rejects_invalid_utf8() {
        let mut decoder = CodexLiveSseDecoder::default();
        let error = decoder
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\xff\"}\n\n")
            .unwrap_err();

        assert!(error.contains("invalid UTF-8"));
    }

    #[test]
    fn incremental_http_sse_decoder_rejects_bom_after_stream_start() {
        let mut decoder = CodexLiveSseDecoder::default();
        decoder
            .push(b"data: {\"type\":\"response.created\"}\n\n")
            .unwrap();
        let error = decoder
            .push(b"\xef\xbb\xbfdata: {\"type\":\"response.completed\"}\n\n")
            .unwrap_err();

        assert!(error.contains("BOM after stream start"));
    }

    #[test]
    fn incremental_http_sse_decoder_retains_only_a_delimiter_overlap_cursor() {
        let mut decoder = CodexLiveSseDecoder::default();
        for _ in 0..100_000 {
            assert!(decoder.push(b"x").unwrap().is_empty());
            assert!(decoder.scan_from >= decoder.pending.len().saturating_sub(3));
        }

        assert_eq!(decoder.pending.len(), 100_000);
        assert_eq!(decoder.scan_from, decoder.pending.len() - 3);
    }

    #[test]
    fn incremental_http_sse_decoder_rejects_complete_oversized_event() {
        let mut decoder = CodexLiveSseDecoder::default();
        let mut event = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"".to_vec();
        event.resize(super::super::MAX_LIVE_EVENT_BYTES + 1, b'x');
        event.extend_from_slice(b"\"}\n\n");

        let error = decoder.push(&event).unwrap_err();

        assert!(error.contains("incremental decode limit"));
    }

    #[tokio::test]
    async fn buffered_http_rejects_declared_oversized_success_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                MAX_BUFFERED_HTTP_SUCCESS_BODY_BYTES + 1
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));

        let error = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
        {
            Ok(_) => panic!("declared oversized response must fail"),
            Err(error) => error,
        };
        server.await.unwrap();

        assert_eq!(error.detail.as_deref(), Some("http_response_size_limit"));
        assert!(!is_retryable_transport_error(&error));
    }

    #[tokio::test]
    async fn buffered_http_rejects_chunked_body_at_cumulative_limit_plus_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let data = vec![b'x'; 64 * 1024];
            let mut remaining = MAX_BUFFERED_HTTP_SUCCESS_BODY_BYTES + 1;
            while remaining > 0 {
                let size = remaining.min(data.len());
                let header = format!("{size:x}\r\n");
                if stream.write_all(header.as_bytes()).await.is_err()
                    || stream.write_all(&data[..size]).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    break;
                }
                remaining -= size;
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        });
        let client = http_test_client(format!("http://{addr}/responses"), 5_000);
        client.auth_manager().set_test_auth(http_test_auth());

        let error = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
        {
            Ok(_) => panic!("chunked oversized response must fail"),
            Err(error) => error,
        };
        server.await.unwrap();

        assert_eq!(error.detail.as_deref(), Some("http_response_size_limit"));
        assert!(!is_retryable_transport_error(&error));
    }

    #[tokio::test]
    async fn total_timeout_preempts_stalled_http_headers_with_504() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            futures_util::future::pending::<()>().await;
        });
        let client =
            CodexHttpClient::new_for_test(format!("http://{addr}/responses"), 5_000, 5_000, 100);
        client.auth_manager().set_test_auth(http_test_auth());

        let started_at = Instant::now();
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;
        server.abort();
        let error = match result {
            Ok(_) => panic!("the total wall-clock budget must preempt the header timeout"),
            Err(error) => error,
        };

        assert_eq!(error.status, 504);
        assert_eq!(error.detail.as_deref(), Some(CODEX_TOTAL_TIMEOUT_DETAIL));
        assert!(started_at.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn total_timeout_is_shared_by_auto_websocket_and_http_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (fallback_started_tx, fallback_started_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let read = websocket.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("Upgrade: websocket"));
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
            let _ = fallback_started_tx.send(());
            futures_util::future::pending::<()>().await;
        });
        let client =
            CodexHttpClient::new_for_test(format!("http://{addr}/responses"), 5_000, 5_000, 120);
        client.auth_manager().set_test_auth(http_test_auth());

        let started_at = Instant::now();
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await;
        tokio::time::timeout(Duration::from_millis(250), fallback_started_rx)
            .await
            .expect("Auto should reach HTTP fallback within the shared budget")
            .expect("fallback observer should remain available");
        server.abort();
        let error = match result {
            Ok(_) => panic!("the shared total budget must preempt stalled HTTP fallback"),
            Err(error) => error,
        };

        assert_eq!(error.status, 504);
        assert_eq!(error.detail.as_deref(), Some(CODEX_TOTAL_TIMEOUT_DETAIL));
        assert!(started_at.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn pooled_websocket_total_timeout_invalidates_before_aborting_turn() {
        let _state_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        use super::super::translate::request::{ResponsesContentPart, ResponsesInputItem};
        use tokio_tungstenite::tungstenite::Message;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (second_request_tx, second_request_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut request_count = 0_u8;
            let mut second_request_tx = Some(second_request_tx);
            while let Some(frame) = futures_util::StreamExt::next(&mut websocket).await {
                match frame.unwrap() {
                    Message::Ping(payload) => {
                        futures_util::SinkExt::send(&mut websocket, Message::Pong(payload))
                            .await
                            .unwrap();
                    }
                    Message::Text(_) => {
                        request_count += 1;
                        if request_count == 1 {
                            futures_util::SinkExt::send(
                                &mut websocket,
                                Message::Text(
                                    r#"{"type":"response.completed","response":{"id":"resp_1","usage":{}}}"#
                                        .into(),
                                ),
                            )
                            .await
                            .unwrap();
                        } else {
                            let _ = second_request_tx.take().unwrap().send(());
                            futures_util::future::pending::<()>().await;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let session_id = "pooled-websocket-deadline-cleanup";
        let input = |text: &str| ResponsesInputItem::Message {
            role: "user".into(),
            content: vec![ResponsesContentPart::InputText { text: text.into() }],
        };
        let mut first_request = buffered_test_request();
        first_request.input.push(input("first"));
        let first_continuation = super::super::continuation::continuation_candidate(
            Some(session_id),
            &first_request,
            true,
        );
        let mut ctx = http_test_context();
        ctx.session_id = Some(session_id.to_string());
        let client =
            CodexHttpClient::new_for_test(format!("http://{addr}/responses"), 2_000, 2_000, 2_000);
        client.auth_manager().set_test_auth(http_test_auth());

        let first_response = client
            .post_codex_with_transport_before(
                &first_request,
                &ctx,
                Some(&first_continuation),
                crate::config::CodexTransport::WebSocket,
                CodexRequestDeadline::from_timeout_ms(2_000),
            )
            .await
            .unwrap();
        assert_eq!(first_response.status, 200);
        assert!(super::super::websocket::codex_websocket_pool_contains_for_tests(session_id));

        super::super::continuation::record_continuation(
            Some(session_id),
            first_continuation.turn_id,
            &first_request,
            Some("resp_1"),
            &[],
        );
        let mut second_request = first_request.clone();
        second_request.input.push(input("second"));
        let second_continuation = super::super::continuation::continuation_candidate(
            Some(session_id),
            &second_request,
            true,
        );
        assert_eq!(
            second_continuation.previous_response_id.as_deref(),
            Some("resp_1")
        );
        assert!(second_continuation.disabled_reason.is_none());

        let result = client
            .post_codex_with_transport_before(
                &second_request,
                &ctx,
                Some(&second_continuation),
                crate::config::CodexTransport::WebSocket,
                CodexRequestDeadline::from_timeout_ms(250),
            )
            .await;
        let error = match result {
            Ok(_) => panic!("the stalled pooled request should hit its total deadline"),
            Err(error) => error,
        };
        assert_eq!(error.detail.as_deref(), Some(CODEX_TOTAL_TIMEOUT_DETAIL));
        tokio::time::timeout(Duration::from_millis(250), second_request_rx)
            .await
            .expect("the timed-out request should have reused the pooled connection")
            .unwrap();
        assert!(
            !super::super::websocket::codex_websocket_pool_contains_for_tests(session_id),
            "a timed-out pooled socket must not remain reusable"
        );
        assert!(!super::super::continuation::is_current_turn(
            Some(session_id),
            second_continuation.turn_id
        ));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn dropping_buffered_request_closes_http_and_aborts_continuation() {
        let _state_guard = super::super::CODEX_STATE_TEST_LOCK.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let _ = request_seen_tx.send(());
            let mut byte = [0_u8; 1];
            stream.read(&mut byte).await
        });

        let session_id = "buffered-cancel-cleans-continuation";
        let request = buffered_test_request();
        let continuation =
            super::super::continuation::continuation_candidate(Some(session_id), &request, true);
        let mut ctx = http_test_context();
        ctx.session_id = Some(session_id.to_string());
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let task = tokio::spawn(async move {
            client
                .post_codex_with_transport(
                    &request,
                    &ctx,
                    Some(&continuation),
                    crate::config::CodexTransport::Http,
                )
                .await
        });

        tokio::time::timeout(Duration::from_millis(500), request_seen_rx)
            .await
            .expect("request should reach the upstream")
            .unwrap();
        task.abort();
        let _ = task.await;
        let closed = tokio::time::timeout(Duration::from_millis(500), server)
            .await
            .expect("cancelling the request should close the upstream socket")
            .unwrap();
        assert!(matches!(closed, Ok(0) | Err(_)));

        let next = super::super::continuation::continuation_candidate(
            Some(session_id),
            &buffered_test_request(),
            true,
        );
        assert_eq!(next.disabled_reason.as_deref(), Some("missing_state"));
        super::super::continuation::abort_continuation(Some(session_id), next.turn_id);
    }

    #[tokio::test]
    async fn active_http_body_can_exceed_header_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for chunk in [b"a".as_slice(), b"b", b"c"] {
                stream.write_all(b"1\r\n").await.unwrap();
                stream.write_all(chunk).await.unwrap();
                stream.write_all(b"\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(45)).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        });

        let response = http_test_client(format!("http://{addr}/responses"), 80)
            .attempt_post_http(
                &http_test_auth(),
                &Bytes::from_static(b"{}"),
                &http_test_context(),
                false,
            )
            .await
            .expect("active body should not hit a whole-request timeout");
        server.await.unwrap();

        assert_eq!(response.body, b"abc");
    }

    #[tokio::test]
    async fn stalled_buffered_http_first_byte_hits_independent_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let result =
            http_test_client_with_body_timeouts(format!("http://{addr}/responses"), 30, 500)
                .attempt_post_http(
                    &http_test_auth(),
                    &Bytes::from_static(b"{}"),
                    &http_test_context(),
                    false,
                )
                .await;
        server.await.unwrap();
        let error = result.err().expect("stalled first byte should time out");

        assert!(
            error
                .message
                .contains("first Codex HTTP response body byte")
        );
        assert_eq!(error.detail.as_deref(), Some("http_response_first_byte"));
    }

    #[tokio::test]
    async fn buffered_http_uses_body_idle_timeout_after_first_chunk() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n1\r\na\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;
            stream.write_all(b"1\r\nb\r\n0\r\n\r\n").await.unwrap();
        });

        let response =
            http_test_client_with_body_timeouts(format!("http://{addr}/responses"), 20, 150)
                .attempt_post_http(
                    &http_test_auth(),
                    &Bytes::from_static(b"{}"),
                    &http_test_context(),
                    false,
                )
                .await
                .expect("the shorter first-byte timeout must not govern later chunks");
        server.await.unwrap();

        assert_eq!(response.body, b"ab");
    }

    #[tokio::test]
    async fn stalled_buffered_http_after_first_chunk_hits_body_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n1\r\na\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let result =
            http_test_client_with_body_timeouts(format!("http://{addr}/responses"), 200, 30)
                .attempt_post_http(
                    &http_test_auth(),
                    &Bytes::from_static(b"{}"),
                    &http_test_context(),
                    false,
                )
                .await;
        server.await.unwrap();
        let error = result.err().expect("a stalled later chunk should time out");

        assert!(error.message.contains("next Codex response body chunk"));
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
    }

    #[tokio::test]
    async fn reset_http_body_returns_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\npartial")
                .await
                .unwrap();
        });

        let result = http_test_client(format!("http://{addr}/responses"), 100)
            .attempt_post_http(
                &http_test_auth(),
                &Bytes::from_static(b"{}"),
                &http_test_context(),
                false,
            )
            .await;
        server.await.unwrap();
        let error = result.err().expect("truncated body should fail");

        assert!(
            error
                .message
                .contains("Transport error reading Codex response body")
        );
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
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
    fn websocket_pre_request_statusless_error_is_retryable() {
        let err = CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: None,
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
    fn statusless_transport_error_matching_is_case_insensitive() {
        let err = CodexError {
            status: 0,
            message: "WebSocket protocol error: Connection reset without closing handshake"
                .to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn structured_websocket_watchdog_errors_are_retryable() {
        for detail in [
            super::super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL,
            super::super::websocket::WEBSOCKET_IDLE_TIMEOUT_DETAIL,
            super::super::websocket::WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL,
            super::super::websocket::WEBSOCKET_POOL_HEALTHCHECK_DETAIL,
            super::super::websocket::WEBSOCKET_POOL_BUSY_DETAIL,
            super::super::websocket::WEBSOCKET_CONNECTION_ERROR_DETAIL,
            super::super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL,
        ] {
            let err = CodexError {
                status: 0,
                message: "localized or implementation-specific message".to_string(),
                detail: Some(detail.to_string()),
                retry_after: None,
                origin: CodexErrorOrigin::WebSocket,
            };
            assert!(is_retryable_transport_error(&err), "detail={detail}");
        }
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
            request_byte_lease: None,
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
            request_byte_lease: None,
        };
        let headers = build_codex_headers(&auth, &ctx, true).unwrap();
        assert_eq!(
            headers
                .get("x-openai-internal-codex-responses-lite")
                .unwrap(),
            "true"
        );
        assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(default_user_agent(true), "codex_cli_rs");
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
            request_byte_lease: None,
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
            request_byte_lease: None,
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
            schema_bridge: None,
            hosted_web_search_max_uses: None,
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
    fn websocket_pool_key_is_independent_of_payload_continuation() {
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("session".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        };
        let disabled = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("disabled".into()),
        };
        let first_enabled = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("missing_state".into()),
        };
        let append = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: Some("resp_1".into()),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: None,
        };

        assert_eq!(websocket_pool_key(&ctx, Some(&disabled)), Some("session"));
        assert_eq!(
            websocket_pool_key(&ctx, Some(&first_enabled)),
            Some("session")
        );
        assert_eq!(websocket_pool_key(&ctx, Some(&append)), Some("session"));
    }

    #[test]
    fn websocket_breaker_key_is_normalized_to_the_upstream_origin() {
        assert_eq!(
            websocket_circuit_key_for_url("https://example.test/v1/responses?lane=fast"),
            websocket_circuit_key_for_url("https://example.test/other/path")
        );
        assert_ne!(
            websocket_circuit_key_for_url("https://example.test/responses"),
            websocket_circuit_key_for_url("https://example.test:8443/responses")
        );
    }

    #[test]
    fn websocket_pool_reset_clears_initial_stale_state() {
        let missing_state = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("missing_state".into()),
        };
        let disabled = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("disabled".into()),
        };
        let prompt_changed = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: Some("prompt_changed".into()),
        };

        assert!(should_reset_websocket_pool(Some(&missing_state)));
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
            request_byte_lease: None,
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
    fn unauthorized_retry_refreshes_every_transport_once() {
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
        let rejected_handshake = Err(CodexError {
            status: 401,
            message: "WebSocket connect error".to_string(),
            detail: Some("policy denied".to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
        let rejected_handshake_err = match &rejected_handshake {
            Err(error) => error,
            Ok(_) => panic!("expected rejected handshake"),
        };
        let stale_pool = CodexError {
            status: 0,
            message: "stale pooled connection".to_string(),
            detail: Some(super::super::websocket::WEBSOCKET_POOL_HEALTHCHECK_DETAIL.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(should_refresh_after_unauthorized(&http_unauthorized, false));
        assert!(should_refresh_after_unauthorized(
            &websocket_unauthorized,
            false
        ));
        assert!(!should_refresh_after_unauthorized(&forbidden, false));
        assert!(should_refresh_after_unauthorized(
            &rejected_handshake,
            false
        ));
        assert!(!should_refresh_after_unauthorized(&http_unauthorized, true));
        // Permanent 4xx handshake rejections are not transport fallback candidates.
        assert!(!should_fallback_to_http(rejected_handshake_err));
        // Structured pre-output WebSocket transport failures fall back in auto mode.
        assert!(should_fallback_to_http(&stale_pool));
    }

    #[test]
    fn should_fallback_to_http_only_for_retryable_websocket_transport_errors() {
        let statusless_ws = CodexError {
            status: 0,
            message: "WebSocket protocol error: Connection reset without closing handshake"
                .to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let retryable_handshake = CodexError {
            status: 503,
            message: "Service unavailable".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let statusless_handshake = CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let http_reset = CodexError {
            status: 0,
            message: "Connection reset".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        };
        let business_400 = CodexError {
            status: 400,
            message: "Bad request".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let business_401 = CodexError {
            status: 401,
            message: "Unauthorized".into(),
            detail: Some("policy denied".into()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let service_429 = CodexError {
            status: 429,
            message: "Rate limited".into(),
            detail: Some("rate limit".into()),
            retry_after: Some("1".into()),
            origin: CodexErrorOrigin::WebSocket,
        };
        let service_503 = CodexError {
            status: 503,
            message: "Overloaded".into(),
            detail: Some("service unavailable".into()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let missing_continuation = CodexError {
            status: 404,
            message: "Previous response not found".into(),
            detail: Some("previous_response_not_found".into()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        // A generic WebSocket reset may happen after the request frame was
        // accepted, so transient does not imply replay-safe.
        assert!(!should_fallback_to_http(&statusless_ws));
        assert!(should_fallback_to_http(&retryable_handshake));
        assert!(should_fallback_to_http(&statusless_handshake));
        assert!(!should_fallback_to_http(&http_reset));
        assert!(!should_fallback_to_http(&business_400));
        assert!(!should_fallback_to_http(&business_401));
        assert!(!should_fallback_to_http(&service_429));
        assert!(!should_fallback_to_http(&service_503));
        assert!(!should_fallback_to_http(&missing_continuation));
    }

    #[test]
    fn auto_http_fallback_is_immediate_only_for_statusless_predispatch_faults() {
        let statusless_handshake = CodexError {
            status: 0,
            message: "connect refused".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let service_unavailable = CodexError {
            status: 503,
            message: "overloaded".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let retry_after = CodexError {
            status: 503,
            message: "overloaded".into(),
            detail: None,
            retry_after: Some("0.4".into()),
            origin: CodexErrorOrigin::WebSocketHandshake,
        };

        assert_eq!(
            auto_http_fallback_delay(&statusless_handshake, &mut ModelRetryBackoff::default())
                .unwrap()
                .wait_ms,
            0
        );
        let service_delay =
            auto_http_fallback_delay(&service_unavailable, &mut ModelRetryBackoff::default())
                .unwrap();
        assert!((100..=300).contains(&service_delay.wait_ms));
        assert_eq!(
            auto_http_fallback_delay(&retry_after, &mut ModelRetryBackoff::default())
                .unwrap()
                .wait_ms,
            400
        );
    }

    #[test]
    fn auto_circuit_counts_only_retryable_websocket_transport_failures() {
        let response_timeout = CodexError {
            status: 0,
            message: "Response start timeout".into(),
            detail: Some(super::super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.into()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let bad_request = CodexError {
            status: 400,
            message: "Bad request".into(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
        let ctx = http_test_context();
        let circuit_key = "auto-circuit-reset-test";
        assert!(!record_auto_websocket_failure(
            &ctx,
            circuit_key,
            &response_timeout
        ));
        assert!(!record_auto_websocket_failure(
            &ctx,
            circuit_key,
            &response_timeout
        ));
        assert!(!record_auto_websocket_failure(
            &ctx,
            circuit_key,
            &bad_request
        ));
        assert!(!record_auto_websocket_failure(
            &ctx,
            circuit_key,
            &response_timeout
        ));
        super::super::websocket::record_codex_websocket_success(circuit_key);
    }

    #[test]
    fn non_semantic_events_keep_live_recovery_window_open() {
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": false}
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "keepalive"
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "codex.response.metadata",
            "headers": {"openai-model": "gpt-5.6-sol"}
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "response.created"
        })));
        assert!(event_closes_live_retry_window(&serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "hello"
        })));
        assert!(event_closes_live_retry_window(&serde_json::json!({
            "type": "response.output_item.added",
            "item": {"type": "function_call", "call_id": "call_1"}
        })));
    }

    #[test]
    fn continuation_retry_requires_previous_response_id() {
        let append = super::super::continuation::ContinuationCandidate {
            turn_id: None,
            previous_response_id: Some("resp_1".into()),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: None,
        };
        let initial = super::super::continuation::ContinuationCandidate {
            turn_id: None,
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
            detail: Some(super::super::websocket::WEBSOCKET_IDLE_TIMEOUT_DETAIL.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let pool_healthcheck = CodexError {
            status: 0,
            message: "WebSocket pooled connection health check failed".to_string(),
            detail: Some(super::super::websocket::WEBSOCKET_POOL_HEALTHCHECK_DETAIL.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let pool_busy = CodexError {
            status: 0,
            message: "WebSocket pooled connection remained busy".to_string(),
            detail: Some(super::super::websocket::WEBSOCKET_POOL_BUSY_DETAIL.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(!should_retry_without_continuation(&timeout, Some(&append)));
        assert!(should_retry_without_continuation(&missing, Some(&append)));
        assert!(!should_retry_without_continuation(&idle, Some(&append)));
        assert!(should_retry_without_continuation(
            &pool_healthcheck,
            Some(&append)
        ));
        assert!(should_retry_without_continuation(&pool_busy, Some(&append)));
        assert!(!should_retry_without_continuation(&timeout, Some(&initial)));
        assert!(!should_retry_without_continuation(&timeout, None));
    }
}
