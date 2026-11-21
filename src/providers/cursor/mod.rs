pub mod auth;
pub mod client;
pub mod connect;
pub mod model;
pub mod proto;
pub mod request;
pub mod response;
pub mod sse;
#[cfg(test)]
pub(crate) mod test_frames;
pub mod tool_bridge;
pub mod tool_use_xml;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::StatusCode;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::cursor::auth::{
    clear_cursor_auth, expired_auth_message, load_cursor_auth, missing_auth_message,
    run_cursor_login,
};
use crate::providers::cursor::client::CursorHttpClient;
use crate::providers::cursor::model::resolve_cursor_model;
use crate::providers::cursor::request::render_cursor_prompt;
use crate::providers::cursor::response::{CursorDecodeError, decode_cursor_upstream};
use crate::providers::cursor::tool_bridge::{
    BridgeRegistry, LiveBridge, PumpOutcome, advertised_tool_names, can_bridge_cursor_native_tools,
    exec_result_messages, find_tool_result, pump_live_bridge,
};

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct CursorProvider;

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Provider for CursorProvider {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn supported_models(&self) -> Vec<String> {
        model::cursor_supported_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &CURSOR_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let model = body.model.as_deref().unwrap_or("cursor");

        let resolved = resolve_cursor_model(model);
        if let Err(e) = resolved {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Model \"{model}\" is not supported: {e}"),
            );
        }

        BridgeRegistry::sweep_abandoned();

        // A live run for this session is waiting on a tool result. Answer it on
        // the open stream and keep pumping the same run: a new run here would
        // make the agent re-plan and re-issue the call it already made.
        if want_stream
            && let Some(session_id) = live_session_id(&ctx)
            && let Some(bridge) = BridgeRegistry::take(&session_id)
        {
            match resume_payload(&bridge, &body) {
                Some(preamble) => {
                    let mut bridge = bridge;
                    bridge.input_floor = response::estimate_request_input_tokens(&body);
                    return live_sse_response(bridge, preamble, &message_id, model, &ctx);
                }
                // No result for the pending exec: the caller moved on. The
                // entry is already gone; this request starts its own run.
                None => drop(bridge),
            }
        }

        let auth = match load_cursor_auth() {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    missing_auth_message(),
                );
            }
            Err(err) => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    format!("Cursor auth failed: {err}"),
                );
            }
        };

        if matches!(auth.expires, Some(expires) if expires <= now_ms() + 60_000) {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                expired_auth_message(&auth),
            );
        }

        let token = auth.access_token;

        let prompt = render_cursor_prompt(&body);
        let images = request::cursor_selected_images(&body);

        let client = CursorHttpClient::new();
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }

        // With tools in play the run has to stay open across requests, so it is
        // opened as a live stream instead of being collected into a body.
        if want_stream
            && let Some(session_id) = live_session_id(&ctx)
            && can_bridge_cursor_native_tools(&body, Some(&session_id))
        {
            let allowed = advertised_tool_names(&body);
            return match client.start_run(&token, &prompt, model, &images).await {
                Ok(run) => {
                    let mut bridge = LiveBridge::new(session_id, run, allowed);
                    bridge.input_floor = (prompt.len() / 4).max(1) as u64;
                    live_sse_response(bridge, Vec::new(), &message_id, model, &ctx)
                }
                Err(error) => map_cursor_error_to_response(&error),
            };
        }

        let upstream = match client.run_agent(&token, &prompt, model, &images).await {
            Ok(r) => r,
            Err(e) => {
                return map_cursor_error_to_response(&e);
            }
        };

        if want_stream {
            let sse_bytes = sse::frame_cursor_stream(&upstream, &message_id, model);
            if let Some(monitor) = ctx.monitor.as_ref() {
                let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse_bytes);
                monitor.stream_progress(
                    &ctx.req_id,
                    sse_bytes.len() as u64,
                    count_sse_events(&sse_bytes),
                    input_tokens,
                    output_tokens,
                );
            }
            let headers = [
                (http::header::CONTENT_TYPE, "text/event-stream"),
                (http::header::CACHE_CONTROL, "no-cache"),
                (http::header::CONNECTION, "keep-alive"),
            ];
            (headers, sse_bytes).into_response()
        } else {
            match decode_cursor_upstream(&upstream, &message_id, model) {
                Ok(json) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()),
                            json.pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(e) => map_cursor_decode_error_to_response(&e),
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let prompt = render_cursor_prompt(&body);
        let tokens = (prompt.len() / 4) as u64; // rough estimate
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

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let requested = body.model.clone().unwrap_or_else(|| "cursor".to_string());
        let resolved = resolve_cursor_model(&requested).map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Model \"{requested}\" is not supported: {error}"),
            )
        })?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model_id);
        }
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

        // This surface answers one request at a time, so a parked run is either
        // resumed here or dropped; the bytes are collected instead of streamed.
        if let Some(session_id) = live_session_id(&ctx)
            && let Some(mut bridge) = BridgeRegistry::take(&session_id)
        {
            if let Some(preamble) = resume_payload(&bridge, &body) {
                let (bytes, outcome) =
                    pump_buffered(&mut bridge, preamble, &message_id, &requested).await;
                finish_buffered_run(bridge, outcome);
                return Ok(Generation {
                    body: GenerationBody::BufferedSse(bytes.into()),
                    resolved_model: resolved.model_id,
                });
            }
            drop(bridge);
        }

        let auth = load_cursor_auth()
            .map_err(|error| {
                ProviderError::new(
                    StatusCode::UNAUTHORIZED,
                    ProviderErrorKind::Authentication,
                    format!("Cursor auth failed: {error}"),
                )
            })?
            .ok_or_else(|| {
                ProviderError::new(
                    StatusCode::UNAUTHORIZED,
                    ProviderErrorKind::Authentication,
                    missing_auth_message(),
                )
            })?;
        if matches!(auth.expires, Some(expires) if expires <= now_ms() + 60_000) {
            return Err(ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                expired_auth_message(&auth),
            ));
        }
        let prompt = render_cursor_prompt(&body);
        let images = request::cursor_selected_images(&body);
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json(
                "020-upstream-request",
                &serde_json::json!({
                    "model": requested,
                    "prompt": prompt,
                    "image_count": images.len(),
                }),
            );
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let client = CursorHttpClient::new();

        // Tools in play: the run has to stay open across requests, so it is
        // pumped as a live stream and its bytes are collected here.
        if let Some(session_id) = live_session_id(&ctx)
            && can_bridge_cursor_native_tools(&body, Some(&session_id))
        {
            let allowed = advertised_tool_names(&body);
            let run = client
                .start_run(&auth.access_token, &prompt, &requested, &images)
                .await
                .map_err(cursor_provider_error)?;
            let mut bridge = LiveBridge::new(session_id, run, allowed);
            let (bytes, outcome) =
                pump_buffered(&mut bridge, Vec::new(), &message_id, &requested).await;
            finish_buffered_run(bridge, outcome);
            if let Some(traffic) = ctx.traffic.as_ref() {
                traffic.write_bytes("050-anthropic-intermediate.sse", &bytes);
            }
            if let Some(monitor) = ctx.monitor.as_ref() {
                let (input_tokens, output_tokens) = usage_from_anthropic_sse(&bytes);
                monitor.stream_progress(
                    &ctx.req_id,
                    bytes.len() as u64,
                    count_sse_events(&bytes),
                    input_tokens,
                    output_tokens,
                );
            }
            return Ok(Generation {
                body: GenerationBody::BufferedSse(bytes.into()),
                resolved_model: resolved.model_id,
            });
        }

        let upstream = client
            .run_agent(&auth.access_token, &prompt, &requested, &images)
            .await
            .map_err(cursor_provider_error)?;
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("032-upstream-response-body.bin", &upstream.body);
        }
        let bytes = sse::frame_cursor_stream(&upstream, &message_id, &requested);
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("050-anthropic-intermediate.sse", &bytes);
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&bytes);
            monitor.stream_progress(
                &ctx.req_id,
                bytes.len() as u64,
                count_sse_events(&bytes),
                input_tokens,
                output_tokens,
            );
        }
        Ok(Generation {
            body: GenerationBody::BufferedSse(bytes.into()),
            resolved_model: resolved.model_id,
        })
    }
}

