use crate::{
    anthropic::json_error,
    logging::{Logger, REDACT_KEYS, create_logger},
    monitor::{EndpointKind, MonitorHandle},
    project,
    provider::RequestContext,
    providers::codex::{
        chat_completions::{ChatCompletionsBackend, request::translate_request},
        images::{
            CodexImagesBackend, ImageOperation, ImageRequestError, MAX_EDIT_REQUEST_BYTES,
            MAX_GENERATION_REQUEST_BYTES, MultipartEditInput, UploadedImage, image_error_response,
            prepare_json_request, prepare_multipart_edit,
        },
        native::{
            CodexNativeBackend, NativeResponseOutcome, openai_error, validate_native_request_model,
        },
    },
    registry::{Registry, normalize_incoming_model},
    session::{self, SessionState},
    traffic::{TrafficCaptureOptions, create_traffic_capture},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Query, State},
    http::{Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use http_body_util::{BodyExt, StreamBody};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::fs::{self, File};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use uuid::Uuid;

const CLAUDE_AUTO_REVIEW_SYSTEM_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";
const CODEX_AUTO_REVIEW_MODEL: &str = "gpt-5.6-luna";

#[derive(Debug, Clone, PartialEq, Eq)]
struct AutoReviewRoute {
    requested_model: String,
    override_model: String,
}

fn is_claude_auto_review_request(body: &crate::anthropic::schema::MessagesRequest) -> bool {
    if body.stream {
        return false;
    }

    let has_tools = body
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        return false;
    }

    body.extra
        .get("system")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(CLAUDE_AUTO_REVIEW_SYSTEM_PREFIX))
            })
        })
}

fn apply_auto_review_model(
    body: &mut crate::anthropic::schema::MessagesRequest,
    count_tokens: bool,
    configured_model: Option<&str>,
    original_provider: &str,
) -> Option<AutoReviewRoute> {
    if count_tokens || !is_claude_auto_review_request(body) {
        return None;
    }

    let override_model = configured_model
        .filter(|model| !model.is_empty())
        .or((original_provider == "codex").then_some(CODEX_AUTO_REVIEW_MODEL))?;
    let route = AutoReviewRoute {
        requested_model: body.model.clone()?,
        override_model: override_model.to_string(),
    };
    body.model = Some(route.override_model.clone());
    Some(route)
}

pub struct ServerConfig {
    pub bind_address: String,
    pub port: u16,
    pub monitor: Option<MonitorHandle>,
}

pub async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    serve_inner(config, std::future::pending::<()>()).await
}

pub async fn serve_with_shutdown(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    serve_inner(config, shutdown).await
}

async fn serve_inner(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind_proxy_listener(&config.bind_address, config.port).await?;
    serve_listener(listener, config.monitor, shutdown).await
}

pub async fn bind_proxy_listener(bind_address: &str, port: u16) -> anyhow::Result<TcpListener> {
    let ip = bind_address
        .parse::<std::net::IpAddr>()
        .map_err(|err| anyhow::anyhow!("invalid proxy bind address {bind_address:?}: {err}"))?;
    let addr = std::net::SocketAddr::new(ip, port);
    TcpListener::bind(addr)
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind proxy listener on {addr}: {err}"))
}

