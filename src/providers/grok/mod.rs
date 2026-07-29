pub mod auth;
pub mod client;
pub mod count_tokens;
mod text;
pub mod translate;

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
#[cfg(test)]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, mpsc};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::monitor::MonitorHandle;
use crate::provider::{CliHandlers, Provider, RequestByteLease, RequestContext};
use crate::providers::downstream_queue::{
    self, BudgetedChunk, ByteBudget, SendOutcome as GrokChunkSendOutcome,
};
use crate::providers::token_count_admission::{self, TokenCountAdmissionError};
use crate::retry::sleep;
use crate::timeutil::now_ms;
use crate::{registry::GROK_MODELS, traffic::StreamTrafficCapture};

use self::auth::token_store::file_store_with_migration;
use self::client::{
    GrokByteStream, GrokError, GrokRequestDeadline, GrokRetryState, PreparedGrokRequest,
};
#[cfg(test)]
use self::translate::accumulate::accumulate_response_with_traffic;
#[cfg(test)]
use self::translate::request::GrokResponsesRequest;
use self::translate::{
    accumulate::accumulate_response_with_traffic_and_tool_policy,
    model_allowlist::{assert_allowed_model, resolve_model_request},
    request::{GrokReasoning, translate_request},
    stream::{SseDecoder, StreamTranslator, stream_ping},
    tool_policy::ToolCallPolicy,
};

const GROK_DOWNSTREAM_CHANNEL_CAPACITY: usize = 3;
const GROK_DOWNSTREAM_QUEUE_BYTES: usize = 2 * 1024 * 1024;
const GROK_DOWNSTREAM_STALL_TIMEOUT: Duration = Duration::from_secs(60);
const GROK_MAX_CUMULATIVE_STREAM_BYTES: u64 = 8 * 1024 * 1024;
const MIN_STREAM_HEARTBEAT: Duration = Duration::from_secs(1);
const MAX_STREAM_HEARTBEAT: Duration = Duration::from_secs(60);

pub struct GrokProvider {
    client: Arc<client::GrokClient>,
}
impl GrokProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(
                client::GrokClient::new(
                    crate::config::grok_base_url(),
                    crate::config::grok_client_version(),
                )
                .expect("Grok transport is unavailable"),
            ),
        }
    }

    pub fn with_client(client: client::GrokClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}
impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GrokProvider {
    fn name(&self) -> &'static str {
        "grok"
    }
    fn supported_models(&self) -> Vec<String> {
        GROK_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &GROK_CLI
    }
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let deadline = GrokRequestDeadline::configured();
        let requested = body.model.clone().unwrap_or_else(|| "grok-4.5".into());
        let stream = body.stream;
        let resolved = resolve_model_request(&requested);
        if let Err(error) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        if let Err(error) =
            crate::providers::translate_shared::validate_generation_max_tokens(&body, "Grok")
        {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let mut translated =
            match translate_request_for_provider(body, resolved.model.clone()).await {
                Ok(value) => value,
                Err(response) => return response,
            };
        if let Some(effort) = resolved.reasoning_effort {
            translated.reasoning = Some(GrokReasoning {
                effort: effort.into(),
            });
        }
        let tool_policy = ToolCallPolicy::from_request(&translated);
        let (function_tool_count, hosted_tool_count) = translated
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .fold((0_usize, 0_usize), |(functions, hosted), tool| {
                        if tool.kind == "function" {
                            (functions + 1, hosted)
                        } else {
                            (functions, hosted + 1)
                        }
                    })
            })
            .unwrap_or_default();
        let reasoning_effort = translated
            .reasoning
            .as_ref()
            .map(|reasoning| reasoning.effort.clone());
        let parallel_tool_calls = translated.parallel_tool_calls;
        let tool_choice = translated
            .tool_choice
            .as_ref()
            .and_then(|choice| serde_json::to_value(choice).ok());
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
            monitor.upstream_started(&ctx.req_id);
        }

        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline_and_req_id(
            deadline,
            ctx.req_id.clone(),
        )));
        let prepared = match PreparedGrokRequest::new(&translated, ctx.traffic.is_some()) {
            Ok(prepared) => Arc::new(prepared),
            Err(error) => return map_error(error),
        };
        // This value is only an observability fallback when upstream usage is
        // unavailable. Keep exact tokenizer work off the model-response hot
        // path so ready response headers and stream bytes are never delayed.
        let estimated_input_tokens = prepared.approximate_input_tokens();
        let upstream = match self
            .client
            .post_prepared_with_retry(&prepared, ctx.traffic.clone(), retry.clone())
            .await
        {
            Ok(response) => response,
            Err(error) => return map_error(error),
        };
        crate::logging::create_logger("grok").info(
            "request_configuration",
            Some(serde_json::Map::from_iter([
                ("reqId".into(), serde_json::json!(ctx.req_id)),
                ("model".into(), serde_json::json!(resolved.model)),
                (
                    "reasoningEffort".into(),
                    serde_json::json!(reasoning_effort),
                ),
                ("transport".into(), serde_json::json!("http")),
                (
                    "parallelToolCalls".into(),
                    serde_json::json!(parallel_tool_calls),
                ),
                ("toolChoice".into(), serde_json::json!(tool_choice)),
                (
                    "functionToolCount".into(),
                    serde_json::json!(function_tool_count),
                ),
                (
                    "hostedToolCount".into(),
                    serde_json::json!(hosted_tool_count),
                ),
                (
                    "estimatedInputTokens".into(),
                    serde_json::json!(estimated_input_tokens),
                ),
            ])),
        );

        if stream {
            let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
            let reconnect = Some(
                GrokReconnectContext::new(
                    self.client.clone(),
                    prepared,
                    ctx.traffic.clone(),
                    retry,
                    estimated_input_tokens,
                )
                .with_request_byte_lease(ctx.request_byte_lease.clone()),
            );
            stream_body_with_policies(
                upstream.into_stream(),
                message_id,
                requested,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
                ctx.traffic.clone(),
                reconnect,
                tool_policy,
                deadline,
                configured_stream_heartbeat(),
            )
        } else {
            match read_non_streaming_body(
                self.client.clone(),
                prepared,
                upstream,
                ctx.traffic.clone(),
                retry,
                deadline,
            )
            .await
            {
                Ok(upstream_bytes) => finish_non_streaming_message_with_tool_policy(
                    &upstream_bytes,
                    &requested,
                    estimated_input_tokens,
                    &ctx,
                    &tool_policy,
                ),
                Err(error) => {
                    write_error(ctx.traffic.as_deref(), "body_read", "transport");
                    map_error(error)
                }
            }
        }
    }
    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.clone().unwrap_or_else(|| "grok-4.5".into());
        let resolved = resolve_model_request(&requested);
        if let Err(error) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let translated = match translate_request_for_provider(body, resolved.model).await {
            Ok(value) => value,
            Err(response) => return response,
        };
        let tokens =
            match token_count_admission::run(move || count_tokens::count_tokens(&translated)).await
            {
                Ok(tokens) => tokens,
                Err(error) => return map_grok_count_tokens_error(error),
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

fn map_grok_count_tokens_error(error: TokenCountAdmissionError) -> Response {
    match error {
        TokenCountAdmissionError::QueueTimeout => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Grok token-count admission queue was saturated for 30 seconds",
        ),
        TokenCountAdmissionError::Closed => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "The shared token-count admission gate is unavailable",
        ),
        TokenCountAdmissionError::WorkerFailed(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            format!("Grok token-count worker failed: {error}"),
        ),
    }
}

async fn translate_request_for_provider(
    body: MessagesRequest,
    model: String,
) -> Result<self::translate::request::GrokResponsesRequest, Response> {
    if !crate::providers::translate_shared::messages_request_contains_base64_image(&body) {
        return translate_request(&body, model).map_err(|error| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            )
        });
    }

    let permit = match crate::providers::translate_shared::acquire_image_decode_slot().await {
        Ok(permit) => permit,
        Err(crate::providers::translate_shared::ImageDecodeAdmissionError::Closed) => {
            return Err(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "Grok image validation is unavailable",
            ));
        }
        Err(crate::providers::translate_shared::ImageDecodeAdmissionError::Timeout) => {
            return Err(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "Grok image validation queue was saturated for 30 seconds",
            ));
        }
    };

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        translate_request(&body, model)
    })
    .await
    .map_err(|error| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            format!("Grok image validation worker failed: {error}"),
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

#[cfg(test)]
fn finish_non_streaming_message(
    upstream_bytes: &[u8],
    requested: &str,
    estimated_input_tokens: u64,
    ctx: &RequestContext,
) -> Response {
    finish_non_streaming_message_with_tool_policy(
        upstream_bytes,
        requested,
        estimated_input_tokens,
        ctx,
        &ToolCallPolicy::permissive(),
    )
}

fn finish_non_streaming_message_with_tool_policy(
    upstream_bytes: &[u8],
    requested: &str,
    estimated_input_tokens: u64,
    ctx: &RequestContext,
    tool_policy: &ToolCallPolicy,
) -> Response {
    match accumulate_response_with_traffic_and_tool_policy(
        upstream_bytes,
        &format!("msg_{}", uuid::Uuid::new_v4().simple()),
        requested,
        ctx.traffic.as_deref(),
        tool_policy,
    ) {
        Ok(value) => {
            if let Some(traffic) = ctx.traffic.as_ref() {
                traffic.write_json("051-downstream-response", &value);
            }
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.usage_updated(
                    &ctx.req_id,
                    value
                        .pointer("/usage/input_tokens")
                        .and_then(|value| value.as_u64()),
                    value
                        .pointer("/usage/output_tokens")
                        .and_then(|value| value.as_u64()),
                );
            }
            (StatusCode::OK, Json(value)).into_response()
        }
        Err(error) => {
            let reduction_error = error.downcast_ref::<translate::reducer::GrokReductionError>();
            let usage = reduction_error.and_then(translate::reducer::GrokReductionError::usage);
            let upstream_failure =
                reduction_error.and_then(translate::reducer::GrokReductionError::upstream_failure);
            if let (Some(monitor), Some(usage)) = (ctx.monitor.as_ref(), usage) {
                monitor.usage_updated(
                    &ctx.req_id,
                    usage.mapped_input_tokens(),
                    usage.output_tokens,
                );
            }
            write_non_streaming_error(
                ctx.traffic.as_deref(),
                "accumulate",
                "invalid_response",
                usage,
                estimated_input_tokens,
            );
            if let Some(failure) = upstream_failure {
                let status =
                    StatusCode::from_u16(failure.status).unwrap_or(StatusCode::BAD_GATEWAY);
                let response = json_error(status, grok_error_type(status), failure.message.clone());
                if let Some(retry_after) = failure.retry_after.clone() {
                    ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
                } else {
                    response
                }
            } else {
                json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Grok response is invalid",
                )
            }
        }
    }
}

async fn read_non_streaming_body(
    client: Arc<client::GrokClient>,
    request: Arc<PreparedGrokRequest>,
    mut response: client::GrokResponse,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
    retry: Arc<Mutex<GrokRetryState>>,
    deadline: GrokRequestDeadline,
) -> Result<Vec<u8>, GrokError> {
    loop {
        match response.into_bytes().await {
            Ok(bytes) => return Ok(bytes),
            Err(error) if error.permits_model_replay() => {
                let wait_ms = {
                    let mut state = retry.lock().await;
                    state
                        .schedule_model_retry(
                            error.replay_safety,
                            error.retry_after.as_deref(),
                            deadline,
                        )
                        .map_err(|stop| error.clone().into_terminal(stop.terminal_reason))?
                };
                client::run_before_deadline(deadline, retry.clone(), error.stage, sleep(wait_ms))
                    .await?;
                response = client
                    .post_prepared_with_retry(&request, traffic.clone(), retry.clone())
                    .await?;
            }
            Err(error) => return Err(error),
        }
    }
}

