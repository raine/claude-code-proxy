use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::StatusCode;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant as TokioInstant;

use super::auth::manager::{GrokAuthError, GrokAuthErrorKind, GrokAuthManager};
use super::auth::token_store::{DefaultGrokAuthStore, StoredAuth, file_store};
use super::translate::request::GrokResponsesRequest;
use crate::retry::{
    MAX_RATE_LIMIT_RETRIES, ModelRetryBackoff, ReplaySafety, should_retry_status, sleep,
};
use crate::traffic::TrafficCapture;

const DEFAULT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REJECTED_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_REJECTED_DETAIL_BYTES: usize = 1_024;
const MAX_WIRE_ATTEMPTS: u32 = MAX_RATE_LIMIT_RETRIES + 1;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_HEADER_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_FIRST_BYTE_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_BODY_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_TOTAL_TIMEOUT_MS: u64 = 540_000;
pub const DEFAULT_STREAM_HEARTBEAT_MS: u64 = 5_000;
const MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

pub type GrokByteStream =
    Pin<Box<dyn Stream<Item = Result<bytes::Bytes, GrokError>> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokErrorOrigin {
    Auth,
    Serialization,
    Http,
    RequestTransport,
    StreamTransport,
    ResponseLimit,
    Deadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokErrorStage {
    Auth,
    Serialize,
    Connect,
    Header,
    Status,
    Body,
    Stream,
}

#[derive(Debug, Clone)]
pub struct GrokError {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub message: String,
    pub origin: GrokErrorOrigin,
    pub stage: GrokErrorStage,
    pub retryable: bool,
    pub replay_safety: ReplaySafety,
}

impl GrokError {
    pub fn auth(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            retry_after: None,
            message: message.into(),
            origin: GrokErrorOrigin::Auth,
            stage: GrokErrorStage::Auth,
            retryable: false,
            replay_safety: ReplaySafety::OutcomeUnknown,
        }
    }

    pub fn http(
        status: StatusCode,
        retry_after: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            retry_after,
            message: message.into(),
            origin: GrokErrorOrigin::Http,
            stage: GrokErrorStage::Status,
            retryable: is_transient_http_status(status),
            replay_safety: ReplaySafety::ExplicitlyRetryableResponse,
        }
    }

    fn serialization(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            retry_after: None,
            message: message.into(),
            origin: GrokErrorOrigin::Serialization,
            stage: GrokErrorStage::Serialize,
            retryable: false,
            replay_safety: ReplaySafety::DefinitelyNotDispatched,
        }
    }

    pub fn upstream_event(
        status: StatusCode,
        retry_after: Option<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            retry_after,
            message: message.into(),
            origin: GrokErrorOrigin::Http,
            stage: GrokErrorStage::Stream,
            retryable,
            replay_safety: ReplaySafety::ExplicitlyRetryableResponse,
        }
    }

    fn auth_temporary(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: None,
            message: message.into(),
            origin: GrokErrorOrigin::Auth,
            stage: GrokErrorStage::Auth,
            retryable: true,
            replay_safety: ReplaySafety::OutcomeUnknown,
        }
    }

    fn auth_rate_limited(retry_after: Option<String>, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            retry_after,
            message: message.into(),
            origin: GrokErrorOrigin::Auth,
            stage: GrokErrorStage::Auth,
            retryable: true,
            replay_safety: ReplaySafety::ExplicitlyRetryableResponse,
        }
    }

    pub fn request_transport(
        stage: GrokErrorStage,
        message: impl Into<String>,
        replay_safety: ReplaySafety,
    ) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            message: message.into(),
            origin: GrokErrorOrigin::RequestTransport,
            stage,
            retryable: true,
            replay_safety,
        }
    }

    pub fn stream_transport(stage: GrokErrorStage, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            message: message.into(),
            origin: GrokErrorOrigin::StreamTransport,
            stage,
            retryable: true,
            replay_safety: ReplaySafety::OutcomeUnknown,
        }
    }

    pub fn response_limit(stage: GrokErrorStage, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            retry_after: None,
            message: message.into(),
            origin: GrokErrorOrigin::ResponseLimit,
            stage,
            retryable: false,
            replay_safety: ReplaySafety::OutcomeUnknown,
        }
    }

    pub fn deadline_exceeded(stage: GrokErrorStage) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            retry_after: None,
            message: "Grok request exceeded its total wall-clock timeout".into(),
            origin: GrokErrorOrigin::Deadline,
            stage,
            retryable: false,
            replay_safety: ReplaySafety::OutcomeUnknown,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn permits_model_replay(&self) -> bool {
        self.retryable && self.replay_safety.permits_model_replay()
    }

    pub fn into_terminal(mut self, reason: &str) -> Self {
        self.retryable = false;
        self.message = format!("{} ({reason})", self.message);
        self
    }

    fn retry_budget_exhausted() -> Self {
        Self::request_transport(
            GrokErrorStage::Header,
            "Grok retry budget was already exhausted",
            ReplaySafety::OutcomeUnknown,
        )
        .into_terminal("retry exhausted")
    }
}

/// Immutable JSON payload shared by every physical attempt for one logical Grok request.
///
/// The serialized bytes are cheap to clone and `capture_value` is populated only when full
/// traffic capture is enabled, keeping the normal request path free of an extra JSON parse/value.
#[derive(Debug)]
pub struct PreparedGrokRequest {
    body: Bytes,
    capture_value: Option<Value>,
}

impl PreparedGrokRequest {
    pub fn new(body: &GrokResponsesRequest, capture_body: bool) -> Result<Self, GrokError> {
        Self::from_serializable(body, capture_body)
    }