pub async fn serve_listener(
    listener: TcpListener,
    monitor: Option<MonitorHandle>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();
    create_logger("server").info(
        "server listening",
        Some(serde_json::Map::from_iter([
            ("port".to_string(), json!(port)),
            (
                "bindAddress".to_string(),
                json!(local_addr.ip().to_string()),
            ),
            (
                "logDir".to_string(),
                json!(
                    crate::paths::log_file()
                        .parent()
                        .map(|path| path.display().to_string())
                ),
            ),
        ])),
    );
    let app = app_with_monitor(Arc::new(Registry::with_default_alias()), monitor);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

pub fn app(registry: Arc<Registry>) -> Router {
    app_with_features(
        registry,
        None,
        AppFeatures {
            responses_api: crate::config::codex_responses_api(),
            images_api: crate::config::codex_images_api(),
        },
    )
}

pub fn app_with_monitor(registry: Arc<Registry>, monitor: Option<MonitorHandle>) -> Router {
    app_with_features(
        registry,
        monitor,
        AppFeatures {
            responses_api: crate::config::codex_responses_api(),
            images_api: crate::config::codex_images_api(),
        },
    )
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AppFeatures {
    pub responses_api: bool,
    pub images_api: bool,
}

pub fn app_with_options(
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    responses_api: bool,
) -> Router {
    app_with_features(
        registry,
        monitor,
        AppFeatures {
            responses_api,
            images_api: false,
        },
    )
}

pub fn app_with_features(
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    features: AppFeatures,
) -> Router {
    let native_responses = features
        .responses_api
        .then(|| Arc::new(CodexNativeBackend::new()));
    let chat_completions = features
        .responses_api
        .then(|| Arc::new(ChatCompletionsBackend::new()));
    let images = if features.images_api {
        match CodexImagesBackend::new() {
            Ok(backend) => Some(Arc::new(backend)),
            Err(error) => {
                create_logger("server").warn(
                    "codex images backend disabled by invalid configuration",
                    Some(Map::from_iter([("error".to_string(), json!(error))])),
                );
                None
            }
        }
    } else {
        None
    };
    let state = Arc::new(AppState {
        registry,
        monitor,
        native_responses,
        chat_completions,
        images,
    });
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/messages", post(handler_messages))
        .route("/v1/messages/count_tokens", post(handler_count_tokens))
        .route("/v1/models", get(handler_models));
    let router = if features.responses_api {
        router
            .route("/v1/responses", post(handler_responses))
            .route("/v1/chat/completions", post(handler_chat_completions))
    } else {
        router
    };
    let router = if features.images_api {
        router
            .route("/v1/images/generations", post(handler_image_generation))
            .route(
                "/v1/images/edits",
                post(handler_image_edit).layer(DefaultBodyLimit::max(MAX_EDIT_REQUEST_BYTES)),
            )
    } else {
        router
    };
    router.fallback(fallback_handler).with_state(state)
}

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    native_responses: Option<Arc<CodexNativeBackend>>,
    chat_completions: Option<Arc<ChatCompletionsBackend>>,
    images: Option<Arc<CodexImagesBackend>>,
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
struct ModelsQuery {
    limit: Option<usize>,
}

/// Anthropic-shaped model listing so Claude Code's gateway model discovery
/// (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) finds the proxy's models.
/// Claude Code only adds entries whose id starts with `claude` or `anthropic`,
/// so the Anthropic-style aliases are what surface in its `/model` picker;
/// raw provider ids are still listed for other Anthropic-compatible clients.
async fn handler_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ModelsQuery>,
) -> Json<serde_json::Value> {
    let mut data: Vec<Value> = state
        .registry
        .all_supported_models()
        .into_iter()
        .map(|(model, provider)| {
            json!({
                "type": "model",
                "id": model,
                "display_name": format!("{model} ({provider})"),
            })
        })
        .collect();
    let has_more = query.limit.is_some_and(|limit| data.len() > limit);
    if let Some(limit) = query.limit {
        data.truncate(limit);
    }
    Json(json!({
        "data": data,
        "has_more": has_more,
        "first_id": data.first().and_then(|entry| entry.get("id")).cloned(),
        "last_id": data.last().and_then(|entry| entry.get("id")).cloned(),
    }))
}

async fn handler_messages(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    dispatch_request(state, req, false).await
}

async fn handler_count_tokens(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    dispatch_request(state, req, true).await
}

async fn handler_image_generation(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    dispatch_image_request(state, req, ImageOperation::Generation).await
}

async fn handler_image_edit(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    dispatch_image_request(state, req, ImageOperation::Edit).await
}

async fn dispatch_image_request(
    state: Arc<AppState>,
    req: Request<Body>,
    operation: ImageOperation,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    log.info(
        "request",
        Some(Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!("POST")),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(redacted_query(&uri))),
        ])),
    );
    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, EndpointKind::Images);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());

    if uri.query().is_some() {
        let response = image_error_response(ImageRequestError {
            status: StatusCode::BAD_REQUEST,
            message: "Image endpoints do not accept query parameters".to_string(),
            param: None,
            code: Some("invalid_request"),
        });
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let prepared = if content_type.starts_with("application/json") {
        let limit = match operation {
            ImageOperation::Generation => MAX_GENERATION_REQUEST_BYTES,
            ImageOperation::Edit => MAX_EDIT_REQUEST_BYTES,
        };
        let body = match axum::body::to_bytes(req.into_body(), limit).await {
            Ok(body) => body,
            Err(_) => {
                let response = image_error_response(ImageRequestError {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    message: "Image request exceeded the size limit".to_string(),
                    param: None,
                    code: Some("request_too_large"),
                });
                log_native_request_completed(
                    &log,
                    &req_id,
                    operation.label(),
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        prepare_json_request(operation, &body)
    } else if operation == ImageOperation::Edit && content_type.starts_with("multipart/form-data") {
        let multipart = match Multipart::from_request(req, &()).await {
            Ok(multipart) => multipart,
            Err(_) => {
                let response = image_error_response(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Invalid multipart image edit request".to_string(),
                    param: None,
                    code: Some("invalid_multipart"),
                });
                log_native_request_completed(
                    &log,
                    &req_id,
                    operation.label(),
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        parse_multipart_image_edit(multipart).await
    } else {
        let response = image_error_response(ImageRequestError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: if operation == ImageOperation::Edit {
                "Image edit request must use application/json or multipart/form-data".to_string()
            } else {
                "Image generation request must use application/json".to_string()
            },
            param: None,
            code: Some("unsupported_media_type"),
        });
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let response = image_error_response(error);
            log_native_request_completed(
                &log,
                &req_id,
                operation.label(),
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let model = prepared.model.clone();
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.provider_selected(&req_id, "codex", &model, None);
    }
    let context = RequestContext {
        req_id: req_id.clone(),
        session_id,
        session_seq: None,
        provider: "codex".to_string(),
        traffic: None,
        monitor: state.monitor.clone(),
    };
    let response = match state.images.as_ref() {
        Some(backend) => backend.handle(operation, prepared, context).await,
        None => image_error_response(ImageRequestError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Codex Images API is unavailable".to_string(),
            param: None,
            code: Some("images_api_unavailable"),
        }),
    };
    log_native_request_completed(
        &log,
        &req_id,
        operation.label(),
        Some(&model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

async fn parse_multipart_image_edit(
    mut multipart: Multipart,
) -> Result<crate::providers::codex::images::PreparedImageRequest, ImageRequestError> {
    let mut input = MultipartEditInput::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ImageRequestError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid multipart image edit request".to_string(),
            param: None,
            code: Some("invalid_multipart"),
        })?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "image" | "image[]" => {
                let bytes = field.bytes().await.map_err(|_| ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Failed to read uploaded image".to_string(),
                    param: Some("image"),
                    code: Some("invalid_image"),
                })?;
                input.images.push(UploadedImage {
                    bytes, // EFFICIENCY: field.bytes() already returns owned Bytes, avoid to_vec() copy
                });
            }
            "prompt" => {
                input.prompt = Some(multipart_text(field, "prompt").await?);
            }
            "model" => {
                input.model = Some(multipart_text(field, "model").await?);
            }
            "background" => {
                input.background = Some(multipart_text(field, "background").await?);
            }
            "quality" => {
                input.quality = Some(multipart_text(field, "quality").await?);
            }
            "size" => {
                input.size = Some(multipart_text(field, "size").await?);
            }
            "n" => {
                let raw = multipart_text(field, "n").await?;
                input.n = Some(raw.parse::<u8>().map_err(|_| ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "'n' must be an integer between 1 and 10".to_string(),
                    param: Some("n"),
                    code: Some("invalid_request"),
                })?);
            }
            "mask" => {
                return Err(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Image masks are not supported by the Codex image backend".to_string(),
                    param: Some("mask"),
                    code: Some("unsupported_parameter"),
                });
            }
            _ => {
                return Err(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("Unsupported multipart field '{name}'"),
                    param: None,
                    code: Some("unsupported_parameter"),
                });
            }
        }
    }
    prepare_multipart_edit(input)
}