/// The session id this request can carry a live run on.
fn live_session_id(ctx: &RequestContext) -> Option<String> {
    ctx.session_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The messages that resume a parked run, when this request carries the
/// `tool_result` for the exec it was waiting on.
fn resume_payload(
    bridge: &LiveBridge,
    body: &MessagesRequest,
) -> Option<Vec<proto::AgentClientMessage>> {
    let pending = bridge.pending.as_ref()?;
    let result = find_tool_result(body, &pending.tool_use_id)?;
    Some(exec_result_messages(pending, result))
}

/// Stream the run's SSE to the client as it is produced.
///
/// The pump runs in its own task and hands the run back to the registry when it
/// pauses, before the response body ends -- Claude Code answers a pause with the
/// next request, and that request has to find the run.
fn live_sse_response(
    mut bridge: LiveBridge,
    preamble: Vec<proto::AgentClientMessage>,
    message_id: &str,
    model: &str,
    ctx: &RequestContext,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
    let message_id = message_id.to_string();
    let model = model.to_string();
    let monitor = ctx.monitor.clone();
    let req_id = ctx.req_id.clone();

    tokio::spawn(async move {
        let mut sse = Vec::new();
        let mut send_failed = false;
        for message in preamble {
            if bridge.run.send(&message).await.is_err() {
                send_failed = true;
                break;
            }
        }

        let outcome = match send_failed {
            true => PumpOutcome::Aborted("Cursor run already closed".to_string()),
            false => {
                pump_live_bridge(&mut bridge, &message_id, &model, &mut |chunk| {
                    sse.extend_from_slice(chunk);
                    let _ = tx.send(Ok(Bytes::copy_from_slice(chunk)));
                })
                .await
            }
        };

        match outcome {
            PumpOutcome::Tool(pending) => {
                bridge.pending = Some(pending);
                BridgeRegistry::insert(bridge);
            }
            PumpOutcome::Aborted(error) => {
                drop(bridge);
                let _ = tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    error,
                )));
            }
            // Nothing left to resume: the run is dropped, which closes its
            // stream upstream.
            PumpOutcome::Replay | PumpOutcome::Ended | PumpOutcome::Failed(_) => drop(bridge),
        }

        if let Some(monitor) = monitor {
            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse);
            monitor.stream_progress(
                &req_id,
                sse.len() as u64,
                count_sse_events(&sse),
                input_tokens,
                output_tokens,
            );
        }
        drop(tx);
    });

    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, body).into_response()
}