    fn from_serializable<T: Serialize>(body: &T, capture_body: bool) -> Result<Self, GrokError> {
        let body = serde_json::to_vec(body).map(Bytes::from).map_err(|error| {
            GrokError::serialization(format!("Failed to serialize Grok request: {error}"))
        })?;
        let capture_value = capture_body
            .then(|| {
                serde_json::from_slice(&body).map_err(|error| {
                    GrokError::serialization(format!(
                        "Failed to prepare Grok traffic capture: {error}"
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            body,
            capture_value,
        })
    }

    fn clone_body(&self) -> Bytes {
        self.body.clone()
    }

    /// Cheap hot-path fallback for observability when upstream usage is absent.
    /// Exact local counting is reserved for `/count_tokens`; model requests must
    /// not wait for tokenizer work before exposing an already-started response.
    pub(super) fn approximate_input_tokens(&self) -> u64 {
        u64::try_from(self.body.len())
            .unwrap_or(u64::MAX)
            .div_ceil(4)
            .max(1)
    }

    fn len(&self) -> usize {
        self.body.len()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GrokRequestDeadline {
    at: TokioInstant,
}

impl GrokRequestDeadline {
    pub fn configured() -> Self {
        Self::after(Duration::from_millis(crate::config::grok_total_timeout_ms(
            DEFAULT_TOTAL_TIMEOUT_MS,
        )))
    }

    pub fn after(timeout: Duration) -> Self {
        let timeout = timeout.min(MAX_TOTAL_TIMEOUT);
        let now = TokioInstant::now();
        Self {
            at: now
                .checked_add(timeout)
                .expect("the capped Grok request deadline must fit in Instant"),
        }
    }

    pub fn at(self) -> TokioInstant {
        self.at
    }

    pub fn is_expired(self) -> bool {
        TokioInstant::now() >= self.at
    }

    pub fn permits_wait_ms(self, wait_ms: u64) -> bool {
        Duration::from_millis(wait_ms) < self.at.saturating_duration_since(TokioInstant::now())
    }

    pub fn remaining_ms(self) -> u64 {
        self.at
            .saturating_duration_since(TokioInstant::now())
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GrokTimeouts {
    pub connect_ms: u64,
    pub header_ms: u64,
    pub first_byte_ms: u64,
    pub body_idle_ms: u64,
}

impl GrokTimeouts {
    pub fn configured() -> Self {
        Self {
            connect_ms: crate::config::grok_connect_timeout_ms(DEFAULT_CONNECT_TIMEOUT_MS),
            header_ms: crate::config::grok_header_timeout_ms(DEFAULT_HEADER_TIMEOUT_MS),
            first_byte_ms: crate::config::grok_first_byte_timeout_ms(DEFAULT_FIRST_BYTE_TIMEOUT_MS),
            body_idle_ms: crate::config::grok_body_idle_timeout_ms(DEFAULT_BODY_IDLE_TIMEOUT_MS),
        }
    }
}

impl Default for GrokTimeouts {
    fn default() -> Self {
        Self {
            connect_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            header_ms: DEFAULT_HEADER_TIMEOUT_MS,
            first_byte_ms: DEFAULT_FIRST_BYTE_TIMEOUT_MS,
            body_idle_ms: DEFAULT_BODY_IDLE_TIMEOUT_MS,
        }
    }
}

#[derive(Debug)]
pub struct GrokRetryState {
    wire_attempt: u32,
    transient_failures: u32,
    auth_refresh_succeeded: bool,
    terminal: bool,
    request_gate: Arc<Mutex<()>>,
    deadline: GrokRequestDeadline,
    req_id: Option<String>,
    model_retry_backoff: ModelRetryBackoff,
}

impl Default for GrokRetryState {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokRetryState {
    pub fn new() -> Self {
        Self::with_deadline(GrokRequestDeadline::after(Duration::from_millis(
            DEFAULT_TOTAL_TIMEOUT_MS,
        )))
    }

    pub fn configured() -> Self {
        Self::with_deadline(GrokRequestDeadline::configured())
    }

    pub fn with_deadline(deadline: GrokRequestDeadline) -> Self {
        Self::with_optional_req_id(deadline, None)
    }

    pub fn with_deadline_and_req_id(deadline: GrokRequestDeadline, req_id: String) -> Self {
        Self::with_optional_req_id(deadline, Some(req_id))
    }

    fn with_optional_req_id(deadline: GrokRequestDeadline, req_id: Option<String>) -> Self {
        Self {
            wire_attempt: 0,
            transient_failures: 0,
            auth_refresh_succeeded: false,
            terminal: false,
            request_gate: Arc::new(Mutex::new(())),
            deadline,
            req_id,
            model_retry_backoff: ModelRetryBackoff::default(),
        }
    }

    pub fn deadline(&self) -> GrokRequestDeadline {
        self.deadline
    }

    pub fn transient_failures(&self) -> u32 {
        self.transient_failures
    }

    pub fn wire_attempt(&self) -> u8 {
        self.wire_attempt.min(u8::MAX as u32) as u8
    }

    pub(super) fn log_context(&self) -> GrokRetryLogContext {
        GrokRetryLogContext {
            req_id: self.req_id.clone(),
            deadline_remaining_ms: self.deadline.remaining_ms(),
        }
    }

    /// Reserve one transient retry and decide whether its delay fits both retry policy and the
    /// request's absolute deadline. All Grok auth/model/body/rebuild paths use this single rule.
    pub fn schedule_retry(
        &mut self,
        retry_after: Option<&str>,
        deadline: GrokRequestDeadline,
    ) -> Result<u64, GrokRetryStop> {
        if !self.can_retry_transient() {
            self.mark_terminal();
            return Err(GrokRetryStop::RETRY_EXHAUSTED);
        }
        let delay = crate::retry::compute_backoff_delay(self.transient_failures, retry_after);
        self.note_transient_failure();
        if delay.exceeds_budget {
            self.mark_terminal();
            return Err(GrokRetryStop::DELAY_EXCEEDS_BUDGET);
        }
        if !deadline.permits_wait_ms(delay.wait_ms) {
            self.mark_terminal();
            return Err(GrokRetryStop::DELAY_EXCEEDS_DEADLINE);
        }
        Ok(delay.wait_ms)
    }

    /// Schedule a model replay using its proven dispatch outcome. Unlike auth
    /// retries, an unknown POST outcome is terminal and the first proven
    /// pre-dispatch transport failure gets one short recovery delay.
    pub fn schedule_model_retry(
        &mut self,
        replay_safety: ReplaySafety,
        retry_after: Option<&str>,
        deadline: GrokRequestDeadline,
    ) -> Result<u64, GrokRetryStop> {
        if !replay_safety.permits_model_replay() {
            self.mark_terminal();
            return Err(GrokRetryStop::OUTCOME_UNKNOWN);
        }
        if !self.can_retry_transient() {
            self.mark_terminal();
            return Err(GrokRetryStop::RETRY_EXHAUSTED);
        }
        let delay = self
            .model_retry_backoff
            .next_delay(replay_safety, retry_after)
            .expect("replay-safe model failure must have a retry delay");
        self.note_transient_failure();
        if delay.exceeds_budget {
            self.mark_terminal();
            return Err(GrokRetryStop::DELAY_EXCEEDS_BUDGET);
        }
        if !deadline.permits_wait_ms(delay.wait_ms) {
            self.mark_terminal();
            return Err(GrokRetryStop::DELAY_EXCEEDS_DEADLINE);
        }
        Ok(delay.wait_ms)
    }

    pub fn can_retry_transient(&self) -> bool {
        !self.terminal
            && self.transient_failures < MAX_RATE_LIMIT_RETRIES
            && self.wire_attempt < MAX_WIRE_ATTEMPTS
    }

    pub fn note_transient_failure(&mut self) {
        self.transient_failures = self.transient_failures.saturating_add(1);
    }

    pub fn mark_terminal(&mut self) {
        self.terminal = true;
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn reserve_wire_attempt(&mut self) -> Option<u8> {
        if self.terminal || self.wire_attempt >= MAX_WIRE_ATTEMPTS {
            self.terminal = true;
            return None;
        }
        self.wire_attempt = self.wire_attempt.saturating_add(1);
        Some(self.wire_attempt as u8)
    }

    fn has_wire_attempt_capacity(&self) -> bool {
        !self.terminal && self.wire_attempt < MAX_WIRE_ATTEMPTS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokRetryStop {
    pub failure_kind: &'static str,
    pub terminal_reason: &'static str,
}

impl GrokRetryStop {
    const OUTCOME_UNKNOWN: Self = Self {
        failure_kind: "outcome_unknown",
        terminal_reason: "model request outcome is unknown",
    };
    const RETRY_EXHAUSTED: Self = Self {
        failure_kind: "retry_exhausted",
        terminal_reason: "retry exhausted",
    };
    const DELAY_EXCEEDS_BUDGET: Self = Self {
        failure_kind: "retry_after_exceeds_budget",
        terminal_reason: "retry delay exceeds budget",
    };
    const DELAY_EXCEEDS_DEADLINE: Self = Self {
        failure_kind: "retry_delay_exceeds_deadline",
        terminal_reason: "retry delay exceeds remaining request deadline",
    };
}

#[derive(Debug, Clone)]
pub(super) struct GrokRetryLogContext {
    req_id: Option<String>,
    deadline_remaining_ms: u64,
}

pub struct GrokClient {
    client: Arc<reqwest::Client>,
    auth: Arc<GrokAuthManager<DefaultGrokAuthStore>>,
    url: String,
    client_version: String,
    timeouts: GrokTimeouts,
}

pub struct GrokResponse {
    response: reqwest::Response,
    timeouts: GrokTimeouts,
    retry: Arc<Mutex<GrokRetryState>>,
    deadline: GrokRequestDeadline,
}

impl GrokResponse {
    pub fn into_stream(self) -> GrokByteStream {
        timed_byte_stream(self.response, self.timeouts, self.retry, self.deadline)
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, GrokError> {
        let mut stream = timed_byte_stream(self.response, self.timeouts, self.retry, self.deadline);
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(GrokError::response_limit(
                    GrokErrorStage::Body,
                    "Grok upstream response exceeds the size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl GrokClient {
    pub fn new(base_url: String, client_version: String) -> anyhow::Result<Self> {
        let timeouts = GrokTimeouts::configured();
        let mut client_builder =
            crate::upstream_http::model_client_builder(Duration::from_millis(timeouts.connect_ms));
        if crate::oauth_http::is_loopback_url(&base_url) {
            client_builder = client_builder.no_proxy();
        }
        let client = Arc::new(client_builder.build()?);
        let auth = Arc::new(GrokAuthManager::new(file_store())?);
        Ok(Self::with_shared(
            url_for(base_url)?,
            client_version,
            client,
            auth,
            timeouts,
        ))
    }

    #[cfg(test)]
    pub fn new_for_test(
        base_url: String,
        client_version: String,
        timeouts: GrokTimeouts,
        auth: GrokAuthManager<DefaultGrokAuthStore>,
    ) -> anyhow::Result<Self> {
        let client = Arc::new(
            crate::upstream_http::model_client_builder(Duration::from_millis(timeouts.connect_ms))
                // Unit tests use loopback fault servers and must not inherit the
                // host's macOS/system proxy configuration.
                .no_proxy()
                .build()?,
        );
        Ok(Self::with_shared(
            url_for(base_url)?,
            client_version,
            client,
            Arc::new(auth),
            timeouts,
        ))
    }

    fn with_shared(
        url: String,
        client_version: String,
        client: Arc<reqwest::Client>,
        auth: Arc<GrokAuthManager<DefaultGrokAuthStore>>,
        timeouts: GrokTimeouts,
    ) -> Self {
        Self {
            client,
            auth,
            url,
            client_version,
            timeouts,
        }
    }

    pub async fn post(
        &self,
        body: &GrokResponsesRequest,
        traffic: Option<Arc<TrafficCapture>>,
    ) -> Result<GrokResponse, GrokError> {
        let retry = Arc::new(Mutex::new(GrokRetryState::configured()));
        self.post_with_retry(body, traffic, retry).await
    }

    async fn authenticate_with_retry(
        &self,
        rejected_access: Option<&str>,
        traffic: Option<&TrafficCapture>,
        retry: Arc<Mutex<GrokRetryState>>,
        deadline: GrokRequestDeadline,
    ) -> Result<StoredAuth, GrokError> {
        let mut auth_attempt = 0_u8;
        loop {
            auth_attempt = auth_attempt.saturating_add(1);
            let result = if let Some(rejected_access) = rejected_access {
                run_before_deadline(
                    deadline,
                    retry.clone(),
                    GrokErrorStage::Auth,
                    self.auth.force_refresh(rejected_access),
                )
                .await?
                .map(|auth| (auth, true))
            } else {
                run_before_deadline(
                    deadline,
                    retry.clone(),
                    GrokErrorStage::Auth,
                    self.auth.get_auth_with_status(),
                )
                .await?
            };
            let auth_failure = match result {
                Ok((auth, refreshed)) => {
                    if refreshed {
                        retry.lock().await.auth_refresh_succeeded = true;
                    }
                    return Ok(auth);
                }
                Err(error) => error,
            };
            let safe_to_retry = auth_failure.safe_to_retry();
            let kind = auth_error_kind_name(auth_failure.kind());
            let error = auth_error(auth_failure);

            if !safe_to_retry {
                retry.lock().await.mark_terminal();
                capture_terminal_failure(
                    traffic,
                    "auth",
                    kind,
                    auth_attempt,
                    Some(error.status.as_u16()),
                    Some(&error.message),
                );
                return Err(error);
            }

            let mut state = retry.lock().await;
            let wait_ms = match state.schedule_retry(error.retry_after.as_deref(), deadline) {
                Ok(wait_ms) => wait_ms,
                Err(stop) => {
                    let exhausted = stop.failure_kind == "retry_exhausted";
                    let error = error.into_terminal(stop.terminal_reason);
                    let log_context = state.log_context();
                    drop(state);
                    capture_terminal_failure(
                        traffic,
                        "auth",
                        stop.failure_kind,
                        auth_attempt,
                        Some(error.status.as_u16()),
                        Some(&error.message),
                    );
                    if exhausted {
                        log_retry_exhausted(auth_attempt, &error, &log_context);
                    }
                    return Err(error);
                }
            };
            let transient = state.transient_failures;
            let log_context = state.log_context();
            drop(state);

            if let Some(capture) = traffic {
                capture.write_json(
                    "023-upstream-auth-retry",
                    &serde_json::json!({
                        "auth_attempt": auth_attempt,
                        "transient_failures": transient,
                        "wait_ms": wait_ms,
                        "status": error.status.as_u16(),
                        "origin": origin_name(error.origin),
                        "stage": stage_name(error.stage),
                        "message": error.message,
                    }),
                );
            }
            log_retry(auth_attempt, transient, wait_ms, &error, &log_context);
            run_before_deadline(
                deadline,
                retry.clone(),
                GrokErrorStage::Auth,
                sleep(wait_ms),
            )
            .await?;
        }
    }

    pub async fn post_with_retry(
        &self,
        body: &GrokResponsesRequest,
        traffic: Option<Arc<TrafficCapture>>,
        retry: Arc<Mutex<GrokRetryState>>,
    ) -> Result<GrokResponse, GrokError> {
        let prepared = PreparedGrokRequest::new(body, traffic.is_some())?;
        self.post_prepared_with_retry(&prepared, traffic, retry)
            .await
    }

    pub async fn post_prepared_with_retry(
        &self,
        body: &PreparedGrokRequest,
        traffic: Option<Arc<TrafficCapture>>,
        retry: Arc<Mutex<GrokRetryState>>,
    ) -> Result<GrokResponse, GrokError> {
        let (request_gate, deadline) = {
            let state = retry.lock().await;
            (state.request_gate.clone(), state.deadline())
        };
        let _request_guard = run_before_deadline(
            deadline,
            retry.clone(),
            GrokErrorStage::Auth,
            request_gate.lock(),
        )
        .await?;
        if retry.lock().await.is_terminal() {
            return Err(GrokError::retry_budget_exhausted());
        }
        if let Some(capture) = traffic.as_ref() {
            let parsed_body;
            let body_value = if let Some(value) = body.capture_value.as_ref() {
                value
            } else {
                parsed_body = serde_json::from_slice(&body.body).map_err(|error| {
                    GrokError::serialization(format!(
                        "Failed to prepare Grok traffic capture: {error}"
                    ))
                })?;
                &parsed_body
            };
            capture.write_json("020-upstream-request", body_value);
            capture.write_json(
                "021-upstream-request-metadata",
                &serde_json::json!({
                    "method": "POST",
                    "url": safe_url(&self.url),
                    "provider": "grok",
                    "transport": "http",
                    "headers": {
                        "accept":"text/event-stream",
                        "content-type":"application/json",
                        "authorization":"[redacted]",
                        "x-xai-token-auth":"[redacted]"
                    },
                    "body_bytes": body.len(),
                }),
            );
        }

        let mut auth = self
            .authenticate_with_retry(None, traffic.as_deref(), retry.clone(), deadline)
            .await?;

        loop {
            let attempt = {
                let mut state = retry.lock().await;
                state.reserve_wire_attempt()
            };
            let Some(attempt) = attempt else {
                return Err(GrokError::retry_budget_exhausted());
            };
            let outcome = self
                .attempt(
                    &auth.access,
                    body,
                    attempt,
                    traffic.as_deref(),
                    deadline,
                    retry.clone(),
                )
                .await;

            match outcome {
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    let mut state = retry.lock().await;
                    if state.auth_refresh_succeeded {
                        state.mark_terminal();
                        drop(state);
                        capture_terminal_failure(
                            traffic.as_deref(),
                            "auth",
                            "unauthorized",
                            attempt,
                            Some(401),
                            None,
                        );
                        return Err(auth_error(GrokAuthError::credentials_invalid(
                            "the refreshed access token was rejected",
                        )));
                    }
                    if !state.has_wire_attempt_capacity() {
                        state.mark_terminal();
                        drop(state);
                        let error = auth_error(GrokAuthError::credentials_invalid(
                            "the access token was rejected and no model retry attempts remain",
                        ));
                        capture_terminal_failure(
                            traffic.as_deref(),
                            "auth",
                            "unauthorized_retry_exhausted",
                            attempt,
                            Some(401),
                            Some(&error.message),
                        );
                        return Err(error);
                    }
                    drop(state);
                    if let Some(capture) = traffic.as_ref() {
                        capture.write_json(
                            "023-upstream-auth-refresh",
                            &serde_json::json!({
                                "attempt": attempt,
                                "status": 401,
                                "origin": "auth",
                                "stage": "unauthorized_refresh",
                            }),
                        );
                    }
                    auth = self
                        .authenticate_with_retry(
                            Some(&auth.access),
                            traffic.as_deref(),
                            retry.clone(),
                            deadline,
                        )
                        .await?;
                    retry.lock().await.auth_refresh_succeeded = true;
                    continue;
                }
                Ok(response) => {
                    return Ok(GrokResponse {
                        response,
                        timeouts: self.timeouts,
                        retry,
                        deadline,
                    });
                }
                Err(error) if error.origin == GrokErrorOrigin::Auth => {
                    capture_terminal_failure(
                        traffic.as_deref(),
                        stage_name(error.stage),
                        "auth",
                        attempt,
                        Some(error.status.as_u16()),
                        Some(&error.message),
                    );
                    return Err(error);
                }
                Err(error) if error.permits_model_replay() => {
                    let mut state = retry.lock().await;
                    let wait_ms = match state.schedule_model_retry(
                        error.replay_safety,
                        error.retry_after.as_deref(),
                        deadline,
                    ) {
                        Ok(wait_ms) => wait_ms,
                        Err(stop) => {
                            let exhausted = stop.failure_kind == "retry_exhausted";
                            let error = error.into_terminal(stop.terminal_reason);
                            let log_context = state.log_context();
                            drop(state);
                            capture_terminal_failure(
                                traffic.as_deref(),
                                stage_name(error.stage),
                                stop.failure_kind,
                                attempt,
                                Some(error.status.as_u16()),
                                Some(&error.message),
                            );
                            if exhausted {
                                log_retry_exhausted(attempt, &error, &log_context);
                            }
                            return Err(error);
                        }
                    };
                    let transient = state.transient_failures;
                    let log_context = state.log_context();
                    drop(state);
                    if let Some(capture) = traffic.as_ref() {
                        capture.write_json(
                            "023-upstream-retry",
                            &serde_json::json!({
                                "attempt": attempt,
                                "transient_failures": transient,
                                "wait_ms": wait_ms,
                                "status": error.status.as_u16(),
                                "origin": origin_name(error.origin),
                                "stage": stage_name(error.stage),
                                "replay_safety": error.replay_safety.as_str(),
                                "deadline_remaining_ms": log_context.deadline_remaining_ms,
                                "message": error.message,
                            }),
                        );
                    }
                    log_retry(attempt, transient, wait_ms, &error, &log_context);
                    run_before_deadline(deadline, retry.clone(), error.stage, sleep(wait_ms))
                        .await?;
                    continue;
                }
                Err(error) => {
                    retry.lock().await.mark_terminal();
                    capture_terminal_failure(
                        traffic.as_deref(),
                        stage_name(error.stage),
                        "final",
                        attempt,
                        Some(error.status.as_u16()),
                        Some(&error.message),
                    );
                    return Err(error);
                }
            }
        }
    }

    async fn attempt(
        &self,
        access: &str,
        body: &PreparedGrokRequest,
        attempt: u8,
        traffic: Option<&TrafficCapture>,
        deadline: GrokRequestDeadline,
        retry: Arc<Mutex<GrokRetryState>>,
    ) -> Result<reqwest::Response, GrokError> {
        let started = Instant::now();
        let send_fut = self
            .client
            .post(&self.url)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {access}"))
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-grok-client-identifier", "grok-shell")
            .header("x-grok-client-version", &self.client_version)
            .body(body.clone_body())
            .send();

        let response = match run_before_deadline(
            deadline,
            retry.clone(),
            GrokErrorStage::Header,
            tokio::time::timeout(Duration::from_millis(self.timeouts.header_ms), send_fut),
        )
        .await?
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let stage = if error.is_connect() {
                    GrokErrorStage::Connect
                } else {
                    GrokErrorStage::Header
                };
                let message = format!("Grok upstream request failed: {error}");
                capture_attempt(
                    traffic,
                    attempt,
                    None,
                    origin_name(GrokErrorOrigin::RequestTransport),
                    stage_name(stage),
                    Some(&message),
                );
                let replay_safety = if error.is_connect() {
                    ReplaySafety::DefinitelyNotDispatched
                } else {
                    ReplaySafety::OutcomeUnknown
                };
                return Err(GrokError::request_transport(stage, message, replay_safety));
            }
            Err(_) => {
                let message = format!(
                    "Timed out waiting {}ms for Grok response headers",
                    self.timeouts.header_ms
                );
                capture_attempt(
                    traffic,
                    attempt,
                    None,
                    origin_name(GrokErrorOrigin::RequestTransport),
                    "header",
                    Some(&message),
                );
                return Err(GrokError::request_transport(
                    GrokErrorStage::Header,
                    message,
                    ReplaySafety::OutcomeUnknown,
                ));
            }
        };

        let status = response.status();
        if let Some(capture) = traffic {
            capture.write_json(
                "022-upstream-attempt",
                &serde_json::json!({
                    "attempt": attempt,
                    "status": status.as_u16(),
                    "elapsed_ms": started.elapsed().as_millis(),
                    "headers": safe_headers(response.headers()),
                }),
            );
        }

        if status == StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let (body, truncated, timed_out) = read_rejected_body(
                response,
                MAX_REJECTED_RESPONSE_BYTES,
                self.timeouts,
                deadline,
                retry,
            )
            .await?;
            if let Some(capture) = traffic {
                let detail = serde_json::from_slice::<serde_json::Value>(&body)
                    .unwrap_or_else(|_| serde_json::json!({"body_bytes": body.len()}));
                capture.write_json(
                    "031-upstream-error-body",
                    &serde_json::json!({
                        "attempt": attempt,
                        "status": status.as_u16(),
                        "truncated": truncated,
                        "timed_out": timed_out,
                        "body": detail
                    }),
                );
            }
            return Err(GrokError::http(
                status,
                retry_after,
                rejected_response_message(status, &body, truncated, timed_out),
            ));
        }

        if status == StatusCode::NO_CONTENT {
            let message = "Grok upstream returned an empty 204 response";
            capture_attempt(
                traffic,
                attempt,
                Some(status.as_u16()),
                origin_name(GrokErrorOrigin::RequestTransport),
                stage_name(GrokErrorStage::Header),
                Some(message),
            );
            return Err(GrokError::request_transport(
                GrokErrorStage::Header,
                message,
                ReplaySafety::OutcomeUnknown,
            ));
        }

        if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE)
            && !is_event_stream_content_type(content_type)
        {
            let content_type = content_type.to_str().unwrap_or("<invalid>");
            let message =
                format!("Grok upstream returned a non-SSE success response ({content_type})");
            capture_attempt(
                traffic,
                attempt,
                Some(status.as_u16()),
                origin_name(GrokErrorOrigin::RequestTransport),
                stage_name(GrokErrorStage::Header),
                Some(&message),
            );
            return Err(GrokError::request_transport(
                GrokErrorStage::Header,
                message,
                ReplaySafety::OutcomeUnknown,
            ));
        }

        if let Some(capture) = traffic {
            capture.write_json(
                "030-upstream-response-headers",
                &serde_json::json!({
                    "status": status.as_u16(),
                    "headers": safe_headers(response.headers()),
                }),
            );
        }
        Ok(response)
    }
}

fn is_transient_http_status(status: StatusCode) -> bool {
    should_retry_status(status.as_u16()) || status.as_u16() == 529
}

fn is_event_stream_content_type(value: &reqwest::header::HeaderValue) -> bool {
    value
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

pub(super) async fn run_before_deadline<F>(
    deadline: GrokRequestDeadline,
    retry: Arc<Mutex<GrokRetryState>>,
    stage: GrokErrorStage,
    future: F,
) -> Result<F::Output, GrokError>
where
    F: Future,
{
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline.at()) => {
            retry.lock().await.mark_terminal();
            Err(GrokError::deadline_exceeded(stage))
        }
        output = future => Ok(output),
    }
}

fn timed_byte_stream(
    response: reqwest::Response,
    timeouts: GrokTimeouts,
    retry: Arc<Mutex<GrokRetryState>>,
    deadline: GrokRequestDeadline,
) -> GrokByteStream {
    let stream = futures_util::stream::unfold(
        (response, timeouts, retry, deadline, true),
        |(mut response, timeouts, retry, deadline, first)| async move {
            let wait = if first {
                timeouts.first_byte_ms
            } else {
                timeouts.body_idle_ms
            };
            let next = run_before_deadline(
                deadline,
                retry.clone(),
                if first {
                    GrokErrorStage::Body
                } else {
                    GrokErrorStage::Stream
                },
                tokio::time::timeout(Duration::from_millis(wait), response.chunk()),
            )
            .await;
            match next {
                Err(error) => Some((Err(error), (response, timeouts, retry, deadline, false))),
                Ok(Ok(Ok(Some(chunk)))) => {
                    Some((Ok(chunk), (response, timeouts, retry, deadline, false)))
                }
                Ok(Ok(Ok(None))) => None,
                Ok(Ok(Err(error))) => Some((
                    Err(GrokError::stream_transport(
                        GrokErrorStage::Stream,
                        format!("Grok upstream stream failed: {error}"),
                    )),
                    (response, timeouts, retry, deadline, false),
                )),
                Ok(Err(_)) => {
                    let stage = if first {
                        GrokErrorStage::Body
                    } else {
                        GrokErrorStage::Stream
                    };
                    let message = if first {
                        format!(
                            "Timed out waiting {}ms for the first Grok response body byte",
                            timeouts.first_byte_ms
                        )
                    } else {
                        format!(
                            "Timed out waiting {}ms for the next Grok response body chunk",
                            timeouts.body_idle_ms
                        )
                    };
                    Some((
                        Err(GrokError::stream_transport(stage, message)),
                        (response, timeouts, retry, deadline, false),
                    ))
                }
            }
        },
    );
    Box::pin(stream)
}

async fn read_rejected_body(
    response: reqwest::Response,
    limit: usize,
    timeouts: GrokTimeouts,
    deadline: GrokRequestDeadline,
    retry: Arc<Mutex<GrokRetryState>>,
) -> Result<(Vec<u8>, bool, bool), GrokError> {
    let mut body = Vec::new();
    let mut first = true;
    let mut response = response;
    loop {
        let wait = if first {
            timeouts.first_byte_ms
        } else {
            timeouts.body_idle_ms
        };
        let next = run_before_deadline(
            deadline,
            retry.clone(),
            GrokErrorStage::Body,
            tokio::time::timeout(Duration::from_millis(wait), response.chunk()),
        )
        .await?;
        match next {
            Ok(Ok(Some(chunk))) => {
                first = false;
                let remaining = limit.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    return Ok((body, true, false));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => return Ok((body, false, false)),
            Ok(Err(_)) => {
                let truncated = !body.is_empty() && body.len() >= limit;
                return Ok((body, truncated, false));
            }
            Err(_) => {
                let truncated = body.len() >= limit;
                return Ok((body, truncated, true));
            }
        }
    }
}

fn rejected_response_message(
    status: StatusCode,
    body: &[u8],
    truncated: bool,
    timed_out: bool,
) -> String {
    let mut message = format!("Grok upstream returned {status}");
    if let Some(detail) = rejected_response_detail(body) {
        message.push_str(": ");
        message.push_str(&detail);
    }
    if truncated {
        message.push_str(" [error detail truncated]");
    }
    if timed_out {
        message.push_str(" [error detail timed out]");
    }
    message
}

fn rejected_response_detail(body: &[u8]) -> Option<String> {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let (message, code) = if let Some(value) = parsed.as_ref() {
        let message = value
            .pointer("/error/message")
            .or_else(|| value.pointer("/error/detail"))
            .or_else(|| value.get("message"))
            .or_else(|| value.get("detail"))
            .or_else(|| value.get("error").filter(|error| error.is_string()))
            .and_then(Value::as_str);
        let code = value
            .pointer("/error/code")
            .or_else(|| value.get("code"))
            .and_then(|code| match code {
                Value::String(code) => Some(code.clone()),
                Value::Number(code) => Some(code.to_string()),
                _ => None,
            });
        (message.map(str::to_string), code)
    } else {
        (std::str::from_utf8(body).ok().map(str::to_string), None)
    };

    let message = message.as_deref().and_then(sanitize_rejected_detail);
    let code = code.as_deref().and_then(sanitize_rejected_detail);
    match (message, code) {
        (Some(message), Some(code)) if !message.contains(&code) => {
            Some(format!("{message} (code: {code})"))
        }
        (Some(message), _) => Some(message),
        (None, Some(code)) => Some(format!("upstream error code: {code}")),
        (None, None) => None,
    }
}

fn sanitize_rejected_detail(value: &str) -> Option<String> {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let lower = collapsed.to_ascii_lowercase();
    if [
        "authorization:",
        "bearer ",
        "access_token",
        "refresh_token",
        "api_key",
        "client_secret",
        "password=",
        "password:",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return Some("[redacted upstream error detail]".to_string());
    }
    let (prefix, truncated) = super::text::truncate_utf8(&collapsed, MAX_REJECTED_DETAIL_BYTES);
    let suffix = if truncated { "…[truncated]" } else { "" };
    Some(format!("{prefix}{suffix}"))
}

pub(super) fn capture_terminal_failure(
    traffic: Option<&TrafficCapture>,
    stage: &str,
    kind: &str,
    attempt: u8,
    status: Option<u16>,
    message: Option<&str>,
) {
    if let Some(capture) = traffic {
        capture.write_json(
            "060-grok-stream-error",
            &serde_json::json!({
                "stage": stage,
                "kind": kind,
                "attempt": attempt,
                "status": status,
                "message": message,
            }),
        );
    }
}

fn capture_attempt(
    traffic: Option<&TrafficCapture>,
    attempt: u8,
    status: Option<u16>,
    origin: &str,
    stage: &str,
    message: Option<&str>,
) {
    if let Some(capture) = traffic {
        capture.write_json(
            "022-upstream-attempt",
            &serde_json::json!({
                "attempt": attempt,
                "status": status,
                "origin": origin,
                "stage": stage,
                "message": message,
            }),
        );
    }
}

pub(super) fn log_retry(
    attempt: u8,
    transient_failures: u32,
    wait_ms: u64,
    error: &GrokError,
    context: &GrokRetryLogContext,
) {
    crate::logging::create_logger("grok").info(
        "upstream_retry",
        Some(serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(context.req_id)),
            ("attempt".into(), serde_json::json!(attempt)),
            (
                "transientFailures".into(),
                serde_json::json!(transient_failures),
            ),
            ("waitMs".into(), serde_json::json!(wait_ms)),
            (
                "replaySafety".into(),
                serde_json::json!(error.replay_safety.as_str()),
            ),
            (
                "deadlineRemainingMs".into(),
                serde_json::json!(context.deadline_remaining_ms),
            ),
            ("status".into(), serde_json::json!(error.status.as_u16())),
            (
                "origin".into(),
                serde_json::json!(origin_name(error.origin)),
            ),
            ("stage".into(), serde_json::json!(stage_name(error.stage))),
            ("message".into(), serde_json::json!(error.message)),
        ])),
    );
}

fn log_retry_exhausted(attempt: u8, error: &GrokError, context: &GrokRetryLogContext) {
    crate::logging::create_logger("grok").info(
        "upstream_retry_exhausted",
        Some(serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(context.req_id)),
            ("attempt".into(), serde_json::json!(attempt)),
            (
                "replaySafety".into(),
                serde_json::json!(error.replay_safety.as_str()),
            ),
            (
                "deadlineRemainingMs".into(),
                serde_json::json!(context.deadline_remaining_ms),
            ),
            ("status".into(), serde_json::json!(error.status.as_u16())),
            (
                "origin".into(),
                serde_json::json!(origin_name(error.origin)),
            ),
            ("stage".into(), serde_json::json!(stage_name(error.stage))),
            ("message".into(), serde_json::json!(error.message)),
        ])),
    );
}

pub(super) fn origin_name(origin: GrokErrorOrigin) -> &'static str {
    match origin {
        GrokErrorOrigin::Auth => "auth",
        GrokErrorOrigin::Serialization => "serialization",
        GrokErrorOrigin::Http => "http",
        GrokErrorOrigin::RequestTransport => "request_transport",
        GrokErrorOrigin::StreamTransport => "stream_transport",
        GrokErrorOrigin::ResponseLimit => "response_limit",
        GrokErrorOrigin::Deadline => "deadline",
    }
}

pub(super) fn stage_name(stage: GrokErrorStage) -> &'static str {
    match stage {
        GrokErrorStage::Auth => "auth",
        GrokErrorStage::Serialize => "serialize",
        GrokErrorStage::Connect => "connect",
        GrokErrorStage::Header => "header",
        GrokErrorStage::Status => "status",
        GrokErrorStage::Body => "body",
        GrokErrorStage::Stream => "stream",
    }
}

fn safe_headers(headers: &reqwest::header::HeaderMap) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    for name in [
        "content-type",
        "content-length",
        "retry-after",
        "x-request-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            result.insert(
                name.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    serde_json::Value::Object(result)
}

fn safe_url(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "[invalid-url]".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.to_string()
}

fn url_for(base_url: String) -> anyhow::Result<String> {
    responses_url(&base_url)
}

fn responses_url(base_url: &str) -> anyhow::Result<String> {
    let base_url = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url.trim()
    };
    let mut url = reqwest::Url::parse(base_url)?;
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        url.set_path(&format!("{path}/responses"));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn auth_error(error: GrokAuthError) -> GrokError {
    let kind = error.kind();
    let safe_to_retry = error.safe_to_retry();
    let retry_after = error.retry_after().map(str::to_string);
    let message = error.to_string();
    let error = match kind {
        GrokAuthErrorKind::CredentialsInvalid => GrokError::auth(format!(
            "{message}. Run `claude-code-proxy grok auth login` to re-authenticate"
        )),
        GrokAuthErrorKind::Temporary => GrokError::auth_temporary(message),
        GrokAuthErrorKind::RateLimited => GrokError::auth_rate_limited(retry_after, message),
    };
    if safe_to_retry {
        error
    } else {
        error.into_terminal("unsafe authentication outcome")
    }
}

fn auth_error_kind_name(kind: GrokAuthErrorKind) -> &'static str {
    match kind {
        GrokAuthErrorKind::CredentialsInvalid => "credentials_invalid",
        GrokAuthErrorKind::Temporary => "auth_temporary",
        GrokAuthErrorKind::RateLimited => "auth_rate_limited",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_schedule_uses_one_terminal_policy_for_budget_and_deadline() {
        let mut over_budget =
            GrokRetryState::with_deadline(GrokRequestDeadline::after(Duration::from_secs(60)));
        let stop = over_budget
            .schedule_retry(Some("120"), over_budget.deadline())
            .unwrap_err();
        assert_eq!(stop.failure_kind, "retry_after_exceeds_budget");
        assert!(over_budget.is_terminal());
        assert_eq!(over_budget.transient_failures(), 1);

        let mut out_of_time =
            GrokRetryState::with_deadline(GrokRequestDeadline::after(Duration::ZERO));
        let stop = out_of_time
            .schedule_retry(Some("0"), out_of_time.deadline())
            .unwrap_err();
        assert_eq!(stop.failure_kind, "retry_delay_exceeds_deadline");
        assert!(out_of_time.is_terminal());
        assert_eq!(out_of_time.transient_failures(), 1);
    }

    #[test]
    fn model_retry_schedule_is_fast_once_and_rejects_unknown_outcomes() {
        let deadline = GrokRequestDeadline::after(Duration::from_secs(60));
        let mut retry = GrokRetryState::with_deadline(deadline);
        assert!(
            retry.req_id.is_none(),
            "test/default retry state has no reqId"
        );

        let fast = retry
            .schedule_model_retry(ReplaySafety::DefinitelyNotDispatched, None, deadline)
            .unwrap();
        assert!((100..=300).contains(&fast));

        let overload = retry
            .schedule_model_retry(ReplaySafety::ExplicitlyRetryableResponse, None, deadline)
            .unwrap();
        assert!((1_600..=2_400).contains(&overload));

        let stop = retry
            .schedule_model_retry(ReplaySafety::OutcomeUnknown, None, deadline)
            .unwrap_err();
        assert_eq!(stop.failure_kind, "outcome_unknown");
        assert!(retry.is_terminal());
    }

    #[test]
    fn production_retry_state_carries_request_id_for_logs() {
        let retry = GrokRetryState::with_deadline_and_req_id(
            GrokRequestDeadline::after(Duration::from_secs(60)),
            "req-observed".into(),
        );
        let context = retry.log_context();
        assert_eq!(context.req_id.as_deref(), Some("req-observed"));
        assert!(context.deadline_remaining_ms <= 60_000);
    }
    use crate::timeutil::now_ms;
    use crate::traffic::test_capture;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_auth() -> StoredAuth {
        StoredAuth {
            access: "test-access".into(),
            refresh: "test-refresh".into(),
            expires_at_ms: now_ms() + 3_600_000,
            issuer: super::super::auth::login::CANONICAL_ISSUER.into(),
            client_id: super::super::auth::login::CLIENT_ID.into(),
        }
    }

    async fn test_client(base_url: &str, timeouts: GrokTimeouts) -> (GrokClient, TempDir) {
        let temp = TempDir::new().unwrap();
        let primary = temp
            .path()
            .join("grok")
            .join("auth.json")
            .to_string_lossy()
            .into_owned();
        let legacy = temp
            .path()
            .join("legacy-grok")
            .join("auth.json")
            .to_string_lossy()
            .into_owned();
        let store = super::super::auth::token_store::test_file_store(primary, legacy);
        store.save_auth(test_auth()).unwrap();
        let auth = GrokAuthManager::new(store).unwrap();
        let client =
            GrokClient::new_for_test(base_url.to_string(), "test-version".into(), timeouts, auth)
                .unwrap();
        (client, temp)
    }

    async fn test_client_with_auth_issuer(
        base_url: &str,
        issuer: String,
        expires_at_ms: u64,
        timeouts: GrokTimeouts,
    ) -> (GrokClient, TempDir) {
        let temp = TempDir::new().unwrap();
        let primary = temp
            .path()
            .join("grok")
            .join("auth.json")
            .to_string_lossy()
            .into_owned();
        let legacy = temp
            .path()
            .join("legacy-grok")
            .join("auth.json")
            .to_string_lossy()
            .into_owned();
        let store = super::super::auth::token_store::test_file_store(primary, legacy);
        store
            .save_auth(StoredAuth {
                access: "expired-access".into(),
                refresh: "refresh-token".into(),
                expires_at_ms,
                issuer: issuer.clone(),
                client_id: super::super::auth::login::CLIENT_ID.into(),
            })
            .unwrap();
        let auth = GrokAuthManager::new_for_test(store, issuer).unwrap();
        let client =
            GrokClient::new_for_test(base_url.to_string(), "test-version".into(), timeouts, auth)
                .unwrap();
        (client, temp)
    }

    #[test]
    fn prepared_request_maps_serialization_failure_without_dispatch() {
        let error = PreparedGrokRequest::from_serializable(&FailingPayload, false).unwrap_err();

        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.origin, GrokErrorOrigin::Serialization);
        assert_eq!(error.stage, GrokErrorStage::Serialize);
        assert!(!error.is_retryable());
        assert_eq!(error.replay_safety, ReplaySafety::DefinitelyNotDispatched);
        assert!(!error.permits_model_replay());
        assert!(error.message.contains("intentional serialization failure"));
    }

    #[test]
    fn prepared_request_only_retains_capture_value_when_requested() {
        let normal = PreparedGrokRequest::new(&sample_body(), false).unwrap();
        let captured = PreparedGrokRequest::new(&sample_body(), true).unwrap();

        assert!(normal.capture_value.is_none());
        assert_eq!(
            captured.capture_value.as_ref().unwrap()["model"],
            "grok-4.5"
        );
    }

    fn sample_body() -> GrokResponsesRequest {
        GrokResponsesRequest {
            model: "grok-4.5".into(),
            instructions: None,
            input: vec![],
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            store: false,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            text: None,
        }
    }

    struct CountingPayload<'a> {
        serializations: &'a AtomicUsize,
    }

    impl Serialize for CountingPayload<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            self.serializations.fetch_add(1, Ordering::SeqCst);
            serde_json::json!({
                "model": "grok-4.5",
                "input": [],
                "store": false,
                "stream": true
            })
            .serialize(serializer)
        }
    }

    struct FailingPayload;

    impl Serialize for FailingPayload {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(<S::Error as serde::ser::Error>::custom(
                "intentional serialization failure",
            ))
        }
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut buf = vec![0_u8; 32 * 1024];
        let mut total = 0usize;
        loop {
            let n = stream.read(&mut buf[total..]).await.unwrap();
            assert!(n > 0, "expected HTTP request bytes");
            total += n;
            if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if total == buf.len() {
                buf.resize(buf.len() * 2, 0);
            }
        }
        buf.truncate(total);
        buf
    }

    async fn read_http_request_body(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = read_http_request(stream).await;
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("HTTP headers must terminate");
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("prepared request must have content-length");
        let mut body_bytes_read = request.len().saturating_sub(header_end).min(content_length);
        request.resize(header_end + content_length, 0);
        while body_bytes_read < content_length {
            let read = stream
                .read(&mut request[header_end + body_bytes_read..])
                .await
                .unwrap();
            assert!(read > 0, "expected complete HTTP request body");
            body_bytes_read += read;
        }
        request[header_end..header_end + content_length].to_vec()
    }

    async fn write_http_response(
        stream: &mut tokio::net::TcpStream,
        status: &str,
        extra_headers: &str,
        body: &[u8],
    ) {
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }

    #[test]
    fn responses_url_appends_responses_to_base_path() {
        assert_eq!(
            responses_url("http://127.0.0.1:8080/v1").unwrap(),
            "http://127.0.0.1:8080/v1/responses"
        );
    }

    #[test]
    fn responses_url_preserves_responses_endpoint() {
        assert_eq!(
            responses_url("https://example.com/custom/responses/").unwrap(),
            "https://example.com/custom/responses"
        );
    }

    #[test]
    fn responses_url_rejects_invalid_url() {
        assert!(responses_url(":invalid").is_err());
    }

    #[test]
    fn configured_timeouts_and_heartbeat_match_defaults() {
        let timeouts = GrokTimeouts::default();
        assert_eq!(timeouts.connect_ms, 10_000);
        assert_eq!(timeouts.header_ms, 60_000);
        assert_eq!(timeouts.first_byte_ms, 60_000);
        assert_eq!(timeouts.body_idle_ms, 300_000);
        assert_eq!(DEFAULT_STREAM_HEARTBEAT_MS, 5_000);
    }

    #[test]
    fn rejected_detail_is_bounded_and_redacts_inline_credentials() {
        assert_eq!(
            sanitize_rejected_detail("invalid request: Bearer top-secret").as_deref(),
            Some("[redacted upstream error detail]")
        );

        let detail = format!("{}😊", "x".repeat(MAX_REJECTED_DETAIL_BYTES));
        let sanitized = sanitize_rejected_detail(&detail).unwrap();
        assert!(sanitized.ends_with("…[truncated]"));
        assert!(sanitized.is_char_boundary(sanitized.len()));
    }

    #[test]
    fn rate_limit_error_keeps_status_retry_after_and_classification() {
        let error = GrokError::http(
            StatusCode::TOO_MANY_REQUESTS,
            Some("15".into()),
            "rate limited",
        );
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.retry_after.as_deref(), Some("15"));
        assert!(error.is_retryable());
        assert_eq!(error.origin, GrokErrorOrigin::Http);
        assert_eq!(error.stage, GrokErrorStage::Status);
    }

    #[test]
    fn overloaded_529_is_retryable() {
        let status = StatusCode::from_u16(529).unwrap();
        let error = GrokError::http(status, Some("0".into()), "overloaded");

        assert_eq!(error.status.as_u16(), 529);
        assert!(error.is_retryable());
    }

    #[test]
    fn event_stream_content_type_accepts_parameters_and_rejects_json() {
        assert!(is_event_stream_content_type(
            &reqwest::header::HeaderValue::from_static("text/event-stream; charset=utf-8")
        ));
        assert!(is_event_stream_content_type(
            &reqwest::header::HeaderValue::from_static("TEXT/EVENT-STREAM")
        ));
        assert!(!is_event_stream_content_type(
            &reqwest::header::HeaderValue::from_static("application/json")
        ));
        assert!(!is_event_stream_content_type(
            &reqwest::header::HeaderValue::from_static("text/html")
        ));
    }

    #[tokio::test]
    async fn explicit_non_sse_success_is_retryable_but_not_replayable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 15\r\nconnection: close\r\n\r\nproxy login page",
                )
                .await
                .unwrap();
        });
        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let deadline = GrokRequestDeadline::after(Duration::from_secs(2));
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(deadline)));
        let prepared = PreparedGrokRequest::new(&sample_body(), false).unwrap();

        let error = client
            .attempt("test-access", &prepared, 1, None, deadline, retry)
            .await
            .unwrap_err();
        server.await.unwrap();

        assert_eq!(error.origin, GrokErrorOrigin::RequestTransport);
        assert_eq!(error.stage, GrokErrorStage::Header);
        assert!(error.is_retryable());
        assert_eq!(error.replay_safety, ReplaySafety::OutcomeUnknown);
        assert!(!error.permits_model_replay());
        assert!(error.message.contains("text/html"));
    }

    #[tokio::test]
    async fn non_retryable_4xx_preserves_status_and_safe_error_detail() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            write_http_response(
                &mut stream,
                "422 Unprocessable Entity",
                "",
                br#"{"error":{"message":"tool schema is invalid","code":"invalid_tool_schema"},"access_token":"must-not-leak"}"#,
            )
            .await;
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let error = match client.post(&sample_body(), None).await {
            Ok(_) => panic!("422 response should be rejected"),
            Err(error) => error,
        };
        server.await.unwrap();

        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.origin, GrokErrorOrigin::Http);
        assert_eq!(error.stage, GrokErrorStage::Status);
        assert!(!error.is_retryable());
        assert!(error.message.contains("422 Unprocessable Entity"));
        assert!(error.message.contains("tool schema is invalid"));
        assert!(error.message.contains("invalid_tool_schema"));
        assert!(!error.message.contains("must-not-leak"));
    }

    #[test]
    fn request_deadline_caps_extreme_configuration_without_overflow() {
        let now = TokioInstant::now();
        let deadline = GrokRequestDeadline::after(Duration::from_millis(u64::MAX));
        let remaining = deadline.at().duration_since(now);

        assert!(remaining <= MAX_TOTAL_TIMEOUT + Duration::from_secs(1));
        assert!(remaining >= MAX_TOTAL_TIMEOUT - Duration::from_secs(1));
    }

    #[test]
    fn deadline_error_is_terminal_gateway_timeout() {
        let error = GrokError::deadline_exceeded(GrokErrorStage::Stream);

        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(error.origin, GrokErrorOrigin::Deadline);
        assert!(!error.is_retryable());
        assert!(error.message.contains("total wall-clock timeout"));
    }

    #[tokio::test]
    async fn total_deadline_cancels_stalled_auth_refresh() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            let mut byte = [0_u8; 1];
            tokio::time::timeout(Duration::from_millis(500), stream.read(&mut byte))
                .await
                .expect("deadline should cancel the OAuth request")
                .unwrap()
        });
        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{addr}/v1"),
            format!("http://{addr}"),
            0,
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(
            GrokRequestDeadline::after(Duration::from_millis(50)),
        )));

        let error = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("stalled auth unexpectedly completed before the deadline"),
            Err(error) => error,
        };

        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(error.origin, GrokErrorOrigin::Deadline);
        assert_eq!(error.stage, GrokErrorStage::Auth);
        assert!(!error.is_retryable());
        assert!(retry.lock().await.is_terminal());
        assert_eq!(server.await.unwrap(), 0);
    }

    #[test]
    fn auth_error_mapping_preserves_failure_category() {
        let credentials = auth_error(GrokAuthError::CredentialsInvalid {
            message: "invalid grant".into(),
        });
        assert_eq!(credentials.status, StatusCode::UNAUTHORIZED);
        assert_eq!(credentials.origin, GrokErrorOrigin::Auth);
        assert!(!credentials.is_retryable());

        let temporary = auth_error(GrokAuthError::Temporary {
            message: "discovery unavailable".into(),
            safe_to_retry: true,
        });
        assert_eq!(temporary.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(temporary.origin, GrokErrorOrigin::Auth);
        assert!(temporary.is_retryable());

        let limited = auth_error(GrokAuthError::RateLimited {
            message: "slow down".into(),
            retry_after: Some("11".into()),
            safe_to_retry: true,
        });
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.retry_after.as_deref(), Some("11"));
        assert_eq!(limited.origin, GrokErrorOrigin::Auth);
        assert!(limited.is_retryable());
    }

    #[tokio::test]
    async fn safe_auth_retry_recovers_and_consumes_the_shared_transient_budget() {
        let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_addr = auth_listener.local_addr().unwrap();
        let issuer = format!("http://{auth_addr}");
        let server_issuer = issuer.clone();
        let auth_server = tokio::spawn(async move {
            let (mut limited_discovery, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut limited_discovery).await;
            write_http_response(
                &mut limited_discovery,
                "429 Too Many Requests",
                "retry-after: 0\r\n",
                b"",
            )
            .await;

            let (mut discovery, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut discovery).await;
            let body = serde_json::json!({
                "issuer": server_issuer.clone(),
                "token_endpoint": format!("{server_issuer}/oauth/token")
            })
            .to_string();
            write_http_response(&mut discovery, "200 OK", "", body.as_bytes()).await;

            let (mut token, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut token).await;
            write_http_response(
                &mut token,
                "200 OK",
                "",
                br#"{"access_token":"fresh-access","expires_in":3600}"#,
            )
            .await;
        });

        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_addr = model_listener.local_addr().unwrap();
        let model_hits = Arc::new(AtomicUsize::new(0));
        let model_hits_server = model_hits.clone();
        let model_server = tokio::spawn(async move {
            for _ in 0..(MAX_WIRE_ATTEMPTS - 1) {
                let (mut stream, _) = model_listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                assert!(
                    String::from_utf8_lossy(&request)
                        .to_ascii_lowercase()
                        .contains("authorization: bearer fresh-access")
                );
                model_hits_server.fetch_add(1, Ordering::SeqCst);
                write_http_response(
                    &mut stream,
                    "503 Service Unavailable",
                    "retry-after: 0\r\n",
                    b"busy",
                )
                .await;
            }
        });

        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{model_addr}/v1"),
            issuer,
            0,
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(
            GrokRequestDeadline::after(Duration::from_secs(5)),
        )));
        let error = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("model retries should exhaust the remaining shared budget"),
            Err(error) => error,
        };
        auth_server.await.unwrap();
        model_server.await.unwrap();

        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!error.is_retryable());
        assert_eq!(model_hits.load(Ordering::SeqCst), 3);
        let state = retry.lock().await;
        assert_eq!(state.transient_failures, MAX_RATE_LIMIT_RETRIES);
        assert_eq!(state.wire_attempt, MAX_WIRE_ATTEMPTS - 1);
        assert!(state.is_terminal());
    }

    #[tokio::test]
    async fn unsafe_token_response_loss_is_not_replayed() {
        let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_addr = auth_listener.local_addr().unwrap();
        let issuer = format!("http://{auth_addr}");
        let server_issuer = issuer.clone();
        let auth_server = tokio::spawn(async move {
            let (mut discovery, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut discovery).await;
            let body = serde_json::json!({
                "issuer": server_issuer.clone(),
                "token_endpoint": format!("{server_issuer}/oauth/token")
            })
            .to_string();
            write_http_response(&mut discovery, "200 OK", "", body.as_bytes()).await;

            let (mut token, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut token).await;
            drop(token);

            tokio::time::timeout(Duration::from_millis(500), auth_listener.accept())
                .await
                .is_err()
        });

        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_addr = model_listener.local_addr().unwrap();
        let model_server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(500), model_listener.accept())
                .await
                .is_err()
        });
        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{model_addr}/v1"),
            issuer,
            0,
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(
            GrokRequestDeadline::after(Duration::from_secs(5)),
        )));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.post_with_retry(&sample_body(), None, retry.clone()),
        )
        .await
        .expect("an unsafe refresh outcome must not enter retry backoff");
        let error = match result {
            Ok(_) => panic!("the lost token response must fail the request"),
            Err(error) => error,
        };

        assert!(auth_server.await.unwrap(), "token refresh was replayed");
        assert!(
            model_server.await.unwrap(),
            "model request unexpectedly started"
        );
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.origin, GrokErrorOrigin::Auth);
        assert!(!error.is_retryable());
        let state = retry.lock().await;
        assert_eq!(state.transient_failures, 0);
        assert_eq!(state.wire_attempt, 0);
        assert!(state.is_terminal());
    }

    #[tokio::test]
    async fn proactive_refresh_then_model_401_does_not_rotate_twice() {
        let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_addr = auth_listener.local_addr().unwrap();
        let issuer = format!("http://{auth_addr}");
        let server_issuer = issuer.clone();
        let auth_server = tokio::spawn(async move {
            let (mut discovery, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut discovery).await;
            let body = serde_json::json!({
                "issuer": server_issuer.clone(),
                "token_endpoint": format!("{server_issuer}/oauth/token")
            })
            .to_string();
            write_http_response(&mut discovery, "200 OK", "", body.as_bytes()).await;

            let (mut token, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut token).await;
            let body = br#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
            write_http_response(&mut token, "200 OK", "", body).await;

            tokio::time::timeout(Duration::from_millis(300), auth_listener.accept())
                .await
                .is_err()
        });

        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_addr = model_listener.local_addr().unwrap();
        let model_server = tokio::spawn(async move {
            let (mut model, _) = model_listener.accept().await.unwrap();
            let request = read_http_request(&mut model).await;
            assert!(String::from_utf8_lossy(&request).contains("Bearer rotated-access"));
            write_http_response(&mut model, "401 Unauthorized", "", b"").await;
            tokio::time::timeout(Duration::from_millis(300), model_listener.accept())
                .await
                .is_err()
        });

        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{model_addr}/v1"),
            issuer,
            0,
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let error = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("a rejected refreshed token must be terminal"),
            Err(error) => error,
        };

        assert!(auth_server.await.unwrap(), "OAuth refresh was repeated");
        assert!(model_server.await.unwrap(), "model request was repeated");
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert!(!error.is_retryable());
        let state = retry.lock().await;
        assert_eq!(state.wire_attempt, 1);
        assert!(state.auth_refresh_succeeded);
        assert!(state.is_terminal());
    }

    #[tokio::test]
    async fn auth_refresh_connect_failure_stops_when_retry_delay_exceeds_deadline() {
        let unused_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", unused_listener.local_addr().unwrap());
        drop(unused_listener);
        let (client, _temp) = test_client_with_auth_issuer(
            "http://127.0.0.1:1/v1",
            issuer,
            0,
            GrokTimeouts {
                connect_ms: 100,
                header_ms: 100,
                first_byte_ms: 100,
                body_idle_ms: 100,
            },
        )
        .await;

        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(
            GrokRequestDeadline::after(Duration::from_millis(50)),
        )));
        let error = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("authentication refresh should fail before the Grok request"),
            Err(error) => error,
        };

        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.origin, GrokErrorOrigin::Auth);
        assert!(!error.is_retryable());
        assert!(error.message.contains("remaining request deadline"));
        assert!(!error.message.contains("Re-authenticate"));
        assert_eq!(retry.lock().await.transient_failures(), 1);
        assert!(retry.lock().await.is_terminal());
    }

    #[tokio::test]
    async fn forced_refresh_rate_limit_is_not_flattened_to_401() {
        let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_addr = auth_listener.local_addr().unwrap();
        let issuer = format!("http://{auth_addr}");
        let auth_server_issuer = issuer.clone();
        let auth_server = tokio::spawn(async move {
            let (mut discovery, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut discovery).await;
            let body = serde_json::json!({
                "issuer": auth_server_issuer.clone(),
                "token_endpoint": format!("{auth_server_issuer}/oauth/token")
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            discovery.write_all(response.as_bytes()).await.unwrap();

            let (mut token, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut token).await;
            let body = br#"{"error":"slow_down"}"#;
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nretry-after: 120\r\nconnection: close\r\n\r\n",
                body.len()
            );
            token.write_all(response.as_bytes()).await.unwrap();
            token.write_all(body).await.unwrap();
        });

        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_addr = model_listener.local_addr().unwrap();
        let model_server = tokio::spawn(async move {
            let (mut stream, _) = model_listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{model_addr}/v1"),
            issuer,
            now_ms().saturating_add(3_600_000),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let error = match client.post(&sample_body(), None).await {
            Ok(_) => panic!("the forced refresh should be rate limited"),
            Err(error) => error,
        };
        model_server.await.unwrap();
        auth_server.await.unwrap();

        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.retry_after.as_deref(), Some("120"));
        assert_eq!(error.origin, GrokErrorOrigin::Auth);
        assert!(!error.is_retryable());
        assert!(!error.message.contains("Re-authenticate"));
    }

    #[tokio::test]
    async fn fourth_wire_attempt_401_is_terminal_without_oauth_refresh() {
        let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", auth_listener.local_addr().unwrap());
        let auth_hits = Arc::new(AtomicUsize::new(0));
        let auth_hits_server = auth_hits.clone();
        let auth_server = tokio::spawn(async move {
            if let Ok(Ok((stream, _))) =
                tokio::time::timeout(Duration::from_millis(500), auth_listener.accept()).await
            {
                auth_hits_server.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });

        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_addr = model_listener.local_addr().unwrap();
        let model_hits = Arc::new(AtomicUsize::new(0));
        let model_hits_server = model_hits.clone();
        let model_server = tokio::spawn(async move {
            for attempt in 0..MAX_WIRE_ATTEMPTS {
                let (mut stream, _) = model_listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                model_hits_server.fetch_add(1, Ordering::SeqCst);
                let (status, body): (&str, &[u8]) = if attempt + 1 == MAX_WIRE_ATTEMPTS {
                    ("401 Unauthorized", b"")
                } else {
                    ("503 Service Unavailable", b"busy")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{model_addr}/v1"),
            issuer,
            now_ms().saturating_add(3_600_000),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let error = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("the fourth attempt should terminate on 401"),
            Err(error) => error,
        };
        model_server.await.unwrap();
        auth_server.await.unwrap();

        assert_eq!(
            model_hits.load(Ordering::SeqCst),
            MAX_WIRE_ATTEMPTS as usize
        );
        assert_eq!(auth_hits.load(Ordering::SeqCst), 0);
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert!(!error.is_retryable());
        assert!(error.message.contains("no model retry attempts remain"));
        let state = retry.lock().await;
        assert_eq!(state.wire_attempt, MAX_WIRE_ATTEMPTS);
        assert!(!state.auth_refresh_succeeded);
        assert!(state.is_terminal());
    }

    #[tokio::test]
    async fn concurrent_reentry_waits_for_refresh_and_uses_rotated_token() {
        let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_addr = auth_listener.local_addr().unwrap();
        let issuer = format!("http://{auth_addr}");
        let auth_server_issuer = issuer.clone();
        let token_refreshes = Arc::new(AtomicUsize::new(0));
        let token_refreshes_server = token_refreshes.clone();
        let auth_server = tokio::spawn(async move {
            let (mut discovery, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut discovery).await;
            let body = serde_json::json!({
                "issuer": auth_server_issuer.clone(),
                "token_endpoint": format!("{auth_server_issuer}/oauth/token")
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            discovery.write_all(response.as_bytes()).await.unwrap();

            let (mut token, _) = auth_listener.accept().await.unwrap();
            let _ = read_http_request(&mut token).await;
            token_refreshes_server.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = br#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            token.write_all(response.as_bytes()).await.unwrap();
            token.write_all(body).await.unwrap();
        });

        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_addr = model_listener.local_addr().unwrap();
        let model_server = tokio::spawn(async move {
            for attempt in 0..3 {
                let (mut stream, _) = model_listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let request = String::from_utf8_lossy(&request);
                if attempt == 0 {
                    assert!(request.contains("Bearer expired-access"));
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    assert!(request.contains("Bearer rotated-access"));
                    let body = b"data: ok\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.write_all(body).await.unwrap();
                }
            }
        });

        let (client, _temp) = test_client_with_auth_issuer(
            &format!("http://{model_addr}/v1"),
            issuer,
            now_ms().saturating_add(3_600_000),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let client = Arc::new(client);
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let first_body = sample_body();
        let second_body = sample_body();
        let (first, second) = tokio::join!(
            client.post_with_retry(&first_body, None, retry.clone()),
            client.post_with_retry(&second_body, None, retry.clone()),
        );
        let first = match first {
            Ok(response) => response,
            Err(error) => panic!("first request failed: {}", error.message),
        };
        let second = match second {
            Ok(response) => response,
            Err(error) => panic!("second request failed: {}", error.message),
        };
        assert_eq!(first.into_bytes().await.unwrap(), b"data: ok\n\n");
        assert_eq!(second.into_bytes().await.unwrap(), b"data: ok\n\n");
        model_server.await.unwrap();
        auth_server.await.unwrap();

        assert_eq!(token_refreshes.load(Ordering::SeqCst), 1);
        let state = retry.lock().await;
        assert_eq!(state.wire_attempt, 3);
        assert!(state.auth_refresh_succeeded);
        assert!(!state.is_terminal());
    }

    #[tokio::test]
    async fn prepared_payload_is_serialized_once_and_shared_across_wire_attempts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                bodies.push(read_http_request_body(&mut stream).await);
                if attempt == 0 {
                    write_http_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "retry-after: 0\r\n",
                        b"busy",
                    )
                    .await;
                } else {
                    let response_body = b"data: ok\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response_body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.write_all(response_body).await.unwrap();
                }
            }
            bodies
        });
        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let serializations = AtomicUsize::new(0);
        let prepared = PreparedGrokRequest::from_serializable(
            &CountingPayload {
                serializations: &serializations,
            },
            false,
        )
        .unwrap();
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));

        let response = client
            .post_prepared_with_retry(&prepared, None, retry)
            .await
            .unwrap();
        assert_eq!(response.into_bytes().await.unwrap(), b"data: ok\n\n");
        let bodies = server.await.unwrap();

        assert_eq!(serializations.load(Ordering::SeqCst), 1);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[0].as_slice(), prepared.body.as_ref());
    }

    #[tokio::test]
    async fn retries_503_then_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
                let (status, body): (&str, &[u8]) = if attempt == 0 {
                    ("503 Service Unavailable", b"retry")
                } else {
                    (
                        "200 OK",
                        b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let response = client.post(&sample_body(), None).await.unwrap();
        let body = response.into_bytes().await.unwrap();
        server.await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&body).contains("response.completed"));
    }

    #[tokio::test]
    async fn retries_429_with_retry_after() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
                let (status, body): (&str, &[u8]) = if attempt == 0 {
                    ("429 Too Many Requests", b"slow down")
                } else {
                    ("200 OK", b"data: ok\n\n")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let response = client.post(&sample_body(), None).await.unwrap();
        let _ = response.into_bytes().await.unwrap();
        server.await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn post_dispatch_transport_reset_is_not_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            // Consume the complete POST before resetting. The upstream may
            // already have accepted it, so a second model dispatch is unsafe.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            drop(stream);
            tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .is_ok()
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let error = match client.post(&sample_body(), None).await {
            Ok(_) => panic!("a post-dispatch reset must fail without replay"),
            Err(error) => error,
        };
        let replayed = server.await.unwrap();
        assert_eq!(error.replay_safety, ReplaySafety::OutcomeUnknown);
        assert!(!error.permits_model_replay());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!replayed, "the uncertain POST must not be replayed");
    }

    #[tokio::test]
    async fn shared_retry_budget_is_not_nested() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            // Exhaust the shared budget: 1 initial + 3 retries = 4 total.
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
                let body = b"busy";
                let response = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let err = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("budget exhaustion should fail"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 4);
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!err.is_retryable());

        let wire_attempts = retry.lock().await.wire_attempt;
        let reentered = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("an exhausted state must reject re-entry without another request"),
            Err(error) => error,
        };
        assert!(!reentered.is_retryable());
        assert_eq!(retry.lock().await.wire_attempt, wire_attempts);
    }

    #[tokio::test]
    async fn concurrent_reentry_atomically_reserves_at_most_four_wire_attempts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let accepted =
                    tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
                let Ok(Ok((mut stream, _))) = accepted else {
                    break;
                };
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
                let body = b"busy";
                let response = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let client = Arc::new(client);
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let first_body = sample_body();
        let second_body = sample_body();
        let (first, second) = tokio::join!(
            client.post_with_retry(&first_body, None, retry.clone()),
            client.post_with_retry(&second_body, None, retry.clone()),
        );
        server.await.unwrap();

        let first = match first {
            Ok(_) => panic!("the first concurrent caller should exhaust the budget"),
            Err(error) => error,
        };
        let second = match second {
            Ok(_) => panic!("the second concurrent caller should exhaust the budget"),
            Err(error) => error,
        };
        assert!(!first.is_retryable());
        assert!(!second.is_retryable());
        assert_eq!(hits.load(Ordering::SeqCst), MAX_WIRE_ATTEMPTS as usize);
        let state = retry.lock().await;
        assert_eq!(state.wire_attempt, MAX_WIRE_ATTEMPTS);
        assert!(state.is_terminal());
    }

    #[tokio::test]
    async fn active_body_chunks_cannot_extend_total_request_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for chunk in [b"a".as_slice(), b"b", b"c"] {
                if stream.write_all(b"1\r\n").await.is_err()
                    || stream.write_all(chunk).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 100,
                first_byte_ms: 200,
                body_idle_ms: 200,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(
            GrokRequestDeadline::after(Duration::from_millis(130)),
        )));
        let response = client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
            .unwrap();
        let error = response.into_bytes().await.unwrap_err();
        server.await.unwrap();
        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(error.origin, GrokErrorOrigin::Deadline);
        assert_eq!(error.stage, GrokErrorStage::Stream);
        assert!(!error.is_retryable());
        assert!(retry.lock().await.is_terminal());
    }

    #[tokio::test]
    async fn retry_delay_beyond_remaining_deadline_preserves_upstream_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(
            GrokRequestDeadline::after(Duration::from_millis(50)),
        )));

        let error = match client
            .post_with_retry(&sample_body(), None, retry.clone())
            .await
        {
            Ok(_) => panic!("retry backoff unexpectedly escaped the total deadline"),
            Err(error) => error,
        };
        server.await.unwrap();

        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.origin, GrokErrorOrigin::Http);
        assert_eq!(error.retry_after.as_deref(), Some("0"));
        assert!(error.message.contains("remaining request deadline"));
        assert!(!error.is_retryable());
        let state = retry.lock().await;
        assert_eq!(state.wire_attempt, 1);
        assert!(state.is_terminal());
    }

    #[tokio::test]
    async fn first_byte_timeout_is_retryable_stream_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 50,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let response = client.post(&sample_body(), None).await.unwrap();
        let err = response.into_bytes().await.unwrap_err();
        let _ = server.await;
        assert_eq!(err.origin, GrokErrorOrigin::StreamTransport);
        assert_eq!(err.stage, GrokErrorStage::Body);
        assert!(err.is_retryable());
        assert_eq!(err.replay_safety, ReplaySafety::OutcomeUnknown);
        assert!(!err.permits_model_replay());
    }

    #[tokio::test]
    async fn header_timeout_exhaustion_is_terminal_request_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            // Keep the first request open without headers and watch long
            // enough for an erroneous retry to arrive.
            let replayed = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .is_ok();
            drop(stream);
            replayed
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 50,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let err = match client.post(&sample_body(), None).await {
            Ok(_) => panic!("header timeout should fail"),
            Err(error) => error,
        };
        let replayed = server.await.unwrap();
        assert_eq!(err.origin, GrokErrorOrigin::RequestTransport);
        assert_eq!(err.stage, GrokErrorStage::Header);
        assert!(err.is_retryable());
        assert_eq!(err.replay_safety, ReplaySafety::OutcomeUnknown);
        assert!(!err.permits_model_replay());
        assert!(err.message.contains("headers"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!replayed, "a timed-out POST must not be replayed");
    }

    #[tokio::test]
    async fn body_idle_timeout_is_retryable_stream_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.write_all(b"1\r\na\r\n").await.unwrap();
            // Stall after first body byte so idle timeout fires.
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 50,
            },
        )
        .await;
        let response = client.post(&sample_body(), None).await.unwrap();
        let err = response.into_bytes().await.unwrap_err();
        let _ = server.await;
        assert_eq!(err.origin, GrokErrorOrigin::StreamTransport);
        assert_eq!(err.stage, GrokErrorStage::Stream);
        assert!(err.is_retryable());
        assert_eq!(err.replay_safety, ReplaySafety::OutcomeUnknown);
        assert!(!err.permits_model_replay());
        assert!(err.message.contains("next Grok response body chunk"));
    }

    #[tokio::test]
    async fn rejected_body_drain_respects_timeouts_and_allows_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            // First attempt: 503 headers, then hang the error body.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ntransfer-encoding: chunked\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(stream);

            // Second attempt succeeds.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            let body = b"data: ok\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 50,
                body_idle_ms: 50,
            },
        )
        .await;
        let response = client.post(&sample_body(), None).await.unwrap();
        let body = response.into_bytes().await.unwrap();
        server.await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(body, b"data: ok\n\n");
    }

    #[tokio::test]
    async fn response_size_limit_is_not_retryable() {
        let err = GrokError::response_limit(
            GrokErrorStage::Body,
            "Grok upstream response exceeds the size limit",
        );
        assert_eq!(err.origin, GrokErrorOrigin::ResponseLimit);
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn non_retryable_4xx_fails_immediately() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            let body = b"bad";
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let err = match client.post(&sample_body(), None).await {
            Ok(_) => panic!("non-retryable 4xx should fail immediately"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn traffic_capture_records_retry_attempts_without_terminal_error_on_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                let (status, body): (&str, &[u8]) = if attempt == 0 {
                    ("503 Service Unavailable", b"retry")
                } else {
                    ("200 OK", b"data: done\n\n")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let (client, _auth_temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let response = client
            .post(&sample_body(), Some(traffic.clone()))
            .await
            .unwrap();
        let _ = response.into_bytes().await.unwrap();
        server.await.unwrap();

        let mut captured = String::new();
        let mut attempt_artifacts = 0;
        for entry in std::fs::read_dir(temp.path().join("traffic")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("022-upstream-attempt"))
                {
                    attempt_artifacts += 1;
                }
                captured.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        assert_eq!(attempt_artifacts, 2);
        assert!(captured.contains("\"wait_ms\""));
        assert!(captured.contains("\"attempt\""));
        assert!(
            captured.contains("023-upstream-retry") || captured.contains("\"transient_failures\"")
        );
        assert!(!captured.contains("060-grok-stream-error"));
        assert!(!captured.contains("test-access"));
    }

    #[tokio::test]
    async fn traffic_capture_records_retry_exhausted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut stream).await;
                let body = b"busy";
                let response = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let (client, _auth_temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let err = match client.post(&sample_body(), Some(traffic.clone())).await {
            Ok(_) => panic!("retry budget should exhaust"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);

        let mut captured = String::new();
        for entry in std::fs::read_dir(temp.path().join("traffic")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                captured.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        assert!(captured.contains("retry_exhausted"));
        assert!(captured.contains("060-grok-stream-error") || captured.contains("\"kind\""));
        assert!(!captured.contains("test-access"));
    }
}