async fn multipart_text(
    field: axum::extract::multipart::Field<'_>,
    param: &'static str,
) -> Result<String, ImageRequestError> {
    field.text().await.map_err(|_| ImageRequestError {
        status: StatusCode::BAD_REQUEST,
        message: format!("Invalid multipart '{param}' field"),
        param: Some(param),
        code: Some("invalid_multipart"),
    })
}

async fn handler_responses(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );

    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, EndpointKind::Responses);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let response = openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid JSON: {error}"),
                None,
                Some("invalid_json"),
            );
            log_native_request_completed(
                &log,
                &req_id,
                "responses",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let body: Value = match parse_native_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            log_native_request_completed(
                &log,
                &req_id,
                "responses",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    if let Err(response) = validate_native_request_model(&body) {
        log_native_request_completed(
            &log,
            &req_id,
            "responses",
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(normalize_incoming_model);
    let effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let now = current_millis();
    let session_state = if let Some(session_id) = session_id.as_deref() {
        session::existing_session(Some(session_id), now)
    } else {
        None
    };
    let current = model.as_deref().and_then(|model| {
        session::record_session_request(
            session_id.as_deref(),
            session_state.as_ref(),
            "codex",
            model,
            now,
        )
    });
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        if let Some(model) = model.as_deref() {
            monitor.provider_selected(&req_id, "codex", model, effort);
        }
    }

    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|state| state.seq),
        provider: Some("codex".to_string()),
        state_dir_override: None,
    })
    .map(Arc::new);
    if let Some(capture) = traffic.as_ref() {
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
        }
        capture.write_json(
            "000-metadata",
            &json!({
                "reqId": &req_id,
                "sessionId": &session_id,
                "sessionSeq": current.as_ref().map(|state| state.seq),
                "kind": "responses",
                "provider": "codex",
                "model": &model,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json("010-openai-responses-request", &body);
    }

    let context = RequestContext {
        req_id: req_id.clone(),
        session_id,
        session_seq: current.map(|state| state.seq),
        provider: "codex".to_string(),
        traffic,
        monitor: state.monitor.clone(),
    };
    let response = match state.native_responses.as_ref() {
        Some(backend) => backend.handle(body, context).await,
        None => openai_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Native Responses API is disabled",
            None,
            None,
        ),
    };
    log_native_request_completed(
        &log,
        &req_id,
        "responses",
        model.as_deref(),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

async fn handler_chat_completions(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );

    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(
            &req_id,
            session_id.clone(),
            None,
            EndpointKind::ChatCompletions,
        );
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let response = openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid JSON: {error}"),
                None,
                Some("invalid_json"),
            );
            log_native_request_completed(
                &log,
                &req_id,
                "chat_completions",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let body = match parse_native_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            log_native_request_completed(
                &log,
                &req_id,
                "chat_completions",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let translated = match translate_request(body.clone()) {
        Ok(translated) => translated,
        Err(error) => {
            let response = error.response();
            log_native_request_completed(
                &log,
                &req_id,
                "chat_completions",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };

    let now = current_millis();
    let session_state = session_id
        .as_deref()
        .and_then(|session_id| session::existing_session(Some(session_id), now));
    let current = session::record_session_request(
        session_id.as_deref(),
        session_state.as_ref(),
        "codex",
        &translated.model,
        now,
    );
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(
            &req_id,
            "codex",
            &translated.model,
            translated.effort.clone(),
        );
    }

    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|state| state.seq),
        provider: Some("codex".to_string()),
        state_dir_override: None,
    })
    .map(Arc::new);
    if let Some(capture) = traffic.as_ref() {
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
        }
        capture.write_json(
            "000-metadata",
            &json!({
                "reqId": &req_id,
                "sessionId": &session_id,
                "sessionSeq": current.as_ref().map(|state| state.seq),
                "kind": "chat_completions",
                "provider": "codex",
                "model": &translated.model,
                "requestedModel": &translated.requested_model,
                "effort": &translated.effort,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json("010-openai-chat-completions-request", &body);
        capture.write_json("020-upstream-request", &translated.upstream);
    }

    let context = RequestContext {
        req_id: req_id.clone(),
        session_id,
        session_seq: current.map(|state| state.seq),
        provider: "codex".to_string(),
        traffic,
        monitor: state.monitor.clone(),
    };
    let response = match state.chat_completions.as_ref() {
        Some(backend) => backend.handle(translated.clone(), context).await,
        None => openai_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Chat Completions API is disabled",
            None,
            None,
        ),
    };
    log_native_request_completed(
        &log,
        &req_id,
        "chat_completions",
        Some(&translated.model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

fn native_session_id(headers: &http::HeaderMap) -> Option<String> {
    [
        "x-claude-code-session-id",
        "session_id",
        "x-client-request-id",
    ]
    .into_iter()
    .find_map(|name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[allow(clippy::result_large_err)]
fn parse_native_json_body(body: &[u8]) -> Result<Value, Response> {
    if body.is_empty() {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid JSON: empty body",
            None,
            Some("invalid_json"),
        ));
    }
    serde_json::from_slice(body).map_err(|error| {
        openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Invalid JSON: {error}"),
            None,
            Some("invalid_json"),
        )
    })
}

fn log_native_request_completed(
    log: &Logger,
    req_id: &str,
    endpoint: &str,
    model: Option<&str>,
    status: StatusCode,
    started_at: Instant,
) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(req_id)),
            ("endpoint".to_string(), json!(endpoint)),
            ("provider".to_string(), json!("codex")),
            ("model".to_string(), json!(model)),
            ("countTokens".to_string(), json!(false)),
            ("status".to_string(), json!(status.as_u16())),
            ("ms".to_string(), json!(started_at.elapsed().as_millis())),
        ])),
    );
}

