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
use axum::response::{IntoResponse, Response};
use http::StatusCode;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::cursor::auth::{
    clear_cursor_auth, expired_auth_message, load_cursor_auth, missing_auth_message,
    run_cursor_login,
};
use crate::providers::cursor::client::CursorHttpClient;
use crate::providers::cursor::model::resolve_cursor_model;
use crate::providers::cursor::request::render_cursor_prompt;
use crate::providers::cursor::response::{
    CursorDecodeError, decode_cursor_upstream, decode_upstream_response,
};
use crate::providers::cursor::tool_bridge::{
    BridgeRegistry, advertised_tool_names, can_bridge_cursor_native_tools, find_tool_result,
    resume_cursor_tool_bridge, start_cursor_tool_bridge,
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

        if let Some(ref session_id) = ctx.session_id
            && let Some(pending) = BridgeRegistry::pending_tool(session_id)
            && let Some(result) = find_tool_result(&body, pending.tool_use_id())
        {
            let (_result_messages, sse_bytes) =
                resume_cursor_tool_bridge(session_id, &message_id, model, result, &pending);
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
            return (headers, sse_bytes).into_response();
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
        let upstream = match client.run_agent(&token, &prompt, model, &images).await {
            Ok(r) => r,
            Err(e) => {
                return map_cursor_error_to_response(&e);
            }
        };

        if want_stream {
            let session_id = ctx.session_id.as_deref();
            let bridge_eligible = can_bridge_cursor_native_tools(&body, session_id);

            if bridge_eligible {
                let events = match decode_upstream_response(&upstream.body) {
                    Ok(e) => e,
                    Err(e) => return map_cursor_decode_error_to_response(&e),
                };

                let allowed = advertised_tool_names(&body);
                let (sse_bytes, _paused) = start_cursor_tool_bridge(
                    &message_id,
                    model,
                    session_id.unwrap(),
                    &events,
                    allowed,
                    Box::new(|| uuid::Uuid::new_v4().to_string().replace('-', "")),
                );
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
            }
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
