mod client;
mod stream;

use async_trait::async_trait;
use axum::{Json, http::StatusCode, response::{IntoResponse, Response}};

use crate::anthropic::{json_error, schema::MessagesRequest};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use client::AnthropicClient;

const MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];

pub fn advertised_models() -> Vec<String> {
    MODELS.iter().map(|model| format!("anthropic/{model}")).collect()
}

pub struct AnthropicProvider {
    client: Option<AnthropicClient>,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            client: AnthropicClient::new(
                crate::config::anthropic_base_url(),
                crate::config::anthropic_api_key(),
            ).ok(),
        }
    }

    fn with_request_headers(&self, headers: &http::HeaderMap) -> Self {
        Self {
            client: self.client.as_ref().map(|client| client.with_request_headers(headers)),
        }
    }

    fn client(&self) -> Result<&AnthropicClient, ProviderError> {
        self.client.as_ref().ok_or_else(|| ProviderError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ProviderErrorKind::Api,
            "Invalid Anthropic provider configuration",
        ))
    }

    async fn buffered(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        count_tokens: bool,
    ) -> Result<serde_json::Value, ProviderError> {
        let (value, model) = prepare_request(&body, count_tokens)?;
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &model);
        }
        let response = self.client()?.post(&value, count_tokens, &ctx).await?;
        let value = client::read_json(response).await?;
        if let Some(monitor) = &ctx.monitor {
            let usage = if count_tokens { &value } else { &value["usage"] };
            monitor.usage_updated(
                &ctx.req_id,
                usage.get("input_tokens").and_then(serde_json::Value::as_u64),
                usage.get("output_tokens").and_then(serde_json::Value::as_u64),
            );
        }
        if let Some(traffic) = &ctx.traffic {
            traffic.write_json("051-downstream-response", &value);
        }
        Ok(value)
    }
}

fn resolve_model(raw: &str) -> Result<String, ProviderError> {
    let normalized = crate::registry::normalize_incoming_model(raw);
    let model = normalized.strip_prefix("anthropic/").unwrap_or(&normalized);
    let model = match model {
        "opus" => "claude-opus-5-5",
        "sonnet" => "claude-sonnet-5-5",
        "haiku" => "claude-haiku-5-5",
        "fable" => "claude-fable-5-1",
        model if model.starts_with("claude-") && model.len() > "claude-".len() => model,
        _ => return Err(ProviderError::new(
            StatusCode::BAD_REQUEST,
            ProviderErrorKind::InvalidRequest,
            "Anthropic requires a Claude model ID or opus/sonnet/haiku/fable alias",
        )),
    };
    Ok(model.to_owned())
}

fn prepare_request(
    body: &MessagesRequest,
    count_tokens: bool,
) -> Result<(serde_json::Value, String), ProviderError> {
    let model = resolve_model(body.model.as_deref().unwrap_or("opus"))?;
    let mut value = serde_json::to_value(body).map_err(|_| ProviderError::new(
        StatusCode::BAD_REQUEST, ProviderErrorKind::InvalidRequest, "Invalid Anthropic request",
    ))?;
    value["model"] = serde_json::Value::String(model.clone());
    if count_tokens {
        // The token-count endpoint does not accept generation-only parameters.
        let object = value.as_object_mut().expect("serialized MessagesRequest object");
        object.retain(|key, _| matches!(
            key.as_str(), "model" | "messages" | "system" | "tools" | "tool_choice" | "thinking"
        ));
    } else if body.max_tokens.is_none() {
        value["max_tokens"] = serde_json::json!(64_000);
    }
    // Native protocol: preserve tools, thinking signatures, cache controls and all
    // other caller parameters; do not inject thinking, fallbacks or alter history.
    Ok((value, model))
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str { "anthropic" }

    fn supported_models(&self) -> Vec<String> { advertised_models() }

    fn cli(&self) -> &'static dyn CliHandlers { &ANTHROPIC_CLI }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        if body.stream {
            return match self.generate_anthropic_stream(body, ctx).await {
                Ok(generation) => {
                    let body = match generation.body {
                        GenerationBody::LiveSse(body) => body,
                        GenerationBody::BufferedSse(bytes) => axum::body::Body::from(bytes),
                    };
                    (
                        [("content-type", "text/event-stream"), ("cache-control", "no-cache")],
                        body,
                    ).into_response()
                }
                Err(error) => error_response(error),
            };
        }
        match self.buffered(body, ctx, false).await {
            Ok(value) => Json(value).into_response(),
            Err(error) => error_response(error),
        }
    }

    async fn handle_messages_with_request_headers(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        _conversation_identity: Option<crate::request_identity::ConversationIdentity>,
        headers: http::HeaderMap,
    ) -> Response {
        self.with_request_headers(&headers).handle_messages(body, ctx).await
    }

    async fn handle_count_tokens_with_request_headers(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        headers: http::HeaderMap,
    ) -> Response {
        self.with_request_headers(&headers).handle_count_tokens(body, ctx).await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        match self.buffered(body, ctx, true).await {
            Ok(value) => Json(value).into_response(),
            Err(error) => error_response(error),
        }
    }

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let (value, model) = prepare_request(&body, false)?;
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &model);
        }
        let response = self.client()?.post(&value, false, &ctx).await?;
        Ok(Generation {
            body: GenerationBody::LiveSse(stream::stream_body(response, ctx)),
            resolved_model: model,
        })
    }
}

fn error_response(error: ProviderError) -> Response {
    let mut response = json_error(error.status, error.error_type(), error.message);
    if let Some(retry_after) = error.retry_after.and_then(|value| value.parse().ok()) {
        response.headers_mut().insert(http::header::RETRY_AFTER, retry_after);
    }
    response
}

struct AnthropicCli;
const ANTHROPIC_CLI: AnthropicCli = AnthropicCli;

impl CliHandlers for AnthropicCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!("Anthropic uses an API key; set CCP_ANTHROPIC_API_KEY, ANTHROPIC_API_KEY, or anthropic.apiKey in config.json. Claude Pro/Max subscriptions do not include API usage.")
    }

    fn device(&self) -> anyhow::Result<()> { self.login() }

    fn status(&self) -> anyhow::Result<()> {
        if crate::config::anthropic_api_key().is_none() {
            anyhow::bail!("Not authenticated");
        }
        println!("Anthropic API key configured (not validated)");
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        println!("Remove CCP_ANTHROPIC_API_KEY, ANTHROPIC_API_KEY, and anthropic.apiKey from your configuration to disconnect Anthropic.");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
