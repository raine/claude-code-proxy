pub mod auth;
pub mod client;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::auth::AuthStorage;
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::registry::{GLM_MODELS, normalize_incoming_model};

use self::auth::{
    auth_location, clear_glm_auth, env_glm_api_key, file_store, load_glm_api_key,
    missing_auth_message, save_glm_api_key,
};
use self::client::{GlmError, GlmHttpClient};

pub struct GlmProvider;

impl GlmProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GlmProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GlmProvider {
    fn name(&self) -> &'static str {
        "glm"
    }

    fn supported_models(&self) -> Vec<String> {
        GLM_MODELS.iter().map(|s| s.to_string()).collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &GLM_CLI
    }

    async fn handle_messages(&self, mut body: MessagesRequest, ctx: RequestContext) -> Response {
        let want_stream = body.stream;

        // z.ai is Anthropic-native: forward the request verbatim after stripping
        // the local [1m] compaction hint, then pipe the upstream Anthropic
        // SSE/JSON reply straight back to Claude Code (no format translation).
        body.model = body.model.map(|m| normalize_incoming_model(&m));

        let api_key = match load_glm_api_key() {
            Some(k) => k,
            None => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    missing_auth_message(),
                );
            }
        };

        let body_bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("Failed to serialize request: {e}"),
                );
            }
        };

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }

        let client = match GlmHttpClient::new() {
            Ok(c) => c,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Failed to create HTTP client: {e}"),
                );
            }
        };

        let upstream = match client.post_messages(&api_key, &body_bytes).await {
            Ok(r) => r,
            Err(e) => return map_glm_error(&e),
        };

        if want_stream {
            let sse_bytes = upstream.body;
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
            let json: serde_json::Value = match serde_json::from_slice(&upstream.body) {
                Ok(v) => v,
                Err(e) => {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        format!("Failed to parse upstream response: {e}"),
                    );
                }
            };
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
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        // z.ai exposes the Anthropic count_tokens endpoint, but calling upstream
        // on every count adds latency. Use a rough local estimate (mirrors the
        // cursor provider), which is sufficient for Claude Code's compaction.
        let tokens = (serde_json::to_vec(&body).unwrap_or_default().len() / 4) as u64;
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

fn map_glm_error(err: &GlmError) -> Response {
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
        0 => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            err.detail.as_deref().unwrap_or(&err.message),
        ),
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            err.detail.as_deref().unwrap_or("Upstream error"),
        ),
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct GlmCli;

impl CliHandlers for GlmCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        use std::io::{self, BufRead};
        println!("Enter your z.ai API key (https://z.ai) and press Enter:");
        let mut buf = String::new();
        io::stdin().lock().read_line(&mut buf)?;
        let key = buf.trim().to_string();
        if key.is_empty() {
            anyhow::bail!("no API key provided");
        }
        save_glm_api_key(key)?;
        println!("GLM API key saved to {}", auth_location());
        println!("Tip: you can also export CCP_GLM_API_KEY (or GLM_API_KEY).");
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "glm: API-key provider — set CCP_GLM_API_KEY / GLM_API_KEY or run `claude-code-proxy glm auth login`"
        );
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let env_present = env_glm_api_key().is_some();
        let stored_present = file_store().load().ok().flatten().is_some();
        if !env_present && !stored_present {
            anyhow::bail!("Not authenticated");
        }
        println!("Authenticated: true");
        println!(
            "Env (CCP_GLM_API_KEY/GLM_API_KEY): {}",
            if env_present { "set" } else { "unset" }
        );
        if stored_present {
            println!("Stored: yes ({})", auth_location());
        } else {
            println!("Stored: no");
        }
        println!("API key has no expiry (static key).");
        Ok(())
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        clear_glm_auth()?;
        println!("GLM stored auth cleared. Unset CCP_GLM_API_KEY / GLM_API_KEY if using env auth.");
        Ok(())
    }
}

pub(crate) static GLM_CLI: GlmCli = GlmCli;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_models_lists_known_glm_models() {
        let provider = GlmProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"glm-4.7".to_string()));
        assert!(models.contains(&"glm-5.2".to_string()));
    }
}
