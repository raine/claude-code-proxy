//! Official Sign in with ChatGPT: independent auth and public HTTP transport.
mod auth;
pub mod cli;
mod client;
mod store;
mod stream;
pub(crate) use stream::StreamOutcome;
#[cfg(test)]
mod tests;
mod translate;

use self::{
    auth::Auth,
    client::{ApiError, Client},
};
use crate::{CliHandlers, MessagesRequest, Provider, RequestContext};
use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub struct SiwcProvider {
    client: Client,
    default_model: Option<String>,
}
impl SiwcProvider {
    pub fn new(profile: String, default_model: Option<String>) -> Result<Self> {
        Ok(Self {
            client: Client::new(Auth::new(profile)?),
            default_model,
        })
    }
    fn model(&self, requested: Option<&str>) -> Result<String> {
        let model = requested.unwrap_or("siwc");
        if model == "siwc" || crate::registry::is_anthropic_alias(model) {
            self.default_model.clone().ok_or_else(|| {
                anyhow::anyhow!("Set CCP_SIWC_MODEL or siwc.model to a slug from `siwc models`")
            })
        } else {
            Ok(model.strip_prefix("siwc/").unwrap_or(model).to_string())
        }
    }
}

#[async_trait]
impl Provider for SiwcProvider {
    fn name(&self) -> &'static str {
        "siwc"
    }
    fn supported_models(&self) -> Vec<String> {
        vec!["siwc".into()]
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &SIWC_CLI
    }
    async fn model_catalog(&self, limit: Option<usize>) -> Option<Response> {
        Some(match self.client.models().await {
            Ok(mut data) => {
                let has_more = limit.is_some_and(|limit| data.len() > limit);
                if let Some(limit) = limit {
                    data.truncate(limit);
                }
                Json(json!({"object":"list", "first_id":data.first().map(|m| &m["id"]),"last_id":data.last().map(|m| &m["id"]),"data":data,"has_more":has_more})).into_response()
            }
            Err(e) => e.response(),
        })
    }
    async fn handle_messages(&self, body: MessagesRequest, _ctx: RequestContext) -> Response {
        let model = match self.model(body.model.as_deref()) {
            Ok(model) => model,
            Err(e) => return bad_request(&e.to_string()),
        };
        let prepared = match translate::prepare(&body, model.clone()) {
            Ok(prepared) => prepared,
            Err(e) => return bad_request(&e.to_string()),
        };
        match self.client.responses(&prepared.body).await {
            Ok(response) => {
                stream::response(
                    response,
                    prepared.tool_names,
                    model,
                    body.stream,
                    prepared.estimated_tokens,
                    self.client.usage_pause.clone(),
                    self.client.auth.clone(),
                )
                .await
            }
            Err(e) => e.response(),
        }
    }
    async fn handle_count_tokens(&self, body: MessagesRequest, _ctx: RequestContext) -> Response {
        match self
            .model(body.model.as_deref())
            .and_then(|model| translate::prepare(&body, model))
        {
            Ok(prepared) => (
                [("x-siwc-token-count", "estimate")],
                Json(json!({"input_tokens":prepared.estimated_tokens})),
            )
                .into_response(),
            Err(e) => bad_request(&e.to_string()),
        }
    }
}
fn bad_request(message: &str) -> Response {
    ApiError::local(
        StatusCode::BAD_REQUEST,
        "siwc_unsupported_or_invalid_request",
        message,
    )
    .response()
}

struct SiwcCli;
static SIWC_CLI: SiwcCli = SiwcCli;
impl CliHandlers for SiwcCli {
    fn login(&self) -> Result<()> {
        anyhow::bail!("Use claude-code-proxy siwc auth login")
    }
    fn device(&self) -> Result<()> {
        anyhow::bail!("SIWC uses browser Authorization Code + PKCE; use siwc auth login")
    }
    fn status(&self) -> Result<()> {
        anyhow::bail!("Use claude-code-proxy siwc auth status")
    }
    fn logout(&self) -> Result<()> {
        anyhow::bail!("Use claude-code-proxy siwc auth logout")
    }
}