async fn dispatch_request(
    state: Arc<AppState>,
    req: Request<Body>,
    count_tokens: bool,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    let endpoint = if count_tokens {
        EndpointKind::CountTokens
    } else {
        EndpointKind::Messages
    };
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );
    let session_id = req
        .headers()
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .map(std::string::ToString::to_string);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, endpoint);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let now = current_millis();
    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let response = json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid JSON: {err}"),
            );
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(response.status()),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Invalid JSON"),
            );
            return response;
        }
    };

    let mut body: crate::anthropic::schema::MessagesRequest = match parse_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            let status = response.status();
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    started_at,
                },
                *response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(status),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Invalid JSON"),
            );
            return response;
        }
    };

    if let Some(project) = project::name_from_request(
        body.extra.get("system"),
        body.messages.iter().rev().map(|message| &message.content),
    ) && let Some(monitor) = state.monitor.as_ref()
    {
        monitor.project_resolved(&req_id, project);
    }

    let model = match body.model.as_deref() {
        Some(model) => model,
        None => {
            let response = json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Missing \"model\" in request body. {}",
                    state.registry.unknown_model_message()
                ),
            );
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(response.status()),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Missing model"),
            );
            return response;
        }
    };

    let mut normalized_model = normalize_incoming_model(model);
    body.model = Some(normalized_model.clone());
    let session_state = if let Some(session_id) = session_id.as_deref() {
        session::existing_session(Some(session_id), now)
    } else {
        None
    };
    let session_affinity = session_state
        .as_ref()
        .and_then(|state| state.affinity_provider.as_ref());
    let original_provider = state
        .registry
        .provider_for_model(&normalized_model, session_affinity);
    let configured_auto_review_model = crate::config::auto_review_model();
    let auto_review_route = original_provider.as_ref().and_then(|provider| {
        apply_auto_review_model(
            &mut body,
            count_tokens,
            configured_auto_review_model.as_deref(),
            provider.name(),
        )
    });
    if auto_review_route.is_some() {
        normalized_model = normalize_incoming_model(body.model.as_deref().expect("override model"));
        body.model = Some(normalized_model.clone());
    }

    let provider = if auto_review_route.is_some() {
        state.registry.provider_for_model(&normalized_model, None)
    } else {
        original_provider
    };

    let provider = match provider {
        Some(provider) => provider,
        None => {
            log.warn(
                "unknown model",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), json!(&req_id)),
                    ("model".to_string(), json!(&normalized_model)),
                ])),
            );
            let response = json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Unknown model \"{normalized_model}\". {}",
                    state.registry.unknown_model_message()
                ),
            );
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: Some(&normalized_model),
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: Some(&normalized_model),
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(response.status()),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Unknown model"),
            );
            return response;
        }
    };

    body.bypass_provider_model_override = auto_review_route.is_some() && provider.name() == "codex";

    if let Some(route) = auto_review_route.as_ref() {
        log.info(
            "auto-review route selected",
            Some(Map::from_iter([
                ("reqId".to_string(), json!(&req_id)),
                ("requestedModel".to_string(), json!(&route.requested_model)),
                ("overrideModel".to_string(), json!(&route.override_model)),
                ("provider".to_string(), json!(provider.name())),
            ])),
        );
    }

    if !count_tokens
        && auto_review_route.is_none()
        && provider.name() != "codex"
        && let Some(session_id) = session_id.as_deref()
    {
        crate::providers::codex::clear_session_compaction(session_id);
    }

    let effort = crate::providers::translate_shared::read_effort(&body)
        .ok()
        .flatten()
        .map(str::to_string);
    let current = session::record_session_request_with_affinity_update(
        session_id.as_deref(),
        session_state.as_ref(),
        provider.name(),
        &normalized_model,
        auto_review_route.is_none(),
        now,
    );
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &normalized_model, effort);
    }

    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|s| s.seq),
        provider: Some(provider.name().to_string()),
        state_dir_override: None,
    })
    .map(Arc::new);

    if let Some(capture) = traffic.as_ref() {
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
        }
        capture.write_json(
            "000-metadata",
            &json!({
                "reqId": &req_id,
                "sessionId": &session_id,
                "sessionSeq": current.as_ref().map(|s| s.seq),
                "kind": if count_tokens { "count_tokens" } else { "messages" },
                "provider": provider.name(),
                "model": &normalized_model,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json(
            "010-anthropic-request",
            &serde_json::to_value(&body).unwrap_or_else(|_| json!({})),
        );
    }

    let context = RequestContext {
        req_id: req_id.clone(),
        session_id,
        session_seq: current.map(|s| s.seq),
        provider: provider.name().to_string(),
        traffic,
        monitor: state.monitor.clone(),
    };

    let response = if count_tokens {
        provider.handle_count_tokens(body, context).await
    } else {
        provider.handle_messages(body, context).await
    };
    log_request_completed(
        &log,
        RequestLogContext {
            req_id: &req_id,
            provider: Some(provider.name()),
            model: Some(&normalized_model),
            count_tokens,
            status: response.status(),
            started_at,
        },
    );
    let status = response.status();
    if status.is_success() {
        return monitor_response_body(response, request_guard);
    }

    let (response, details) = record_failed_response(
        &log,
        FailedResponseLogContext {
            req_id: &req_id,
            provider: Some(provider.name()),
            model: Some(&normalized_model),
            count_tokens,
            started_at,
        },
        response,
    )
    .await;
    if let Some(details) = details.as_ref() {
        monitor_failed(
            state.monitor.as_ref(),
            &req_id,
            Some(status),
            details.message.as_str(),
        );
    } else {
        monitor_failed(
            state.monitor.as_ref(),
            &req_id,
            Some(status),
            format!("HTTP {}", status.as_u16()),
        );
    }
    response
}

fn monitor_response_body(response: Response, guard: RequestMonitorGuard) -> Response {
    let status = response.status();
    let outcome = response
        .extensions()
        .get::<NativeResponseOutcome>()
        .cloned();
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold(
        (body, guard, outcome),
        move |(mut body, mut guard, outcome)| async move {
            match body.frame().await {
                Some(Ok(frame)) => Some((Ok(frame), (body, guard, outcome))),
                Some(Err(err)) => {
                    guard.failed(status, err.to_string());
                    Some((Err(err), (body, guard, outcome)))
                }
                None => {
                    if let Some(message) = outcome.as_ref().and_then(NativeResponseOutcome::failure)
                    {
                        guard.failed(status, message);
                    } else if status.is_success() {
                        guard.completed(status);
                    } else {
                        guard.failed(status, format!("HTTP {}", status.as_u16()));
                    }
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::new(StreamBody::new(stream)))
}

struct RequestLogContext<'a> {
    req_id: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    count_tokens: bool,
    status: StatusCode,
    started_at: Instant,
}

fn log_request_completed(log: &Logger, ctx: RequestLogContext<'_>) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(ctx.req_id)),
            ("provider".to_string(), json!(ctx.provider)),
            ("model".to_string(), json!(ctx.model)),
            ("countTokens".to_string(), json!(ctx.count_tokens)),
            ("status".to_string(), json!(ctx.status.as_u16())),
            (
                "ms".to_string(),
                json!(ctx.started_at.elapsed().as_millis()),
            ),
        ])),
    );
}

