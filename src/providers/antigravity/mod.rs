//! Experimental, opt-in native Argon subscription transport.
pub(crate) mod auth;
pub(crate) mod config;
#[cfg(any(unix, test))]
mod manual;
mod refresh;
pub(crate) mod replay;
pub(crate) mod request;
pub(crate) mod schema;
pub(crate) mod transport;

use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    provider::{CliHandlers, Provider, RequestContext},
    providers::codex::native::NativeResponseOutcome,
};
use async_trait::async_trait;
use auth::{Auth, Store};
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use config::Config;
use replay::{Binding, Result};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub struct AntigravityProvider {
    auth: Result<Arc<Auth>>,
}
impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl AntigravityProvider {
    pub fn new() -> Self {
        Self {
            auth: Config::load()
                .and_then(|c| Auth::new(c, Store::configured()))
                .map(Arc::new),
        }
    }
    #[cfg(test)]
    pub(crate) fn with_auth(auth: Auth) -> Self {
        Self {
            auth: Ok(Arc::new(auth)),
        }
    }

    async fn generate(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let auth = match &self.auth {
            Ok(auth) => auth,
            Err(_) => return invalid("Antigravity configuration is invalid"),
        };
        let stored = match auth.store.load() {
            Ok(value) => value,
            Err(_) => return authentication("Antigravity credentials are invalid or unreadable"),
        };
        let binding = binding(auth, &body, stored.as_ref());
        let prepared = match request::prepare(&body, &binding) {
            Ok(p) => p,
            Err(e) => return invalid(e),
        };
        let credential = match auth.access().await {
            Ok(c) => c,
            Err(error) => {
                return match error {
                    auth::AccessError::PendingPersistence
                    | auth::AccessError::ConnectFailure
                    | auth::AccessError::ConnectCooldown
                    | auth::AccessError::StoppedBeforeSend
                    | auth::AccessError::Lock(_)
                    | auth::AccessError::Closed => json_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "api_error",
                        error.message(),
                    ),
                    _ => authentication(error.message()),
                };
            }
        };
        let current = binding_for(auth, &credential, body.model.as_deref().unwrap_or_default());
        if current.profile_id != binding.profile_id || current.project_id != binding.project_id {
            return invalid(
                "Antigravity profile/project changed during request; start a fresh session",
            );
        }
        if current.project_id == "unconfigured" {
            return invalid(
                "Antigravity project missing; run auth login or configure your authorized project",
            );
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, body.model.as_deref().unwrap_or_default());
        }
        let endpoint = match auth
            .config
            .base_url
            .join("/v1internal:streamGenerateContent?alt=sse")
        {
            Ok(v) => v,
            Err(_) => return invalid("invalid native endpoint"),
        };
        let header_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let sent = tokio::time::timeout_at(
            header_deadline,
            auth.client
                .post(endpoint)
                .bearer_auth(&credential.access_token)
                .header("accept", "text/event-stream")
                .headers(auth.config.native_headers())
                .json(&prepared.wire)
                .send(),
        )
        .await;
        let upstream = match sent {
            Ok(Ok(response)) => response,
            _ => return upstream_error("Antigravity native headers timed out or transport failed"),
        };
        if !upstream.status().is_success() {
            return transport::http_error(upstream, header_deadline).await;
        }
        if !upstream
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim() == "text/event-stream")
            })
        {
            return upstream_error("Antigravity returned an unexpected content type");
        }
        let model = body.model.unwrap_or_default();
        let work = async move {
            let turn = tokio::time::timeout(Duration::from_secs(180), transport::collect(upstream))
                .await
                .map_err(|_| "native turn deadline exceeded")??;
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.usage_updated(
                    &ctx.req_id,
                    turn.usage["input_tokens"].as_u64(),
                    turn.usage["output_tokens"].as_u64(),
                );
            }
            let mapping = prepared.tools.mapping(&turn.contents)?;
            let blocks = replay::encode(
                &turn.contents,
                &binding,
                &prepared.session,
                &prepared.display,
                &mapping,
            )?;
            Ok::<_, &'static str>(transport::message(blocks, &model, &turn))
        };
        if !body.stream {
            return match work.await {
                Ok(message) => Json(message).into_response(),
                Err(e) => upstream_error(e),
            };
        }
        // The returned Body owns this future, including reqwest's response. No
        // worker survives a downstream drop. Initial HTTP errors occurred above.
        let outcome = NativeResponseOutcome::default();
        let shared = outcome.clone();
        let stream = futures_util::stream::once(async move {
            let result = work.await.and_then(|message| transport::sse(&message));
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    shared.fail(format!("Antigravity: {error}"));
                    format!("event: error\ndata: {}\n\n",json!({"type":"error","error":{"type":"api_error","message":format!("Antigravity: {error}")}})).into_bytes()
                }
            };
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from(bytes))
        });
        let mut response = Response::new(Body::from_stream(stream));
        response.headers_mut().insert(
            "content-type",
            http::HeaderValue::from_static("text/event-stream"),
        );
        response
            .headers_mut()
            .insert("cache-control", http::HeaderValue::from_static("no-cache"));
        response.extensions_mut().insert(outcome);
        response
    }
}