struct GrokReplayMaterial {
    client: Arc<client::GrokClient>,
    request: Arc<PreparedGrokRequest>,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
}

struct GrokReconnectContext {
    replay: Option<GrokReplayMaterial>,
    retry: Arc<Mutex<GrokRetryState>>,
    estimated_input_tokens: u64,
    request_byte_lease: Option<RequestByteLease>,
}

impl GrokReconnectContext {
    fn new(
        client: Arc<client::GrokClient>,
        request: Arc<PreparedGrokRequest>,
        traffic: Option<Arc<crate::traffic::TrafficCapture>>,
        retry: Arc<Mutex<GrokRetryState>>,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            replay: Some(GrokReplayMaterial {
                client,
                request,
                traffic,
            }),
            retry,
            estimated_input_tokens,
            request_byte_lease: None,
        }
    }

    fn with_request_byte_lease(mut self, lease: Option<RequestByteLease>) -> Self {
        self.request_byte_lease = lease;
        self
    }
}

#[cfg(test)]
fn stream_body<S>(
    upstream: S,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
    reconnect: Option<GrokReconnectContext>,
) -> Response
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin + Send + 'static,
{
    stream_body_with_policy(
        upstream,
        message_id,
        model,
        monitor,
        req_id,
        traffic,
        reconnect,
        GrokRequestDeadline::after(Duration::from_millis(client::DEFAULT_TOTAL_TIMEOUT_MS)),
        Duration::from_millis(client::DEFAULT_STREAM_HEARTBEAT_MS),
    )
}

fn configured_stream_heartbeat() -> Duration {
    Duration::from_millis(crate::config::grok_stream_heartbeat_ms(
        client::DEFAULT_STREAM_HEARTBEAT_MS,
    ))
    .clamp(MIN_STREAM_HEARTBEAT, MAX_STREAM_HEARTBEAT)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn stream_body_with_policy<S>(
    upstream: S,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
    reconnect: Option<GrokReconnectContext>,
    deadline: GrokRequestDeadline,
    heartbeat_interval: Duration,
) -> Response
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin + Send + 'static,
{
    stream_body_with_policies(
        upstream,
        message_id,
        model,
        monitor,
        req_id,
        traffic,
        reconnect,
        ToolCallPolicy::permissive(),
        deadline,
        heartbeat_interval,
    )
}

#[allow(clippy::too_many_arguments)]
fn stream_body_with_policies<S>(
    upstream: S,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
    reconnect: Option<GrokReconnectContext>,
    tool_policy: ToolCallPolicy,
    deadline: GrokRequestDeadline,
    heartbeat_interval: Duration,
) -> Response
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin + Send + 'static,
{
    let next_heartbeat = tokio::time::Instant::now()
        .checked_add(heartbeat_interval)
        .unwrap_or_else(|| deadline.at())
        .min(deadline.at());
    let estimated_input_tokens = reconnect
        .as_ref()
        .map(|reconnect| reconnect.estimated_input_tokens);
    let state = GrokStreamState {
        upstream: Box::pin(upstream),
        decoder: SseDecoder::default(),
        reducer: translate::reducer::Reducer::with_tool_policy(tool_policy.clone()),
        tool_policy,
        translator: StreamTranslator::new(message_id.clone(), model.clone()),
        message_id,
        model,
        terminal: false,
        error_sent: false,
        downstream_emitted: false,
        hosted_side_effect_started: false,
        attempt_bytes: 0,
        usage: None,
        terminal_status: None,
        upstream_incomplete_reason: None,
        estimated_input_tokens,
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
        stream_capture: traffic.as_ref().map(|traffic| traffic.stream_capture()),
        traffic,
        reconnect,
        rebuild: None,
        deadline,
        heartbeat_interval,
        next_heartbeat,
    };
    let (tx, rx) = mpsc::channel(GROK_DOWNSTREAM_CHANNEL_CAPACITY);
    tokio::spawn(run_grok_stream_producer(state, tx));
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk: BudgetedChunk| (Ok::<Bytes, Infallible>(chunk.into_bytes()), rx))
    });
    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn send_grok_chunk(
    tx: &mpsc::Sender<BudgetedChunk>,
    budget: &ByteBudget,
    bytes: Vec<u8>,
    deadline: tokio::time::Instant,
) -> GrokChunkSendOutcome {
    downstream_queue::send_before_deadline(
        tx,
        budget,
        bytes,
        deadline,
        GROK_DOWNSTREAM_STALL_TIMEOUT,
        std::convert::identity,
    )
    .await
}

async fn run_grok_stream_producer(mut state: GrokStreamState, tx: mpsc::Sender<BudgetedChunk>) {
    let mut terminal_permit = match tx.clone().try_reserve_owned() {
        Ok(permit) => Some(permit),
        Err(_) => return,
    };
    let budget = ByteBudget::new(GROK_DOWNSTREAM_QUEUE_BYTES);
    loop {
        let output = tokio::select! {
            biased;
            _ = tx.closed() => return,
            output = state.next_output() => output,
        };
        let Some(bytes) = output else {
            return;
        };
        // `next_output` can itself observe the absolute deadline and render the
        // terminal error. Sending that through the ordinary deadline-gated path
        // would immediately time out a second time and replace it with an empty
        // idempotent render. The reserved slot exists specifically for this case.
        if state.error_sent {
            send_reserved_grok_terminal(&mut terminal_permit, bytes);
            return;
        }
        match send_grok_chunk(&tx, &budget, bytes, state.deadline.at()).await {
            GrokChunkSendOutcome::Sent => {
                if let Some(bytes) = state.output_enqueued() {
                    send_reserved_grok_terminal(&mut terminal_permit, bytes);
                    return;
                }
            }
            GrokChunkSendOutcome::Closed => return,
            GrokChunkSendOutcome::Deadline => {
                if let Some(retry) = state.retry_state() {
                    retry.lock().await.mark_terminal();
                }
                let error = GrokError::deadline_exceeded(client::GrokErrorStage::Stream);
                let bytes = state.fail_mapped(error, "deadline", "total_timeout");
                send_reserved_grok_terminal(&mut terminal_permit, bytes);
                return;
            }
            GrokChunkSendOutcome::Stalled => {
                if let Some(retry) = state.retry_state() {
                    retry.lock().await.mark_terminal();
                }
                let bytes = state.fail_at("downstream", "consumer_stalled");
                send_reserved_grok_terminal(&mut terminal_permit, bytes);
                return;
            }
            GrokChunkSendOutcome::TooLarge => {
                let bytes = state.fail_at("downstream", "event_size_limit");
                send_reserved_grok_terminal(&mut terminal_permit, bytes);
                return;
            }
        }
    }
}

fn send_reserved_grok_terminal(
    terminal_permit: &mut Option<mpsc::OwnedPermit<BudgetedChunk>>,
    bytes: Vec<u8>,
) {
    if let Some(permit) = terminal_permit.take() {
        let _ = permit.send(BudgetedChunk::unbudgeted(bytes));
    }
}

fn grok_event_starts_hosted_side_effect(value: &serde_json::Value) -> bool {
    let Some(event_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };
    match event_type {
        "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed"
        | "response.x_search_call.in_progress"
        | "response.x_search_call.searching"
        | "response.x_search_call.completed"
        | "response.custom_tool_call_input.delta"
        | "response.custom_tool_call_input.done" => true,
        "response.output_item.added" | "response.output_item.done" => value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "web_search_call" | "x_search_call" | "custom_tool_call"
                )
            }),
        _ => false,
    }
}

struct GrokStreamState {
    upstream: GrokByteStream,
    decoder: SseDecoder,
    reducer: translate::reducer::Reducer,
    tool_policy: ToolCallPolicy,
    translator: StreamTranslator,
    message_id: String,
    model: String,
    terminal: bool,
    error_sent: bool,
    downstream_emitted: bool,
    /// Logical-request replay barrier. Unlike reducer state, this must survive any attempt reset:
    /// observing a hosted search means a retry could repeat a real external action and its cost.
    hosted_side_effect_started: bool,
    attempt_bytes: u64,
    usage: Option<translate::reducer::GrokUsage>,
    terminal_status: Option<translate::reducer::GrokTerminal>,
    upstream_incomplete_reason: Option<String>,
    estimated_input_tokens: Option<u64>,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    chunks: u64,
    stream_capture: Option<StreamTrafficCapture>,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
    reconnect: Option<GrokReconnectContext>,
    rebuild: Option<GrokPendingRebuild>,
    deadline: GrokRequestDeadline,
    heartbeat_interval: Duration,
    next_heartbeat: tokio::time::Instant,
}

impl GrokStreamState {
    fn retry_state(&self) -> Option<Arc<Mutex<GrokRetryState>>> {
        self.reconnect
            .as_ref()
            .map(|reconnect| reconnect.retry.clone())
    }

    /// Commit the logical response as soon as any bytes, including a heartbeat, actually enter the
    /// downstream queue. The retry state remains available for total-deadline and stalled consumer
    /// accounting until the stream producer itself is dropped, but the request body and its
    /// admission lease are no longer needed because no later failure may replay the model request.
    ///
    /// A heartbeat can win while a retry is sleeping or waiting on response headers. Once that
    /// heartbeat is observable downstream, cancel the pending replay and terminate the committed
    /// stream with the original error instead.
    fn output_enqueued(&mut self) -> Option<Vec<u8>> {
        self.downstream_emitted = true;
        if let Some(reconnect) = self.reconnect.as_mut() {
            reconnect.replay.take();
            reconnect.request_byte_lease.take();
        }
        let rebuild = self.rebuild.take()?;
        Some(self.fail_mapped(
            rebuild.original_error,
            rebuild.fail_stage,
            rebuild.fail_kind,
        ))
    }

    fn observe_hosted_side_effects(&mut self, values: &[serde_json::Value]) {
        if self.hosted_side_effect_started
            || !values.iter().any(grok_event_starts_hosted_side_effect)
        {
            return;
        }
        self.hosted_side_effect_started = true;
        // Nothing after this point may replay the model request. Release the large prepared body
        // and its admission lease immediately instead of retaining them until stream teardown.
        if let Some(reconnect) = self.reconnect.as_mut() {
            reconnect.replay.take();
            reconnect.request_byte_lease.take();
        }
    }