struct FailedResponseLogContext<'a> {
    req_id: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    count_tokens: bool,
    started_at: Instant,
}

struct FailedResponseDetails {
    message: String,
}

async fn record_failed_response(
    log: &Logger,
    ctx: FailedResponseLogContext<'_>,
    response: Response,
) -> (Response, Option<FailedResponseDetails>) {
    if response.status().is_success() {
        return (response, None);
    }

    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) => {
            log.info(
                "request_failed",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), json!(ctx.req_id)),
                    ("provider".to_string(), json!(ctx.provider)),
                    ("model".to_string(), json!(ctx.model)),
                    ("countTokens".to_string(), json!(ctx.count_tokens)),
                    ("status".to_string(), json!(status.as_u16())),
                    (
                        "ms".to_string(),
                        json!(ctx.started_at.elapsed().as_millis()),
                    ),
                    ("bodyReadError".to_string(), json!(err.to_string())),
                ])),
            );
            return (Response::from_parts(parts, Body::empty()), None);
        }
    };

    let response_body = response_body_value(&bytes);
    let message = error_message_from_response(&response_body)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    let document = json!({
        "reqId": ctx.req_id,
        "provider": ctx.provider,
        "model": ctx.model,
        "countTokens": ctx.count_tokens,
        "status": status.as_u16(),
        "elapsedMs": ctx.started_at.elapsed().as_millis(),
        "message": message,
        "response": response_body,
    });
    let error_file = write_error_capture(ctx.req_id, &redact_error_value(document));

    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), json!(ctx.req_id)),
        ("provider".to_string(), json!(ctx.provider)),
        ("model".to_string(), json!(ctx.model)),
        ("countTokens".to_string(), json!(ctx.count_tokens)),
        ("status".to_string(), json!(status.as_u16())),
        (
            "ms".to_string(),
            json!(ctx.started_at.elapsed().as_millis()),
        ),
        ("message".to_string(), json!(message)),
    ]);
    if let Some(path) = error_file.as_ref() {
        fields.insert("errorFile".to_string(), json!(path.display().to_string()));
    }
    log.info("request_failed", Some(fields));

    (
        Response::from_parts(parts, Body::from(bytes)),
        Some(FailedResponseDetails { message }),
    )
}

