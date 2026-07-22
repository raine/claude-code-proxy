use super::{
    auth::{
        constants::CODEX_API_ENDPOINT,
        manager::CodexAuthManager,
        token_store::{DefaultCodexAuthStore, StoredAuth, file_store},
    },
    client::build_codex_headers,
};
use crate::{anthropic::json_error, provider::RequestContext};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::Response,
    routing::post,
};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

const ENABLE_ENV: &str = "CCP_ENABLE_CODEX_RESPONSES";

#[derive(Clone)]
struct ResponsesProxyState {
    client: reqwest::Client,
    auth: Arc<CodexAuthManager<DefaultCodexAuthStore>>,
}

pub fn enabled() -> bool {
    std::env::var(ENABLE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn router() -> Router {
    let state = ResponsesProxyState {
        client: reqwest::Client::new(),
        auth: Arc::new(CodexAuthManager::new(file_store())),
    };
    Router::new()
        .route("/v1/responses", post(handler))
        .with_state(state)
}

async fn handler(State(state): State<ResponsesProxyState>, req: Request<Body>) -> Response {
    let session_id = req
        .headers()
        .get("x-client-request-id")
        .or_else(|| req.headers().get("session_id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(body) if serde_json::from_slice::<Value>(&body).is_ok() => body,
        Ok(_) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Invalid JSON request body",
            );
        }
        Err(err) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Unable to read request body: {err}"),
            );
        }
    };
    let context = RequestContext {
        req_id: Uuid::new_v4().to_string(),
        session_id,
        session_seq: None,
        provider: "codex-responses".to_string(),
        traffic: None,
        monitor: None,
    };
    let auth = match state.auth.get_auth().await {
        Ok(auth) => auth,
        Err(err) => {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                err.to_string(),
            );
        }
    };
    let mut upstream = match send(&state, &body, &context, &auth).await {
        Ok(response) => response,
        Err(response) => return response,
    };
    if upstream.status() == reqwest::StatusCode::UNAUTHORIZED {
        let refreshed = match state.auth.force_refresh(&auth.access).await {
            Ok(auth) => auth,
            Err(err) => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    format!("Codex authentication refresh failed: {err}"),
                );
            }
        };
        upstream = match send(&state, &body, &context, &refreshed).await {
            Ok(response) => response,
            Err(response) => return response,
        };
    }
    upstream_response(upstream)
}

async fn send(
    state: &ResponsesProxyState,
    body: &[u8],
    context: &RequestContext,
    auth: &StoredAuth,
) -> Result<reqwest::Response, Response> {
    let headers = build_codex_headers(auth, context, false).map_err(|err| {
        json_error(StatusCode::INTERNAL_SERVER_ERROR, "proxy_error", err.to_string())
    })?;
    let mut request = state
        .client
        .post(crate::config::codex_base_url(CODEX_API_ENDPOINT));
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    request.body(body.to_vec()).send().await.map_err(|err| {
        json_error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            format!("Codex upstream request failed: {err}"),
        )
    })
}

fn upstream_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    for name in [
        http::header::CONTENT_TYPE,
        http::header::CACHE_CONTROL,
        http::header::RETRY_AFTER,
        http::header::HeaderName::from_static("x-request-id"),
    ] {
        if let Some(value) = headers.get(&name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    response
}