/// Keep or drop a run the buffered pump stopped on.
fn finish_buffered_run(bridge: LiveBridge, outcome: PumpOutcome) {
    match outcome {
        PumpOutcome::Tool(pending) => {
            let mut bridge = bridge;
            bridge.pending = Some(pending);
            BridgeRegistry::insert(bridge);
        }
        PumpOutcome::Replay
        | PumpOutcome::Ended
        | PumpOutcome::Aborted(_)
        | PumpOutcome::Failed(_) => drop(bridge),
    }
}

/// Pump a run to its first stop, collecting the SSE (no live client).
async fn pump_buffered(
    bridge: &mut LiveBridge,
    preamble: Vec<proto::AgentClientMessage>,
    message_id: &str,
    model: &str,
) -> (Vec<u8>, PumpOutcome) {
    let mut sse = Vec::new();
    for message in preamble {
        if bridge.run.send(&message).await.is_err() {
            return (
                sse,
                PumpOutcome::Failed("Cursor run already closed".to_string()),
            );
        }
    }
    let outcome = pump_live_bridge(bridge, message_id, model, &mut |chunk| {
        sse.extend_from_slice(chunk)
    })
    .await;
    if let PumpOutcome::Aborted(ref error) = outcome {
        sse.extend_from_slice(&self::sse::format_sse_error(error));
    }
    (sse, outcome)
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn cursor_provider_error(err: client::CursorError) -> ProviderError {
    let (status, kind) = match err.status {
        401 | 403 => (StatusCode::UNAUTHORIZED, ProviderErrorKind::Authentication),
        429 => (StatusCode::TOO_MANY_REQUESTS, ProviderErrorKind::RateLimit),
        _ => (StatusCode::BAD_GATEWAY, ProviderErrorKind::Api),
    };
    let mut error = ProviderError::new(status, kind, err.detail.unwrap_or(err.message));
    if err.status == 429 {
        error.retry_after = Some(err.retry_after.unwrap_or_else(|| "5".to_string()));
    }
    error
}

fn map_cursor_error_to_response(err: &client::CursorError) -> Response {
    match err.status {
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
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            err.detail.as_deref().unwrap_or("Upstream error"),
        ),
    }
}

fn map_cursor_decode_error_to_response(err: &CursorDecodeError) -> Response {
    match err.status() {
        Some(401 | 403) => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.to_string(),
        ),
        Some(429) => json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            err.to_string(),
        ),
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("Response decoding error: {err}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CursorCli;

impl CliHandlers for CursorCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        let auth = run_cursor_login()?.ok_or_else(|| anyhow::anyhow!("Cursor login timed out"))?;
        println!("Cursor auth saved in {}", auth.source);
        if let Some(ref user_id) = auth.user_id {
            println!("User: {user_id}");
        }
        if let Some(ref email) = auth.email {
            println!("Email: {email}");
        }
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!("cursor: device login not yet implemented");
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        match load_cursor_auth()? {
            Some(auth) => {
                println!("Auth source: {}", auth.source);
                if let Some(ref user_id) = auth.user_id {
                    println!("User: {user_id}");
                }
                if let Some(ref email) = auth.email {
                    println!("Email: {email}");
                }
                if let Some(expires) = auth.expires {
                    let remaining = expires.saturating_sub(now_ms()) / 1000;
                    println!("Access token expires in: {remaining}s");
                } else {
                    println!("Access token expiry: unknown");
                }
                Ok(())
            }
            None => {
                anyhow::bail!("Not authenticated");
            }
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        clear_cursor_auth()?;
        println!(
            "Cursor persistent auth cleared. Unset CCP_CURSOR_AUTH_TOKEN or CURSOR_AUTH_TOKEN if using env auth."
        );
        Ok(())
    }
}

pub(crate) static CURSOR_CLI: CursorCli = CursorCli;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_models_includes_legacy_and_agent() {
        let provider = CursorProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"cursor".to_string()));
        assert!(models.contains(&"cursor-agent".to_string()));
        assert!(models.contains(&"cursor-plan".to_string()));
        assert!(models.contains(&"cursor-ask".to_string()));
    }

    #[test]
    fn cursor_cli_logout_does_not_error() {
        let result = CURSOR_CLI.logout();
        assert!(result.is_ok());
    }
}