    async fn next_output(&mut self) -> Option<Vec<u8>> {
        if self.terminal {
            return None;
        }
        if self.error_sent {
            self.terminal = true;
            return None;
        }
        loop {
            if self.rebuild.is_some() {
                let (fail_stage, fail_kind) = {
                    let rebuild = self.rebuild.as_ref().expect("rebuild must exist");
                    (rebuild.fail_stage, rebuild.fail_kind)
                };
                match self.next_rebuild_poll().await {
                    GrokRebuildPoll::Rebuilt(upstream) => {
                        self.reset_attempt(upstream);
                        continue;
                    }
                    GrokRebuildPoll::Heartbeat => {
                        let ping = stream_ping();
                        self.capture_downstream(&ping);
                        return Some(ping);
                    }
                    GrokRebuildPoll::Failed(error) => {
                        return Some(self.fail_mapped(error, fail_stage, fail_kind));
                    }
                }
            }
            let chunk = match self.next_upstream_chunk().await {
                Ok(GrokStreamPoll::Chunk(chunk)) => chunk,
                Ok(GrokStreamPoll::Heartbeat) => {
                    let ping = stream_ping();
                    self.capture_downstream(&ping);
                    return Some(ping);
                }
                Ok(GrokStreamPoll::End) => {
                    if self.decoder.finish().is_err() || !self.reducer.finished() {
                        match self.begin_rebuild(
                            client::GrokError::stream_transport(
                                client::GrokErrorStage::Stream,
                                "Grok stream closed before a complete terminal event",
                            ),
                            "decoder",
                            "incomplete_stream",
                        ) {
                            Ok(()) => continue,
                            Err(bytes) => return Some(bytes),
                        }
                    }
                    self.terminal = true;
                    self.finish_capture(true);
                    return None;
                }
                Err(error) => match self.begin_rebuild(error, "transport", "upstream_stream") {
                    Ok(()) => continue,
                    Err(bytes) => return Some(bytes),
                },
            };

            if cumulative_stream_size_exceeded(self.bytes, chunk.len()) {
                return Some(self.fail_at("transport", "response_size_limit"));
            }
            if self.bytes == 0
                && let Some(monitor) = self.monitor.as_ref()
            {
                monitor.generation_started(&self.req_id);
            }
            self.bytes = self.bytes.saturating_add(chunk.len() as u64);
            self.attempt_bytes = self.attempt_bytes.saturating_add(chunk.len() as u64);
            self.chunks = self.chunks.saturating_add(1);
            if let Some(monitor) = self.monitor.as_ref() {
                monitor.stream_progress(&self.req_id, chunk.len() as u64, 1, None, None);
            }
            let events = match self.decoder.push(&chunk) {
                Ok(events) => events,
                Err(_) => return Some(self.fail_at("decoder", "malformed_sse")),
            };
            let mut values = Vec::with_capacity(events.len());
            for event in events {
                let value: serde_json::Value = match serde_json::from_str(&event.data) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(capture) = self.stream_capture.as_mut() {
                            capture.malformed("json", "malformed_event");
                        }
                        return Some(self.fail_at("json", "malformed_event"));
                    }
                };
                if let Some(capture) = self.stream_capture.as_mut() {
                    capture.upstream_event(event.event.as_deref(), &value);
                }
                values.push(value);
            }

            // A retryable failure in the same decoded batch rolls reducer state back. Observe the
            // irreversible hosted boundary first so that transaction rollback can never reopen the
            // logical request's replay window.
            self.observe_hosted_side_effects(&values);

            // Validate and render the entire decoded chunk transactionally. In particular, a
            // terminal followed by text/tool/unknown data in the same chunk must not leak the
            // staged completion (or any earlier staged delta) before the protocol error.
            let reduced = match self.reducer.push_batch_in_place(values) {
                Ok(events) => events,
                Err(error) => {
                    let Some(failure) = error
                        .downcast_ref::<translate::reducer::GrokUpstreamFailure>()
                        .cloned()
                    else {
                        let detail = error.to_string();
                        crate::logging::create_logger("grok").info(
                            "stream_reducer_error",
                            Some(serde_json::Map::from_iter([
                                ("reqId".into(), serde_json::json!(self.req_id)),
                                ("model".into(), serde_json::json!(self.model)),
                                ("stage".into(), serde_json::json!("reducer")),
                                ("kind".into(), serde_json::json!("invalid_event")),
                                ("message".into(), serde_json::json!(detail)),
                                ("bytes".into(), serde_json::json!(self.bytes)),
                                ("chunks".into(), serde_json::json!(self.chunks)),
                                (
                                    "downstreamEmitted".into(),
                                    serde_json::json!(self.downstream_emitted),
                                ),
                            ])),
                        );
                        return Some(self.fail_at("reducer", "invalid_event"));
                    };
                    let status =
                        StatusCode::from_u16(failure.status).unwrap_or(StatusCode::BAD_GATEWAY);
                    let error = GrokError::upstream_event(
                        status,
                        failure.retry_after,
                        failure.message,
                        failure.retryable,
                    );
                    let kind = match failure.event_type.as_str() {
                        "response.failed" => "response_failed",
                        "response.error" => "response_error",
                        _ => "error_event",
                    };
                    match self.begin_rebuild(error, "upstream", kind) {
                        Ok(()) => continue,
                        Err(bytes) => return Some(bytes),
                    }
                }
            };
            let usage = reduced.iter().find_map(|event| match event {
                translate::reducer::ReducerEvent::Usage(usage) => Some(usage.clone()),
                _ => None,
            });
            let terminal_status = reduced.iter().find_map(|event| match event {
                translate::reducer::ReducerEvent::Terminal(status) => Some(*status),
                _ => None,
            });
            let stop_reason = reduced.iter().find_map(|event| match event {
                translate::reducer::ReducerEvent::Finish { stop_reason, .. } => {
                    Some(stop_reason.clone())
                }
                _ => None,
            });
            let finished = self.reducer.finished();
            // A terminal event is the upstream protocol's commit point. All fully decoded events
            // from this push batch have already passed reducer validation, so a partial frame left
            // in the decoder is only an unclassifiable post-terminal tail. Ignoring it makes the
            // outcome independent of arbitrary HTTP chunk boundaries while a fully decoded
            // semantic tail in this same batch is still rejected above.
            let out = match self.translator.render(reduced) {
                Ok(bytes) => bytes,
                Err(_) => return Some(self.fail_at("render", "invalid_event")),
            };
            let upstream_incomplete_reason = self.reducer.incomplete_reason().map(str::to_owned);
            if let Some(usage) = usage {
                if let Some(monitor) = self.monitor.as_ref() {
                    monitor.usage_updated(
                        &self.req_id,
                        usage.mapped_input_tokens(),
                        usage.output_tokens,
                    );
                }
                self.usage = Some(usage);
            }
            if let Some(status) = terminal_status {
                self.terminal_status = Some(status);
                self.upstream_incomplete_reason = upstream_incomplete_reason;
            }
            if finished {
                self.terminal = true;
                let outcome = terminal_status
                    .map(translate::reducer::GrokTerminal::outcome)
                    .unwrap_or("completed");
                self.log_stream_terminal(outcome, stop_reason.as_deref(), None, None);
                self.capture_downstream(&out);
                self.finish_capture(true);
                return (!out.is_empty()).then_some(out);
            }
            if !out.is_empty() {
                self.capture_downstream(&out);
                self.schedule_next_heartbeat();
                return Some(out);
            }
        }
    }

    async fn next_upstream_chunk(&mut self) -> Result<GrokStreamPoll, GrokError> {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.deadline.at()) => {
                if let Some(reconnect) = self.reconnect.as_ref() {
                    reconnect.retry.lock().await.mark_terminal();
                }
                Err(GrokError::deadline_exceeded(client::GrokErrorStage::Stream))
            }
            _ = tokio::time::sleep_until(self.next_heartbeat) => {
                self.schedule_next_heartbeat();
                Ok(GrokStreamPoll::Heartbeat)
            },
            item = self.upstream.next() => match item {
                Some(Ok(chunk)) => Ok(GrokStreamPoll::Chunk(chunk)),
                Some(Err(error)) => Err(error),
                None => Ok(GrokStreamPoll::End),
            },
        }
    }

    fn schedule_next_heartbeat(&mut self) {
        self.next_heartbeat = tokio::time::Instant::now()
            .checked_add(self.heartbeat_interval)
            .unwrap_or_else(|| self.deadline.at())
            .min(self.deadline.at());
    }

    fn begin_rebuild(
        &mut self,
        error: GrokError,
        fail_stage: &'static str,
        fail_kind: &'static str,
    ) -> Result<(), Vec<u8>> {
        if self.downstream_emitted
            || self.hosted_side_effect_started
            || !error.permits_model_replay()
        {
            return Err(self.fail_mapped(error, fail_stage, fail_kind));
        }
        let Some(reconnect) = self.reconnect.as_ref() else {
            return Err(self.fail_at(fail_stage, fail_kind));
        };
        let Some(replay) = reconnect.replay.as_ref() else {
            return Err(self.fail_at(fail_stage, fail_kind));
        };
        let client = replay.client.clone();
        let request = replay.request.clone();
        let traffic = replay.traffic.clone();
        let retry = reconnect.retry.clone();
        let deadline = self.deadline;
        let attempt_bytes = self.attempt_bytes;
        let original_error = error.clone();
        let future = Box::pin(async move {
            let wait_ms = {
                let mut state = retry.lock().await;
                let wait_ms = state
                    .schedule_model_retry(
                        error.replay_safety,
                        error.retry_after.as_deref(),
                        deadline,
                    )
                    .map_err(|stop| error.clone().into_terminal(stop.terminal_reason))?;
                let transient = state.transient_failures();
                let wire_attempt = state.wire_attempt();
                let log_context = state.log_context();
                client::log_retry(wire_attempt, transient, wait_ms, &error, &log_context);
                wait_ms
            };
            client::run_before_deadline(deadline, retry.clone(), error.stage, sleep(wait_ms))
                .await?;
            // Record an actual rebuild attempt only after its backoff has completed. A downstream
            // heartbeat can commit the response while this future is sleeping; in that case the
            // future is dropped and must not leave an artifact claiming a replay was dispatched.
            if let Some(traffic) = traffic.as_ref() {
                traffic.write_json(
                    "024-upstream-stream-rebuild",
                    &serde_json::json!({
                        "wait_ms": wait_ms,
                        "origin": client::origin_name(error.origin),
                        "error_stage": client::stage_name(error.stage),
                        "status": error.status.as_u16(),
                        "replay_safety": error.replay_safety.as_str(),
                        "deadline_remaining_ms": deadline.remaining_ms(),
                        "message": error.message,
                        "stage": fail_stage,
                        "kind": fail_kind,
                        "attempt_bytes": attempt_bytes,
                    }),
                );
            }
            client
                .post_prepared_with_retry(&request, traffic, retry)
                .await
                .map(client::GrokResponse::into_stream)
        });
        self.rebuild = Some(GrokPendingRebuild {
            future,
            original_error,
            fail_stage,
            fail_kind,
        });
        Ok(())
    }

    async fn next_rebuild_poll(&mut self) -> GrokRebuildPoll {
        enum WaitResult {
            Deadline,
            Heartbeat,
            Finished(Result<GrokByteStream, GrokError>),
        }

        let wait = {
            let rebuild = self.rebuild.as_mut().expect("rebuild must exist");
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(self.deadline.at()) => WaitResult::Deadline,
                _ = tokio::time::sleep_until(self.next_heartbeat) => WaitResult::Heartbeat,
                result = &mut rebuild.future => WaitResult::Finished(result),
            }
        };
        match wait {
            WaitResult::Deadline => {
                // Drop the in-flight auth/header/body request before taking the retry lock. This
                // makes downstream cancellation and the wall-clock deadline cancel the socket,
                // rather than leaving a detached reconnect task behind.
                self.rebuild.take();
                if let Some(reconnect) = self.reconnect.as_ref() {
                    reconnect.retry.lock().await.mark_terminal();
                }
                GrokRebuildPoll::Failed(GrokError::deadline_exceeded(
                    client::GrokErrorStage::Stream,
                ))
            }
            WaitResult::Heartbeat => {
                self.schedule_next_heartbeat();
                GrokRebuildPoll::Heartbeat
            }
            WaitResult::Finished(result) => {
                self.rebuild.take();
                match result {
                    Ok(upstream) => GrokRebuildPoll::Rebuilt(upstream),
                    Err(error) => GrokRebuildPoll::Failed(error),
                }
            }
        }
    }

    fn reset_attempt(&mut self, upstream: GrokByteStream) {
        self.upstream = upstream;
        self.decoder = SseDecoder::default();
        self.reducer = translate::reducer::Reducer::with_tool_policy(self.tool_policy.clone());
        self.translator = StreamTranslator::new(self.message_id.clone(), self.model.clone());
        self.attempt_bytes = 0;
        self.usage = None;
        self.terminal_status = None;
        self.upstream_incomplete_reason = None;
        self.error_sent = false;
        self.terminal = false;
    }

    fn fail_mapped(&mut self, error: GrokError, stage: &str, kind: &str) -> Vec<u8> {
        self.fail(stage, kind, Some(&error))
    }

    fn fail_at(&mut self, stage: &str, kind: &str) -> Vec<u8> {
        self.fail(stage, kind, None)
    }

    fn fail(&mut self, stage: &str, kind: &str, error: Option<&GrokError>) -> Vec<u8> {
        self.error_sent = true;
        if self.usage.is_none() {
            self.usage = self.reducer.usage().cloned();
            if let (Some(monitor), Some(usage)) = (self.monitor.as_ref(), self.usage.as_ref()) {
                monitor.usage_updated(
                    &self.req_id,
                    usage.mapped_input_tokens(),
                    usage.output_tokens,
                );
            }
        }
        let (error_type, stream_message) = match error {
            Some(error) => (grok_error_type(error.status), error.message.as_str()),
            None => ("api_error", "Grok stream is invalid"),
        };
        let bytes = self
            .translator
            .render_typed_error(error_type, stream_message);
        if let Some(capture) = self.stream_capture.as_mut() {
            capture.malformed(stage, kind);
        }
        self.capture_downstream(&bytes);
        if let Some(traffic) = self.traffic.as_ref() {
            let mut fields = serde_json::Map::from_iter([
                ("stage".into(), serde_json::json!(stage)),
                ("kind".into(), serde_json::json!(kind)),
                ("bytes".into(), serde_json::json!(self.bytes)),
                ("chunks".into(), serde_json::json!(self.chunks)),
                (
                    "downstream_emitted".into(),
                    serde_json::json!(self.downstream_emitted),
                ),
                ("usage".into(), self.usage_observability()),
            ]);
            if let Some(error) = error {
                fields.insert("status".into(), serde_json::json!(error.status.as_u16()));
                fields.insert(
                    "origin".into(),
                    serde_json::json!(client::origin_name(error.origin)),
                );
                fields.insert(
                    "error_stage".into(),
                    serde_json::json!(client::stage_name(error.stage)),
                );
                fields.insert("message".into(), serde_json::json!(error.message));
                if let Some(retry_after) = error.retry_after.as_ref() {
                    fields.insert("retry_after".into(), serde_json::json!(retry_after));
                }
            }
            traffic.write_json("060-grok-stream-error", &serde_json::Value::Object(fields));
        }
        self.log_stream_terminal("failed", None, Some((stage, kind)), error);
        self.finish_capture(false);
        bytes
    }

    fn capture_downstream(&mut self, bytes: &[u8]) {
        let Some(capture) = self.stream_capture.as_mut() else {
            return;
        };
        let mut decoder = SseDecoder::default();
        if let Ok(events) = decoder.push(bytes) {
            for event in events {
                if let Ok(data) = serde_json::from_str(&event.data) {
                    capture.downstream_event(event.event.as_deref().unwrap_or("message"), data);
                }
            }
        }
    }

    fn finish_capture(&mut self, completed: bool) {
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish(
                traffic,
                serde_json::json!({
                    "kind": if completed { "stream_completion" } else { "stream_error" },
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                    "terminalOutcome":self.terminal_status.map(translate::reducer::GrokTerminal::outcome),
                    "incompleteReason":self.terminal_status.and_then(translate::reducer::GrokTerminal::incomplete_reason),
                    "upstreamIncompleteReason":self.upstream_incomplete_reason,
                    "usage": self.usage_observability(),
                }),
            );
        }
    }

    fn log_stream_terminal(
        &self,
        outcome: &str,
        stop_reason: Option<&str>,
        failure: Option<(&str, &str)>,
        error: Option<&GrokError>,
    ) {
        let mut fields = serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(self.req_id)),
            ("model".into(), serde_json::json!(self.model)),
            ("outcome".into(), serde_json::json!(outcome)),
            ("stopReason".into(), serde_json::json!(stop_reason)),
            ("bytes".into(), serde_json::json!(self.bytes)),
            ("chunks".into(), serde_json::json!(self.chunks)),
            (
                "downstreamEmitted".into(),
                serde_json::json!(self.downstream_emitted),
            ),
            ("usage".into(), self.usage_observability()),
            (
                "incompleteReason".into(),
                serde_json::json!(
                    self.terminal_status
                        .and_then(translate::reducer::GrokTerminal::incomplete_reason)
                ),
            ),
            (
                "upstreamIncompleteReason".into(),
                serde_json::json!(self.upstream_incomplete_reason),
            ),
        ]);
        if let Some((stage, kind)) = failure {
            fields.insert("stage".into(), serde_json::json!(stage));
            fields.insert("kind".into(), serde_json::json!(kind));
        }
        if let Some(error) = error {
            fields.insert("status".into(), serde_json::json!(error.status.as_u16()));
            fields.insert(
                "origin".into(),
                serde_json::json!(client::origin_name(error.origin)),
            );
            fields.insert(
                "errorStage".into(),
                serde_json::json!(client::stage_name(error.stage)),
            );
        }
        crate::logging::create_logger("grok").info("stream_terminal", Some(fields));
    }

    fn usage_observability(&self) -> serde_json::Value {
        grok_usage_observability(self.usage.as_ref(), self.estimated_input_tokens)
    }
}

