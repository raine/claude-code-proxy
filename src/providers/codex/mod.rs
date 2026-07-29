pub mod auth;
pub mod client;
pub mod continuation;
pub mod count_tokens;
pub(crate) mod dispatch_budget;
pub(crate) mod events;
pub mod request_summary;
pub mod translate;
pub mod websocket;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::StatusCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::config;
use crate::monitor::{anthropic_total_input_tokens, usage_from_anthropic_sse};
use crate::provider::{CliHandlers, Provider, RequestByteLease, RequestContext};
use crate::providers::downstream_queue::{
    self, BudgetedChunk, ByteBudget, SendOutcome as LiveChunkSendOutcome,
};
use crate::providers::token_count_admission::{self, TokenCountAdmissionError};
use crate::providers::translate_shared::{
    ImageDecodeAdmissionError, acquire_image_decode_slot, is_claude_code_compaction_request,
    messages_request_contains_base64_image,
};
use crate::registry;
use crate::retry::{ModelRetryBackoff, sleep};
use crate::timeutil::now_ms;

use self::auth::browser_login::run_browser_login;
use self::auth::device::DeviceAuthClient;
use self::auth::manager::CodexAuthManager;
use self::auth::token_store::file_store;
use self::client::CodexHttpClient;
use self::continuation::{
    ContinuationCandidate, abort_continuation, continuation_candidate, record_continuation,
};
use self::count_tokens::count_translated_tokens;
use self::translate::accumulate::accumulate_response_with_traffic_schema_tool_policy_and_metadata;
use self::translate::live_stream::LiveStreamTranslator;
use self::translate::model_allowlist::{
    assert_allowed_model, resolve_model_request, uses_responses_lite,
};
use self::translate::reducer::{FinishMetadata, finish_metadata_from_upstream_with_tool_policy};
use self::translate::request::{
    ServiceTier, TranslateOptions, TranslationOverrides, has_hosted_web_search,
    has_parallel_callable_function, translate_request_with_overrides,
};
use self::translate::tool_policy::ToolCallPolicy;

const MAX_RETRYABLE_LIVE_STREAM_RETRIES: u32 = 10;
const MAX_LIVE_TRANSPORT_RETRIES: u32 = 2;
const MAX_LIVE_UPSTREAM_SSE_BYTES: usize = crate::traffic::MAX_STREAM_CAPTURE_EVENT_BYTES;
pub(super) const MAX_LIVE_EVENT_BYTES: usize = 1024 * 1024;
pub(super) const LIVE_EVENT_CHANNEL_CAPACITY: usize = 2;
const MAX_DOWNSTREAM_QUEUE_BYTES: usize = 2 * 1024 * 1024;
const DOWNSTREAM_STALL_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_STREAM_HEARTBEAT: Duration = Duration::from_secs(5);
const MIN_STREAM_HEARTBEAT: Duration = Duration::from_secs(1);
const MAX_STREAM_HEARTBEAT: Duration = Duration::from_secs(60);
const DOWNSTREAM_PING: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
static LIVE_CONTINUATION_CAPTURE_BYTES: once_cell::sync::Lazy<Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| {
        Arc::new(tokio::sync::Semaphore::new(
            continuation::MAX_TOTAL_TRANSCRIPT_BYTES as usize,
        ))
    });

fn translation_overrides() -> TranslationOverrides {
    TranslationOverrides::configured()
}

fn configured_stream_heartbeat() -> Duration {
    stream_heartbeat_duration(config::codex_stream_heartbeat_ms(
        DEFAULT_STREAM_HEARTBEAT.as_millis() as u64,
    ))
}

fn stream_heartbeat_duration(configured_ms: u64) -> Duration {
    Duration::from_millis(configured_ms).clamp(MIN_STREAM_HEARTBEAT, MAX_STREAM_HEARTBEAT)
}
#[cfg(test)]
pub(super) static CODEX_STATE_TEST_LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));
// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct CodexProvider {
    client: Arc<CodexHttpClient>,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(CodexHttpClient::new()),
        }
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn supported_models(&self) -> Vec<String> {
        let mut models: Vec<String> = registry::CODEX_MODELS
            .iter()
            .map(|m| m.to_string())
            .collect();
        for m in registry::CODEX_MODELS {
            models.push(format!("{m}-fast"));
        }
        models.sort_unstable();
        models.dedup();
        models
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &CODEX_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let provider_started_at = Instant::now();
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let requested_model = body
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5.6-sol".to_string());
        let model = requested_model.as_str();

        let resolved = resolve_model_request(model);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        let requested_max_tokens =
            match crate::providers::translate_shared::validate_generation_max_tokens(&body, "Codex")
            {
                Ok(max_tokens) => max_tokens,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        error.to_string(),
                    );
                }
            };
        let native_web_search = has_hosted_web_search(&body);
        let use_responses_lite = match apply_model_lane_for_request(&resolved.model, &body) {
            Ok(lite) => lite,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!(
                        "Model \"{model}\" resolves to \"{}\": {error}",
                        resolved.model
                    ),
                );
            }
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
        }

        let translated = match translate_codex_request_for_provider(
            body,
            TranslateOptions {
                session_id: ctx.session_id.clone(),
                service_tier: resolved.service_tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
            },
            translation_overrides(),
        )
        .await
        {
            Ok(t) => t,
            Err(response) => return response,
        };
        log_codex_request_configuration(
            &ctx,
            &translated,
            use_responses_lite,
            native_web_search,
            self.client.transport_decision(),
            requested_max_tokens,
        );

        // Check continuation
        let previous_response_id_enabled = config::codex_previous_response_id();
        let continuation = continuation_candidate(
            ctx.session_id.as_deref(),
            &translated,
            previous_response_id_enabled,
        );
        let turn_id = continuation.turn_id;
        let deadline = client::CodexRequestDeadline::configured_from(provider_started_at);

        // Post to upstream with continuation
        let client = self.client.clone();
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        if want_stream {
            let stream_heartbeat = configured_stream_heartbeat();
            return live_stream_response(
                client,
                message_id,
                model,
                ctx,
                translated,
                continuation,
                provider_started_at,
                deadline,
                stream_heartbeat,
            )
            .await;
        }

        let upstream = match client
            .post_codex_before(&translated, &ctx, Some(&continuation), deadline)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                abort_continuation(ctx.session_id.as_deref(), turn_id);
                return map_codex_error_to_response(&e);
            }
        };

        let tool_policy = ToolCallPolicy::from_request(&translated);
        match accumulate_response_with_traffic_schema_tool_policy_and_metadata(
            &upstream.body,
            &message_id,
            model,
            ctx.traffic.as_deref(),
            translated.schema_bridge.as_deref(),
            &tool_policy,
        ) {
            Ok((json, finish_metadata)) => {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    let usage = json.pointer("/usage");
                    monitor.usage_updated(
                        &ctx.req_id,
                        usage.and_then(anthropic_total_input_tokens),
                        usage
                            .and_then(|value| value.get("output_tokens"))
                            .and_then(|value| value.as_u64()),
                    );
                }
                update_continuation_from_finish_metadata(
                    ctx.session_id.as_deref(),
                    turn_id,
                    &translated,
                    finish_metadata.as_ref(),
                );
                (StatusCode::OK, Json(json)).into_response()
            }
            Err(e) => {
                abort_continuation(ctx.session_id.as_deref(), turn_id);
                map_codex_failure_to_response(&format!("Accumulation error: {e}"))
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested_model = body
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5.6-sol".to_string());
        let model = requested_model.as_str();
        let resolved = resolve_model_request(model);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        let use_responses_lite = match apply_model_lane_for_request(&resolved.model, &body) {
            Ok(lite) => lite,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!(
                        "Model \"{model}\" resolves to \"{}\": {error}",
                        resolved.model
                    ),
                );
            }
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
        }

        let translated = match translate_codex_request_for_provider(
            body,
            TranslateOptions {
                session_id: None,
                service_tier: resolved.service_tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
            },
            translation_overrides(),
        )
        .await
        {
            Ok(t) => t,
            Err(response) => return response,
        };

        let tokens = match count_codex_tokens_bounded(translated).await {
            Ok(tokens) => tokens,
            Err(error) => return map_codex_count_tokens_error(error),
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

async fn count_codex_tokens_bounded(
    translated: translate::request::ResponsesRequest,
) -> Result<u64, TokenCountAdmissionError> {
    token_count_admission::run(move || count_translated_tokens(&translated)).await
}

fn map_codex_count_tokens_error(error: TokenCountAdmissionError) -> Response {
    match error {
        TokenCountAdmissionError::QueueTimeout => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Codex token-count admission queue was saturated for 30 seconds",
        ),
        TokenCountAdmissionError::Closed => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "The shared token-count admission gate is unavailable",
        ),
        TokenCountAdmissionError::WorkerFailed(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            format!("Codex token-count worker failed: {error}"),
        ),
    }
}

async fn translate_codex_request_for_provider(
    body: MessagesRequest,
    options: TranslateOptions,
    overrides: TranslationOverrides,
) -> Result<translate::request::ResponsesRequest, Response> {
    if !messages_request_contains_base64_image(&body) {
        return translate_request_with_overrides(&body, options, overrides).map_err(|error| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            )
        });
    }

    let permit = match acquire_image_decode_slot().await {
        Ok(permit) => permit,
        Err(ImageDecodeAdmissionError::Closed) => {
            return Err(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "Codex image validation is unavailable",
            ));
        }
        Err(ImageDecodeAdmissionError::Timeout) => {
            return Err(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "Codex image validation queue was saturated for 30 seconds",
            ));
        }
    };

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        translate_request_with_overrides(&body, options, overrides)
    })
    .await
    .map_err(|error| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            format!("Codex image validation worker failed: {error}"),
        )
    })?
    .map_err(|error| {
        json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            error.to_string(),
        )
    })
}

/// Picks the upstream lane for a request. Luna is a Lite-only model, so any
/// request that requires the full Responses API fails locally rather than
/// silently changing the caller's selected model.
fn apply_model_lane_for_request(model: &str, body: &MessagesRequest) -> Result<bool, &'static str> {
    apply_model_lane_for_request_with_options(
        model,
        body,
        config::codex_responses_lite(),
        config::codex_parallel_tools(),
    )
}

fn apply_model_lane_for_request_with_options(
    model: &str,
    body: &MessagesRequest,
    responses_lite: bool,
    parallel_tools: bool,
) -> Result<bool, &'static str> {
    if has_hosted_web_search(body) {
        return full_lane_for_model(model);
    }
    if responses_lite && parallel_tools && parallel_tool_lane_eligible(model, body) {
        return Ok(false);
    }
    let use_responses_lite = responses_lite && uses_responses_lite(model);
    if use_responses_lite {
        Ok(true)
    } else {
        full_lane_for_model(model)
    }
}

fn full_lane_for_model(model: &str) -> Result<bool, &'static str> {
    if model == "gpt-5.6-luna" {
        Err(
            "gpt-5.6-luna is available only through Responses Lite, but this request requires the full Responses lane; enable codex.responsesLite and remove hosted web_search, or explicitly select gpt-5.6-sol/gpt-5.6-terra",
        )
    } else {
        Ok(false)
    }
}

fn parallel_tool_lane_eligible(model: &str, body: &MessagesRequest) -> bool {
    if !matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra")
        || is_claude_code_compaction_request(body)
        || has_structured_output(body)
        || !has_parallel_callable_function(body)
    {
        return false;
    }

    true
}

fn has_structured_output(body: &MessagesRequest) -> bool {
    body.extra
        .get("output_config")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|output| output.contains_key("format"))
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

enum LiveStreamStart {
    Response(Response),
    Retry {
        error: client::CodexError,
        replay_state: Option<Box<LiveReplayState>>,
    },
}

#[cfg(test)]
type ScriptedLiveAttemptQueue = Arc<
    std::sync::Mutex<
        std::collections::VecDeque<
            Result<websocket::CodexWebSocketEventReceiver, client::CodexError>,
        >,
    >,
>;

struct LiveReplayState {
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: String,
    request_body: translate::request::ResponsesRequest,
    request_byte_lease: Option<RequestByteLease>,
    continuation: Option<ContinuationCandidate>,
    dispatch_budget: dispatch_budget::CodexDispatchBudget,
    attempt: u32,
    model_retry_backoff: ModelRetryBackoff,
    configured_transport: config::CodexTransport,
    active_transport: config::CodexTransport,
    circuit_key: String,
    schema_bridge: Option<Arc<translate::schema_bridge::SchemaBridge>>,
    tool_policy: ToolCallPolicy,
    #[cfg(test)]
    scripted_attempts: Option<ScriptedLiveAttemptQueue>,
}

