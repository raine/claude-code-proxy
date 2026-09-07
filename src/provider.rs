use crate::anthropic::schema::MessagesRequest;
use crate::monitor::MonitorHandle;
use crate::request_identity::ConversationIdentity;
use crate::traffic::TrafficCapture;
use anyhow::Result;
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
};
use bytes::Bytes;
use clap::Subcommand;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Sign in using browser-based authentication
    Login,
    /// Sign in using a device code
    Device,
    /// Show the current authentication status
    Status,
    /// Delete stored authentication credentials
    Logout,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_models(&self) -> Vec<String>;
    fn cli(&self) -> &'static dyn CliHandlers;

    async fn handle_request(&self, request: ProviderRequest, ctx: RequestContext) -> Response {
        match request.endpoint {
            RequestEndpoint::Messages(identity) => {
                self.handle_messages_with_conversation_identity(request.body, ctx, identity)
                    .await
            }
            RequestEndpoint::CountTokens => self.handle_count_tokens(request.body, ctx).await,
        }
    }
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let _ = conversation_identity;
        self.handle_messages(body, ctx).await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn generate_anthropic_stream(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        Err(ProviderError::new(
            StatusCode::NOT_IMPLEMENTED,
            ProviderErrorKind::InvalidRequest,
            format!(
                "provider '{}' does not support OpenAI-compatible generation",
                self.name()
            ),
        ))
    }
}

/// The parsed routing input and the original HTTP representation travel together.
/// Translating providers use `body`; passthrough providers retain fields unknown
/// to the proxy by forwarding `original`.
pub struct ProviderRequest {
    pub body: MessagesRequest,
    pub original: Request<Bytes>,
    pub endpoint: RequestEndpoint,
}

pub enum RequestEndpoint {
    Messages(Option<ConversationIdentity>),
    CountTokens,
}

/// Records a protocol failure discovered after response headers were sent.
#[derive(Clone, Default)]
pub struct ResponseOutcome {
    failure: Arc<Mutex<Option<String>>>,
}

impl ResponseOutcome {
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }

    pub(crate) fn fail(&self, message: String) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(message);
        }
    }
}

pub enum GenerationBody {
    BufferedSse(Bytes),
    LiveSse(Body),
}

pub struct Generation {
    pub body: GenerationBody,
    pub resolved_model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    Permission,
    RateLimit,
    InvalidRequest,
    Api,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status: StatusCode,
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retry_after: Option<String>,
    pub param: Option<String>,
    pub code: Option<String>,
}

impl ProviderError {
    pub fn new(status: StatusCode, kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            param: None,
            code: None,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self.kind {
            ProviderErrorKind::Authentication => "authentication_error",
            ProviderErrorKind::Permission => "permission_error",
            ProviderErrorKind::RateLimit => "rate_limit_error",
            ProviderErrorKind::InvalidRequest => "invalid_request_error",
            ProviderErrorKind::Api => "api_error",
        }
    }
}

pub trait CliHandlers: Send + Sync {
    fn login(&self) -> Result<()>;
    fn device(&self) -> Result<()>;
    fn status(&self) -> Result<()>;
    fn logout(&self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: String,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
}