fn grok_usage_observability(
    usage: Option<&translate::reducer::GrokUsage>,
    estimated_input_tokens: Option<u64>,
) -> serde_json::Value {
    let Some(usage) = usage else {
        return serde_json::json!({
            "state":"unavailable",
            "inputTokens":null,
            "outputTokens":null,
            "cacheReadInputTokens":null,
            "reasoningTokens":null,
            "upstreamTotalTokens":null,
            "estimatedInputTokens":estimated_input_tokens
        });
    };
    serde_json::json!({
        "state":usage.availability_state(),
        "inputTokens":usage.input_tokens,
        "mappedInputTokens":usage.mapped_input_tokens(),
        "outputTokens":usage.output_tokens,
        "cacheReadInputTokens":usage.mapped_cache_read_input_tokens(),
        "cacheBreakdownInconsistent":usage.cache_breakdown_is_inconsistent(),
        "cacheCreationInputTokensState":"unavailable",
        "reasoningTokens":usage.reasoning_tokens,
        "upstreamTotalTokens":usage.total_tokens,
        "estimatedInputTokens":estimated_input_tokens
    })
}

fn cumulative_stream_size_exceeded(current: u64, next: usize) -> bool {
    current.saturating_add(next as u64) > GROK_MAX_CUMULATIVE_STREAM_BYTES
}

type GrokRebuildFuture =
    Pin<Box<dyn Future<Output = Result<GrokByteStream, GrokError>> + Send + 'static>>;

struct GrokPendingRebuild {
    future: GrokRebuildFuture,
    original_error: GrokError,
    fail_stage: &'static str,
    fail_kind: &'static str,
}

enum GrokRebuildPoll {
    Rebuilt(GrokByteStream),
    Heartbeat,
    Failed(GrokError),
}

enum GrokStreamPoll {
    Chunk(Bytes),
    Heartbeat,
    End,
}