impl LiveReplayState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        client: Arc<CodexHttpClient>,
        message_id: String,
        model: String,
        request_body: translate::request::ResponsesRequest,
        request_byte_lease: Option<RequestByteLease>,
        continuation: ContinuationCandidate,
    ) -> Self {
        let configured_transport = client.transport();
        let circuit_key = client.websocket_circuit_key().to_string();
        let schema_bridge = request_body.schema_bridge.clone();
        let tool_policy = ToolCallPolicy::from_request(&request_body);
        Self {
            client,
            message_id,
            model,
            request_body,
            request_byte_lease,
            continuation: Some(continuation),
            dispatch_budget: dispatch_budget::CodexDispatchBudget::new(),
            attempt: 0,
            model_retry_backoff: ModelRetryBackoff::default(),
            configured_transport,
            active_transport: configured_transport,
            circuit_key,
            schema_bridge,
            tool_policy,
            #[cfg(test)]
            scripted_attempts: None,
        }
    }

    fn turn_id(&self) -> Option<u64> {
        self.continuation
            .as_ref()
            .and_then(|candidate| candidate.turn_id)
    }

    fn fresh_translator(&self) -> LiveStreamTranslator {
        LiveStreamTranslator::with_schema_bridge_and_tool_policy(
            self.message_id.clone(),
            self.model.clone(),
            self.schema_bridge.clone(),
            self.tool_policy.clone(),
        )
    }

    fn fresh_continuation_capture(&self) -> Option<LiveContinuationCapture> {
        LiveContinuationCapture::for_turn(
            self.turn_id(),
            &self.request_body,
            self.request_byte_lease.as_ref(),
        )
    }

    async fn open_upstream(
        &self,
        ctx: &RequestContext,
        deadline: client::CodexRequestDeadline,
    ) -> Result<websocket::CodexWebSocketEventReceiver, client::CodexError> {
        #[cfg(test)]
        if let Some(scripted_attempts) = self.scripted_attempts.as_ref() {
            self.dispatch_budget
                .reserve_model()
                .map_err(|error| client::CodexError {
                    status: 503,
                    message: error.to_string(),
                    detail: Some(dispatch_budget::CODEX_DISPATCH_BUDGET_DETAIL.to_string()),
                    retry_after: None,
                    origin: client::CodexErrorOrigin::Http,
                })?;
            return scripted_attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
                .expect("scripted live replay attempt was not provided");
        }

        match self.active_transport {
            config::CodexTransport::Http => {
                self.client
                    .stream_codex_http_events(
                        &self.request_body,
                        ctx,
                        deadline,
                        self.dispatch_budget.clone(),
                    )
                    .await
            }
            config::CodexTransport::WebSocket | config::CodexTransport::Auto => {
                self.client
                    .stream_codex_websocket_events(
                        &self.request_body,
                        ctx,
                        self.continuation.as_ref(),
                        self.dispatch_budget.clone(),
                    )
                    .await
            }
        }
    }

    fn plan_retry(
        &mut self,
        ctx: &RequestContext,
        error: client::CodexError,
    ) -> Result<u64, client::CodexError> {
        let auto_websocket_attempt = self.configured_transport == config::CodexTransport::Auto
            && !matches!(self.active_transport, config::CodexTransport::Http);
        if auto_websocket_attempt {
            client::record_auto_websocket_failure(ctx, &self.circuit_key, &error);
        }
        if !replayable_live_start_codex_error(&error) {
            return Err(error);
        }
        if !self.dispatch_budget.can_reserve_model() {
            let snapshot = self.dispatch_budget.snapshot();
            crate::logging::create_logger("codex").warn(
                "live_transport_retry_exhausted",
                Some(serde_json::Map::from_iter([
                    ("reqId".into(), serde_json::json!(ctx.req_id)),
                    (
                        "transport".into(),
                        serde_json::json!(self.active_transport.as_str()),
                    ),
                    ("attempts".into(), serde_json::json!(snapshot.model)),
                    (
                        "maxAttempts".into(),
                        serde_json::json!(dispatch_budget::MAX_CODEX_MODEL_DISPATCHES),
                    ),
                    ("status".into(), serde_json::json!(error.status)),
                    (
                        "replaySafety".into(),
                        serde_json::json!(error.replay_safety().as_str()),
                    ),
                ])),
            );
            return Err(error);
        }
        if auto_websocket_attempt && client::should_fallback_to_http(&error) {
            client::log_auto_http_fallback(ctx, &error);
            let Some(delay) =
                client::auto_http_fallback_delay(&error, &mut self.model_retry_backoff)
            else {
                return Err(error);
            };
            if delay.exceeds_budget {
                return Err(error);
            }
            drop_live_continuation_for_retry(&mut self.continuation);
            self.active_transport = config::CodexTransport::Http;
            return Ok(delay.wait_ms);
        }
        let dropped = drop_live_continuation_for_retry(&mut self.continuation);
        if dropped && is_missing_previous_response_error(&error) {
            self.attempt = self.attempt.saturating_add(1);
            return Ok(0);
        }
        let max_retries = max_live_retries(&error);
        if self.attempt >= max_retries {
            return Err(error);
        }
        let Some(delay) = self
            .model_retry_backoff
            .next_delay(error.replay_safety(), error.retry_after.as_deref())
        else {
            return Err(error);
        };
        if delay.exceeds_budget {
            return Err(error);
        }
        let failed_attempt = self.dispatch_budget.snapshot().model.max(1);
        let max_attempts = (max_retries + 1).min(dispatch_budget::MAX_CODEX_MODEL_DISPATCHES);
        client::log_live_transport_retry(
            ctx,
            self.active_transport,
            failed_attempt,
            max_attempts,
            delay.wait_ms,
            &error,
        );
        self.attempt = self.attempt.saturating_add(1);
        Ok(delay.wait_ms)
    }

    #[cfg(test)]
    fn with_scripted_attempts(
        mut self,
        attempts: std::collections::VecDeque<
            Result<websocket::CodexWebSocketEventReceiver, client::CodexError>,
        >,
    ) -> Self {
        self.configured_transport = config::CodexTransport::Http;
        self.active_transport = config::CodexTransport::Http;
        self.scripted_attempts = Some(Arc::new(std::sync::Mutex::new(attempts)));
        self
    }
}

struct LiveContinuationCapture {
    request_body: translate::request::ResponsesRequest,
    upstream_sse_body: Vec<u8>,
    captured_bytes: usize,
    capture_byte_permit: tokio::sync::OwnedSemaphorePermit,
}

impl LiveContinuationCapture {
    fn for_turn(
        turn_id: Option<u64>,
        request_body: &translate::request::ResponsesRequest,
        request_byte_lease: Option<&RequestByteLease>,
    ) -> Option<Self> {
        turn_id?;
        let translated_bytes =
            serde_json::to_vec(request_body).map_or(usize::MAX, |encoded| encoded.len());
        let request_bytes = request_byte_lease
            .map(RequestByteLease::buffered_bytes)
            .unwrap_or_default()
            .max(translated_bytes);
        if request_bytes > continuation::MAX_SESSION_TRANSCRIPT_BYTES as usize {
            return None;
        }
        let capture_byte_permit = LIVE_CONTINUATION_CAPTURE_BYTES
            .clone()
            .try_acquire_many_owned(request_bytes as u32)
            .ok()?;
        Some(Self {
            request_body: request_body.clone(),
            upstream_sse_body: Vec::new(),
            captured_bytes: request_bytes,
            capture_byte_permit,
        })
    }

    fn append(&mut self, payload: &serde_json::Value) -> bool {
        let text = payload.to_string();
        let encoded_len = text
            .lines()
            .map(|line| b"data: ".len() + line.len() + 1)
            .sum::<usize>()
            .saturating_add(1);
        if self.captured_bytes.saturating_add(encoded_len)
            > continuation::MAX_SESSION_TRANSCRIPT_BYTES as usize
            || self.upstream_sse_body.len().saturating_add(encoded_len)
                > MAX_LIVE_UPSTREAM_SSE_BYTES
        {
            return false;
        }
        let Ok(extra_permit) = LIVE_CONTINUATION_CAPTURE_BYTES
            .clone()
            .try_acquire_many_owned(encoded_len as u32)
        else {
            return false;
        };
        self.capture_byte_permit.merge(extra_permit);
        self.captured_bytes += encoded_len;
        for line in text.lines() {
            self.upstream_sse_body.extend_from_slice(b"data: ");
            self.upstream_sse_body.extend_from_slice(line.as_bytes());
            self.upstream_sse_body.push(b'\n');
        }
        self.upstream_sse_body.push(b'\n');
        true
    }
}

fn update_live_continuation_from_capture(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    capture: Option<&LiveContinuationCapture>,
) {
    let Some(capture) = capture else {
        abort_continuation(session_id, turn_id);
        return;
    };
    update_continuation_from_upstream(
        session_id,
        turn_id,
        &capture.request_body,
        &capture.upstream_sse_body,
    );
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    request_body: translate::request::ResponsesRequest,
    continuation: ContinuationCandidate,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    stream_heartbeat: Duration,
) -> Response {
    let turn_id = continuation.turn_id;
    let transport = client.transport();
    match tokio::time::timeout_at(
        deadline.at(),
        live_stream_response_inner(
            client,
            message_id,
            model,
            ctx.clone(),
            request_body,
            continuation,
            provider_started_at,
            deadline,
            stream_heartbeat,
        ),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => {
            abort_continuation(ctx.session_id.as_deref(), turn_id);
            map_codex_error_to_response(&client::codex_total_timeout_error(
                transport,
                deadline.timeout_ms(),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response_inner(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: &str,
    mut ctx: RequestContext,
    request_body: translate::request::ResponsesRequest,
    continuation: ContinuationCandidate,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    stream_heartbeat: Duration,
) -> Response {
    let request_byte_lease = ctx.request_byte_lease.take();
    let mut replay_state = LiveReplayState::new(
        client,
        message_id,
        model.to_string(),
        request_body,
        request_byte_lease,
        continuation,
    );
    let turn_id = replay_state.turn_id();
    // Auto may fall back once, but it never switches back to WebSocket within
    // the same logical request. This prevents retry multiplication.
    if replay_state.configured_transport == config::CodexTransport::Auto
        && websocket::codex_websocket_circuit_open(&replay_state.circuit_key)
    {
        client::log_websocket_circuit_fallback(&ctx);
        drop_live_continuation_for_retry(&mut replay_state.continuation);
        replay_state.active_transport = config::CodexTransport::Http;
    }

    loop {
        let upstream_events = match replay_state.open_upstream(&ctx, deadline).await {
            Ok(events) => events,
            Err(error) => match replay_state.plan_retry(&ctx, error) {
                Ok(wait_ms) => {
                    if wait_ms > 0 {
                        sleep(wait_ms).await;
                    }
                    continue;
                }
                Err(error) => {
                    abort_continuation(ctx.session_id.as_deref(), turn_id);
                    return map_codex_error_to_response(&error);
                }
            },
        };

        let message_id = replay_state.message_id.clone();
        let model = replay_state.model.clone();
        let schema_bridge = replay_state.schema_bridge.clone();
        let tool_policy = replay_state.tool_policy.clone();
        let continuation_capture = replay_state.fresh_continuation_capture();
        let auto_websocket_attempt = replay_state.configured_transport
            == config::CodexTransport::Auto
            && replay_state.active_transport == config::CodexTransport::Auto;
        let circuit_key = replay_state.circuit_key.clone();
        match live_stream_response_once_with_schema_and_tool_policy(
            upstream_events,
            message_id,
            &model,
            ctx.clone(),
            turn_id,
            continuation_capture,
            provider_started_at,
            deadline,
            stream_heartbeat,
            schema_bridge,
            tool_policy,
            Some(Box::new(replay_state)),
        )
        .await
        {
            LiveStreamStart::Response(response) => {
                if auto_websocket_attempt {
                    websocket::record_codex_websocket_success(&circuit_key);
                }
                return response;
            }
            LiveStreamStart::Retry {
                error,
                replay_state: Some(state),
            } => {
                replay_state = *state;
                match replay_state.plan_retry(&ctx, error) {
                    Ok(wait_ms) => {
                        if wait_ms > 0 {
                            sleep(wait_ms).await;
                        }
                    }
                    Err(error) => {
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                        return map_codex_error_to_response(&error);
                    }
                }
            }
            LiveStreamStart::Retry {
                replay_state: None, ..
            } => unreachable!("production live stream retry must return its replay state"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn live_stream_response_once(
    upstream_events: websocket::CodexWebSocketEventReceiver,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    turn_id: Option<u64>,
    continuation_capture: Option<LiveContinuationCapture>,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    keepalive_delay: Duration,
) -> LiveStreamStart {
    live_stream_response_once_with_schema(
        upstream_events,
        message_id,
        model,
        ctx,
        turn_id,
        continuation_capture,
        provider_started_at,
        deadline,
        keepalive_delay,
        None,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn live_stream_response_once_with_schema(
    upstream_events: websocket::CodexWebSocketEventReceiver,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    turn_id: Option<u64>,
    continuation_capture: Option<LiveContinuationCapture>,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    keepalive_delay: Duration,
    schema_bridge: Option<Arc<translate::schema_bridge::SchemaBridge>>,
) -> LiveStreamStart {
    live_stream_response_once_with_schema_and_tool_policy(
        upstream_events,
        message_id,
        model,
        ctx,
        turn_id,
        continuation_capture,
        provider_started_at,
        deadline,
        keepalive_delay,
        schema_bridge,
        ToolCallPolicy::permissive(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response_once_with_schema_and_tool_policy(
    mut upstream_events: websocket::CodexWebSocketEventReceiver,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    turn_id: Option<u64>,
    mut continuation_capture: Option<LiveContinuationCapture>,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    keepalive_delay: Duration,
    schema_bridge: Option<Arc<translate::schema_bridge::SchemaBridge>>,
    tool_policy: ToolCallPolicy,
    mut replay_state: Option<Box<LiveReplayState>>,
) -> LiveStreamStart {
    let mut translator = LiveStreamTranslator::with_schema_bridge_and_tool_policy(
        message_id,
        model.to_string(),
        schema_bridge,
        tool_policy,
    );
    let mut generation_started = false;
    let mut hosted_side_effect_started = false;
    // Start the downstream grace window as soon as either live transport is ready.
    // Waiting for the first WebSocket generation event can otherwise leave Claude with
    // no bytes for the full response-start timeout and trigger a false interruption warning.
    let keepalive_at = tokio::time::Instant::now().checked_add(keepalive_delay);

    loop {
        let item = if let Some(at) = keepalive_at {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(at) => {
                    // Preserve the original pre-output retry/fallback window for a grace period.
                    // Once the model remains silent beyond it, establish downstream SSE so the
                    // active request does not look hung to Claude or an intermediary.
                    record_codex_stream_commit(&ctx, generation_started, keepalive_delay);
                    if hosted_side_effect_started {
                        replay_state = None;
                    }
                    return LiveStreamStart::Response(remaining_live_stream_response_with_replay(
                        upstream_events,
                        translator,
                        DOWNSTREAM_PING.to_vec(),
                        ctx,
                        turn_id,
                        continuation_capture,
                        provider_started_at,
                        deadline,
                        generation_started,
                        keepalive_delay,
                        replay_state,
                    ));
                },
                item = upstream_events.recv() => item,
            }
        } else {
            upstream_events.recv().await
        };
        let Some(item) = item else {
            break;
        };
        let payload = match item {
            Ok(payload) => payload,
            Err(err) => {
                if !hosted_side_effect_started
                    && (retryable_live_start_codex_error(&err)
                        || err.origin == client::CodexErrorOrigin::WebSocketHandshake)
                {
                    return LiveStreamStart::Retry {
                        error: err,
                        replay_state,
                    };
                }
                abort_continuation(ctx.session_id.as_deref(), turn_id);
                return LiveStreamStart::Response(map_codex_error_to_response(&err));
            }
        };
        hosted_side_effect_started |= events::starts_hosted_side_effect(&payload);
        if hosted_side_effect_started {
            replay_state = None;
        }
        log_native_web_search_phase(&ctx, &payload, provider_started_at);
        record_codex_generation_start(&ctx, &payload, provider_started_at, &mut generation_started);
        if continuation_capture
            .as_mut()
            .is_some_and(|capture| !capture.append(&payload))
        {
            continuation_capture = None;
            abort_continuation(ctx.session_id.as_deref(), turn_id);
        }
        let (chunk, terminal) = match translate_live_stream_payload(&mut translator, &payload, &ctx)
        {
            Ok(result) => result,
            Err(message) => {
                if retryable_live_start_payload(&payload, &message) {
                    let error = live_payload_codex_error(
                        &payload,
                        message,
                        replay_state
                            .as_ref()
                            .map_or(config::CodexTransport::WebSocket, |state| {
                                state.active_transport
                            }),
                    );
                    if !hosted_side_effect_started {
                        return LiveStreamStart::Retry {
                            error,
                            replay_state,
                        };
                    }
                    abort_continuation(ctx.session_id.as_deref(), turn_id);
                    return LiveStreamStart::Response(map_codex_error_to_response(&error));
                }
                abort_continuation(ctx.session_id.as_deref(), turn_id);
                return LiveStreamStart::Response(map_codex_failure_to_response(&message));
            }
        };
        if !chunk.is_empty() {
            replay_state = None;
            record_live_stream_progress(&ctx, &chunk);
            if terminal {
                record_codex_terminal_resolution(
                    &ctx,
                    "authoritative",
                    payload.get("type").and_then(serde_json::Value::as_str),
                );
                update_live_continuation_from_capture(
                    ctx.session_id.as_deref(),
                    turn_id,
                    continuation_capture.as_ref(),
                );
                return LiveStreamStart::Response(single_live_stream_response(chunk));
            }
            return LiveStreamStart::Response(remaining_live_stream_response_with_replay(
                upstream_events,
                translator,
                chunk,
                ctx,
                turn_id,
                continuation_capture,
                provider_started_at,
                deadline,
                generation_started,
                keepalive_delay,
                replay_state,
            ));
        }
        if terminal {
            record_codex_terminal_resolution(
                &ctx,
                "authoritative",
                payload.get("type").and_then(serde_json::Value::as_str),
            );
            update_live_continuation_from_capture(
                ctx.session_id.as_deref(),
                turn_id,
                continuation_capture.as_ref(),
            );
            return LiveStreamStart::Response(empty_live_stream_response());
        }
    }

    let error = client::CodexError {
        status: 0,
        message: "WebSocket connection closed before terminal Codex response event".to_string(),
        detail: Some(websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
        retry_after: None,
        origin: client::CodexErrorOrigin::WebSocket,
    };
    if hosted_side_effect_started {
        abort_continuation(ctx.session_id.as_deref(), turn_id);
        LiveStreamStart::Response(map_codex_error_to_response(&error))
    } else {
        LiveStreamStart::Retry {
            error,
            replay_state,
        }
    }
}

fn codex_generation_event(payload: &serde_json::Value) -> bool {
    !matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some("codex.rate_limits" | "codex.response.metadata" | "keepalive") | None
    )
}

fn record_codex_generation_start(
    ctx: &RequestContext,
    payload: &serde_json::Value,
    provider_started_at: Instant,
    generation_started: &mut bool,
) {
    if *generation_started || !codex_generation_event(payload) {
        return;
    }
    log_codex_first_event(ctx, payload, provider_started_at);
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.generation_started(&ctx.req_id);
    }
    *generation_started = true;
}

fn log_codex_request_configuration(
    ctx: &RequestContext,
    request: &translate::request::ResponsesRequest,
    use_responses_lite: bool,
    native_web_search: bool,
    transport: config::CodexTransportDecision,
    requested_max_tokens: u32,
) {
    let service_tier = request.service_tier.as_ref().map(|tier| match tier {
        ServiceTier::Priority => "priority",
        ServiceTier::Flex => "flex",
    });
    let reasoning_effort = request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort.as_ref())
        .map(ToString::to_string);
    let (top_level_function_tools, top_level_hosted_tools) =
        request
            .tools
            .iter()
            .flatten()
            .fold((0_usize, 0_usize), |(functions, hosted), tool| match tool {
                translate::request::ResponsesTool::Function(_) => (functions + 1, hosted),
                translate::request::ResponsesTool::WebSearch(_) => (functions, hosted + 1),
            });
    let (input_function_tools, input_hosted_tools) =
        request
            .input
            .iter()
            .fold((0_usize, 0_usize), |(functions, hosted), item| {
                let translate::request::ResponsesInputItem::AdditionalTools { tools, .. } = item
                else {
                    return (functions, hosted);
                };
                tools.iter().fold((functions, hosted), |counts, tool| {
                    match tool.get("type").and_then(serde_json::Value::as_str) {
                        Some("function") => (counts.0 + 1, counts.1),
                        Some("web_search") => (counts.0, counts.1 + 1),
                        _ => counts,
                    }
                })
            });
    let function_tool_count = top_level_function_tools + input_function_tools;
    let hosted_tool_count = top_level_hosted_tools + input_hosted_tools;
    let lane_reason = if use_responses_lite {
        "responses_lite"
    } else if native_web_search {
        "hosted_web_search"
    } else if request.parallel_tool_calls && function_tool_count > 0 {
        "parallel_functions"
    } else {
        "full_responses"
    };
    crate::logging::create_logger("codex").info(
        "request_configuration",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(ctx.req_id)),
            ("model".to_string(), serde_json::json!(request.model)),
            ("serviceTier".to_string(), serde_json::json!(service_tier)),
            (
                "reasoningEffort".to_string(),
                serde_json::json!(reasoning_effort),
            ),
            (
                "transport".to_string(),
                serde_json::json!(transport.effective().as_str()),
            ),
            (
                "transportRequested".to_string(),
                serde_json::json!(transport.requested().as_str()),
            ),
            (
                "transportReason".to_string(),
                serde_json::json!(transport.reason()),
            ),
            (
                "responsesLite".to_string(),
                serde_json::json!(use_responses_lite),
            ),
            (
                "nativeWebSearch".to_string(),
                serde_json::json!(native_web_search),
            ),
            (
                "parallelToolCalls".to_string(),
                serde_json::json!(request.parallel_tool_calls),
            ),
            (
                "promptCacheKeyPresent".to_string(),
                serde_json::json!(request.prompt_cache_key.is_some()),
            ),
            (
                "toolChoice".to_string(),
                serde_json::json!(request.tool_choice),
            ),
            (
                "functionToolCount".to_string(),
                serde_json::json!(function_tool_count),
            ),
            (
                "hostedToolCount".to_string(),
                serde_json::json!(hosted_tool_count),
            ),
            (
                "inputFunctionToolCount".to_string(),
                serde_json::json!(input_function_tools),
            ),
            ("laneReason".to_string(), serde_json::json!(lane_reason)),
            (
                "anthropicMaxTokens".to_string(),
                serde_json::json!(requested_max_tokens),
            ),
            (
                "outputBudgetEnforcement".to_string(),
                serde_json::json!("unsupported_by_private_codex_gateway"),
            ),
        ])),
    );
}

fn native_web_search_phase(payload: &serde_json::Value) -> Option<&'static str> {
    match payload.get("type").and_then(|value| value.as_str()) {
        Some("response.web_search_call.in_progress") => Some("in_progress"),
        Some("response.web_search_call.searching") => Some("searching"),
        Some("response.web_search_call.completed") => Some("completed"),
        Some("response.output_item.added")
            if payload
                .pointer("/item/type")
                .and_then(|value| value.as_str())
                == Some("web_search_call") =>
        {
            Some("added")
        }
        Some("response.output_item.done")
            if payload
                .pointer("/item/type")
                .and_then(|value| value.as_str())
                == Some("web_search_call") =>
        {
            Some("done")
        }
        _ => None,
    }
}

fn log_native_web_search_phase(
    ctx: &RequestContext,
    payload: &serde_json::Value,
    provider_started_at: Instant,
) {
    let Some(phase) = native_web_search_phase(payload) else {
        return;
    };
    crate::logging::create_logger("codex").info(
        "native_web_search_phase",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(ctx.req_id)),
            ("phase".to_string(), serde_json::json!(phase)),
            (
                "elapsedMs".to_string(),
                serde_json::json!(provider_started_at.elapsed().as_millis()),
            ),
        ])),
    );
}

fn log_codex_first_event(
    ctx: &RequestContext,
    payload: &serde_json::Value,
    provider_started_at: Instant,
) {
    crate::logging::create_logger("codex").info(
        "upstream_first_event",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(ctx.req_id)),
            (
                "event".to_string(),
                serde_json::json!(payload.get("type").and_then(|value| value.as_str())),
            ),
            (
                "elapsedMs".to_string(),
                serde_json::json!(provider_started_at.elapsed().as_millis()),
            ),
        ])),
    );
}