fn response_body_value(bytes: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => json!({ "json": value }),
        Err(_) => json!({ "text": String::from_utf8_lossy(bytes) }),
    }
}

fn error_message_from_response(response_body: &Value) -> Option<String> {
    response_body
        .get("json")
        .and_then(|body| body.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            response_body
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .map(std::string::ToString::to_string)
}

fn write_error_capture(req_id: &str, document: &Value) -> Option<PathBuf> {
    let dir = crate::paths::state_dir().join("errors");
    fs::create_dir_all(&dir).ok()?;
    set_mode(&dir, 0o700);
    let path = dir.join(format!(
        "{}-{}.json",
        current_millis(),
        sanitize_path_part(req_id)
    ));
    let mut file = File::create(&path).ok()?;
    set_mode(&path, 0o600);
    let payload = serde_json::to_vec_pretty(document).ok()?;
    file.write_all(&payload).ok()?;
    file.write_all(b"\n").ok()?;
    Some(path)
}

fn sanitize_path_part(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn redact_error_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(redact_error_value).collect()),
        Value::Object(fields) => {
            let mut out = Map::new();
            for (key, value) in fields {
                if REDACT_KEYS.contains(&key.to_lowercase().as_str()) {
                    out.insert(key, redact_error_key(value));
                } else {
                    out.insert(key, redact_error_value(value));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

fn redact_error_key(value: Value) -> Value {
    match value {
        Value::String(value) => Value::String(format!("[redacted len={}]", value.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

struct RequestMonitorGuard {
    monitor: Option<MonitorHandle>,
    req_id: String,
}

impl RequestMonitorGuard {
    fn new(monitor: Option<MonitorHandle>, req_id: String) -> Self {
        Self { monitor, req_id }
    }

    fn completed(&mut self, status: StatusCode) {
        if let Some(monitor) = self.monitor.take() {
            monitor.request_completed(&self.req_id, status.as_u16(), None, None);
        }
    }

    fn failed(&mut self, status: StatusCode, error: String) {
        if let Some(monitor) = self.monitor.take() {
            monitor.request_failed(&self.req_id, Some(status.as_u16()), error);
        }
    }
}

impl Drop for RequestMonitorGuard {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.request_abandoned(&self.req_id, "Request future ended before completion");
        }
    }
}

fn monitor_failed(
    monitor: Option<&MonitorHandle>,
    req_id: &str,
    status: Option<StatusCode>,
    error: impl Into<String>,
) {
    if let Some(monitor) = monitor {
        monitor.request_failed(req_id, status.map(|status| status.as_u16()), error);
    }
}

fn headers_to_record(headers: &http::HeaderMap) -> Value {
    let mut out = Map::new();
    for (key, value) in headers {
        if let Ok(raw) = value.to_str() {
            out.insert(key.as_str().to_string(), Value::String(raw.to_string()));
        }
    }
    Value::Object(out)
}

fn redacted_query(uri: &http::Uri) -> Value {
    let mut out = Map::new();
    let Some(query) = uri.query() else {
        return Value::Object(out);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key = key.into_owned();
        let lower = key.to_lowercase();
        let value = if REDACT_KEYS.contains(&lower.as_str()) {
            Value::String(format!("[redacted len={}]", value.len()))
        } else {
            Value::String(value.into_owned())
        };
        out.insert(key, value);
    }
    Value::Object(out)
}

fn parse_json_body<T>(body: &[u8]) -> Result<T, Box<Response>>
where
    T: DeserializeOwned,
{
    if body.is_empty() {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid JSON: empty body",
        )));
    }

    serde_json::from_slice::<T>(body).map_err(|err| {
        Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Invalid JSON: {err}"),
        ))
    })
}

async fn fallback_handler(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("No route for {method} {}", uri.path()),
    )
}