impl Drop for GrokStreamState {
    fn drop(&mut self) {
        if self.terminal || self.stream_capture.is_none() {
            return;
        }
        if let Some(traffic) = self.traffic.as_ref() {
            traffic.write_json(
                "060-grok-stream-abandoned",
                &serde_json::json!({
                    "stage": "downstream",
                    "kind": "client_disconnect",
                    "reason": "downstream_body_dropped",
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish(
                traffic,
                serde_json::json!({
                    "kind": "stream_abandoned",
                    "reason": "downstream_body_dropped",
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
    }
}

fn write_error(traffic: Option<&crate::traffic::TrafficCapture>, stage: &str, kind: &str) {
    if let Some(traffic) = traffic {
        traffic.write_json(
            "060-grok-stream-error",
            &serde_json::json!({"stage":stage,"kind":kind}),
        );
    }
}

fn write_non_streaming_error(
    traffic: Option<&crate::traffic::TrafficCapture>,
    stage: &str,
    kind: &str,
    usage: Option<&translate::reducer::GrokUsage>,
    estimated_input_tokens: u64,
) {
    if let Some(traffic) = traffic {
        traffic.write_json(
            "060-grok-stream-error",
            &serde_json::json!({
                "stage":stage,
                "kind":kind,
                "usage":grok_usage_observability(usage, Some(estimated_input_tokens)),
            }),
        );
    }
}

fn map_error(error: client::GrokError) -> Response {
    let status = error.status;
    let mapped_status = if status.is_client_error() || status.is_server_error() {
        status
    } else {
        StatusCode::BAD_GATEWAY
    };
    let kind = grok_error_type(status);
    let response = json_error(mapped_status, kind, error.message);
    if let Some(retry_after) = error.retry_after {
        ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
    } else {
        response
    }
}

fn grok_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => {
            "invalid_request_error"
        }
        status if status.as_u16() == 529 => "overloaded_error",
        status if status.is_client_error() => "invalid_request_error",
        _ => "api_error",
    }
}

pub struct GrokCli;
pub static GROK_CLI: GrokCli = GrokCli;
impl CliHandlers for GrokCli {
    fn login(&self) -> anyhow::Result<()> {
        let store = file_store_with_migration();
        auth::login::login(&store)?;
        println!("Grok authentication saved in {}", store.auth_path());
        Ok(())
    }
    fn device(&self) -> anyhow::Result<()> {
        let store = file_store_with_migration();
        auth::device::device_login(&store)?;
        println!("Grok authentication saved in {}", store.auth_path());
        Ok(())
    }
    fn status(&self) -> anyhow::Result<()> {
        let store = file_store_with_migration();
        match store.load_auth()? {
            Some(auth) => {
                println!("Auth path: {}", store.auth_path());
                println!("Authenticated: true");
                println!(
                    "Expires in {}s",
                    auth.expires_at_ms.saturating_sub(now_ms()) / 1000
                );
                Ok(())
            }
            None => anyhow::bail!("Not authenticated"),
        }
    }
    fn logout(&self) -> anyhow::Result<()> {
        let store = file_store_with_migration();
        store.clear_auth_exclusive()?;
        println!("Grok proxy credentials removed");
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::monitor::{EndpointKind, MonitorHandle};
    use crate::traffic::test_capture;
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::client::{GrokErrorStage, GrokTimeouts};
    use super::*;

    #[tokio::test]
    async fn count_tokens_admission_errors_map_to_retryable_503_or_worker_500() {
        let saturated = token_count_admission::run_with(
            Arc::new(tokio::sync::Semaphore::new(0)),
            Duration::from_millis(10),
            || 1_u64,
        )
        .await
        .expect_err("a gate with no permits must time out");
        assert_eq!(
            map_grok_count_tokens_error(saturated).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        let failed = token_count_admission::run_with(
            Arc::new(tokio::sync::Semaphore::new(1)),
            Duration::from_secs(1),
            || -> u64 { panic!("synthetic Grok token-count worker failure") },
        )
        .await
        .expect_err("a panicking blocking worker must return JoinError");
        assert_eq!(
            map_grok_count_tokens_error(failed).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn cumulative_stream_byte_limit_accepts_boundary_and_rejects_next_byte() {
        assert!(!cumulative_stream_size_exceeded(
            GROK_MAX_CUMULATIVE_STREAM_BYTES - 1,
            1
        ));
        assert!(cumulative_stream_size_exceeded(
            GROK_MAX_CUMULATIVE_STREAM_BYTES,
            1
        ));
    }

    #[test]
    fn hosted_side_effect_detection_is_conservative_and_specific() {
        for value in [
            serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"web_search_call","id":"ws1"}
            }),
            serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"x_search_call","id":"xs1"}
            }),
            serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"custom_tool_call","name":"x_search","id":"xs2"}
            }),
            serde_json::json!({
                "type":"response.web_search_call.searching",
                "item_id":"ws1"
            }),
            serde_json::json!({
                "type":"response.custom_tool_call_input.delta",
                "item_id":"xs2",
                "delta":"{}"
            }),
        ] {
            assert!(grok_event_starts_hosted_side_effect(&value), "{value}");
        }

        for value in [
            serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"function_call","call_id":"c1","name":"Read"}
            }),
            serde_json::json!({"type":"response.reasoning_text.delta","delta":"draft"}),
            serde_json::json!({"type":"response.failed","response":{}}),
        ] {
            assert!(!grok_event_starts_hosted_side_effect(&value), "{value}");
        }
    }

    #[test]
    fn base64_image_detection_finds_direct_and_nested_tool_result_images() {
        for content in [
            serde_json::json!([{
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":"AA=="}
            }]),
            serde_json::json!([{
                "type":"tool_result",
                "tool_use_id":"call_1",
                "content":[{
                    "type":"image",
                    "source":{"type":"base64","media_type":"image/jpeg","data":"AA=="}
                }]
            }]),
        ] {
            let body: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":content}]
            }))
            .unwrap();
            assert!(
                crate::providers::translate_shared::messages_request_contains_base64_image(&body)
            );
        }

        let url_image: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[{
                "type":"image",
                "source":{"type":"url","url":"https://example.com/image.png"}
            }]}]
        }))
        .unwrap();
        assert!(
            !crate::providers::translate_shared::messages_request_contains_base64_image(&url_image)
        );
    }

    struct ChannelStream(mpsc::Receiver<Result<Bytes, client::GrokError>>);

    impl Stream for ChannelStream {
        type Item = Result<Bytes, client::GrokError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    #[tokio::test]
    async fn completely_silent_stream_emits_pings_then_explicit_total_timeout() {
        let (tx, rx) = mpsc::channel(1);
        let response = stream_body_with_policy(
            ChannelStream(rx),
            "msg_silent".into(),
            "grok-4.5".into(),
            None,
            "req_silent".into(),
            None,
            None,
            GrokRequestDeadline::after(Duration::from_millis(90)),
            Duration::from_millis(20),
        );

        let body = tokio::time::timeout(Duration::from_millis(500), response.into_body().collect())
            .await
            .expect("the total deadline should end a silent stream")
            .unwrap()
            .to_bytes();
        drop(tx);
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.matches("event: ping").count() >= 2, "{body}");
        assert!(body.contains(r#"{"type":"ping"}"#), "{body}");
        assert!(body.contains("total wall-clock timeout"), "{body}");
        assert!(!body.contains("Grok stream is invalid"), "{body}");
    }

    #[tokio::test]
    async fn continuous_reasoning_cannot_starve_ping_or_extend_total_deadline() {
        let (tx, rx) = mpsc::channel(8);
        let producer = tokio::spawn(async move {
            let chunk = Bytes::from_static(
                b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"draft \\ud83d\\ude0a\"}\n\n",
            );
            loop {
                if tx.send(Ok(chunk.clone())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        });
        let response = stream_body_with_policy(
            ChannelStream(rx),
            "msg_reasoning".into(),
            "grok-4.5".into(),
            None,
            "req_reasoning".into(),
            None,
            None,
            GrokRequestDeadline::after(Duration::from_millis(90)),
            Duration::from_millis(20),
        );

        let body = tokio::time::timeout(Duration::from_millis(500), response.into_body().collect())
            .await
            .expect("reasoning traffic must not extend the total deadline")
            .unwrap()
            .to_bytes();
        producer.await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let pings = body.matches("event: ping").count();

        assert!((2..=6).contains(&pings), "pings={pings}, body={body}");
        assert!(!body.contains("draft"), "{body}");
        assert!(!body.contains('😊'), "{body}");
        assert!(body.contains("total wall-clock timeout"), "{body}");
    }

    #[tokio::test]
    async fn continuous_reasoning_emits_pings_before_one_final_answer() {
        let (tx, rx) = mpsc::channel(8);
        let producer = tokio::spawn(async move {
            let reasoning = Bytes::from_static(
                b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"draft\"}\n\n",
            );
            for _ in 0..24 {
                tx.send(Ok(reasoning.clone())).await.unwrap();
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
            tx.send(Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"FINAL_ONCE\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
            )))
            .await
            .unwrap();
        });
        let response = stream_body_with_policy(
            ChannelStream(rx),
            "msg_reasoning_final".into(),
            "grok-4.5".into(),
            None,
            "req_reasoning_final".into(),
            None,
            None,
            GrokRequestDeadline::after(Duration::from_millis(500)),
            Duration::from_millis(20),
        );

        let body = tokio::time::timeout(Duration::from_secs(1), response.into_body().collect())
            .await
            .expect("the final answer should complete before the total deadline")
            .unwrap()
            .to_bytes();
        producer.await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.matches("event: ping").count() >= 2, "{body}");
        assert_eq!(body.matches("FINAL_ONCE").count(), 1, "{body}");
        assert!(!body.contains("draft"), "{body}");
        assert!(!body.contains("total wall-clock timeout"), "{body}");
    }

    #[tokio::test]
    async fn dropping_downstream_body_immediately_drops_upstream_stream() {
        let (tx, rx) = mpsc::channel(1);
        let response = stream_body_with_policy(
            ChannelStream(rx),
            "msg_drop".into(),
            "grok-4.5".into(),
            None,
            "req_drop".into(),
            None,
            None,
            GrokRequestDeadline::after(Duration::from_secs(5)),
            Duration::from_millis(20),
        );
        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_millis(200), body.frame())
            .await
            .expect("heartbeat should make the body observable")
            .expect("stream should yield a heartbeat")
            .unwrap();
        assert!(String::from_utf8_lossy(frame.data_ref().unwrap()).contains("event: ping"));

        drop(body);

        tokio::time::timeout(Duration::from_millis(100), tx.closed())
            .await
            .expect("dropping the downstream body must cancel the upstream poller");
    }

    #[tokio::test]
    async fn grok_heartbeat_commit_closes_replay_window() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_millis(200), listener.accept()).await {
                Err(_) => 0,
                Ok(Ok((mut stream, _))) => {
                    let _ = read_complete_http_request(&mut stream).await;
                    1
                }
                Ok(Err(error)) => panic!("replay listener failed: {error}"),
            }
        });
        let (client, temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 5_000,
                first_byte_ms: 5_000,
                body_idle_ms: 5_000,
            },
        )
        .await;
        let deadline = GrokRequestDeadline::after(Duration::from_millis(500));
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(deadline)));
        let capture_root = temp.path().join("traffic");
        let traffic = Arc::new(test_capture(capture_root.clone()));
        let reconnect = Some(GrokReconnectContext::new(
            Arc::new(client),
            prepared_request(false),
            Some(traffic.clone()),
            retry.clone(),
            1,
        ));
        // Keep the permitted replay in its Retry-After sleep until the heartbeat is committed.
        // The pending future must be cancelled before it can issue the second logical POST.
        let reset = client::GrokError::upstream_event(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("0.25".into()),
            "synthetic retryable upstream event",
            true,
        );

        let response = stream_body_with_policy(
            futures_util::stream::iter(vec![Err(reset)]),
            "msg_heartbeat_commit".into(),
            "grok-4.5".into(),
            None,
            "req_heartbeat_commit".into(),
            Some(traffic),
            reconnect,
            deadline,
            Duration::from_millis(25),
        );
        let body = tokio::time::timeout(Duration::from_secs(1), response.into_body().collect())
            .await
            .expect("the committed heartbeat must terminate the pending rebuild")
            .unwrap()
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(body.matches("event: ping").count(), 1, "{body}");
        assert!(body.contains("event: error"), "{body}");
        assert!(
            body.contains("synthetic retryable upstream event"),
            "{body}"
        );
        assert!(!body.contains("total wall-clock timeout"), "{body}");
        assert_eq!(
            server.await.unwrap(),
            0,
            "a committed heartbeat must prevent the replay POST"
        );
        assert!(
            capture_contents_matching(capture_root, "024-upstream-stream-rebuild").is_empty(),
            "a replay cancelled during backoff must not leave a rebuild artifact"
        );
    }

    #[tokio::test]
    async fn heartbeat_during_inflight_rebuild_cancels_socket_and_emits_original_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_complete_http_request(&mut stream).await;
            let _ = request_seen_tx.send(());
            let mut byte = [0_u8; 1];
            tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
                .await
                .expect("the committed heartbeat must promptly cancel the rebuild socket")
                .unwrap()
        });
        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 5_000,
                first_byte_ms: 5_000,
                body_idle_ms: 5_000,
            },
        )
        .await;
        let deadline = GrokRequestDeadline::after(Duration::from_secs(5));
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(deadline)));
        let reconnect = Some(GrokReconnectContext::new(
            Arc::new(client),
            prepared_request(false),
            None,
            retry,
            1,
        ));
        let reset = client::GrokError::upstream_event(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("0".into()),
            "synthetic retryable upstream event",
            true,
        );
        let response = stream_body_with_policy(
            futures_util::stream::iter(vec![Err(reset)]),
            "msg_cancel_rebuild".into(),
            "grok-4.5".into(),
            None,
            "req_cancel_rebuild".into(),
            None,
            reconnect,
            deadline,
            Duration::from_millis(500),
        );
        tokio::time::timeout(Duration::from_secs(1), request_seen_rx)
            .await
            .expect("the rebuild request must start before the heartbeat")
            .expect("the rebuild server must observe the request");
        let body = tokio::time::timeout(Duration::from_secs(2), response.into_body().collect())
            .await
            .expect("the heartbeat must terminate the in-flight rebuild")
            .unwrap()
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(body.matches("event: ping").count(), 1, "{body}");
        assert!(body.contains("event: error"), "{body}");
        assert!(
            body.contains("synthetic retryable upstream event"),
            "{body}"
        );
        assert!(!body.contains("total wall-clock timeout"), "{body}");
        assert_eq!(
            server.await.unwrap(),
            0,
            "the in-flight replay socket must close after heartbeat commit"
        );
    }

    fn sample_request() -> GrokResponsesRequest {
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

    fn response_tool_policy(tool_choice: serde_json::Value) -> ToolCallPolicy {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"use tools"}],
            "tools":[{"name":"Read","input_schema":{"type":"object"}}],
            "tool_choice":tool_choice
        }))
        .unwrap();
        let translated =
            translate_request(&request, "grok-4.5".into()).expect("test request must translate");
        ToolCallPolicy::from_request(&translated)
    }

    #[tokio::test]
    async fn production_stream_wrapper_applies_request_tool_policy() {
        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"Read\"}}\n\n",
        ))]);
        let response = stream_body_with_policies(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            None,
            None,
            response_tool_policy(serde_json::json!({"type":"none"})),
            GrokRequestDeadline::after(Duration::from_secs(1)),
            Duration::from_secs(1),
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("event: error"), "{body}");
        assert!(!body.contains("tool_use"), "{body}");
    }

    #[tokio::test]
    async fn production_non_stream_wrapper_applies_request_tool_policy() {
        let ctx = RequestContext {
            req_id: "req_1".into(),
            session_id: None,
            session_seq: None,
            provider: "grok".into(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        };
        let upstream = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"Read\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"arguments\":\"{}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n";
        let response = finish_non_streaming_message_with_tool_policy(
            upstream,
            "grok-4.5",
            1,
            &ctx,
            &response_tool_policy(serde_json::json!({"type":"none"})),
        );
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn production_live_rejects_cross_kind_downstream_tool_id_collision_before_rendering() {
        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws1\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"srvtoolu_ws1\",\"name\":\"Read\"}}\n\n",
        ))]);
        let response = stream_body_with_policy(
            upstream,
            "msg_collision".into(),
            "grok-4.5".into(),
            None,
            "req_collision".into(),
            None,
            None,
            GrokRequestDeadline::after(Duration::from_secs(1)),
            Duration::from_secs(1),
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("event: error"), "{body}");
        assert!(!body.contains("content_block_start"), "{body}");
        assert!(!body.contains("server_tool_use"), "{body}");
    }

    #[tokio::test]
    async fn production_non_stream_rejects_cross_kind_downstream_tool_id_collision() {
        let ctx = RequestContext {
            req_id: "req_collision".into(),
            session_id: None,
            session_seq: None,
            provider: "grok".into(),
            traffic: None,
            monitor: None,
            request_byte_lease: None,
        };
        let upstream = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws1\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"srvtoolu_ws1\",\"name\":\"Read\"}}\n\n";
        let response = finish_non_streaming_message(upstream, "grok-4.5", 1, &ctx);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "api_error");
    }

    fn prepared_request(capture_body: bool) -> Arc<PreparedGrokRequest> {
        Arc::new(PreparedGrokRequest::new(&sample_request(), capture_body).unwrap())
    }

    async fn test_client(base_url: &str, timeouts: GrokTimeouts) -> (client::GrokClient, TempDir) {
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
        let store = auth::token_store::test_file_store(primary, legacy);
        store
            .save_auth(auth::token_store::StoredAuth {
                access: "test-access".into(),
                refresh: "test-refresh".into(),
                expires_at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 3_600_000,
                issuer: auth::login::CANONICAL_ISSUER.into(),
                client_id: auth::login::CLIENT_ID.into(),
            })
            .unwrap();
        let auth_manager = auth::manager::GrokAuthManager::new(store).unwrap();
        let client = client::GrokClient::new_for_test(
            base_url.to_string(),
            "test-version".into(),
            timeouts,
            auth_manager,
        )
        .unwrap();
        (client, temp)
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut buf = vec![0_u8; 32 * 1024];
        let mut total = 0usize;
        loop {
            let n = stream.read(&mut buf[total..]).await.unwrap();
            assert!(n > 0);
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

    async fn read_complete_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = read_http_request(stream).await;
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("HTTP request must contain complete headers");
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let complete_len = header_end.saturating_add(content_length);
        if request.len() < complete_len {
            let previous_len = request.len();
            request.resize(complete_len, 0);
            stream
                .read_exact(&mut request[previous_len..])
                .await
                .expect("HTTP request body must arrive");
        }
        request
    }

    fn http_request_body(request: &[u8]) -> &[u8] {
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("HTTP request must contain complete headers");
        &request[header_end..]
    }

    #[tokio::test]
    async fn map_error_preserves_transient_gateway_statuses() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let response = map_error(client::GrokError {
                status,
                retry_after: None,
                message: "temporary authentication failure".into(),
                origin: client::GrokErrorOrigin::Auth,
                stage: client::GrokErrorStage::Auth,
                retryable: true,
                replay_safety: crate::retry::ReplaySafety::OutcomeUnknown,
            });
            assert_eq!(response.status(), status);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "api_error");
            assert_eq!(body["error"]["message"], "temporary authentication failure");
        }
    }

    #[tokio::test]
    async fn map_error_preserves_safe_upstream_client_failures() {
        for (status, expected_kind) in [
            (StatusCode::BAD_REQUEST, "invalid_request_error"),
            (StatusCode::NOT_FOUND, "not_found_error"),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request_error"),
        ] {
            let response = map_error(client::GrokError::http(
                status,
                None,
                "tool schema is invalid",
            ));
            assert_eq!(response.status(), status);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], expected_kind);
            assert_eq!(body["error"]["message"], "tool schema is invalid");
        }
    }

    #[tokio::test]
    async fn streaming_usage_updates_completed_monitor_request() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "req_1",
            Some("session_1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.provider_selected("req_1", "grok", "grok-4.5", None);
        monitor.request_completed("req_1", 200, None, None);

        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens\":3}}}\n\n",
        ))]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            Some(monitor.clone()),
            "req_1".into(),
            None,
            None,
        );
        let _ = response.into_body().collect().await.unwrap();

        let snapshot = monitor.snapshot();
        let request = snapshot
            .recent
            .iter()
            .find(|request| request.request_id == "req_1")
            .unwrap();
        assert_eq!(request.input_tokens, Some(9));
        assert_eq!(request.output_tokens, Some(3));
        assert!(request.streamed_bytes > 0);
        assert!(request.stream_chunks > 0);
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id.as_deref() == Some("session_1"))
            .unwrap();
        assert_eq!(session.input_tokens, 9);
        assert_eq!(session.output_tokens, 3);
    }

    #[test]
    fn usage_observability_distinguishes_missing_partial_and_reported_zero() {
        let missing = grok_usage_observability(None, Some(11));
        let partial_usage = translate::reducer::GrokUsage {
            output_tokens: Some(7),
            ..Default::default()
        };
        let partial = grok_usage_observability(Some(&partial_usage), Some(11));
        let zero_usage = translate::reducer::GrokUsage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            ..Default::default()
        };
        let zero = grok_usage_observability(Some(&zero_usage), Some(11));

        assert_eq!(missing["state"], "unavailable");
        assert!(missing["inputTokens"].is_null());
        assert_eq!(missing["estimatedInputTokens"], 11);
        assert_eq!(partial["state"], "partial");
        assert!(partial["inputTokens"].is_null());
        assert_eq!(partial["outputTokens"], 7);
        assert_eq!(zero["state"], "reported");
        assert_eq!(zero["inputTokens"], 0);
        assert_eq!(zero["outputTokens"], 0);
    }

    #[tokio::test]
    async fn upstream_failure_after_visible_text_closes_content_before_error() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.failed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_failed".into(),
            "grok-4.5".into(),
            None,
            "req_failed".into(),
            Some(traffic),
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(body.matches("partial").count(), 1, "{body}");
        let stop = body.rfind("event: content_block_stop").unwrap();
        let error = body.find("event: error").unwrap();
        assert!(stop < error, "{body}");
        assert!(!body.contains("event: message_stop"), "{body}");
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("\"inputTokens\": 4"), "{captured}");
        assert!(captured.contains("\"outputTokens\": 2"), "{captured}");
    }

    #[tokio::test]
    async fn typed_failure_after_visible_text_is_not_replayed() {
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial once\"}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.error\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"capacity is temporarily limited\"}}}\n\n",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_typed_failure".into(),
            "grok-4.5".into(),
            None,
            "req_typed_failure".into(),
            None,
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(body.matches("partial once").count(), 1, "{body}");
        assert!(body.contains("\"type\":\"rate_limit_error\""), "{body}");
        assert!(body.contains("capacity is temporarily limited"), "{body}");
        assert!(
            body.rfind("event: content_block_stop").unwrap() < body.find("event: error").unwrap()
        );
        assert!(!body.contains("event: message_stop"), "{body}");
    }

    #[tokio::test]
    async fn invalid_semantic_tail_rolls_back_every_event_from_the_same_chunk() {
        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"STAGED_ONLY\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"UNSAFE_TAIL\"}\n\n",
        ))]);
        let response = stream_body(
            upstream,
            "msg_invalid_tail".into(),
            "grok-4.5".into(),
            None,
            "req_invalid_tail".into(),
            None,
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains("event: error"), "{body}");
        assert!(!body.contains("STAGED_ONLY"), "{body}");
        assert!(!body.contains("UNSAFE_TAIL"), "{body}");
        assert!(!body.contains("event: message_stop"), "{body}");
    }

    #[tokio::test]
    async fn semantic_tail_after_a_complete_terminal_is_not_polled() {
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"UNSAFE_TAIL\"}\n\n",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_cross_chunk_tail".into(),
            "grok-4.5".into(),
            None,
            "req_cross_chunk_tail".into(),
            None,
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(!body.contains("event: error"), "{body}");
        assert!(!body.contains("UNSAFE_TAIL"), "{body}");
        assert_eq!(body.matches("event: message_stop").count(), 1, "{body}");
    }

    #[tokio::test]
    async fn terminal_with_partial_tail_is_chunk_boundary_invariant() {
        const TERMINAL: &[u8] =
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        const PARTIAL_TAIL: &[u8] =
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"truncated";

        let same_chunk = vec![Bytes::from([TERMINAL, PARTIAL_TAIL].concat())];
        let split_chunks = vec![
            Bytes::from_static(TERMINAL),
            Bytes::from_static(PARTIAL_TAIL),
        ];
        assert_eq!(
            same_chunk.iter().flatten().copied().collect::<Vec<_>>(),
            split_chunks.iter().flatten().copied().collect::<Vec<_>>()
        );

        for (case, chunks) in [("same_chunk", same_chunk), ("next_chunk", split_chunks)] {
            let upstream = futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(Ok::<Bytes, client::GrokError>)
                    .collect::<Vec<_>>(),
            );
            let response = stream_body(
                upstream,
                format!("msg_incomplete_tail_{case}"),
                "grok-4.5".into(),
                None,
                format!("req_incomplete_tail_{case}"),
                None,
                None,
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body = String::from_utf8(body.to_vec()).unwrap();

            assert!(!body.contains("event: error"), "case={case}: {body}");
            assert_eq!(
                body.matches("event: message_stop").count(),
                1,
                "case={case}: {body}"
            );
            assert!(!body.contains("truncated"), "case={case}: {body}");
        }
    }

    #[tokio::test]
    async fn post_terminal_rate_limit_telemetry_does_not_delay_terminal_render() {
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"type\":\"rate_limits.updated\",\"remaining\":42}\n\n",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_terminal_telemetry".into(),
            "grok-4.5".into(),
            None,
            "req_terminal_telemetry".into(),
            None,
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(!body.contains("event: error"), "{body}");
        assert_eq!(body.matches("event: message_stop").count(), 1, "{body}");
    }

    #[tokio::test]
    async fn transport_reset_after_terminal_keeps_the_completed_response() {
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"complete\"}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
            )),
            Err(client::GrokError::stream_transport(
                client::GrokErrorStage::Stream,
                "reset after terminal",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_terminal_reset".into(),
            "grok-4.5".into(),
            None,
            "req_terminal_reset".into(),
            None,
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains("complete"), "{body}");
        assert!(!body.contains("event: error"), "{body}");
        assert_eq!(body.matches("event: message_stop").count(), 1, "{body}");
    }

    #[tokio::test]
    async fn semantic_output_streams_early_and_terminal_does_not_wait_for_upstream_eof() {
        let (tx, rx) = mpsc::channel(2);
        let response = stream_body(
            ChannelStream(rx),
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            None,
            None,
        );
        let mut body = response.into_body();

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n",
        )))
        .await
        .unwrap();

        let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("downstream body waited for upstream completion")
            .expect("downstream body ended before its first event")
            .expect("downstream body frame failed")
            .into_data()
            .expect("first downstream frame was not data");
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.contains("event: message_start"));
        assert!(first.contains("first"));

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        )))
        .await
        .unwrap();
        let terminal = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("downstream completion waited for upstream EOF")
            .expect("downstream completion was missing")
            .expect("downstream completion frame failed")
            .into_data()
            .expect("downstream completion frame was not data");
        assert!(
            String::from_utf8(terminal.to_vec())
                .unwrap()
                .contains("event: message_stop")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), body.frame())
                .await
                .expect("downstream EOF was not delivered after the terminal event")
                .is_none()
        );
        tokio::time::timeout(Duration::from_millis(250), tx.closed())
            .await
            .expect("terminal completion must cancel the upstream body");
    }

    #[tokio::test]
    async fn semantic_enqueue_releases_replay_material_but_keeps_retry_state() {
        let (client, _temp) = test_client(
            "http://127.0.0.1:1/v1",
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let client = Arc::new(client);
        let request = prepared_request(false);
        let capture_temp = TempDir::new().unwrap();
        let replay_traffic = Arc::new(test_capture(capture_temp.path().join("traffic")));
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let client_weak = Arc::downgrade(&client);
        let request_weak = Arc::downgrade(&request);
        let traffic_weak = Arc::downgrade(&replay_traffic);
        let retry_weak = Arc::downgrade(&retry);
        let request_byte_budget = Arc::new(Semaphore::new(1));
        let request_byte_lease =
            RequestByteLease::new(request_byte_budget.clone().try_acquire_owned().unwrap(), 1);
        let reconnect = Some(
            GrokReconnectContext::new(client, request, Some(replay_traffic), retry, 1)
                .with_request_byte_lease(Some(request_byte_lease)),
        );
        assert!(request_byte_budget.clone().try_acquire_owned().is_err());

        let (tx, rx) = mpsc::channel(1);
        let response = stream_body_with_policy(
            ChannelStream(rx),
            "msg_release_replay".into(),
            "grok-4.5".into(),
            None,
            "req_release_replay".into(),
            None,
            reconnect,
            GrokRequestDeadline::after(Duration::from_secs(5)),
            Duration::from_secs(1),
        );
        let mut body = response.into_body();
        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first semantic output\"}\n\n",
        )))
        .await
        .unwrap();

        let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("semantic output must enter the downstream queue")
            .expect("stream must remain open after its first semantic output")
            .unwrap();
        assert!(
            String::from_utf8_lossy(first.data_ref().unwrap()).contains("first semantic output")
        );
        tokio::time::timeout(Duration::from_millis(250), async {
            while request_weak.strong_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queued semantic output must release replay-only state promptly");

        assert_eq!(client_weak.strong_count(), 0);
        assert_eq!(request_weak.strong_count(), 0);
        assert_eq!(traffic_weak.strong_count(), 0);
        assert!(request_byte_budget.try_acquire_owned().is_ok());
        let retry = retry_weak
            .upgrade()
            .expect("retry state must outlive replay material while the stream is active");
        assert!(!retry.lock().await.is_terminal());

        drop(retry);
        drop(body);
        tokio::time::timeout(Duration::from_millis(250), tx.closed())
            .await
            .expect("dropping downstream must still cancel the upstream producer");
    }

    #[tokio::test]
    async fn hosted_side_effect_releases_replay_material_before_downstream_output() {
        let (client, _temp) = test_client(
            "http://127.0.0.1:1/v1",
            GrokTimeouts {
                connect_ms: 1_000,
                header_ms: 1_000,
                first_byte_ms: 1_000,
                body_idle_ms: 1_000,
            },
        )
        .await;
        let client = Arc::new(client);
        let request = prepared_request(false);
        let retry = Arc::new(Mutex::new(GrokRetryState::new()));
        let client_weak = Arc::downgrade(&client);
        let request_weak = Arc::downgrade(&request);
        let retry_weak = Arc::downgrade(&retry);
        let request_byte_budget = Arc::new(Semaphore::new(1));
        let request_byte_lease =
            RequestByteLease::new(request_byte_budget.clone().try_acquire_owned().unwrap(), 1);
        let reconnect = Some(
            GrokReconnectContext::new(client, request, None, retry, 1)
                .with_request_byte_lease(Some(request_byte_lease)),
        );

        let (tx, rx) = mpsc::channel(1);
        let response = stream_body_with_policy(
            ChannelStream(rx),
            "msg_hosted_release".into(),
            "grok-4.5".into(),
            None,
            "req_hosted_release".into(),
            None,
            reconnect,
            GrokRequestDeadline::after(Duration::from_secs(5)),
            Duration::from_secs(1),
        );
        let body = response.into_body();
        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\n",
        )))
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_millis(250), async {
            while request_weak.strong_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("hosted boundary must release replay-only state promptly");
        assert_eq!(client_weak.strong_count(), 0);
        assert_eq!(request_weak.strong_count(), 0);
        assert!(request_byte_budget.try_acquire_owned().is_ok());
        assert!(retry_weak.upgrade().is_some());

        drop(body);
        tokio::time::timeout(Duration::from_millis(250), tx.closed())
            .await
            .expect("dropping downstream must cancel the upstream producer");
    }

    #[tokio::test]
    async fn dropped_downstream_body_finalizes_partial_capture() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let (tx, rx) = mpsc::channel(2);
        let response = stream_body(
            ChannelStream(rx),
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            Some(traffic),
            None,
        );
        let mut body = response.into_body();

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        )))
        .await
        .unwrap();
        let _ = body.frame().await.unwrap().unwrap();
        drop(body);

        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let ready = std::fs::read_dir(temp.path().join("traffic"))
                    .ok()
                    .is_some_and(|entries| {
                        entries.filter_map(Result::ok).any(|entry| {
                            entry
                                .path()
                                .to_string_lossy()
                                .contains("061-grok-stream-summary")
                        })
                    });
                if ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the body should finalize capture promptly");

        let entries: Vec<_> = std::fs::read_dir(temp.path().join("traffic"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let abandoned = entries
            .iter()
            .find(|path| path.to_string_lossy().contains("060-grok-stream-abandoned"))
            .unwrap();
        let summary = entries
            .iter()
            .find(|path| path.to_string_lossy().contains("061-grok-stream-summary"))
            .unwrap();
        let abandoned: serde_json::Value =
            serde_json::from_slice(&std::fs::read(abandoned).unwrap()).unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&std::fs::read(summary).unwrap()).unwrap();
        assert_eq!(abandoned["kind"], "client_disconnect");
        assert_eq!(summary["completion"]["kind"], "stream_abandoned");
        assert_eq!(summary["completion"]["reason"], "downstream_body_dropped");
        assert_eq!(summary["completion"]["chunks"], 1);
        assert!(summary["upstream_events"]["captured"].as_u64().unwrap() > 0);
        assert!(summary["downstream_events"]["captured"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn streaming_capture_writes_redacted_complete_artifacts() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        ))]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            Some(traffic),
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("tool_use"));
        let names: Vec<_> = std::fs::read_dir(temp.path().join("traffic"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|name| name.contains("032-upstream-response-body.sse"))
        );
        assert!(
            names
                .iter()
                .any(|name| name.contains("061-grok-stream-summary"))
        );
    }

    #[tokio::test]
    async fn streaming_capture_records_fragmented_search_and_tool_events() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fir",
            )),
            Ok(Bytes::from_static(
                b"st\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            Some(traffic),
            None,
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("tool_use"));
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("x_search"));
        assert!(captured.contains("function_call"));
        assert!(captured.contains("stream_completion"));
    }

    #[tokio::test]
    async fn streaming_capture_records_malformed_and_failed_streams() {
        for (payload, stage) in [
            (b"data: {bad json}\n\n".as_slice(), "json"),
            (
                b"data: {\"type\":\"response.failed\",\"response\":{}}\n\n".as_slice(),
                "upstream",
            ),
        ] {
            let temp = TempDir::new().unwrap();
            let traffic = Arc::new(test_capture(temp.path().join("traffic")));
            let response = stream_body(
                futures_util::stream::iter(vec![Ok(Bytes::copy_from_slice(payload))]),
                "msg_1".into(),
                "grok-4.5".into(),
                None,
                "req_1".into(),
                Some(traffic),
                None,
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("event: error"));
            let captured = capture_contents(temp.path().join("traffic"));
            assert!(captured.contains(&format!("\"stage\": \"{stage}\"")));
            assert!(captured.contains("stream_error"));
        }
    }

    #[test]
    fn non_streaming_malformed_capture_keeps_diagnostics() {
        let temp = TempDir::new().unwrap();
        let traffic = test_capture(temp.path().join("traffic"));
        assert!(
            accumulate_response_with_traffic(
                b"data: {bad json}\n\n",
                "msg_1",
                "grok-4.5",
                Some(&traffic),
            )
            .is_err()
        );
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("malformed_event"));
        assert!(captured.contains("\"outcome\": \"error\""));
    }

    #[tokio::test]
    async fn non_streaming_failed_response_preserves_usage_without_becoming_success() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "req_failed_non_stream",
            Some("session_failed_non_stream".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.provider_selected("req_failed_non_stream", "grok", "grok-4.5", None);
        let ctx = RequestContext {
            req_id: "req_failed_non_stream".into(),
            session_id: Some("session_failed_non_stream".into()),
            session_seq: Some(1),
            provider: "grok".into(),
            traffic: Some(traffic),
            monitor: Some(monitor.clone()),
            request_byte_lease: None,
        };
        let upstream = b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"model failed\"},\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens\":2,\"total_tokens\":14}}}\n\n";

        let response = finish_non_streaming_message(upstream, "grok-4.5", 77, &ctx);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["message"], "model failed");

        // The server owns the final failed lifecycle event. Usage observed by the provider before
        // returning the 502 must survive that transition into the completed-request history.
        monitor.request_failed(
            "req_failed_non_stream",
            Some(502),
            "Grok response is invalid",
        );
        let snapshot = monitor.snapshot();
        let request = snapshot
            .recent
            .iter()
            .find(|request| request.request_id == "req_failed_non_stream")
            .expect("failed request must be retained by the monitor");
        assert_eq!(request.status, crate::monitor::RequestStatus::Failed);
        assert_eq!(request.http_status, Some(502));
        assert_eq!(request.input_tokens, Some(9));
        assert_eq!(request.output_tokens, Some(2));

        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("\"outcome\": \"error\""), "{captured}");
        assert!(captured.contains("\"mappedInputTokens\": 9"), "{captured}");
        assert!(captured.contains("\"outputTokens\": 2"), "{captured}");
        assert!(
            captured.contains("\"estimatedInputTokens\": 77"),
            "{captured}"
        );
    }

    #[test]
    fn transport_failure_capture_contains_no_credentials() {
        let temp = TempDir::new().unwrap();
        let traffic = test_capture(temp.path().join("traffic"));
        client::capture_terminal_failure(
            Some(&traffic),
            "transport",
            "transport",
            1,
            Some(502),
            Some("Grok upstream request failed"),
        );
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("transport"));
        assert!(captured.contains("060-grok-stream-error") || captured.contains("\"kind\""));
        for secret in [
            "Bearer token",
            "refresh-secret",
            "oauth-code",
            "person@example.com",
            "test-access",
        ] {
            assert!(!captured.contains(secret));
        }
    }

    fn capture_contents(root: std::path::PathBuf) -> String {
        let mut captured = String::new();
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    captured.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }
        captured
    }

    fn capture_contents_matching(root: std::path::PathBuf, name_fragment: &str) -> String {
        let mut captured = String::new();
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(name_fragment))
                {
                    captured.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }
        captured
    }

    #[tokio::test]
    async fn stream_capture_keeps_reasoning_only_upstream() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let (tx, rx) = mpsc::channel(1);
        let upstream = "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"draft only 😊\"}\n\ndata: {\"type\":\"response.reasoning_text.done\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"final only 🚀\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        tx.send(Ok(Bytes::copy_from_slice(upstream.as_bytes())))
            .await
            .unwrap();
        drop(tx);

        let body = stream_body(
            ChannelStream(rx),
            "msg_capture".into(),
            "grok-4.5".into(),
            None,
            "req_capture".into(),
            Some(traffic),
            None,
        )
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains("draft only"));
        assert!(!body.contains('😊'));
        assert_eq!(body.matches("final only 🚀").count(), 1);

        let root = temp.path().join("traffic");
        let downstream = capture_contents_matching(root.clone(), "050-downstream-event");
        assert!(!downstream.contains("draft only"));
        assert!(!downstream.contains('😊'));
        assert_eq!(downstream.matches("final only 🚀").count(), 1);

        let upstream_capture = capture_contents_matching(root, "040-upstream-event");
        assert!(upstream_capture.contains("draft only"));
        assert!(upstream_capture.contains('😊'));
    }

    #[test]
    fn non_streaming_capture_writes_response_and_redacts_secrets() {
        let temp = TempDir::new().unwrap();
        let traffic = test_capture(temp.path().join("traffic"));
        let upstream = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\",\"access_token\":\"secret\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let value = accumulate_response_with_traffic(upstream, "msg_1", "grok-4.5", Some(&traffic))
            .unwrap();
        traffic.write_json("051-downstream-response", &value);
        let mut captured = String::new();
        for entry in std::fs::read_dir(temp.path().join("traffic")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                captured.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        assert!(captured.contains("[redacted len=6]"));
        assert!(!captured.contains("secret"));
    }

    #[tokio::test]
    async fn pre_emit_overload_event_rebuilds_with_shared_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first_request = read_complete_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            let body = b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\",\"message\":\"capacity exhausted\"},\"retry_after\":0}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let rebuilt_request = read_complete_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"recovered once\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            (first_request, rebuilt_request)
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
        let deadline = GrokRequestDeadline::after(Duration::from_secs(5));
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(deadline)));
        let prepared = prepared_request(false);
        let response = client
            .post_prepared_with_retry(&prepared, None, retry.clone())
            .await
            .unwrap();
        let reconnect = Some(GrokReconnectContext::new(
            client.clone(),
            prepared,
            None,
            retry,
            1,
        ));
        let body = tokio::time::timeout(
            Duration::from_secs(3),
            stream_body_with_policy(
                response.into_stream(),
                "msg_overload_rebuild".into(),
                "grok-4.5".into(),
                None,
                "req_overload_rebuild".into(),
                None,
                reconnect,
                deadline,
                // Keep this test focused on a replay that completes before any downstream
                // heartbeat commits the response.
                Duration::from_secs(1),
            )
            .into_body()
            .collect(),
        )
        .await
        .expect("the transient SSE failure must rebuild before the deadline")
        .unwrap()
        .to_bytes();
        let (first_request, rebuilt_request) = server.await.unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(
            http_request_body(&first_request),
            http_request_body(&rebuilt_request),
            "stream rebuild must reuse the prepared JSON bytes"
        );
        let body = String::from_utf8_lossy(&body);
        assert_eq!(body.matches("recovered once").count(), 1, "{body}");
        assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
        assert!(!body.contains("capacity exhausted"), "{body}");
        assert!(body.contains("event: message_stop"), "{body}");
    }

    #[tokio::test]
    async fn same_batch_hosted_side_effect_and_retryable_failure_never_rebuilds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await
            else {
                return false;
            };
            let _ = read_complete_http_request(&mut stream).await;
            let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"incorrect replay\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            true
        });

        let (client, _temp) = test_client(
            &format!("http://{addr}/v1"),
            GrokTimeouts {
                connect_ms: 500,
                header_ms: 500,
                first_byte_ms: 500,
                body_idle_ms: 500,
            },
        )
        .await;
        let deadline = GrokRequestDeadline::after(Duration::from_secs(2));
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(deadline)));
        let reconnect = Some(GrokReconnectContext::new(
            Arc::new(client),
            prepared_request(false),
            None,
            retry,
            1,
        ));
        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\",\"message\":\"capacity after search\"},\"retry_after\":0}}\n\n",
        ))]);

        let body = stream_body_with_policy(
            upstream,
            "msg_hosted_barrier".into(),
            "grok-4.5".into(),
            None,
            "req_hosted_barrier".into(),
            None,
            reconnect,
            deadline,
            Duration::from_millis(20),
        )
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
        let body = String::from_utf8_lossy(&body);

        assert!(
            !server.await.unwrap(),
            "hosted side effect must close replay material"
        );
        assert!(body.contains("event: error"), "{body}");
        assert!(body.contains("capacity after search"), "{body}");
        assert!(!body.contains("incorrect replay"), "{body}");
    }

    #[tokio::test]
    async fn pre_emit_stream_reset_does_not_replay_unknown_outcome() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            // First attempt: 200 headers then reset before body.
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(stream);

            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
            {
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
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
        let deadline = GrokRequestDeadline::after(Duration::from_secs(5));
        let retry = Arc::new(Mutex::new(GrokRetryState::with_deadline(deadline)));
        let prepared = prepared_request(false);
        let response = client
            .post_prepared_with_retry(&prepared, None, retry.clone())
            .await
            .unwrap();
        let reconnect = Some(GrokReconnectContext::new(
            client.clone(),
            prepared,
            None,
            retry,
            1,
        ));
        let body = tokio::time::timeout(
            Duration::from_secs(3),
            stream_body_with_policy(
                response.into_stream(),
                "msg_rebuild".into(),
                "grok-4.5".into(),
                None,
                "req_rebuild".into(),
                None,
                reconnect,
                deadline,
                Duration::from_millis(10),
            )
            .into_body()
            .collect(),
        )
        .await
        .expect("the unknown-outcome failure must terminate promptly")
        .unwrap()
        .to_bytes();
        server.await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("event: error"), "{body}");
        assert!(!body.contains("\"delta\":\"ok\""), "{body}");
    }

    #[tokio::test]
    async fn reasoning_only_reset_does_not_replay_unknown_outcome() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk = b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"draft \\ud83d\\ude0a\"}\n\ndata: {\"type\":\"response.reasoning_text.done\"}\n\n";
            let header = format!("{:x}\r\n", chunk.len());
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(chunk).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            drop(stream);

            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
            {
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
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
        let prepared = prepared_request(false);
        let response = client
            .post_prepared_with_retry(&prepared, None, retry.clone())
            .await
            .unwrap();
        let reconnect = Some(GrokReconnectContext::new(client, prepared, None, retry, 1));
        let body = stream_body(
            response.into_stream(),
            "msg_reasoning_rebuild".into(),
            "grok-4.5".into(),
            None,
            "req_reasoning_rebuild".into(),
            None,
            reconnect,
        )
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
        server.await.unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("draft"));
        assert!(!body.contains('😊'));
        assert!(!body.contains("final once"));
        assert!(body.contains("event: error"), "{body}");
    }

    #[tokio::test]
    async fn post_emit_stream_reset_does_not_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            // First body chunk is a complete Anthropic-producing event.
            let chunk = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n";
            let header = format!("{:x}\r\n", chunk.len());
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(chunk).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            // Then reset.
            drop(stream);
            // If a second request arrives, count it (should not happen).
            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
            {
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
            }
        });

        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let (client, _auth) = test_client(
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
        let prepared = prepared_request(true);
        let response = client
            .post_prepared_with_retry(&prepared, Some(traffic.clone()), retry.clone())
            .await
            .unwrap();
        let reconnect = Some(GrokReconnectContext::new(
            client,
            prepared,
            Some(traffic.clone()),
            retry,
            1,
        ));
        let body = stream_body(
            response.into_stream(),
            "msg_post".into(),
            "grok-4.5".into(),
            None,
            "req_post".into(),
            Some(traffic),
            reconnect,
        )
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
        let _ = server.await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("first"));
        assert!(body.contains("event: error"));
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("stream_error") || captured.contains("downstream_emitted"));
    }

    #[tokio::test]
    async fn incomplete_socket_close_without_terminal_is_not_success() {
        let (tx, rx) = mpsc::channel(1);
        let response = stream_body(
            ChannelStream(rx),
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            None,
            None,
        );
        let mut body = response.into_body();
        // No terminal event; just close.
        drop(tx);
        let frame = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("event: error"));
    }

    #[tokio::test]
    async fn malformed_json_is_not_retried_even_with_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            hits_server.fetch_add(1, Ordering::SeqCst);
            let body = b"data: {bad json}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                let _ = read_http_request(&mut stream).await;
                hits_server.fetch_add(1, Ordering::SeqCst);
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
        let prepared = prepared_request(false);
        let response = client
            .post_prepared_with_retry(&prepared, None, retry.clone())
            .await
            .unwrap();
        let reconnect = Some(GrokReconnectContext::new(client, prepared, None, retry, 1));
        let body = stream_body(
            response.into_stream(),
            "msg_bad".into(),
            "grok-4.5".into(),
            None,
            "req_bad".into(),
            None,
            reconnect,
        )
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
        let _ = server.await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(String::from_utf8_lossy(&body).contains("event: error"));
    }

    #[test]
    fn stream_error_classifier_marks_transport_retryable() {
        let err = client::GrokError::stream_transport(GrokErrorStage::Stream, "reset");
        assert!(err.is_retryable());
        assert_eq!(
            err.replay_safety,
            crate::retry::ReplaySafety::OutcomeUnknown
        );
        assert!(!err.permits_model_replay());
        assert_eq!(err.origin, client::GrokErrorOrigin::StreamTransport);
        let bad = client::GrokError::http(StatusCode::BAD_REQUEST, None, "nope");
        assert!(!bad.is_retryable());
        let limit = client::GrokError::response_limit(GrokErrorStage::Body, "too large");
        assert!(!limit.is_retryable());
        assert_eq!(limit.origin, client::GrokErrorOrigin::ResponseLimit);
    }
}