fn record_codex_stream_commit(
    ctx: &RequestContext,
    generation_started: bool,
    heartbeat_interval: Duration,
) {
    crate::logging::create_logger("codex").info(
        "stream_committed_before_semantic",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(ctx.req_id)),
            (
                "generationStarted".to_string(),
                serde_json::json!(generation_started),
            ),
            ("semanticEmitted".to_string(), serde_json::json!(false)),
            (
                "heartbeatIntervalMs".to_string(),
                serde_json::json!(heartbeat_interval.as_millis()),
            ),
        ])),
    );
    if let Some(traffic) = ctx.traffic.as_deref() {
        traffic.write_json_event(
            "stream-commit",
            &serde_json::json!({
                "committedBeforeSemantic": true,
                "generationStarted": generation_started,
                "semanticEmitted": false,
                "heartbeatIntervalMs": heartbeat_interval.as_millis(),
            }),
        );
    }
}

fn record_codex_terminal_resolution(
    ctx: &RequestContext,
    authority: &'static str,
    source_event: Option<&str>,
) {
    crate::logging::create_logger("codex").info(
        "terminal_resolution",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(ctx.req_id)),
            ("authority".to_string(), serde_json::json!(authority)),
            ("sourceEvent".to_string(), serde_json::json!(source_event)),
        ])),
    );
    if let Some(traffic) = ctx.traffic.as_deref() {
        traffic.write_json_event(
            "terminal-resolution",
            &serde_json::json!({
                "authority": authority,
                "sourceEvent": source_event,
            }),
        );
    }
}

#[derive(Clone, Copy)]
enum NonAuthoritativeClose {
    TransportError,
    StreamClose,
}

fn non_authoritative_close_authority(
    close: NonAuthoritativeClose,
    unsafe_salvaged: bool,
) -> &'static str {
    match (close, unsafe_salvaged) {
        (NonAuthoritativeClose::TransportError, true) => "unsafe_salvaged_after_transport_error",
        (NonAuthoritativeClose::TransportError, false) => "failed_closed_after_transport_error",
        (NonAuthoritativeClose::StreamClose, true) => "unsafe_salvaged_after_stream_close",
        (NonAuthoritativeClose::StreamClose, false) => "failed_closed_after_stream_close",
    }
}

fn translate_live_stream_payload(
    translator: &mut LiveStreamTranslator,
    payload: &serde_json::Value,
    ctx: &RequestContext,
) -> Result<(Vec<u8>, bool), String> {
    let chunk = translator.accept(payload, ctx.traffic.as_deref())?;
    let terminal = is_codex_terminal_event(payload) || translator.is_finished();
    Ok((chunk, terminal))
}

fn record_live_stream_progress(ctx: &RequestContext, chunk: &[u8]) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        let (input_tokens, output_tokens) = usage_from_anthropic_sse(chunk);
        monitor.stream_progress(
            &ctx.req_id,
            chunk.len() as u64,
            count_sse_events(chunk),
            input_tokens,
            output_tokens,
        );
    }
}

fn single_live_stream_response(chunk: Vec<u8>) -> Response {
    event_stream_response(futures_util::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(chunk))
    }))
}

fn empty_live_stream_response() -> Response {
    event_stream_response(futures_util::stream::empty::<Result<Bytes, std::io::Error>>())
}

async fn send_live_chunk_before_deadline(
    tx: &tokio::sync::mpsc::Sender<Result<BudgetedChunk, std::io::Error>>,
    byte_budget: &ByteBudget,
    chunk: Vec<u8>,
    deadline: client::CodexRequestDeadline,
) -> LiveChunkSendOutcome {
    downstream_queue::send_before_deadline(
        tx,
        byte_budget,
        chunk,
        deadline.at(),
        DOWNSTREAM_STALL_TIMEOUT,
        Ok,
    )
    .await
}

fn finish_live_stream_after_downstream_stall(
    terminal_permit: &mut Option<
        tokio::sync::mpsc::OwnedPermit<Result<BudgetedChunk, std::io::Error>>,
    >,
    translator: &mut LiveStreamTranslator,
    ctx: &RequestContext,
    turn_id: Option<u64>,
) {
    abort_continuation(ctx.session_id.as_deref(), turn_id);
    let chunk = translator.error_chunk(
        "Claude Code did not consume the proxy response for 60 seconds",
        "api_error",
        ctx.traffic.as_deref(),
    );
    if !chunk.is_empty() {
        record_live_stream_progress(ctx, &chunk);
        if let Some(permit) = terminal_permit.take() {
            let _ = permit.send(Ok(BudgetedChunk::unbudgeted(chunk)));
        }
    }
}

fn finish_live_stream_at_deadline(
    terminal_permit: &mut Option<
        tokio::sync::mpsc::OwnedPermit<Result<BudgetedChunk, std::io::Error>>,
    >,
    translator: &mut LiveStreamTranslator,
    ctx: &RequestContext,
    turn_id: Option<u64>,
    deadline: client::CodexRequestDeadline,
) {
    abort_continuation(ctx.session_id.as_deref(), turn_id);
    let error =
        client::codex_total_timeout_error(config::CodexTransport::WebSocket, deadline.timeout_ms());
    let chunk = translator.error_chunk(&error.message, "api_error", ctx.traffic.as_deref());
    if !chunk.is_empty() {
        record_live_stream_progress(ctx, &chunk);
        // One channel slot is reserved before ordinary streaming begins, so the
        // terminal deadline event cannot be displaced by a slow/full downstream.
        if let Some(permit) = terminal_permit.take() {
            let _ = permit.send(Ok(BudgetedChunk::unbudgeted(chunk)));
        }
    }
}

enum LiveHeartbeatWait<T> {
    Ready(T),
    Closed,
    Deadline,
    Stalled,
    TooLarge,
}

async fn wait_live_future_with_heartbeat<T>(
    future: impl std::future::Future<Output = T>,
    heartbeat: &mut tokio::time::Interval,
    tx: &tokio::sync::mpsc::Sender<Result<BudgetedChunk, std::io::Error>>,
    byte_budget: &ByteBudget,
    deadline: client::CodexRequestDeadline,
) -> LiveHeartbeatWait<T> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            _ = tx.closed() => return LiveHeartbeatWait::Closed,
            _ = tokio::time::sleep_until(deadline.at()) => {
                return LiveHeartbeatWait::Deadline;
            }
            _ = heartbeat.tick() => {
                match send_live_chunk_before_deadline(
                    tx,
                    byte_budget,
                    DOWNSTREAM_PING.to_vec(),
                    deadline,
                ).await {
                    LiveChunkSendOutcome::Sent => {}
                    LiveChunkSendOutcome::Closed => return LiveHeartbeatWait::Closed,
                    LiveChunkSendOutcome::Deadline => return LiveHeartbeatWait::Deadline,
                    LiveChunkSendOutcome::Stalled => return LiveHeartbeatWait::Stalled,
                    LiveChunkSendOutcome::TooLarge => return LiveHeartbeatWait::TooLarge,
                }
            }
            result = &mut future => return LiveHeartbeatWait::Ready(result),
        }
    }
}

enum LiveReopenOutcome {
    Opened(websocket::CodexWebSocketEventReceiver),
    Rejected(client::CodexError),
    Closed,
    Deadline,
    Stalled,
    TooLarge,
}