fn binding_for(auth: &Auth, credential: &auth::Credential, model: &str) -> Binding {
    Binding {
        profile_id: credential.profile_id.clone(),
        project_id: auth
            .config
            .project
            .clone()
            .or_else(|| credential.project_id.clone())
            .unwrap_or_else(|| "unconfigured".into()),
        native_model: model
            .strip_prefix("antigravity/")
            .unwrap_or(model)
            .to_string(),
    }
}
fn binding(auth: &Auth, body: &MessagesRequest, credential: Option<&auth::Credential>) -> Binding {
    let model = body.model.as_deref().unwrap_or_default();
    credential
        .map(|c| binding_for(auth, c, model))
        .unwrap_or_else(|| Binding {
            profile_id: "unconfigured".into(),
            project_id: auth
                .config
                .project
                .clone()
                .unwrap_or_else(|| "unconfigured".into()),
            native_model: model.strip_prefix("antigravity/").unwrap_or(model).into(),
        })
}
fn invalid(message: &str) -> Response {
    json_error(StatusCode::BAD_REQUEST, "invalid_request_error", message)
}
fn authentication(message: &str) -> Response {
    json_error(StatusCode::UNAUTHORIZED, "authentication_error", message)
}
fn upstream_error(message: &str) -> Response {
    json_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("Antigravity: {message}"),
    )
}

#[async_trait]
impl Provider for AntigravityProvider {
    fn name(&self) -> &'static str {
        "antigravity"
    }
    fn supported_models(&self) -> Vec<String> {
        request::MODELS.iter().map(|m| (*m).into()).collect()
    }
    fn local_capability(&self) -> Option<Value> {
        Some(match &self.auth {
            Ok(auth) => auth::descriptor(&Ok(auth.config.clone()), &auth.store),
            Err(error) => auth::descriptor(&Err(*error), &Store::configured()),
        })
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &CLI
    }
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let tier = request::effective_tier(body.model.as_deref().unwrap_or_default());
        let mut response = self.generate(body, ctx).await;
        if let Some(tier) = tier {
            response.headers_mut().insert(
                "x-antigravity-reasoning-tier",
                http::HeaderValue::from_static(tier),
            );
        }
        response
    }
    async fn handle_count_tokens(&self, body: MessagesRequest, _ctx: RequestContext) -> Response {
        let auth = match &self.auth {
            Ok(auth) => auth,
            Err(_) => return invalid("Antigravity configuration is invalid"),
        };
        let credential = match auth.store.load() {
            Ok(c) => c,
            Err(_) => return authentication("Antigravity credentials invalid or unreadable"),
        };
        match request::prepare(&body, &binding(auth, &body, credential.as_ref())) {
            Ok(p) => Json(json!({"input_tokens":p.estimate})).into_response(),
            Err(e) => invalid(e),
        }
    }
    async fn finish_maintenance(&self) -> anyhow::Result<()> {
        if let Ok(auth) = &self.auth {
            auth.finish_maintenance().await?;
        }
        Ok(())
    }
    // Optional OpenAI generation deliberately retains the trait's 501 default.
}

pub fn enabled() -> bool {
    Config::load().is_ok_and(|c| c.enabled)
}
pub fn known_model(model: &str) -> bool {
    request::MODELS.contains(&model)
}
pub fn capability() -> Value {
    auth::descriptor(&Config::load(), &Store::configured())
}
pub fn status_json() -> Value {
    let mut value = capability();
    value["schema_version"] = json!(1);
    value["provider"] = json!("antigravity");
    value
}

/// Explicit manual completion is separate from the shared provider CLI contract.
pub fn login_manual() -> anyhow::Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("Antigravity manual login is unsupported on this platform; use auth login");
    #[cfg(unix)]
    {
        match manual::terminal::run_cli().map_err(anyhow::Error::msg)? {
            manual::terminal::CliOutcome::Success => Ok(()),
            manual::terminal::CliOutcome::HandledFailure => {
                // run_cli has dropped EVERY auth/runtime/terminal guard. Do not
                // return Err: Rust main's stderr report can block on a full tty.
                std::process::exit(1)
            }
        }
    }
}

struct AntigravityCli;
static CLI: AntigravityCli = AntigravityCli;
fn run_auth(action: &str) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if action == "logout" {
        return runtime
            .block_on(Store::configured().logout())
            .map_err(anyhow::Error::msg);
    }
    let config = Config::load().map_err(anyhow::Error::msg)?;
    let auth = Auth::new(config, Store::configured()).map_err(anyhow::Error::msg)?;
    runtime
        .block_on(async {
            match action {
                "login" => auth.login().await,
                "logout" => auth.logout().await,
                _ => Err("unsupported auth operation"),
            }
        })
        .map_err(anyhow::Error::msg)
}
impl CliHandlers for AntigravityCli {
    fn login(&self) -> anyhow::Result<()> {
        run_auth("login")
    }
    fn logout(&self) -> anyhow::Result<()> {
        run_auth("logout")
    }
    fn device(&self) -> anyhow::Result<()> {
        anyhow::bail!("Antigravity device flow is unsupported; use explicit browser auth login")
    }
    fn status(&self) -> anyhow::Result<()> {
        println!("{}", status_json());
        Ok(())
    }
}
pub fn cli_handlers() -> &'static dyn CliHandlers {
    &CLI
}
