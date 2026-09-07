mod headers;
mod request;
mod response;

use std::time::Duration;

use async_trait::async_trait;
use axum::{http::StatusCode, response::Response};
use reqwest::{Client, Url};

use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    provider::{CliHandlers, Provider, ProviderRequest, RequestContext},
    registry::ANTHROPIC_STYLE_ALIASES,
};

pub struct AnthropicProvider {
    client: Client,
    base_url: Url,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(300))
            .build()
            .expect("Anthropic HTTP client");
        Self::with_client(
            client,
            Url::parse("https://api.anthropic.com").expect("Anthropic URL"),
        )
    }

    pub fn with_client(client: Client, base_url: Url) -> Self {
        Self { client, base_url }
    }

    async fn relay(&self, request: ProviderRequest, ctx: RequestContext) -> Response {
        let Some(model) = request.body.model.as_deref() else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Missing model",
            );
        };
        let model = request::resolve_model(model);
        let (parts, raw) = request.original.into_parts();
        let body = match request::prepare_body(raw, model) {
            Ok(body) => body,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
        let mut url = self.base_url.clone();
        url.set_path(parts.uri.path());
        url.set_query(parts.uri.query());
        let mut headers = headers::forwarded_headers(&parts.headers);
        headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("identity"),
        );
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, model);
            monitor.upstream_started(&ctx.req_id);
        }
        match self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(upstream) => response::relay(upstream, ctx),
            Err(error) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Anthropic request failed: {}", error.without_url()),
            ),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<String> {
        ANTHROPIC_STYLE_ALIASES
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &ANTHROPIC_CLI
    }

    async fn handle_request(&self, request: ProviderRequest, ctx: RequestContext) -> Response {
        self.relay(request, ctx).await
    }

    async fn handle_messages(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        original_request_required()
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        original_request_required()
    }
}

fn original_request_required() -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "Anthropic passthrough requires the original Messages HTTP request and client credentials",
    )
}

struct AnthropicCli;
static ANTHROPIC_CLI: AnthropicCli = AnthropicCli;

impl CliHandlers for AnthropicCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!("Sign in with Claude Code; Anthropic credentials are forwarded per request")
    }
    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }
    fn status(&self) -> anyhow::Result<()> {
        println!("Anthropic credentials are forwarded from Claude Code; the proxy stores none");
        Ok(())
    }
    fn logout(&self) -> anyhow::Result<()> {
        anyhow::bail!("Sign out in Claude Code; the proxy stores no Anthropic credentials")
    }
}