async fn reopen_live_stream_after_commit(
    mut error: client::CodexError,
    replay_state: &mut LiveReplayState,
    ctx: &RequestContext,
    heartbeat: &mut tokio::time::Interval,
    tx: &tokio::sync::mpsc::Sender<Result<BudgetedChunk, std::io::Error>>,
    byte_budget: &ByteBudget,
    deadline: client::CodexRequestDeadline,
) -> LiveReopenOutcome {
    loop {
        let failed_transport = replay_state.active_transport;
        let status = error.status;
        let replay_safety = error.replay_safety();
        let wait_ms = match replay_state.plan_retry(ctx, error) {
            Ok(wait_ms) => {
                record_post_ping_replay_decision(
                    ctx,
                    "post_ping_replay_scheduled",
                    failed_transport,
                    status,
                    replay_safety,
                    Some(wait_ms),
                );
                wait_ms
            }
            Err(error) => {
                record_post_ping_replay_decision(
                    ctx,
                    "post_ping_replay_rejected",
                    failed_transport,
                    status,
                    replay_safety,
                    None,
                );
                return LiveReopenOutcome::Rejected(error);
            }
        };
        if wait_ms > 0 {
            match wait_live_future_with_heartbeat(
                tokio::time::sleep(Duration::from_millis(wait_ms)),
                heartbeat,
                tx,
                byte_budget,
                deadline,
            )
            .await
            {
                LiveHeartbeatWait::Ready(()) => {}
                LiveHeartbeatWait::Closed => return LiveReopenOutcome::Closed,
                LiveHeartbeatWait::Deadline => return LiveReopenOutcome::Deadline,
                LiveHeartbeatWait::Stalled => return LiveReopenOutcome::Stalled,
                LiveHeartbeatWait::TooLarge => return LiveReopenOutcome::TooLarge,
            }
        }
        if tx.is_closed() {
            return LiveReopenOutcome::Closed;
        }
        match wait_live_future_with_heartbeat(
            replay_state.open_upstream(ctx, deadline),
            heartbeat,
            tx,
            byte_budget,
            deadline,
        )
        .await
        {
            LiveHeartbeatWait::Ready(Ok(events)) => {
                return LiveReopenOutcome::Opened(events);
            }
            LiveHeartbeatWait::Ready(Err(next_error)) => error = next_error,
            LiveHeartbeatWait::Closed => return LiveReopenOutcome::Closed,
            LiveHeartbeatWait::Deadline => return LiveReopenOutcome::Deadline,
            LiveHeartbeatWait::Stalled => return LiveReopenOutcome::Stalled,
            LiveHeartbeatWait::TooLarge => return LiveReopenOutcome::TooLarge,
        }
    }
}

fn record_post_ping_replay_decision(
    ctx: &RequestContext,
    event: &'static str,
    transport: config::CodexTransport,
    status: u16,
    replay_safety: crate::retry::ReplaySafety,
    delay_ms: Option<u64>,
) {
    let mut fields = serde_json::Map::from_iter([
        ("reqId".into(), serde_json::json!(ctx.req_id)),
        ("transport".into(), serde_json::json!(transport.as_str())),
        ("status".into(), serde_json::json!(status)),
        (
            "replaySafety".into(),
            serde_json::json!(replay_safety.as_str()),
        ),
    ]);
    if let Some(delay_ms) = delay_ms {
        fields.insert("delayMs".into(), serde_json::json!(delay_ms));
    }
    crate::logging::create_logger("codex").warn(event, Some(fields));
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn remaining_live_stream_response(
    upstream_events: websocket::CodexWebSocketEventReceiver,
    translator: LiveStreamTranslator,
    first_chunk: Vec<u8>,
    ctx: RequestContext,
    turn_id: Option<u64>,
    continuation_capture: Option<LiveContinuationCapture>,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    generation_started: bool,
    heartbeat_interval: Duration,
) -> Response {
    remaining_live_stream_response_with_heartbeat(
        upstream_events,
        translator,
        first_chunk,
        ctx,
        turn_id,
        continuation_capture,
        provider_started_at,
        deadline,
        generation_started,
        heartbeat_interval,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn remaining_live_stream_response_with_heartbeat(
    upstream_events: websocket::CodexWebSocketEventReceiver,
    translator: LiveStreamTranslator,
    first_chunk: Vec<u8>,
    ctx: RequestContext,
    turn_id: Option<u64>,
    continuation_capture: Option<LiveContinuationCapture>,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    generation_started: bool,
    heartbeat_interval: Duration,
) -> Response {
    remaining_live_stream_response_with_replay(
        upstream_events,
        translator,
        first_chunk,
        ctx,
        turn_id,
        continuation_capture,
        provider_started_at,
        deadline,
        generation_started,
        heartbeat_interval,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn remaining_live_stream_response_with_replay(
    mut upstream_events: websocket::CodexWebSocketEventReceiver,
    mut translator: LiveStreamTranslator,
    first_chunk: Vec<u8>,
    ctx: RequestContext,
    turn_id: Option<u64>,
    mut continuation_capture: Option<LiveContinuationCapture>,
    provider_started_at: Instant,
    deadline: client::CodexRequestDeadline,
    mut generation_started: bool,
    heartbeat_interval: Duration,
    mut replay_state: Option<Box<LiveReplayState>>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<BudgetedChunk, std::io::Error>>(
        LIVE_EVENT_CHANNEL_CAPACITY,
    );
    let byte_budget = ByteBudget::new(MAX_DOWNSTREAM_QUEUE_BYTES);
    tokio::spawn(async move {
        let mut terminal_permit = match tx.clone().try_reserve_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                abort_continuation(ctx.session_id.as_deref(), turn_id);
                return;
            }
        };
        macro_rules! send_chunk_or_stop {
            ($chunk:expr) => {
                match send_live_chunk_before_deadline(&tx, &byte_budget, $chunk, deadline).await {
                    LiveChunkSendOutcome::Sent => {}
                    LiveChunkSendOutcome::Closed => {
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                        return;
                    }
                    LiveChunkSendOutcome::Deadline => {
                        finish_live_stream_at_deadline(
                            &mut terminal_permit,
                            &mut translator,
                            &ctx,
                            turn_id,
                            deadline,
                        );
                        return;
                    }
                    LiveChunkSendOutcome::Stalled => {
                        finish_live_stream_after_downstream_stall(
                            &mut terminal_permit,
                            &mut translator,
                            &ctx,
                            turn_id,
                        );
                        return;
                    }
                    LiveChunkSendOutcome::TooLarge => {
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                        let chunk = translator.error_chunk(
                            "Codex translated stream chunk exceeded the queue byte limit",
                            "api_error",
                            ctx.traffic.as_deref(),
                        );
                        if let Some(permit) = terminal_permit.take() {
                            let _ = permit.send(Ok(BudgetedChunk::unbudgeted(chunk)));
                        }
                        return;
                    }
                }
            };
        }

        send_chunk_or_stop!(first_chunk);
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + heartbeat_interval,
            heartbeat_interval,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        macro_rules! reopen_or_stop {
            ($error:expr) => {{
                drop(continuation_capture.take());
                drop(upstream_events);
                let outcome = reopen_live_stream_after_commit(
                    $error,
                    replay_state
                        .as_mut()
                        .expect("open replay gate must retain replay state"),
                    &ctx,
                    &mut heartbeat,
                    &tx,
                    &byte_budget,
                    deadline,
                )
                .await;
                match outcome {
                    LiveReopenOutcome::Opened(events) => {
                        upstream_events = events;
                        let state = replay_state
                            .as_ref()
                            .expect("opened replay must retain replay state");
                        translator = state.fresh_translator();
                        continuation_capture = state.fresh_continuation_capture();
                        if continuation_capture.is_none() {
                            abort_continuation(ctx.session_id.as_deref(), turn_id);
                        }
                        continue;
                    }
                    LiveReopenOutcome::Rejected(error) => {
                        drop(replay_state.take());
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                        record_codex_terminal_resolution(
                            &ctx,
                            non_authoritative_close_authority(
                                NonAuthoritativeClose::TransportError,
                                false,
                            ),
                            None,
                        );
                        let error_type = codex_stream_error_type(&error);
                        let chunk = translator.error_chunk(
                            codex_error_message(&error),
                            error_type,
                            ctx.traffic.as_deref(),
                        );
                        if !chunk.is_empty() {
                            record_live_stream_progress(&ctx, &chunk);
                            send_chunk_or_stop!(chunk);
                        }
                        return;
                    }
                    LiveReopenOutcome::Closed => {
                        drop(replay_state.take());
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                        return;
                    }
                    LiveReopenOutcome::Deadline => {
                        drop(replay_state.take());
                        finish_live_stream_at_deadline(
                            &mut terminal_permit,
                            &mut translator,
                            &ctx,
                            turn_id,
                            deadline,
                        );
                        return;
                    }
                    LiveReopenOutcome::Stalled => {
                        drop(replay_state.take());
                        finish_live_stream_after_downstream_stall(
                            &mut terminal_permit,
                            &mut translator,
                            &ctx,
                            turn_id,
                        );
                        return;
                    }
                    LiveReopenOutcome::TooLarge => {
                        drop(replay_state.take());
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                        let chunk = translator.error_chunk(
                            "Codex translated stream chunk exceeded the queue byte limit",
                            "api_error",
                            ctx.traffic.as_deref(),
                        );
                        if let Some(permit) = terminal_permit.take() {
                            let _ = permit.send(Ok(BudgetedChunk::unbudgeted(chunk)));
                        }
                        return;
                    }
                }
            }};
        }
        loop {
            if tokio::time::Instant::now() >= deadline.at() {
                finish_live_stream_at_deadline(
                    &mut terminal_permit,
                    &mut translator,
                    &ctx,
                    turn_id,
                    deadline,
                );
                return;
            }
            let item = tokio::select! {
                biased;
                _ = tx.closed() => {
                    abort_continuation(ctx.session_id.as_deref(), turn_id);
                    return;
                }
                _ = tokio::time::sleep_until(deadline.at()) => {
                    finish_live_stream_at_deadline(
                        &mut terminal_permit,
                        &mut translator,
                        &ctx,
                        turn_id,
                        deadline,
                    );
                    return;
                }
                _ = heartbeat.tick() => {
                    // Anthropic ping events keep event-aware clients and intermediaries connected.
                    // They bypass semantic translation, monitoring, and traffic capture.
                    send_chunk_or_stop!(DOWNSTREAM_PING.to_vec());
                    continue;
                }
                item = upstream_events.recv() => item,
            };
            let Some(item) = item else {
                break;
            };
            match item {
                Ok(payload) => {
                    if events::starts_hosted_side_effect(&payload) {
                        replay_state = None;
                    }
                    log_native_web_search_phase(&ctx, &payload, provider_started_at);
                    record_codex_generation_start(
                        &ctx,
                        &payload,
                        provider_started_at,
                        &mut generation_started,
                    );
                    if continuation_capture
                        .as_mut()
                        .is_some_and(|capture| !capture.append(&payload))
                    {
                        continuation_capture = None;
                        abort_continuation(ctx.session_id.as_deref(), turn_id);
                    }
                    let (chunk, terminal) =
                        match translate_live_stream_payload(&mut translator, &payload, &ctx) {
                            Ok(result) => result,
                            Err(message) => {
                                if retryable_live_start_payload(&payload, &message)
                                    && replay_state.is_some()
                                {
                                    let transport = replay_state
                                        .as_ref()
                                        .expect("checked replay state")
                                        .active_transport;
                                    let error =
                                        live_payload_codex_error(&payload, message, transport);
                                    reopen_or_stop!(error);
                                }
                                abort_continuation(ctx.session_id.as_deref(), turn_id);
                                let error_type = codex_stream_payload_error_type(&payload);
                                let chunk = translator.error_chunk(
                                    &message,
                                    error_type,
                                    ctx.traffic.as_deref(),
                                );
                                if !chunk.is_empty() {
                                    record_live_stream_progress(&ctx, &chunk);
                                    send_chunk_or_stop!(chunk);
                                }
                                return;
                            }
                        };
                    if !chunk.is_empty() {
                        replay_state = None;
                        record_live_stream_progress(&ctx, &chunk);
                        send_chunk_or_stop!(chunk);
                    }
                    if terminal {
                        record_codex_terminal_resolution(
                            &ctx,
                            "authoritative",
                            payload.get("type").and_then(serde_json::Value::as_str),
                        );
                        update_live_continuation_from_capture(
                            ctx.session_id.as_deref(),
                            turn_id,
                            continuation_capture.as_ref(),
                        );
                        return;
                    }
                }
                Err(err) => {
                    if replay_state.is_some() {
                        reopen_or_stop!(err);
                    }
                    drop(replay_state.take());
                    abort_continuation(ctx.session_id.as_deref(), turn_id);
                    let chunk = translator.finish_after_closed_completed_tool_call(
                        config::codex_unsafe_salvage_tool_call_on_close(),
                        ctx.traffic.as_deref(),
                    );
                    if !chunk.is_empty() {
                        record_codex_terminal_resolution(
                            &ctx,
                            non_authoritative_close_authority(
                                NonAuthoritativeClose::TransportError,
                                true,
                            ),
                            None,
                        );
                        record_live_stream_progress(&ctx, &chunk);
                        send_chunk_or_stop!(chunk);
                        return;
                    }
                    record_codex_terminal_resolution(
                        &ctx,
                        non_authoritative_close_authority(
                            NonAuthoritativeClose::TransportError,
                            false,
                        ),
                        None,
                    );
                    let error_type = codex_stream_error_type(&err);
                    let chunk = translator.error_chunk(
                        codex_error_message(&err),
                        error_type,
                        ctx.traffic.as_deref(),
                    );
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        send_chunk_or_stop!(chunk);
                    }
                    return;
                }
            }
        }

        abort_continuation(ctx.session_id.as_deref(), turn_id);
        let chunk = translator.finish_after_closed_completed_tool_call(
            config::codex_unsafe_salvage_tool_call_on_close(),
            ctx.traffic.as_deref(),
        );
        if !chunk.is_empty() {
            record_codex_terminal_resolution(
                &ctx,
                non_authoritative_close_authority(NonAuthoritativeClose::StreamClose, true),
                None,
            );
            record_live_stream_progress(&ctx, &chunk);
            send_chunk_or_stop!(chunk);
            return;
        }
        record_codex_terminal_resolution(
            &ctx,
            non_authoritative_close_authority(NonAuthoritativeClose::StreamClose, false),
            None,
        );
        let chunk = translator.error_chunk(
            "WebSocket connection closed before terminal Codex response event",
            "api_error",
            ctx.traffic.as_deref(),
        );
        if !chunk.is_empty() {
            record_live_stream_progress(&ctx, &chunk);
            send_chunk_or_stop!(chunk);
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| {
            let item = item.map(BudgetedChunk::into_bytes);
            (item, rx)
        })
    });
    event_stream_response(stream)
}

fn event_stream_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, Body::from_stream(stream)).into_response()
}

fn is_codex_terminal_event(payload: &serde_json::Value) -> bool {
    events::CodexTerminalKind::from_payload(payload).is_some()
}

fn retryable_live_start_codex_error(err: &client::CodexError) -> bool {
    if matches!(
        err.detail.as_deref(),
        Some(
            client::CODEX_TOTAL_TIMEOUT_DETAIL
                | dispatch_budget::CODEX_DISPATCH_BUDGET_DETAIL
                | websocket::WEBSOCKET_TERMINAL_BARRIER_DETAIL
        )
    ) {
        return false;
    }
    if err.origin == client::CodexErrorOrigin::WebSocketHandshake {
        return err.status == 0 || matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529);
    }
    if websocket::is_retryable_transport_detail(err.detail.as_deref()) {
        return true;
    }
    matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529)
        || (err.origin == client::CodexErrorOrigin::Http
            && err.status == 0
            && matches!(
                err.detail.as_deref(),
                Some("http_response_headers" | "http_response_body")
            ))
        || (err.status == 0 && retryable_live_message(codex_error_message(err)))
}

