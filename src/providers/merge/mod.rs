//! Generic Anthropic Messages upstream, configured for Merge Gateway by default.
//!
//! This is intentionally a thin passthrough (not a proprietary translator):
//! Claude Code already speaks Anthropic Messages, and Merge's Anthropic path
//! does too. The provider exists so the same shape can be upstreamed to raine
//! as a generic Anthropic-compatible backend — Merge is configuration, not a
//! hard-coded one-off.

pub mod auth;
pub mod client;
pub mod count_tokens;
pub mod model;

use std::convert::Infallible;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::provider::{CliHandlers, Provider, RequestContext};

use self::auth::{
    clear_auth_file, load_auth_file, load_merge_token, save_auth_file, StoredMergeAuth,
};
use self::client::{MergeClient, MergeError};
use self::model::{catalog_models, resolve_upstream_model};

pub struct MergeProvider {
    client: Arc<MergeClient>,
}

impl MergeProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(
                MergeClient::new(crate::config::merge_base_url())
                    .expect("Merge / Anthropic-compatible transport is unavailable"),
            ),
        }
    }

    pub fn with_client(client: MergeClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}

impl Default for MergeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for MergeProvider {
    fn name(&self) -> &'static str {
        "merge"
    }

    fn supported_models(&self) -> Vec<String> {
        catalog_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &MERGE_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body
            .model
            .clone()
            .unwrap_or_else(|| "merge:anthropic/claude-sonnet-5".into());
        let upstream_model = match resolve_upstream_model(&requested) {
            Ok(model) => model,
            Err(error) => {
                return json_error(StatusCode::BAD_REQUEST, "invalid_request_error", error);
            }
        };

        let mut forward = body.clone();
        forward.model = Some(upstream_model.clone());

        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &upstream_model);
            monitor.upstream_started(&ctx.req_id);
        }

        let upstream = match self.client.post_messages(&forward, ctx.traffic.clone()).await {
            Ok(response) => response,
            Err(error) => return map_error(error),
        };

        if body.stream {
            passthrough_sse(
                upstream,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
                ctx.traffic.clone(),
            )
        } else {
            let upstream_bytes = match upstream.into_bytes().await {
                Ok(bytes) => bytes,
                Err(error) => return map_error(error),
            };
            match serde_json::from_slice::<serde_json::Value>(&upstream_bytes) {
                Ok(value) => {
                    if let Some(traffic) = ctx.traffic.as_ref() {
                        traffic.write_json("051-downstream-response", &value);
                    }
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            value
                                .pointer("/usage/input_tokens")
                                .and_then(|v| v.as_u64()),
                            value
                                .pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    (StatusCode::OK, Json(value)).into_response()
                }
                Err(_) => json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Anthropic-compatible upstream returned invalid JSON",
                ),
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body
            .model
            .clone()
            .unwrap_or_else(|| "merge:anthropic/claude-sonnet-5".into());
        if let Err(error) = resolve_upstream_model(&requested) {
            return json_error(StatusCode::BAD_REQUEST, "invalid_request_error", error);
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

fn passthrough_sse(
    response: client::MergeResponse,
    monitor: Option<crate::monitor::MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
) -> Response {
    let stream = response.into_stream();
    let body = Body::from_stream(map_stream(stream, monitor, req_id, traffic));
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .header(http::header::CACHE_CONTROL, "no-cache")
        .header(http::header::CONNECTION, "keep-alive")
        .body(body)
        .expect("valid SSE response")
}

fn map_stream(
    stream: impl Stream<Item = Result<Bytes, MergeError>> + Send + 'static,
    monitor: Option<crate::monitor::MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    let mut total_bytes: u64 = 0;
    let mut event_count: u64 = 0;
    stream.map(move |item| {
        match item {
            Ok(bytes) => {
                total_bytes = total_bytes.saturating_add(bytes.len() as u64);
                event_count = event_count
                    .saturating_add(bytes.windows(2).filter(|w| *w == *b"\n\n").count() as u64);
                if let Some(monitor) = monitor.as_ref() {
                    monitor.stream_progress(&req_id, total_bytes, event_count, None, None);
                }
                Ok(bytes)
            }
            Err(error) => {
                if let Some(traffic) = traffic.as_ref() {
                    traffic.write_json(
                        "060-upstream-stream-error",
                        &serde_json::json!({"message": error.message}),
                    );
                }
                let payload = format!(
                    "event: error\ndata: {}\n\n",
                    serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": "api_error",
                            "message": error.message,
                        }
                    })
                );
                Ok(Bytes::from(payload))
            }
        }
    })
}

fn map_error(error: MergeError) -> Response {
    let status = error.status;
    match status.as_u16() {
        401 | 403 => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            error.message,
        ),
        429 => {
            let mut response = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                error.message,
            );
            if let Some(retry_after) = error.retry_after.as_deref() {
                if let Ok(value) = retry_after.parse::<http::HeaderValue>() {
                    response.headers_mut().insert("retry-after", value);
                }
            }
            response
        }
        400 => json_error(StatusCode::BAD_REQUEST, "invalid_request_error", error.message),
        _ if status.is_server_error() || status == StatusCode::BAD_GATEWAY => {
            json_error(StatusCode::BAD_GATEWAY, "api_error", error.message)
        }
        _ => json_error(status, "api_error", error.message),
    }
}

#[derive(Clone, Copy)]
struct MergeCli;

impl CliHandlers for MergeCli {
    fn login(&self) -> anyhow::Result<()> {
        eprintln!(
            "Browser login is not used for Merge / Anthropic-compatible upstreams.\n\
             Set CCP_MERGE_AUTH_TOKEN, or write an auth file:\n\
               {{\"access\":\"YOUR_TOKEN\"}} → {}",
            auth::auth_file_path().display()
        );
        anyhow::bail!("use CCP_MERGE_AUTH_TOKEN or merge/auth.json instead of login");
    }

    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }

    fn status(&self) -> anyhow::Result<()> {
        if load_merge_token().is_some() {
            println!("Authenticated (token available)");
            Ok(())
        } else {
            Err(anyhow::anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> anyhow::Result<()> {
        clear_auth_file()?;
        Ok(())
    }
}

const MERGE_CLI: MergeCli = MergeCli;

/// Helper used by tests / CLI helpers that want to seed auth without env.
#[allow(dead_code)]
pub fn write_test_auth(access: &str) -> anyhow::Result<()> {
    save_auth_file(&StoredMergeAuth {
        access: access.to_string(),
    })
}

#[allow(dead_code)]
pub fn read_test_auth() -> anyhow::Result<Option<StoredMergeAuth>> {
    load_auth_file()
}