fn current_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[allow(dead_code)]
fn _unused(session_state: Option<&SessionState>) {
    let _ = session_state;
}

#[cfg(test)]
mod auto_review_tests {
    use super::{apply_auto_review_model, is_claude_auto_review_request};
    use crate::anthropic::schema::MessagesRequest;
    use serde_json::json;

    fn request(system: &str, stream: bool, tools: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "max_tokens": 2112,
            "stream": stream,
            "system": [{"type": "text", "text": system}],
            "messages": [{"role": "user", "content": "review this Bash command"}],
            "tools": tools
        }))
        .unwrap()
    }

    #[test]
    fn detects_claude_auto_review_classifier() {
        let body = request(
            "You are a security monitor for autonomous AI coding agents.\n\n## Context",
            false,
            json!([]),
        );
        assert!(is_claude_auto_review_request(&body));
    }

    #[test]
    fn ignores_normal_streaming_and_tool_using_requests() {
        assert!(!is_claude_auto_review_request(&request(
            "You are an interactive coding agent.",
            false,
            json!([]),
        )));
        assert!(!is_claude_auto_review_request(&request(
            "You are a security monitor for autonomous AI coding agents.",
            true,
            json!([]),
        )));
        assert!(!is_claude_auto_review_request(&request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([{"name": "Bash"}]),
        )));
    }

    #[test]
    fn codex_classifier_defaults_to_luna() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        let route = apply_auto_review_model(&mut classifier, false, None, "codex")
            .expect("classifier should be routed");
        assert_eq!(route.requested_model, "gpt-5.6-sol");
        assert_eq!(route.override_model, "gpt-5.6-luna");
        assert_eq!(classifier.model.as_deref(), Some("gpt-5.6-luna"));
    }

    #[test]
    fn non_codex_classifier_keeps_requested_model_without_override() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        classifier.model = Some("kimi-for-coding".to_string());
        assert!(apply_auto_review_model(&mut classifier, false, None, "kimi").is_none());
        assert_eq!(classifier.model.as_deref(), Some("kimi-for-coding"));
    }

    #[test]
    fn configured_model_overrides_provider_default() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        let route = apply_auto_review_model(&mut classifier, false, Some("grok-4.5"), "codex")
            .expect("configured classifier should be routed");
        assert_eq!(route.override_model, "grok-4.5");
        assert_eq!(classifier.model.as_deref(), Some("grok-4.5"));
    }

    #[test]
    fn count_tokens_keeps_requested_model() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        assert!(
            apply_auto_review_model(&mut classifier, true, Some("grok-4.5"), "codex").is_none()
        );
        assert_eq!(classifier.model.as_deref(), Some("gpt-5.6-sol"));
    }
}