fn replayable_live_start_codex_error(err: &client::CodexError) -> bool {
    retryable_live_start_codex_error(err) && err.replay_safety().permits_model_replay()
}

fn max_live_retries(err: &client::CodexError) -> u32 {
    if err.status == 0
        && matches!(
            err.origin,
            client::CodexErrorOrigin::WebSocket | client::CodexErrorOrigin::WebSocketHandshake
        )
    {
        MAX_LIVE_TRANSPORT_RETRIES
    } else {
        MAX_RETRYABLE_LIVE_STREAM_RETRIES
    }
}

fn is_missing_previous_response_error(err: &client::CodexError) -> bool {
    err.detail.as_deref() == Some("previous_response_not_found")
}

fn drop_live_continuation_for_retry(continuation: &mut Option<ContinuationCandidate>) -> bool {
    if continuation
        .as_ref()
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    if let Some(candidate) = continuation.as_mut() {
        candidate.previous_response_id = None;
        candidate.input_delta = None;
        candidate.disabled_reason = Some("full_context_retry".to_string());
    }
    true
}

fn retryable_live_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "overloaded",
        "rate limit",
        "you can retry your request",
        "temporarily unavailable",
        "timed out",
        "connection closed",
        "connection reset",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn retryable_live_start_payload(payload: &serde_json::Value, _message: &str) -> bool {
    events::classify_event_failure(payload).is_some_and(|failure| failure.retryable())
}

fn retry_after_from_live_payload(payload: &serde_json::Value) -> Option<String> {
    events::classify_event_failure(payload).and_then(|failure| failure.retry_after)
}

fn live_payload_codex_error(
    payload: &serde_json::Value,
    message: String,
    transport: config::CodexTransport,
) -> client::CodexError {
    let lower_message = message.to_ascii_lowercase();
    let status = websocket::event_error_status(payload).unwrap_or_else(|| {
        let error = payload
            .get("error")
            .or_else(|| payload.get("response").and_then(|value| value.get("error")));
        let overloaded = error.is_some_and(|error| {
            error.get("code").and_then(|value| value.as_str()) == Some("overloaded_error")
                || error.get("type").and_then(|value| value.as_str()) == Some("overloaded_error")
        });
        if payload.get("type").and_then(|value| value.as_str()) == Some("codex.rate_limits")
            || lower_message.contains("rate limit")
        {
            429
        } else if overloaded || lower_message.contains("overloaded") {
            529
        } else {
            503
        }
    });
    client::CodexError {
        status,
        message: message.clone(),
        detail: Some(message),
        retry_after: retry_after_from_live_payload(payload),
        origin: if matches!(transport, config::CodexTransport::Http) {
            client::CodexErrorOrigin::UpstreamHttp
        } else {
            client::CodexErrorOrigin::WebSocket
        },
    }
}

fn codex_stream_payload_error_type(payload: &serde_json::Value) -> &'static str {
    match events::classify_event_failure(payload).map(|failure| failure.kind) {
        Some(events::CodexFailureKind::RateLimit) => "rate_limit_error",
        Some(events::CodexFailureKind::Overloaded) => "overloaded_error",
        _ => "api_error",
    }
}

fn codex_stream_error_type(err: &client::CodexError) -> &'static str {
    match err.status {
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ if codex_error_message(err)
            .to_lowercase()
            .contains("overloaded") =>
        {
            "overloaded_error"
        }
        _ => "api_error",
    }
}

fn update_continuation_from_finish_metadata(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    request_body: &translate::request::ResponsesRequest,
    finish: Option<&FinishMetadata>,
) {
    if session_id.is_none() || turn_id.is_none() {
        return;
    }
    // The upstream response id refers to the raw structured JSON, while the
    // SchemaBridge may remove synthetic nulls before Claude Code records its
    // transcript. Reusing that id against a normalized transcript would make
    // the continuation prefix semantically inconsistent. Keep these turns on
    // the safe full-context path.
    if request_body.schema_bridge.is_some() {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "reason".to_string(),
            serde_json::Value::String("structured_output_schema_bridge".to_string()),
        );
        crate::logging::create_logger("codex")
            .debug("Codex continuation was not recorded", Some(fields));
        abort_continuation(session_id, turn_id);
        return;
    }
    match finish {
        Some(finish) if finish.continuation_eligible => {
            record_continuation(
                session_id,
                turn_id,
                request_body,
                finish.response_id.as_deref(),
                &finish.output_items,
            );
        }
        _ => abort_continuation(session_id, turn_id),
    }
}

fn update_continuation_from_upstream(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    request_body: &translate::request::ResponsesRequest,
    upstream_body: &[u8],
) {
    if session_id.is_none() || turn_id.is_none() || request_body.schema_bridge.is_some() {
        update_continuation_from_finish_metadata(session_id, turn_id, request_body, None);
        return;
    }
    let tool_policy = ToolCallPolicy::from_request(request_body);
    let finish = finish_metadata_from_upstream_with_tool_policy(upstream_body, &tool_policy)
        .ok()
        .flatten();
    update_continuation_from_finish_metadata(session_id, turn_id, request_body, finish.as_ref());
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn map_codex_error_to_response(err: &client::CodexError) -> Response {
    let message = codex_error_message(err);
    if is_context_window_overflow(message) {
        return map_codex_failure_to_response(message);
    }

    match err.status {
        400 if err.detail.as_deref()
            == Some(client::CODEX_WEBSOCKET_ENV_PROXY_UNSUPPORTED_DETAIL) =>
        {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                err.message.as_str(),
            )
        }
        401 | 403 => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.detail.as_deref().unwrap_or("Authentication failed"),
        ),
        429 => {
            let retry_after = err.retry_after.as_deref().unwrap_or("5");
            let resp = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &err.message,
            );
            let headers = [(http::header::RETRY_AFTER, retry_after)];
            (headers, resp).into_response()
        }
        status @ (500 | 502 | 503 | 504 | 529)
            if matches!(
                err.origin,
                client::CodexErrorOrigin::BufferedHttp
                    | client::CodexErrorOrigin::BufferedWebSocket
            ) =>
        {
            let response = json_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                if status == 529 {
                    "overloaded_error"
                } else {
                    "api_error"
                },
                codex_error_message(err),
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            codex_error_message(err),
        ),
    }
}

fn map_codex_failure_to_response(message: &str) -> Response {
    if is_context_window_overflow(message) {
        json_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", message)
    } else {
        json_error(StatusCode::BAD_GATEWAY, "api_error", message)
    }
}

fn is_context_window_overflow(message: &str) -> bool {
    message.to_ascii_lowercase().contains("context window")
}

fn codex_error_message(err: &client::CodexError) -> &str {
    if websocket::is_retryable_transport_detail(err.detail.as_deref()) {
        return err.message.as_str();
    }
    err.detail.as_deref().unwrap_or({
        if err.status == 0 {
            err.message.as_str()
        } else {
            "Upstream error"
        }
    })
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CodexCli;

impl CliHandlers for CodexCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        let tokens = run_browser_login()?;
        let store = file_store();
        let manager = CodexAuthManager::new(store);
        let saved = manager.persist_initial_tokens(&tokens)?;
        print!(
            "{}",
            format_auth_saved_output(&manager.store.auth_path(), saved.account_id.as_deref())
        );
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        let tokens = DeviceAuthClient::new().run()?;
        let store = file_store();
        let manager = CodexAuthManager::new(store);
        let saved = manager.persist_initial_tokens(&tokens)?;
        print!(
            "{}",
            format_auth_saved_output(&manager.store.auth_path(), saved.account_id.as_deref())
        );
        Ok(())
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        let stored = store.load_auth()?;
        match stored {
            Some(auth) => {
                println!(
                    "Account: {}",
                    auth.account_id.as_deref().unwrap_or("(none)")
                );
                println!("{}", format_expiry(auth.expires, now_ms()));
                println!("Storage: {}", store.auth_path());
                Ok(())
            }
            None => {
                anyhow::bail!("Not authenticated");
            }
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        store.clear_auth_exclusive()?;
        println!("Logged out");
        Ok(())
    }
}

pub(crate) static CODEX_CLI: CodexCli = CodexCli;

// ---------------------------------------------------------------------------
// CLI helpers
// ---------------------------------------------------------------------------

fn format_expiry(expires: u64, now: u64) -> String {
    let remaining = (i128::from(expires) - i128::from(now)).div_euclid(1000);
    let iso = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(expires) * 1_000_000)
        .ok()
        .and_then(|dt| {
            let fmt = time::format_description::parse_borrowed::<2>(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
            )
            .ok()?;
            dt.format(&fmt).ok()
        })
        .unwrap_or_else(|| "invalid".to_string());
    format!("Expires: {iso} (in {remaining}s)")
}

