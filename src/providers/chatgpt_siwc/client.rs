use super::auth::{Auth, http_client, read_json};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone)]
pub(super) struct ApiError {
    pub status: StatusCode,
    pub body: Value,
    pub request_id: Option<String>,
    pub retry_after: Option<String>,
}

impl ApiError {
    pub fn local(status: StatusCode, code: &str, message: &str) -> Self {
        Self {
            status,
            body: json!({"error":{"code":code,"message":message}}),
            request_id: None,
            retry_after: None,
        }
    }
    pub fn error_type(&self) -> &'static str {
        match self.status.as_u16() {
            400 | 404 | 422 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            503 => "overloaded_error",
            _ => "api_error",
        }
    }
    pub fn envelope(&self) -> Value {
        let message = self
            .body
            .pointer("/error/message")
            .or_else(|| self.body.get("detail"))
            .and_then(Value::as_str)
            .unwrap_or("SIWC upstream request failed");
        json!({"type":"error","error":{"type":self.error_type(),"message":message,"code":self.body.pointer("/error/code"),"param":self.body.pointer("/error/param")},"upstream_status":self.status.as_u16(),"upstream_body":self.body,"upstream_request_id":self.request_id})
    }
    pub fn response(&self) -> Response {
        let mut response = (self.status, Json(self.envelope())).into_response();
        for (key, value) in [
            ("x-openai-request-id", &self.request_id),
            ("retry-after", &self.retry_after),
        ] {
            if let Some(value) = value
                && let Ok(value) = value.parse()
            {
                response.headers_mut().insert(key, value);
            }
        }
        response
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately exclude upstream text from CLI diagnostics and logs.
        write!(
            f,
            "SIWC HTTP {} ({})",
            self.status.as_u16(),
            self.body
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("upstream_error")
        )
    }
}
impl std::error::Error for ApiError {}

#[derive(Clone)]
pub(super) struct Client {
    pub auth: Auth,
    http: reqwest::Client,
    pub usage_pause: std::sync::Arc<std::sync::Mutex<Option<ApiError>>>,
}

impl Client {
    pub fn new(auth: Auth) -> Self {
        Self {
            auth,
            http: http_client(),
            usage_pause: Default::default(),
        }
    }
    async fn request(&self, body: Option<&Value>) -> Result<reqwest::Response, ApiError> {
        if body.is_some()
            && let Some(error) = self.usage_pause.lock().expect("usage pause").clone()
        {
            return Err(error);
        }
        let token = self.auth.access_token().await.map_err(|e| {
            ApiError::local(
                StatusCode::UNAUTHORIZED,
                "siwc_authorization_required",
                &e.to_string(),
            )
        })?;
        for attempt in 0..3 {
            if body.is_some()
                && let Some(error) = self.usage_pause.lock().expect("usage pause").clone()
            {
                return Err(error);
            }
            if !self.auth.session_active().await.unwrap_or(false) {
                return Err(ApiError::local(
                    StatusCode::UNAUTHORIZED,
                    "siwc_signed_out",
                    "SIWC account signed out or permission removed",
                ));
            }
            let request = if let Some(body) = body {
                self.http
                    .post(format!("{}/responses", self.auth.endpoints.api))
                    .header("accept", "text/event-stream")
                    .json(body)
            } else {
                self.http.get(format!("{}/models", self.auth.endpoints.api))
            };
            let result =
                tokio::time::timeout(Duration::from_secs(60), request.bearer_auth(&token).send())
                    .await;
            let response = result.ok().and_then(Result::ok).ok_or_else(|| {
                ApiError::local(
                    StatusCode::BAD_GATEWAY,
                    "siwc_network_error",
                    "SIWC request failed or timed out before headers; no automatic replay",
                )
            })?;
            if response.status().is_success() {
                return Ok(response);
            }
            let status = response.status();
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let raw = tokio::time::timeout(Duration::from_secs(15), read_json(response))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(
                    || json!({"detail":"Upstream returned an unreadable error response"}),
                );
            // Error payloads are untrusted; never return a reflected bearer token.
            let raw = serde_json::to_string(&raw)
                .unwrap_or_default()
                .replace(&token, "[REDACTED]");
            let error = ApiError {
                status,
                body: serde_json::from_str(&raw).unwrap_or(Value::Null),
                request_id,
                retry_after,
            };
            if error.body.pointer("/error/code").and_then(Value::as_str)
                == Some("subscription_sharing_usage_limit_exceeded")
            {
                *self.usage_pause.lock().expect("usage pause") = Some(error.clone());
            }
            let code = error
                .body
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            let temporary = status == StatusCode::SERVICE_UNAVAILABLE
                && matches!(
                    code,
                    "" | "subscription_sharing_usage_unavailable"
                        | "subscription_sharing_user_unavailable"
                );
            if !temporary || attempt == 2 {
                return Err(error);
            }
            let delay = error
                .retry_after
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_millis(250 * (1 << attempt)));
            if delay > Duration::from_secs(5) {
                return Err(error);
            }
            tokio::time::sleep(delay).await;
        }
        unreachable!("bounded request loop")
    }
    pub async fn responses(&self, body: &Value) -> Result<reqwest::Response, ApiError> {
        self.request(Some(body)).await
    }
    pub async fn models(&self) -> Result<Vec<Value>, ApiError> {
        let response = self.request(None).await?;
        let raw = tokio::time::timeout(Duration::from_secs(20), read_json(response))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                ApiError::local(
                    StatusCode::BAD_GATEWAY,
                    "invalid_model_catalog",
                    "Could not read SIWC model catalog",
                )
            })?;
        let models = raw["models"].as_array().ok_or_else(|| {
            ApiError::local(
                StatusCode::BAD_GATEWAY,
                "invalid_model_catalog",
                "SIWC model catalog has no models array",
            )
        })?;
        models
            .iter()
            .filter(|m| m["visibility"] == "list")
            .map(|m| {
                let slug = m["slug"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ApiError::local(
                            StatusCode::BAD_GATEWAY,
                            "invalid_model_catalog",
                            "SIWC model entry has no slug",
                        )
                    })?;
                let display = m["display_name"].as_str().unwrap_or(slug);
                Ok(json!({"type":"model","object":"model","id":slug,"display_name":display}))
            })
            .collect()
    }
}
