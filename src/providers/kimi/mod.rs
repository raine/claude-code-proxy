pub mod auth;
pub mod client;
pub mod count_tokens;
pub mod translate;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::kimi::auth::token_store::file_store;
use crate::providers::kimi::translate::accumulate::accumulate_response;
use crate::providers::kimi::translate::model_allowlist::{assert_allowed_model, resolve_model};
use crate::providers::kimi::translate::request::{TranslateOptions, translate_request};
use crate::providers::kimi::translate::stream::translate_stream_bytes;

const KIMI_MODELS: &[&str] = &["kimi-for-coding", "kimi-k2.6", "k2.6"];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct KimiProvider;

impl Default for KimiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KimiProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Provider for KimiProvider {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn supported_models(&self) -> Vec<String> {
        KIMI_MODELS.iter().map(|s| s.to_string()).collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &KIMI_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let model = body.model.as_deref().unwrap_or("kimi-for-coding");
        let resolved = resolve_model(model);

        if let Err(e) = assert_allowed_model(&resolved) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }

        let translated = match translate_request(
            &body,
            TranslateOptions {
                session_id: ctx.session_id.clone(),
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        // KimiHttpClient uses a blocking client whose lifecycle belongs on a
        // blocking thread.
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match tokio::task::spawn_blocking(move || {
            let client = client::KimiHttpClient::new();
            let result = client.post_kimi(&translated);
            drop(client);
            result
        })
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return map_kimi_error_to_response(&e);
            }
            Err(join_err) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Blocking task join error: {join_err}"),
                );
            }
        };

        if want_stream {
            let sse_bytes = match translate_stream_bytes(&upstream.body, &message_id, model) {
                Ok(b) => b,
                Err(e) => {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        format!("Stream translation error: {e}"),
                    );
                }
            };
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
            match accumulate_response(&upstream.body, &message_id, model) {
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
                Err(e) => json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Accumulation error: {e}"),
                ),
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let model = body.model.as_deref().unwrap_or("kimi-for-coding");
        let resolved = resolve_model(model);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }
        let tokens = count_tokens::count_tokens(&body);
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

fn map_kimi_error_to_response(err: &client::KimiError) -> Response {
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
            // Forward retry-after header
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

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct KimiCli;

impl CliHandlers for KimiCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        let tokens = auth::login::run_device_login()?;
        let store = file_store();
        let manager = auth::manager::KimiAuthManager::new(store);
        let saved = manager.persist_initial_tokens(&tokens)?;
        println!("Auth saved in {}", manager.store.auth_path());
        if let Some(ref uid) = saved.user_id {
            println!("User: {uid}");
        }
        println!("Authentication complete");
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        self.login()
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        let stored = store.load_auth()?;
        match stored {
            Some(auth) => {
                println!("Auth path: {}", store.auth_path());
                println!("Authenticated: true");
                if let Some(ref uid) = auth.user_id {
                    println!("User: {uid}");
                }
                if let Some(ref scope) = auth.scope {
                    println!("Scope: {scope}");
                }
                let remaining = auth.expires.saturating_sub(now_ms()) / 1000;
                println!("Expires in {remaining}s");
                Ok(())
            }
            None => {
                anyhow::bail!("Not authenticated");
            }
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        store.clear_auth()?;
        println!("Logged out");
        Ok(())
    }
}

pub(crate) static KIMI_CLI: KimiCli = KimiCli;