fn format_auth_saved_output(auth_path: &str, account_id: Option<&str>) -> String {
    let mut out = format!("Auth saved in {auth_path}\n");
    if let Some(account_id) = account_id {
        out.push_str(&format!("Account: {account_id}\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::time::Duration;

    #[test]
    fn stream_heartbeat_is_clamped_to_safe_bounds() {
        assert_eq!(stream_heartbeat_duration(1), MIN_STREAM_HEARTBEAT);
        assert_eq!(stream_heartbeat_duration(5_000), DEFAULT_STREAM_HEARTBEAT);
        assert_eq!(stream_heartbeat_duration(120_000), MAX_STREAM_HEARTBEAT);
    }

    #[tokio::test]
    async fn count_tokens_admission_errors_map_to_retryable_503_or_worker_500() {
        let saturated = token_count_admission::run_with(
            Arc::new(tokio::sync::Semaphore::new(0)),
            Duration::from_millis(10),
            || 1_u64,
        )
        .await
        .expect_err("a pool with no permits must time out");
        assert!(matches!(saturated, TokenCountAdmissionError::QueueTimeout));
        assert_eq!(
            map_codex_count_tokens_error(saturated).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        let failed = token_count_admission::run_with(
            Arc::new(tokio::sync::Semaphore::new(1)),
            Duration::from_secs(1),
            || -> u64 { panic!("synthetic token-count worker failure") },
        )
        .await
        .expect_err("a panicking blocking worker must return JoinError");
        assert!(matches!(failed, TokenCountAdmissionError::WorkerFailed(_)));
        assert_eq!(
            map_codex_count_tokens_error(failed).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn non_authoritative_close_records_fail_closed_or_explicitly_unsafe_authority() {
        assert_eq!(
            non_authoritative_close_authority(NonAuthoritativeClose::TransportError, false),
            "failed_closed_after_transport_error"
        );
        assert_eq!(
            non_authoritative_close_authority(NonAuthoritativeClose::StreamClose, false),
            "failed_closed_after_stream_close"
        );
        assert_eq!(
            non_authoritative_close_authority(NonAuthoritativeClose::TransportError, true),
            "unsafe_salvaged_after_transport_error"
        );
        assert_eq!(
            non_authoritative_close_authority(NonAuthoritativeClose::StreamClose, true),
            "unsafe_salvaged_after_stream_close"
        );
    }

    fn live_test_context() -> RequestContext {
        RequestContext {
            req_id: "live-retry-test".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        }
    }

    fn live_test_request() -> translate::request::ResponsesRequest {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .unwrap();
        translate_request_with_overrides(
            &body,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
            },
            TranslationOverrides::default(),
        )
        .unwrap()
    }

    fn structured_continuation_test_request() -> translate::request::ResponsesRequest {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "return status"}],
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {
                        "status": {"type": "string"},
                        "note": {"type": "string"}
                    },
                    "required": ["status"],
                    "additionalProperties": false
                }
            }}
        }))
        .unwrap();
        translate_request_with_overrides(
            &body,
            TranslateOptions {
                session_id: Some("structured-continuation-test".to_string()),
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: false,
            },
            TranslationOverrides::default(),
        )
        .unwrap()
    }

    fn structured_validation_test_bridge() -> Arc<translate::schema_bridge::SchemaBridge> {
        Arc::new(
            translate::schema_bridge::SchemaBridge::build(&serde_json::json!({
                "type":"object",
                "properties":{
                    "ok":{"type":"boolean"},
                    "reason":{"type":"string"}
                },
                "required":["ok"],
                "additionalProperties":false
            }))
            .unwrap(),
        )
    }

    fn invalid_structured_stream_events() -> Vec<serde_json::Value> {
        let text = r#"{"ok":"wrong","reason":null}"#;
        vec![
            serde_json::json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"msg_structured"}
            }),
            serde_json::json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "item_id":"msg_structured",
                "delta":text
            }),
            serde_json::json!({
                "type":"response.output_text.done",
                "output_index":0,
                "item_id":"msg_structured",
                "text":text
            }),
            serde_json::json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"message","id":"msg_structured"}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{"id":"resp_structured","usage":{}}
            }),
        ]
    }

    #[tokio::test]
    async fn structured_validation_failure_before_first_chunk_returns_http_error() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        for event in invalid_structured_stream_events() {
            tx.send(Ok(event)).await.unwrap();
        }
        drop(tx);

        let start = live_stream_response_once_with_schema(
            rx,
            "msg_structured".to_string(),
            "gpt-5.6-sol",
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(1_000),
            Duration::from_secs(1),
            Some(structured_validation_test_bridge()),
        )
        .await;
        let LiveStreamStart::Response(response) = start else {
            panic!("invalid structured output must not be retried");
        };
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("wrong"));
        assert!(!body.contains("message_stop"));
    }

    #[tokio::test]
    async fn structured_validation_failure_after_heartbeat_is_in_band_sse_error() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let start = live_stream_response_once_with_schema(
            rx,
            "msg_structured".to_string(),
            "gpt-5.6-sol",
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(1_000),
            Duration::from_millis(1),
            Some(structured_validation_test_bridge()),
        )
        .await;
        let LiveStreamStart::Response(response) = start else {
            panic!("heartbeat should commit a downstream response");
        };
        assert_eq!(response.status(), StatusCode::OK);

        for event in invalid_structured_stream_events() {
            tx.send(Ok(event)).await.unwrap();
        }
        drop(tx);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("event: ping"));
        assert!(body.contains("event: error"));
        assert!(!body.contains("wrong"));
        assert!(!body.contains("message_stop"));
    }

    #[tokio::test]
    async fn structured_output_turn_does_not_record_inconsistent_continuation() {
        let _state_guard = CODEX_STATE_TEST_LOCK.lock().await;
        let session_id = "structured-continuation-test";
        let request = structured_continuation_test_request();
        assert!(request.schema_bridge.is_some());
        let candidate = continuation_candidate(Some(session_id), &request, true);
        let turn_id = candidate
            .turn_id
            .expect("continuation turn should register");
        let upstream = [
            serde_json::json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"msg_up"}
            }),
            serde_json::json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "delta":"{\"status\":\"ok\",\"note\":null}"
            }),
            serde_json::json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"message","id":"msg_up"}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{"id":"resp_structured","usage":{}}
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();

        update_continuation_from_upstream(
            Some(session_id),
            Some(turn_id),
            &request,
            upstream.as_bytes(),
        );
        assert!(!continuation::has_continuation_for_tests(session_id));
    }

    #[test]
    fn live_continuation_capture_only_exists_for_registered_turns() {
        let request = live_test_request();
        let disabled = continuation_candidate(Some("disabled-session"), &request, false);

        assert!(disabled.turn_id.is_none());
        assert!(LiveContinuationCapture::for_turn(disabled.turn_id, &request, None).is_none());

        let mut capture = LiveContinuationCapture::for_turn(Some(7), &request, None)
            .expect("a registered continuation turn must retain replay metadata");
        assert_eq!(capture.request_body.model, request.model);
        assert!(capture.upstream_sse_body.is_empty());

        assert!(capture.append(&serde_json::json!({
            "type": "response.created",
            "response": {"id": "resp_1"}
        })));
        assert!(!capture.upstream_sse_body.is_empty());
        assert!(String::from_utf8_lossy(&capture.upstream_sse_body).contains("response.created"));
    }

    #[test]
    fn live_continuation_capture_uses_an_independent_byte_budget() {
        let request = live_test_request();
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let lease = RequestByteLease::new(budget.clone().try_acquire_owned().unwrap(), 1);
        let capture = LiveContinuationCapture::for_turn(Some(7), &request, Some(&lease))
            .expect("registered turn");
        drop(lease);

        assert!(budget.clone().try_acquire_owned().is_ok());
        drop(capture);
    }

    #[test]
    fn oversized_request_skips_live_continuation_capture_and_releases_its_lease() {
        let request = live_test_request();
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let lease = RequestByteLease::new(
            budget.clone().try_acquire_owned().unwrap(),
            continuation::MAX_SESSION_TRANSCRIPT_BYTES as usize + 1,
        );

        assert!(LiveContinuationCapture::for_turn(Some(7), &request, Some(&lease)).is_none());
        drop(lease);
        assert!(budget.try_acquire_owned().is_ok());
    }

    fn heartbeat_test_error() -> client::CodexError {
        client::CodexError {
            status: 0,
            message: "WebSocket heartbeat timed out after 10000ms without a Pong".to_string(),
            detail: Some(websocket::WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL.to_string()),
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocket,
        }
    }

    fn scripted_event_receiver(
        payloads: impl IntoIterator<Item = Result<serde_json::Value, client::CodexError>>,
    ) -> websocket::CodexWebSocketEventReceiver {
        let payloads: Vec<_> = payloads.into_iter().collect();
        let (tx, rx) = tokio::sync::mpsc::channel(payloads.len().max(1));
        for payload in payloads {
            tx.try_send(payload).unwrap();
        }
        drop(tx);
        rx
    }

    fn successful_text_receiver(
        text: &str,
        response_id: &str,
    ) -> websocket::CodexWebSocketEventReceiver {
        scripted_event_receiver([
            Ok(serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"message", "id":"msg_replayed"}
            })),
            Ok(serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": "msg_replayed",
                "delta": text
            })),
            Ok(serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type":"message", "id":"msg_replayed"}
            })),
            Ok(serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "status": "completed",
                    "incomplete_details": null,
                    "usage": {"input_tokens": 4, "output_tokens": 2}
                }
            })),
        ])
    }

    fn retryable_failure_payload() -> serde_json::Value {
        serde_json::json!({
            "type": "response.failed",
            "retry_after_seconds": 0,
            "response": {
                "status": "failed",
                "error": {
                    "status": 503,
                    "type": "server_error",
                    "message": "temporarily unavailable; you can retry your request"
                }
            }
        })
    }

    fn scripted_replay_state(
        request: translate::request::ResponsesRequest,
        continuation: ContinuationCandidate,
        attempts: std::collections::VecDeque<
            Result<websocket::CodexWebSocketEventReceiver, client::CodexError>,
        >,
    ) -> (
        LiveReplayState,
        dispatch_budget::CodexDispatchBudget,
        ScriptedLiveAttemptQueue,
    ) {
        let state = LiveReplayState::new(
            Arc::new(CodexHttpClient::new()),
            "msg_replay_gate".to_string(),
            "gpt-5.6-sol".to_string(),
            request,
            None,
            continuation,
        )
        .with_scripted_attempts(attempts);
        state
            .dispatch_budget
            .reserve_model()
            .expect("the initial scripted attempt must fit the shared budget");
        let budget = state.dispatch_budget.clone();
        let attempts = state
            .scripted_attempts
            .as_ref()
            .expect("scripted attempts were configured")
            .clone();
        (state, budget, attempts)
    }

    async fn start_scripted_heartbeat_response(
        upstream_events: websocket::CodexWebSocketEventReceiver,
        state: LiveReplayState,
        ctx: RequestContext,
        keepalive_delay: Duration,
    ) -> Response {
        let turn_id = state.turn_id();
        let continuation_capture = state.fresh_continuation_capture();
        let message_id = state.message_id.clone();
        let model = state.model.clone();
        let schema_bridge = state.schema_bridge.clone();
        let tool_policy = state.tool_policy.clone();
        match live_stream_response_once_with_schema_and_tool_policy(
            upstream_events,
            message_id,
            &model,
            ctx,
            turn_id,
            continuation_capture,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(3_000),
            keepalive_delay,
            schema_bridge,
            tool_policy,
            Some(Box::new(state)),
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => {
                panic!("a silent scripted attempt should commit with a downstream ping")
            }
        }
    }

    fn started_live_translator(message_id: &str) -> (LiveStreamTranslator, Vec<u8>) {
        let mut translator = LiveStreamTranslator::new(message_id, "gpt-5.6-sol");
        let mut first_chunk = translator
            .accept(
                &serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type":"message", "id":"msg_up"}
                }),
                None,
            )
            .unwrap();
        first_chunk.extend(
            translator
                .accept(
                    &serde_json::json!({
                        "type": "response.output_text.delta",
                        "output_index": 0,
                        "delta": "hello"
                    }),
                    None,
                )
                .unwrap(),
        );
        assert!(!first_chunk.is_empty());
        (translator, first_chunk)
    }

    async fn post_output_failure_events(
        payload: serde_json::Value,
        ctx: RequestContext,
        turn_id: Option<u64>,
        continuation_capture: Option<LiveContinuationCapture>,
    ) -> Vec<serde_json::Value> {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(Ok(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type":"message", "id":"msg_up"}
        })))
        .await
        .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello"
        })))
        .await
        .unwrap();
        tx.send(Ok(payload)).await.unwrap();
        drop(tx);

        let response = match live_stream_response_once(
            rx,
            "msg_post_output_failure".to_string(),
            "gpt-5.6-sol",
            ctx,
            turn_id,
            continuation_capture,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(10_000),
            DEFAULT_STREAM_HEARTBEAT,
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => panic!("must not replay after streaming output"),
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        crate::anthropic::sse::try_parse_sse_events(&body)
            .unwrap()
            .into_iter()
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect()
    }

    fn request_with_tools(tools: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": tools
        }))
        .unwrap()
    }

    #[test]
    fn web_search_requests_require_a_supported_full_lane_model() {
        let body = request_with_tools(serde_json::json!([
            {"type":"web_search_20250305", "name":"web_search"}
        ]));
        let error = apply_model_lane_for_request_with_options("gpt-5.6-luna", &body, true, false)
            .unwrap_err();
        assert!(error.contains("available only through Responses Lite"));

        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.4"] {
            assert_eq!(
                apply_model_lane_for_request_with_options(model, &body, true, false),
                Ok(false),
                "{model} with web_search must use the full lane"
            );
        }
    }

    #[test]
    fn deferred_web_search_changes_lane_only_when_forced_or_loaded() {
        let tools = serde_json::json!([
            {"name":"ToolSearch", "input_schema":{}},
            {
                "type":"web_search_20260318",
                "name":"web_search",
                "defer_loading":true,
                "allowed_callers":["direct"]
            }
        ]);
        let cases = [
            (
                serde_json::json!({
                    "model":"gpt-5.6-luna",
                    "messages":[{"role":"user", "content":"find it"}],
                    "tools":tools
                }),
                Some(true),
            ),
            (
                serde_json::json!({
                    "model":"gpt-5.6-luna",
                    "messages":[{"role":"user", "content":"find it"}],
                    "tools":tools,
                    "tool_choice":{"type":"tool", "name":"web_search"}
                }),
                None,
            ),
            (
                serde_json::json!({
                    "model":"gpt-5.6-luna",
                    "messages":[
                        {"role":"assistant", "content":[{
                            "type":"tool_use", "id":"search_1", "name":"ToolSearch", "input":{}
                        }]},
                        {"role":"user", "content":[{
                            "type":"tool_result", "tool_use_id":"search_1", "content":[{
                                "type":"tool_reference", "tool_name":"web_search"
                            }]
                        }]}
                    ],
                    "tools":tools
                }),
                None,
            ),
        ];

        for (body, expected_lite) in cases {
            let body: MessagesRequest = serde_json::from_value(body).unwrap();
            let result =
                apply_model_lane_for_request_with_options("gpt-5.6-luna", &body, true, false);
            match expected_lite {
                Some(lite) => assert_eq!(result, Ok(lite)),
                None => assert!(result.is_err()),
            }
        }
    }

    #[test]
    fn requests_without_web_search_keep_model_and_lite_lane() {
        let body = request_with_tools(serde_json::json!([
            {"name":"Bash", "input_schema":{}}
        ]));
        for (resolved, lite_expected) in [
            ("gpt-5.6-luna", true),
            ("gpt-5.6-sol", true),
            ("gpt-5.4", false),
        ] {
            let lite =
                apply_model_lane_for_request_with_options(resolved, &body, true, false).unwrap();
            assert_eq!(lite, lite_expected);
        }
    }

    #[test]
    fn parallel_tools_use_full_lane_for_eligible_sol_and_terra_requests() {
        let body = request_with_tools(serde_json::json!([
            {"type":"custom", "name":"Read", "input_schema":{}},
            {"type":"function", "name":"Grep", "input_schema":{}}
        ]));

        for resolved in ["gpt-5.6-sol", "gpt-5.6-terra"] {
            let model = resolved.to_string();
            let lite =
                apply_model_lane_for_request_with_options(&model, &body, true, true).unwrap();
            assert!(!lite, "{resolved} should use the full parallel-tool lane");

            let translated = translate_request_with_overrides(
                &body,
                TranslateOptions {
                    session_id: None,
                    service_tier: None,
                    model,
                    use_responses_lite: lite,
                },
                TranslationOverrides::default(),
            )
            .unwrap();
            assert!(translated.parallel_tool_calls);
            assert_eq!(translated.tools.as_ref().map(Vec::len), Some(2));
            assert!(translated.client_metadata.is_none());
        }
    }

    #[test]
    fn parallel_tools_use_full_lane_for_single_and_forced_function_tools() {
        let cases = [
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"read both files"}],
                "tools":[{"name":"Read", "input_schema":{}}]
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"read both files"}],
                "tools":[
                    {"name":"Read", "input_schema":{}},
                    {"name":"Grep", "input_schema":{}}
                ],
                "tool_choice":{"type":"tool", "name":"Read"}
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"run it twice"}],
                "tools":[{
                    "name":"DeferredTool", "defer_loading":true, "input_schema":{}
                }],
                "tool_choice":{"type":"tool", "name":"DeferredTool"}
            }),
        ];

        for case in cases {
            let body: MessagesRequest = serde_json::from_value(case).unwrap();
            let model = "gpt-5.6-sol".to_string();
            let lite =
                apply_model_lane_for_request_with_options(&model, &body, true, true).unwrap();
            assert!(!lite, "callable function request should use the full lane");

            let translated = translate_request_with_overrides(
                &body,
                TranslateOptions {
                    session_id: None,
                    service_tier: None,
                    model,
                    use_responses_lite: lite,
                },
                TranslationOverrides::default(),
            )
            .unwrap();
            assert!(translated.parallel_tool_calls);
        }
    }

    #[test]
    fn parallel_tools_keep_luna_and_ineligible_requests_on_lite() {
        let two_tools = serde_json::json!([
            {"name":"Read", "input_schema":{}},
            {"name":"Grep", "input_schema":{}}
        ]);
        let cases = [
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":two_tools,
                "output_config":{"format":{"type":"json_object"}}
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":two_tools,
                "tool_choice":{"type":"none"}
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":two_tools,
                "tool_choice":{"type":"auto", "disable_parallel_tool_use":true}
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":two_tools,
                "tool_choice":{
                    "type":"tool", "name":"Read", "disable_parallel_tool_use":true
                }
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":[{
                    "name":"ProgrammaticOnly",
                    "allowed_callers":["code_execution_20260120"],
                    "input_schema":{}
                }]
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":[{
                    "name":"DeferredTool", "defer_loading":true, "input_schema":{}
                }]
            }),
            serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{
                    "role":"user",
                    "content":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. Your entire response must be plain text: an <analysis> block followed by a <summary> block. Your task is to create a detailed summary"
                }],
                "tools":two_tools
            }),
        ];

        let luna_body = request_with_tools(two_tools.clone());
        assert_eq!(
            apply_model_lane_for_request_with_options("gpt-5.6-luna", &luna_body, true, true,),
            Ok(true)
        );

        for body in cases {
            let body: MessagesRequest = serde_json::from_value(body).unwrap();
            assert!(
                apply_model_lane_for_request_with_options("gpt-5.6-sol", &body, true, true,)
                    .unwrap(),
                "ineligible request unexpectedly left Responses Lite: {body:?}"
            );
        }
    }

    #[test]
    fn parallel_tools_disabled_preserves_responses_lite_behavior() {
        let body = request_with_tools(serde_json::json!([
            {"name":"Read", "input_schema":{}},
            {"name":"Grep", "input_schema":{}}
        ]));
        assert_eq!(
            apply_model_lane_for_request_with_options("gpt-5.6-sol", &body, true, false),
            Ok(true)
        );
    }

    #[test]
    fn responses_lite_disabled_rejects_luna_instead_of_changing_models() {
        let body = request_with_tools(serde_json::json!([
            {"name":"Bash", "input_schema":{}}
        ]));
        for parallel_tools in [false, true] {
            let error = apply_model_lane_for_request_with_options(
                "gpt-5.6-luna",
                &body,
                false,
                parallel_tools,
            )
            .unwrap_err();
            assert!(error.contains("available only through Responses Lite"));
        }

        let alias = resolve_model_request("haiku").model;
        assert!(apply_model_lane_for_request_with_options(&alias, &body, false, false).is_err());
    }

    #[test]
    fn generation_timing_ignores_control_events() {
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "codex.rate_limits"
        })));
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "keepalive"
        })));
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "codex.response.metadata"
        })));
        assert!(codex_generation_event(&serde_json::json!({
            "type": "response.created"
        })));
    }

    #[test]
    fn classifies_native_web_search_phases() {
        for (event, expected) in [
            ("response.web_search_call.in_progress", "in_progress"),
            ("response.web_search_call.searching", "searching"),
            ("response.web_search_call.completed", "completed"),
        ] {
            assert_eq!(
                native_web_search_phase(&serde_json::json!({"type": event})),
                Some(expected)
            );
        }
        assert_eq!(
            native_web_search_phase(&serde_json::json!({
                "type": "response.output_item.added",
                "item": {"type": "web_search_call"}
            })),
            Some("added")
        );
        assert_eq!(
            native_web_search_phase(&serde_json::json!({
                "type": "response.output_item.done",
                "item": {"type": "web_search_call"}
            })),
            Some("done")
        );
        assert_eq!(
            native_web_search_phase(&serde_json::json!({
                "type": "response.output_item.added",
                "item": {"type": "message"}
            })),
            None
        );
    }

    #[test]
    fn live_stream_progress_records_terminal_usage() {
        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(
            "request",
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        let ctx = RequestContext {
            req_id: "request".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: Some(monitor.clone()),
            request_byte_lease: None,
        };
        let chunk = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":48}}\n\n";

        record_live_stream_progress(&ctx, chunk);

        let state = monitor.snapshot();
        assert_eq!(state.active[0].input_tokens, Some(12));
        assert_eq!(state.active[0].output_tokens, Some(48));
    }

    #[tokio::test]
    async fn live_transport_retry_stops_after_anthropic_output_begins() {
        let ctx = live_test_context();

        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(Err(heartbeat_test_error())).await.unwrap();
        drop(tx);
        assert!(matches!(
            live_stream_response_once(
                rx,
                "msg_before".to_string(),
                "gpt-5.6-sol",
                ctx.clone(),
                None,
                None,
                Instant::now(),
                client::CodexRequestDeadline::from_timeout_ms(10_000),
                DEFAULT_STREAM_HEARTBEAT,
            )
            .await,
            LiveStreamStart::Retry { .. }
        ));

        let (tx, rx) = tokio::sync::mpsc::channel(3);
        tx.send(Ok(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type":"message", "id":"msg_up"}
        })))
        .await
        .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello"
        })))
        .await
        .unwrap();
        tx.send(Err(heartbeat_test_error())).await.unwrap();
        drop(tx);
        let response = match live_stream_response_once(
            rx,
            "msg_after".to_string(),
            "gpt-5.6-sol",
            ctx,
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(10_000),
            DEFAULT_STREAM_HEARTBEAT,
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => panic!("must not replay after streaming output"),
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("hello"));
        assert!(body.contains("heartbeat timed out"));
    }

    #[tokio::test]
    async fn post_output_failure_uses_codex_classifier_without_replay() {
        let cases = [
            (
                serde_json::json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {
                        "limit_reached": true,
                        "primary": {"reset_after_seconds": 1}
                    }
                }),
                "rate_limit_error",
            ),
            (
                serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {"type": "overloaded_error", "message": "busy"}
                    }
                }),
                "overloaded_error",
            ),
            (
                serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {"status": 400, "message": "invalid request"}
                    }
                }),
                "api_error",
            ),
        ];

        for (payload, expected_error_type) in cases {
            let events = post_output_failure_events(payload, live_test_context(), None, None).await;
            assert!(events.iter().any(|event| {
                event
                    .pointer("/delta/text")
                    .and_then(serde_json::Value::as_str)
                    == Some("hello")
            }));
            let error = events
                .iter()
                .find(|event| {
                    event.get("type").and_then(serde_json::Value::as_str) == Some("error")
                })
                .expect("post-output failure must emit a terminal Anthropic error event");
            assert_eq!(
                error
                    .pointer("/error/type")
                    .and_then(serde_json::Value::as_str),
                Some(expected_error_type)
            );
            assert_eq!(
                events
                    .last()
                    .and_then(|event| event.get("type"))
                    .and_then(serde_json::Value::as_str),
                Some("error")
            );
            assert!(!events.iter().any(|event| {
                event.get("type").and_then(serde_json::Value::as_str) == Some("message_stop")
            }));
        }
    }

    #[tokio::test]
    async fn post_output_failure_aborts_continuation_and_captures_error_event() {
        let _state_guard = CODEX_STATE_TEST_LOCK.lock().await;
        continuation::clear_all_continuations_for_tests();
        let session_id = "post-output-failure";
        let request = live_test_request();
        let candidate = continuation_candidate(Some(session_id), &request, true);
        let turn_id = candidate
            .turn_id
            .expect("continuation turn should register");
        assert!(continuation::is_current_turn(
            Some(session_id),
            Some(turn_id)
        ));
        let continuation_capture = LiveContinuationCapture::for_turn(Some(turn_id), &request, None);

        let temp = tempfile::TempDir::new().unwrap();
        let traffic = Arc::new(crate::traffic::test_capture(
            temp.path().join("post-output-failure-traffic"),
        ));
        let traffic_root = traffic.root().to_path_buf();
        let mut ctx = live_test_context();
        ctx.session_id = Some(session_id.to_string());
        ctx.traffic = Some(traffic);

        let events = post_output_failure_events(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {"type": "overloaded_error", "message": "busy"}
                }
            }),
            ctx,
            Some(turn_id),
            continuation_capture,
        )
        .await;
        assert!(!continuation::is_current_turn(
            Some(session_id),
            Some(turn_id)
        ));
        assert_eq!(
            events
                .last()
                .and_then(|event| event.pointer("/error/type"))
                .and_then(serde_json::Value::as_str),
            Some("overloaded_error")
        );

        let captured_error = std::fs::read_dir(traffic_root.join("events"))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .find(|event| event.get("event").and_then(serde_json::Value::as_str) == Some("error"))
            .expect("traffic capture must retain the downstream error event");
        assert_eq!(
            captured_error
                .pointer("/data/error/type")
                .and_then(serde_json::Value::as_str),
            Some("overloaded_error")
        );
        continuation::clear_all_continuations_for_tests();
    }

    #[tokio::test]
    async fn live_hosted_side_effect_blocks_outer_retry_and_preserves_failure_status() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(Ok(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type":"web_search_call", "id":"ws_1"}
        })))
        .await
        .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.failed",
            "response": {
                "status": "failed",
                "error": {"status":503, "message":"hosted search failed after dispatch"}
            }
        })))
        .await
        .unwrap();
        drop(tx);

        let response = match live_stream_response_once(
            rx,
            "msg_hosted_barrier".to_string(),
            "gpt-5.6-sol",
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(10_000),
            DEFAULT_STREAM_HEARTBEAT,
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => {
                panic!("hosted side effects must close the outer replay window")
            }
        };
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn live_retry_budget_exhaustion_preserves_the_original_upstream_error() {
        let request = live_test_request();
        let continuation = continuation_candidate(None, &request, true);
        let mut state = LiveReplayState::new(
            Arc::new(CodexHttpClient::new()),
            "msg_budget_exhausted".to_string(),
            "gpt-5.6-sol".to_string(),
            request,
            None,
            continuation,
        );
        for _ in 0..dispatch_budget::MAX_CODEX_MODEL_DISPATCHES {
            state
                .dispatch_budget
                .reserve_model()
                .expect("test setup must fill the model dispatch budget");
        }
        let original_attempt = state.attempt;
        let error = client::CodexError {
            status: 503,
            message: "temporarily unavailable".to_string(),
            detail: Some("explicit upstream retry response".to_string()),
            retry_after: Some("30".to_string()),
            origin: client::CodexErrorOrigin::UpstreamHttp,
        };

        let rejected = state
            .plan_retry(&live_test_context(), error)
            .expect_err("an exhausted hard dispatch budget must reject before backoff");

        assert_eq!(rejected.status, 503);
        assert_eq!(rejected.retry_after.as_deref(), Some("30"));
        assert_eq!(state.attempt, original_attempt);
        assert_eq!(
            state.dispatch_budget.snapshot().model,
            dispatch_budget::MAX_CODEX_MODEL_DISPATCHES
        );
    }

    #[test]
    fn live_retry_drops_wire_continuation_before_redispatch() {
        let request = live_test_request();
        let continuation = ContinuationCandidate {
            turn_id: Some(42),
            previous_response_id: Some("resp_failed_attempt".to_string()),
            input_delta: Some(request.input.clone()),
            input_delta_count: request.input.len(),
            disabled_reason: None,
        };
        let mut state = LiveReplayState::new(
            Arc::new(CodexHttpClient::new()),
            "msg_full_context_replay".to_string(),
            "gpt-5.6-sol".to_string(),
            request,
            None,
            continuation,
        );
        state
            .dispatch_budget
            .reserve_model()
            .expect("test setup must account for the failed first dispatch");
        let error = client::CodexError {
            status: 503,
            message: "temporarily unavailable".to_string(),
            detail: Some("explicit upstream retry response".to_string()),
            retry_after: Some("0".to_string()),
            origin: client::CodexErrorOrigin::UpstreamHttp,
        };

        state
            .plan_retry(&live_test_context(), error)
            .expect("an explicit 503 should remain replayable");

        let continuation = state.continuation.expect("turn identity must be retained");
        assert_eq!(continuation.turn_id, Some(42));
        assert!(continuation.previous_response_id.is_none());
        assert!(continuation.input_delta.is_none());
        assert_eq!(
            continuation.disabled_reason.as_deref(),
            Some("full_context_retry")
        );
    }

    #[tokio::test]
    async fn heartbeat_only_commit_replays_explicit_503_in_the_same_downstream_stream() {
        let request = live_test_request();
        let continuation = continuation_candidate(None, &request, true);
        let attempts = std::collections::VecDeque::from([Ok(successful_text_receiver(
            "recovered",
            "resp_replayed",
        ))]);
        let (state, budget, _) = scripted_replay_state(request, continuation, attempts);
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(2);
        let response = start_scripted_heartbeat_response(
            upstream_rx,
            state,
            live_test_context(),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        upstream_tx
            .send(Ok(retryable_failure_payload()))
            .await
            .unwrap();
        drop(upstream_tx);
        let body = tokio::time::timeout(
            Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("the replayed response should finish")
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("event: ping"), "{body}");
        assert_eq!(body.matches("recovered").count(), 1, "{body}");
        assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
        assert_eq!(body.matches("event: message_stop").count(), 1, "{body}");
        assert!(!body.contains("event: error"), "{body}");
        assert_eq!(budget.snapshot().model, 2);
    }

    #[tokio::test]
    async fn non_ping_anthropic_bytes_close_the_committed_replay_gate() {
        let request = live_test_request();
        let continuation = continuation_candidate(None, &request, true);
        let attempts = std::collections::VecDeque::from([Ok(successful_text_receiver(
            "must not appear",
            "resp_forbidden_replay",
        ))]);
        let (state, budget, attempts) = scripted_replay_state(request, continuation, attempts);
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(4);
        let response = start_scripted_heartbeat_response(
            upstream_rx,
            state,
            live_test_context(),
            Duration::from_millis(20),
        )
        .await;
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"message", "id":"msg_partial"}
            })))
            .await
            .unwrap();
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": "msg_partial",
                "delta": "partial"
            })))
            .await
            .unwrap();
        upstream_tx
            .send(Ok(retryable_failure_payload()))
            .await
            .unwrap();
        drop(upstream_tx);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("partial"), "{body}");
        assert!(body.contains("event: error"), "{body}");
        assert!(!body.contains("must not appear"), "{body}");
        assert_eq!(budget.snapshot().model, 1);
        assert_eq!(
            attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn hosted_side_effect_closes_committed_replay_gate_before_translation() {
        let mut request = live_test_request();
        request.tools = Some(vec![translate::request::ResponsesTool::WebSearch(
            translate::request::ResponsesWebSearchTool {
                kind: "web_search".to_string(),
                external_web_access: true,
                search_content_types: vec!["text".to_string()],
                filters: None,
                user_location: None,
            },
        )]);
        let continuation = continuation_candidate(None, &request, true);
        let attempts = std::collections::VecDeque::from([Ok(successful_text_receiver(
            "must not appear",
            "resp_forbidden_hosted_replay",
        ))]);
        let (state, budget, attempts) = scripted_replay_state(request, continuation, attempts);
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(3);
        let response = start_scripted_heartbeat_response(
            upstream_rx,
            state,
            live_test_context(),
            Duration::from_millis(20),
        )
        .await;
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"web_search_call", "id":"ws_committed"}
            })))
            .await
            .unwrap();
        upstream_tx
            .send(Ok(retryable_failure_payload()))
            .await
            .unwrap();
        drop(upstream_tx);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("event: error"), "{body}");
        assert!(!body.contains("must not appear"), "{body}");
        assert_eq!(budget.snapshot().model, 1);
        assert_eq!(
            attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn outcome_unknown_after_heartbeat_is_never_replayed() {
        let request = live_test_request();
        let continuation = continuation_candidate(None, &request, true);
        let attempts = std::collections::VecDeque::from([Ok(successful_text_receiver(
            "must not appear",
            "resp_forbidden_unknown_replay",
        ))]);
        let (state, budget, attempts) = scripted_replay_state(request, continuation, attempts);
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(1);
        let response = start_scripted_heartbeat_response(
            upstream_rx,
            state,
            live_test_context(),
            Duration::from_millis(20),
        )
        .await;
        upstream_tx.send(Err(heartbeat_test_error())).await.unwrap();
        drop(upstream_tx);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("heartbeat timed out"), "{body}");
        assert!(!body.contains("must not appear"), "{body}");
        assert_eq!(budget.snapshot().model, 1);
        assert_eq!(
            attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn replay_backoff_keeps_heartbeats_and_downstream_drop_cancels_redispatch() {
        let request = live_test_request();
        let continuation = continuation_candidate(None, &request, true);
        let attempts = std::collections::VecDeque::from([Ok(successful_text_receiver(
            "must not appear",
            "resp_cancelled_replay",
        ))]);
        let (state, budget, attempts) = scripted_replay_state(request, continuation, attempts);
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(1);
        let response = start_scripted_heartbeat_response(
            upstream_rx,
            state,
            live_test_context(),
            Duration::from_millis(20),
        )
        .await;
        let mut body = response.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first.as_ref(), DOWNSTREAM_PING);

        upstream_tx
            .send(Ok(retryable_failure_payload()))
            .await
            .unwrap();
        drop(upstream_tx);
        let during_backoff = tokio::time::timeout(Duration::from_millis(80), body.frame())
            .await
            .expect("a heartbeat must be emitted while replay backoff is pending")
            .expect("the response must remain open during replay backoff")
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(during_backoff.as_ref(), DOWNSTREAM_PING);
        drop(body);
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(budget.snapshot().model, 1);
        assert_eq!(
            attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn successful_replay_rebuilds_continuation_capture_for_the_current_turn() {
        let _state_guard = CODEX_STATE_TEST_LOCK.lock().await;
        continuation::clear_all_continuations_for_tests();
        let session_id = "post-ping-replay-capture";
        let request = live_test_request();
        let continuation = continuation_candidate(Some(session_id), &request, true);
        let attempts = std::collections::VecDeque::from([Ok(successful_text_receiver(
            "captured",
            "resp_second_attempt",
        ))]);
        let (state, budget, _) = scripted_replay_state(request, continuation, attempts);
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(1);
        let mut ctx = live_test_context();
        ctx.session_id = Some(session_id.to_string());
        let response =
            start_scripted_heartbeat_response(upstream_rx, state, ctx, Duration::from_millis(20))
                .await;
        upstream_tx
            .send(Ok(retryable_failure_payload()))
            .await
            .unwrap();
        drop(upstream_tx);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("captured"));
        assert_eq!(budget.snapshot().model, 2);
        assert!(continuation::has_continuation_for_tests(session_id));
        continuation::clear_all_continuations_for_tests();
    }

    #[tokio::test]
    async fn response_created_starts_downstream_after_retry_grace_with_transport_only_ping() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(4);
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_silent", "status": "in_progress"}
            })))
            .await
            .unwrap();

        let response = tokio::time::timeout(
            Duration::from_millis(250),
            live_stream_response_once(
                upstream_rx,
                "msg_silent".to_string(),
                "gpt-5.6-sol",
                live_test_context(),
                None,
                None,
                Instant::now(),
                client::CodexRequestDeadline::from_timeout_ms(10_000),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("silent response should establish downstream SSE after its retry grace");
        let response = match response {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => panic!("silent response should become a live stream"),
        };
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("transport keepalive should be ready")
            .expect("stream ended before transport keepalive")
            .expect("transport keepalive frame failed")
            .into_data()
            .expect("transport keepalive was not data");
        assert_eq!(first.as_ref(), DOWNSTREAM_PING);
        let events = crate::anthropic::sse::parse_sse_events(&first);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, r#"{"type":"ping"}"#);

        drop(body);
        tokio::time::timeout(Duration::from_millis(250), upstream_tx.closed())
            .await
            .expect("dropping a heartbeat-only body should cancel the upstream stream");
    }

    #[tokio::test]
    async fn live_transport_starts_ping_grace_before_the_first_upstream_event() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(1);
        let response = tokio::time::timeout(
            Duration::from_millis(250),
            live_stream_response_once(
                upstream_rx,
                "msg_live_silent".to_string(),
                "gpt-5.6-sol",
                live_test_context(),
                None,
                None,
                Instant::now(),
                client::CodexRequestDeadline::from_timeout_ms(10_000),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("the live transport should start the downstream ping grace");
        let response = match response {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => {
                panic!("a silent upstream should become a live stream")
            }
        };
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_millis(100), body.frame())
            .await
            .expect("live transport ping should be ready")
            .expect("stream ended before the live transport ping")
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first.as_ref(), DOWNSTREAM_PING);

        drop(body);
        tokio::time::timeout(Duration::from_millis(100), upstream_tx.closed())
            .await
            .expect("dropping the response should cancel its upstream body");
    }

    #[tokio::test]
    async fn first_generation_event_after_initial_ping_starts_monitor_timing() {
        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(
            "live-retry-test",
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        let mut ctx = live_test_context();
        ctx.monitor = Some(monitor.clone());
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(1);
        let response = tokio::time::timeout(
            Duration::from_millis(250),
            live_stream_response_once(
                upstream_rx,
                "msg_ping_then_generation".to_string(),
                "gpt-5.6-sol",
                ctx,
                None,
                None,
                Instant::now(),
                client::CodexRequestDeadline::from_timeout_ms(10_000),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("initial downstream ping should establish the live response");
        let response = match response {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => {
                panic!("a silent upstream should become a live stream")
            }
        };
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_millis(100), body.frame())
            .await
            .expect("initial ping should be ready")
            .expect("stream ended before the initial ping")
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first.as_ref(), DOWNSTREAM_PING);

        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_after_ping", "status": "in_progress"}
            })))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if monitor.snapshot().active.iter().any(|request| {
                    request.request_id == "live-retry-test"
                        && request.generation_started_at.is_some()
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first generation event after a ping was not recorded");

        drop(body);
        tokio::time::timeout(Duration::from_millis(250), upstream_tx.closed())
            .await
            .expect("dropping the response should cancel its upstream body");
    }

    #[tokio::test]
    async fn continuous_control_events_cannot_starve_the_ping_deadline() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(2);
        let producer = tokio::spawn(async move {
            upstream_tx
                .send(Ok(serde_json::json!({"type":"response.created"})))
                .await
                .unwrap();
            loop {
                if upstream_tx
                    .send(Ok(serde_json::json!({"type":"keepalive"})))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let response = tokio::time::timeout(
            Duration::from_millis(150),
            live_stream_response_once(
                upstream_rx,
                "msg_control_flood".to_string(),
                "gpt-5.6-sol",
                live_test_context(),
                None,
                None,
                Instant::now(),
                client::CodexRequestDeadline::from_timeout_ms(10_000),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("continuous control events must not starve the ping timer");
        let response = match response {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { .. } => panic!("control events should keep the stream open"),
        };
        let mut body = response.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first.as_ref(), DOWNSTREAM_PING);
        drop(body);
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn response_created_keeps_retryable_error_recovery_during_grace() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(4);
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_retry", "status": "in_progress"}
            })))
            .await
            .unwrap();
        upstream_tx.send(Err(heartbeat_test_error())).await.unwrap();
        drop(upstream_tx);

        assert!(matches!(
            live_stream_response_once(
                upstream_rx,
                "msg_retry_grace".to_string(),
                "gpt-5.6-sol",
                live_test_context(),
                None,
                None,
                Instant::now(),
                client::CodexRequestDeadline::from_timeout_ms(10_000),
                Duration::from_secs(1),
            )
            .await,
            LiveStreamStart::Retry { .. }
        ));
    }

    #[tokio::test]
    async fn silent_live_stream_emits_periodic_pings_without_duplication() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(4);
        let (translator, first_chunk) = started_live_translator("msg_long");
        let response = remaining_live_stream_response_with_heartbeat(
            upstream_rx,
            translator,
            first_chunk,
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(1_000),
            true,
            Duration::from_millis(20),
        );

        let mut body = response.into_body();
        let mut collected = Vec::new();
        let mut pings = 0;
        while pings < 2 {
            let frame = tokio::time::timeout(Duration::from_millis(250), body.frame())
                .await
                .expect("periodic ping should arrive")
                .expect("stream ended before two pings")
                .expect("periodic ping frame failed")
                .into_data()
                .expect("periodic ping frame was not data");
            pings += String::from_utf8_lossy(&frame)
                .matches("event: ping\n")
                .count();
            collected.extend_from_slice(&frame);
        }
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": " world"
            })))
            .await
            .unwrap();
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type":"message", "id":"msg_up"}
            })))
            .await
            .unwrap();
        upstream_tx
            .send(Ok(serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_long",
                    "status": "completed",
                    "incomplete_details": null,
                    "usage": {"input_tokens": 2, "output_tokens": 2}
                }
            })))
            .await
            .unwrap();
        drop(upstream_tx);
        let rest = tokio::time::timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(body, usize::MAX),
        )
        .await
        .expect("long stream should finish before its deadline")
        .unwrap();
        collected.extend_from_slice(&rest);
        let body = String::from_utf8(collected).unwrap();
        assert!(body.matches("event: ping\n").count() >= 2, "{body}");
        assert_eq!(body.matches("hello").count(), 1, "{body}");
        assert_eq!(body.matches(" world").count(), 1, "{body}");
        assert!(body.contains("message_stop"), "{body}");
    }

    #[test]
    fn total_timeout_maps_to_gateway_timeout_before_streaming_starts() {
        let response = map_codex_error_to_response(&client::codex_total_timeout_error(
            config::CodexTransport::WebSocket,
            100,
        ));

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn post_first_byte_deadline_emits_sse_error_and_closes_upstream() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(4);
        let (translator, first_chunk) = started_live_translator("msg_deadline");
        let response = remaining_live_stream_response_with_heartbeat(
            upstream_rx,
            translator,
            first_chunk,
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(100),
            true,
            Duration::from_millis(20),
        );

        let body = tokio::time::timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("deadline should terminate the downstream stream")
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("hello"));
        assert!(body.contains("event: error"));
        assert!(body.contains("api_error"));
        assert!(body.contains("total wall-clock budget of 100ms"));
        assert!(body.contains("event: ping\ndata: {\"type\":\"ping\"}\n\n"));
        tokio::time::timeout(Duration::from_millis(250), upstream_tx.closed())
            .await
            .expect("deadline should drop the upstream event receiver");
    }

    #[tokio::test]
    async fn dropping_live_response_body_immediately_closes_upstream_receiver() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(4);
        let (translator, first_chunk) = started_live_translator("msg_cancel");
        let response = remaining_live_stream_response(
            upstream_rx,
            translator,
            first_chunk,
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(10_000),
            true,
            DEFAULT_STREAM_HEARTBEAT,
        );
        let mut body = response.into_body();

        let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("first downstream frame should be ready")
            .expect("stream ended before the first frame")
            .expect("first downstream frame failed")
            .into_data()
            .expect("first downstream frame was not data");
        assert!(String::from_utf8_lossy(&first).contains("hello"));
        drop(body);

        tokio::time::timeout(Duration::from_millis(250), upstream_tx.closed())
            .await
            .expect("dropping the downstream body should drop the upstream receiver");
    }

    #[tokio::test]
    async fn deadline_does_not_block_on_a_full_downstream_queue() {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::channel(128);
        for index in 0..96 {
            upstream_tx
                .send(Ok(serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": format!("-{index}")
                })))
                .await
                .unwrap();
        }
        let (translator, first_chunk) = started_live_translator("msg_full_queue");
        let response = remaining_live_stream_response(
            upstream_rx,
            translator,
            first_chunk,
            live_test_context(),
            None,
            None,
            Instant::now(),
            client::CodexRequestDeadline::from_timeout_ms(100),
            true,
            DEFAULT_STREAM_HEARTBEAT,
        );
        let body = response.into_body();

        tokio::time::timeout(Duration::from_secs(1), upstream_tx.closed())
            .await
            .expect("deadline must cancel upstream even when downstream is not consuming");
        let body = tokio::time::timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(body, usize::MAX),
        )
        .await
        .expect("reserved terminal slot should drain without blocking")
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("event: error"), "{body}");
        assert!(body.contains("total wall-clock budget of 100ms"), "{body}");
    }

    #[tokio::test]
    async fn live_chunk_byte_budget_is_held_until_the_consumer_drops_the_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let byte_budget = ByteBudget::new(4);
        let deadline = client::CodexRequestDeadline::from_timeout_ms(1_000);

        assert_eq!(
            send_live_chunk_before_deadline(&tx, &byte_budget, vec![1; 4], deadline).await,
            LiveChunkSendOutcome::Sent
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                send_live_chunk_before_deadline(&tx, &byte_budget, vec![2], deadline),
            )
            .await
            .is_err(),
            "queued bytes must retain the byte permit"
        );

        drop(rx.recv().await.unwrap().unwrap());
        assert_eq!(
            send_live_chunk_before_deadline(&tx, &byte_budget, vec![2], deadline).await,
            LiveChunkSendOutcome::Sent
        );
    }

    #[tokio::test]
    async fn live_chunk_larger_than_the_queue_budget_is_rejected_without_enqueueing() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let byte_budget = ByteBudget::new(MAX_DOWNSTREAM_QUEUE_BYTES);

        assert_eq!(
            send_live_chunk_before_deadline(
                &tx,
                &byte_budget,
                vec![0; MAX_DOWNSTREAM_QUEUE_BYTES + 1],
                client::CodexRequestDeadline::from_timeout_ms(1_000),
            )
            .await,
            LiveChunkSendOutcome::TooLarge
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(byte_budget.available_permits(), MAX_DOWNSTREAM_QUEUE_BYTES);
    }

    #[test]
    fn supported_models_includes_fast_variants() {
        let provider = CodexProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"gpt-5.6-sol".to_string()));
        assert!(models.contains(&"gpt-5.6-sol-fast".to_string()));
        assert!(models.contains(&"gpt-5.6-terra".to_string()));
        assert!(models.contains(&"gpt-5.6-luna".to_string()));
        assert!(models.contains(&"gpt-5.4".to_string()));
        assert!(models.contains(&"gpt-5.4-mini".to_string()));
    }

    #[test]
    fn format_auth_saved_output_with_account() {
        assert_eq!(
            format_auth_saved_output("/tmp/auth.json", Some("acct_1")),
            "Auth saved in /tmp/auth.json\nAccount: acct_1\n"
        );
    }

    #[test]
    fn format_auth_saved_output_without_account() {
        assert_eq!(
            format_auth_saved_output("/tmp/auth.json", None),
            "Auth saved in /tmp/auth.json\n"
        );
    }

    #[test]
    fn format_expiry_with_future_expiry() {
        // 2100-01-01T00:00:00Z in ms
        let expires = 4102444800000;
        let now = 4102444790000; // 10s before
        let output = format_expiry(expires, now);
        assert!(output.starts_with("Expires: 2100-01-01T00:00:00.000Z (in "));
        assert!(output.ends_with("s)"));
    }

    #[test]
    fn format_expiry_with_past_expiry() {
        // 2000-01-01T00:00:00Z in ms
        let expires = 946684800000;
        let now = 946684810000; // 10s after
        let output = format_expiry(expires, now);
        assert!(output.starts_with("Expires: 2000-01-01T00:00:00.000Z (in -"));
    }

    #[tokio::test]
    async fn statusless_codex_error_returns_source_message() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket connect error: HTTP error: 502 Bad Gateway".to_string(),
            detail: None,
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body.pointer("/error/message").and_then(|v| v.as_str()),
            Some("WebSocket connect error: HTTP error: 502 Bad Gateway")
        );
    }

    #[tokio::test]
    async fn websocket_watchdog_error_returns_descriptive_message() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket heartbeat timed out after 10000ms without a Pong".to_string(),
            detail: Some(websocket::WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL.to_string()),
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        let response = map_codex_error_to_response(&err);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body.pointer("/error/message")
                .and_then(|value| value.as_str()),
            Some("WebSocket heartbeat timed out after 10000ms without a Pong")
        );
    }

    #[test]
    fn live_start_statusless_websocket_handshake_error_is_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".to_string(),
            detail: None,
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(retryable_live_start_codex_error(&err));
    }

    #[test]
    fn live_start_heartbeat_error_is_retryable_with_short_network_budget() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket heartbeat timed out".to_string(),
            detail: Some(websocket::WEBSOCKET_HEARTBEAT_TIMEOUT_DETAIL.to_string()),
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        assert!(retryable_live_start_codex_error(&err));
        assert_eq!(max_live_retries(&err), MAX_LIVE_TRANSPORT_RETRIES);
    }

    #[test]
    fn live_start_terminal_barrier_error_is_never_replayed() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket terminal ordering barrier failed: connection closed".to_string(),
            detail: Some(websocket::WEBSOCKET_TERMINAL_BARRIER_DETAIL.to_string()),
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        assert!(!retryable_live_start_codex_error(&err));
        assert!(!replayable_live_start_codex_error(&err));
        assert!(!client::should_fallback_to_http(&err));
    }

    #[test]
    fn live_upstream_overload_keeps_service_retry_budget() {
        let err = client::CodexError {
            status: 529,
            message: "Overloaded".to_string(),
            detail: None,
            retry_after: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        assert!(retryable_live_start_codex_error(&err));
        assert_eq!(max_live_retries(&err), MAX_RETRYABLE_LIVE_STREAM_RETRIES);
    }

    #[test]
    fn live_start_payload_retry_detection_covers_rate_limit_and_overload() {
        assert!(retryable_live_start_payload(
            &serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": true}
            }),
            "rate limit reached",
        ));
        assert!(retryable_live_start_payload(
            &serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"type": "overloaded_error", "message": "overloaded"}}
            }),
            "overloaded",
        ));
        assert!(!retryable_live_start_payload(
            &serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"message": "bad request"}}
            }),
            "bad request",
        ));
    }
}
