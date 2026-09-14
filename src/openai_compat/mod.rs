pub mod request;
pub mod response;
pub mod stream;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::provider::{ProviderError, ProviderErrorKind};

pub const MAX_OPENAI_REQUEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PROVIDER_STREAM_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiSurface {
    ChatCompletions,
    Responses,
}

impl OpenAiSurface {
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiResponseMetadata {
    pub tools: Vec<Value>,
    pub tool_choice: Value,
}

impl Default for OpenAiResponseMetadata {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            tool_choice: json!("auto"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiError {
    pub status: StatusCode,
    pub kind: Box<str>,
    pub message: Box<str>,
    pub param: Option<Box<str>>,
    pub code: Option<Box<str>>,
    pub retry_after: Option<Box<str>>,
}

impl OpenAiError {
    pub fn invalid(message: impl Into<String>, param: Option<impl Into<String>>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error".into(),
            message: message.into().into_boxed_str(),
            param: param.map(|value| value.into().into_boxed_str()),
            code: None,
            retry_after: None,
        }
    }

    pub fn upstream_protocol(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            kind: "api_error".into(),
            message: message.into().into_boxed_str(),
            param: None,
            code: Some("upstream_protocol_error".into()),
            retry_after: None,
        }
    }

    pub fn unsupported(param: impl Into<String>) -> Self {
        let param = param.into();
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error".into(),
            message: format!("Unsupported parameter: '{param}'").into(),
            param: Some(param.into()),
            code: Some("unsupported_parameter".into()),
            retry_after: None,
        }
    }

    pub fn response(self) -> Response {
        let status = self.status;
        let retry_after = self.retry_after.clone();
        let mut response = (
            status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": self.kind,
                    "param": self.param,
                    "code": self.code,
                }
            })),
        )
            .into_response();
        if let Some(value) = retry_after.and_then(|value| value.parse().ok()) {
            response
                .headers_mut()
                .insert(http::header::RETRY_AFTER, value);
        }
        response
    }
}

impl From<ProviderError> for OpenAiError {
    fn from(error: ProviderError) -> Self {
        Self {
            status: error.status,
            kind: match error.kind {
                ProviderErrorKind::Authentication => "authentication_error",
                ProviderErrorKind::Permission => "permission_error",
                ProviderErrorKind::RateLimit => "rate_limit_error",
                ProviderErrorKind::InvalidRequest => "invalid_request_error",
                ProviderErrorKind::Api => "api_error",
            }
            .into(),
            message: error.message.into(),
            param: error.param.map(Into::into),
            code: error.code.map(Into::into),
            retry_after: error.retry_after.map(Into::into),
        }
    }
}
