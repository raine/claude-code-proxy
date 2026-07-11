pub mod auth;
pub mod client;
pub mod continuation;
pub mod count_tokens;
pub mod translate;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::config;
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::registry::XAI_MODELS;

use self::auth::browser_login::run_browser_login;
use self::auth::device::run_device_login;
use self::auth::manager::XaiAuthManager;
use self::auth::token_store::file_store;
use self::client::XaiHttpClient;
use self::continuation::{
    clear_continuation, continuation_candidate, record_continuation,
};
use self::count_tokens::count_translated_tokens;
use self::translate::accumulate::accumulate_response_with_traffic;
use self::translate::model_allowlist::{assert_allowed_model, resolve_model};
use self::translate::reducer::finish_metadata_from_upstream;
use self::translate::request::{TranslateOptions, translate_request};
use self::translate::stream::translate_stream_bytes_with_traffic;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct XaiProvider;

impl Default for XaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl XaiProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Provider for XaiProvider {
    fn name(&self) -> &'static str {
        "xai"
    }

    fn supported_models(&self) -> Vec<String> {
        XAI_MODELS.iter().map(|s| s.to_string()).collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &XAI_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let model = body.model.as_deref().unwrap_or("grok-build-0.1");
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
                service_tier: None,
                model: resolved.clone(),
                use_responses_lite: false,
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

        let previous_response_id_enabled = config::xai_previous_response_id();
        let continuation = continuation_candidate(
            ctx.session_id.as_deref(),
            &translated,
            previous_response_id_enabled,
        );

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }

        let traffic = ctx.traffic.clone();
        let session_id = ctx.session_id.clone();
        let translated_for_cont = translated.clone();
        let upstream = match tokio::task::spawn_blocking(move || {
            let client = XaiHttpClient::new();
            client.post_responses(&translated, Some(&continuation))
        })
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                clear_continuation(session_id.as_deref());
                return map_xai_error_to_response(&e);
            }
            Err(join_err) => {
                clear_continuation(session_id.as_deref());
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Blocking task join error: {join_err}"),
                );
            }
        };

        if want_stream {
            let sse_bytes = match translate_stream_bytes_with_traffic(
                &upstream.body,
                &message_id,
                model,
                traffic.as_deref(),
            ) {
                Ok(b) => b,
                Err(e) => {
                    clear_continuation(session_id.as_deref());
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
            update_continuation_from_upstream(
                session_id.as_deref(),
                &translated_for_cont,
                &upstream.body,
            );

            let headers = [
                (http::header::CONTENT_TYPE, "text/event-stream"),
                (http::header::CACHE_CONTROL, "no-cache"),
                (http::header::CONNECTION, "keep-alive"),
            ];
            (headers, sse_bytes).into_response()
        } else {
            match accumulate_response_with_traffic(
                &upstream.body,
                &message_id,
                model,
                traffic.as_deref(),
            ) {
                Ok(json) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()),
                            json.pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    update_continuation_from_upstream(
                        session_id.as_deref(),
                        &translated_for_cont,
                        &upstream.body,
                    );
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(e) => {
                    clear_continuation(session_id.as_deref());
                    json_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        format!("Accumulation error: {e}"),
                    )
                }
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let model = body.model.as_deref().unwrap_or("grok-build-0.1");
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
                session_id: None,
                service_tier: None,
                model: resolved,
                use_responses_lite: false,
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

        let tokens = count_translated_tokens(&translated);
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

fn update_continuation_from_upstream(
    session_id: Option<&str>,
    request_body: &translate::request::ResponsesRequest,
    upstream_body: &[u8],
) {
    match finish_metadata_from_upstream(upstream_body) {
        Ok(Some(finish)) if finish.continuation_eligible => {
            record_continuation(
                session_id,
                request_body,
                finish.response_id.as_deref(),
                &finish.output_items,
            );
        }
        _ => clear_continuation(session_id),
    }
}

fn map_xai_error_to_response(err: &client::XaiError) -> Response {
    // Prefer the actionable message (tier hints, re-auth guidance) over raw upstream JSON.
    let body = if err.message.is_empty() {
        err.detail.as_deref().unwrap_or("Upstream error")
    } else {
        &err.message
    };
    match err.status {
        401 => json_error(StatusCode::UNAUTHORIZED, "authentication_error", body),
        // 403 tier denial is not "wrong password" — keep 403 so clients don't thrash re-login.
        403 => json_error(
            if err.tier_denied {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::UNAUTHORIZED
            },
            "authentication_error",
            body,
        ),
        429 => {
            let retry_after = err.retry_after.as_deref().unwrap_or("5");
            let resp = json_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", body);
            let headers = [(http::header::RETRY_AFTER, retry_after)];
            (headers, resp).into_response()
        }
        _ => json_error(StatusCode::BAD_GATEWAY, "api_error", body),
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct XaiCli;

impl CliHandlers for XaiCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        println!(
            "xAI SuperGrok OAuth (browser PKCE via OIDC discovery). For headless hosts use: xai auth device"
        );
        let tokens = run_browser_login()?;
        persist_and_print(&tokens)
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        println!("xAI SuperGrok OAuth (device code — open the URL on any browser)");
        let tokens = run_device_login()?;
        persist_and_print(&tokens)
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        let api_key = config::xai_api_key().is_some();
        match store.load_auth()? {
            Some(auth) => {
                println!("Auth path: {}", store.auth_path());
                println!("Authenticated: true (OAuth)");
                if let Some(ref scope) = auth.scope {
                    println!("Scope: {scope}");
                }
                let remaining = auth.expires.saturating_sub(now_ms()) as i64 / 1000;
                println!("Access token expires in {remaining}s");
                println!("API base: {}", crate::providers::xai::auth::constants::api_base_url());
                if api_key {
                    println!("API key fallback: configured (used if OAuth returns 403)");
                }
                Ok(())
            }
            None => {
                if api_key {
                    println!("Authenticated: false (OAuth)");
                    println!("API key fallback: configured");
                    println!("API base: {}", crate::providers::xai::auth::constants::api_base_url());
                    return Ok(());
                }
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

fn persist_and_print(tokens: &auth::jwt::TokenResponse) -> Result<(), anyhow::Error> {
    let store = file_store();
    let manager = XaiAuthManager::new(store);
    let saved = manager.persist_initial_tokens(tokens)?;
    println!("Auth saved in {}", manager.store.auth_path());
    if let Some(ref scope) = saved.scope {
        println!("Scope: {scope}");
    }
    println!("Authentication complete");
    Ok(())
}

pub(crate) static XAI_CLI: XaiCli = XaiCli;
