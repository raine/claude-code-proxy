use crate::{
    anthropic::json_error,
    logging::{Logger, REDACT_KEYS, create_logger},
    monitor::{EndpointKind, MonitorHandle},
    project,
    provider::{Provider, RequestByteLease, RequestContext},
    registry::{Registry, normalize_incoming_model},
    session,
    traffic::{TrafficCaptureOptions, create_traffic_capture_async},
};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_MAX_BUFFERED_REQUEST_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 64;
pub const DEFAULT_MAX_CONCURRENT_PER_PROVIDER: usize = 48;
pub const DEFAULT_MAX_CONCURRENT_PER_SESSION: usize = 24;
pub const DEFAULT_REQUEST_BODY_IDLE_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_REQUEST_BODY_TOTAL_TIMEOUT_MS: u64 = 30_000;
const MAX_ERROR_RESPONSE_BODY_BYTES: usize = 64 * 1024;
const ERROR_RESPONSE_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_RESPONSE_BODY_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_CAPTURE_FILES: usize = 128;
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const COMPACTION_MODEL_HEADER: &str = "x-ccproxy-compaction-model";
const CODEX_XHIGH_AS_MAX_HEADER: &str = "x-ccproxy-codex-xhigh-as-max";

fn compaction_model_override(
    headers: &HeaderMap,
    request: &crate::anthropic::schema::MessagesRequest,
) -> Option<String> {
    if !crate::providers::translate_shared::is_claude_code_compaction_request(request) {
        return None;
    }
    headers
        .get(COMPACTION_MODEL_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

fn promote_codex_xhigh_as_max(
    headers: &HeaderMap,
    request: &mut crate::anthropic::schema::MessagesRequest,
    provider: &str,
    count_tokens: bool,
) -> bool {
    if count_tokens
        || provider != "codex"
        || headers
            .get(CODEX_XHIGH_AS_MAX_HEADER)
            .is_none_or(|value| value.as_bytes() != b"1")
        || crate::providers::translate_shared::is_claude_code_compaction_request(request)
    {
        return false;
    }

    let Some(Value::Object(output_config)) = request.extra.get_mut("output_config") else {
        return false;
    };
    if output_config.get("effort").and_then(Value::as_str) != Some("xhigh") {
        return false;
    }

    output_config.insert("effort".to_string(), Value::String("max".to_string()));
    true
}

#[derive(Debug, Clone)]
pub struct ServerLimits {
    pub max_request_body_bytes: usize,
    pub max_buffered_request_bytes: usize,
    pub max_concurrent_requests: usize,
    pub max_concurrent_per_provider: usize,
    pub max_concurrent_per_session: usize,
    pub request_body_idle_timeout: Duration,
    pub request_body_total_timeout: Duration,
}

impl ServerLimits {
    pub fn configured() -> Self {
        Self {
            max_request_body_bytes: crate::config::max_request_body_bytes(
                DEFAULT_MAX_REQUEST_BODY_BYTES,
            ),
            max_buffered_request_bytes: crate::config::max_buffered_request_bytes(
                DEFAULT_MAX_BUFFERED_REQUEST_BYTES,
            ),
            max_concurrent_requests: crate::config::max_concurrent_requests(
                DEFAULT_MAX_CONCURRENT_REQUESTS,
            ),
            max_concurrent_per_provider: crate::config::max_concurrent_per_provider(
                DEFAULT_MAX_CONCURRENT_PER_PROVIDER,
            ),
            max_concurrent_per_session: crate::config::max_concurrent_per_session(
                DEFAULT_MAX_CONCURRENT_PER_SESSION,
            ),
            request_body_idle_timeout: Duration::from_millis(
                crate::config::request_body_idle_timeout_ms(DEFAULT_REQUEST_BODY_IDLE_TIMEOUT_MS),
            ),
            request_body_total_timeout: Duration::from_millis(
                crate::config::request_body_total_timeout_ms(DEFAULT_REQUEST_BODY_TOTAL_TIMEOUT_MS),
            ),
        }
    }
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            max_buffered_request_bytes: DEFAULT_MAX_BUFFERED_REQUEST_BYTES,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            max_concurrent_per_provider: DEFAULT_MAX_CONCURRENT_PER_PROVIDER,
            max_concurrent_per_session: DEFAULT_MAX_CONCURRENT_PER_SESSION,
            request_body_idle_timeout: Duration::from_millis(DEFAULT_REQUEST_BODY_IDLE_TIMEOUT_MS),
            request_body_total_timeout: Duration::from_millis(
                DEFAULT_REQUEST_BODY_TOTAL_TIMEOUT_MS,
            ),
        }
    }
}

pub struct ServerConfig {
    pub bind_address: String,
    pub port: u16,
    pub monitor: Option<MonitorHandle>,
    pub allow_remote_unauthenticated: bool,
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
    let listener = bind_proxy_listener_with_ack(
        &config.bind_address,
        config.port,
        config.allow_remote_unauthenticated,
    )
    .await?;
    serve_listener(listener, config.monitor, shutdown).await
}

pub async fn bind_proxy_listener(bind_address: &str, port: u16) -> anyhow::Result<TcpListener> {
    bind_proxy_listener_with_ack(bind_address, port, false).await
}

pub async fn bind_proxy_listener_with_ack(
    bind_address: &str,
    port: u16,
    allow_remote_unauthenticated: bool,
) -> anyhow::Result<TcpListener> {
    let ip = bind_address
        .parse::<std::net::IpAddr>()
        .map_err(|err| anyhow::anyhow!("invalid proxy bind address {bind_address:?}: {err}"))?;
    if !ip.is_loopback() && !allow_remote_unauthenticated {
        anyhow::bail!(
            "refusing unauthenticated non-loopback bind address {bind_address:?}; pass \
             --allow-remote-unauthenticated or set \
             CCP_ALLOW_REMOTE_UNAUTHENTICATED=1 only when a firewall or authenticating reverse \
             proxy protects this listener"
        );
    }
    let addr = std::net::SocketAddr::new(ip, port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind proxy listener on {addr}: {err}"))?;
    if !ip.is_loopback() {
        let local_addr = listener.local_addr().map_err(|err| {
            anyhow::anyhow!("failed to inspect bound proxy listener on {addr}: {err}")
        })?;
        create_logger("server").warn(
            "SECURITY WARNING: unauthenticated proxy bound to a non-loopback address",
            Some(serde_json::Map::from_iter([
                (
                    "bindAddress".to_string(),
                    json!(local_addr.ip().to_string()),
                ),
                ("port".to_string(), json!(local_addr.port())),
                ("remoteUnauthenticated".to_string(), json!(true)),
            ])),
        );
    }
    Ok(listener)
}

pub async fn serve_listener(
    listener: TcpListener,
    monitor: Option<MonitorHandle>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    serve_listener_with_timeout(listener, monitor, shutdown, GRACEFUL_SHUTDOWN_TIMEOUT).await
}

async fn serve_listener_with_timeout(
    listener: TcpListener,
    monitor: Option<MonitorHandle>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    graceful_timeout: Duration,
) -> anyhow::Result<()> {
    initialize_process_identity();
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
    let limits = ServerLimits::configured();
    initialize_config_fingerprint(
        &limits,
        Some(&local_addr.ip().to_string()),
        Some(local_addr.port()),
    );
    let app = app_with_limits(Arc::new(Registry::with_default_alias()), monitor, limits);
    let (shutdown_started_tx, shutdown_started_rx) = tokio::sync::oneshot::channel();
    let graceful_shutdown = async move {
        shutdown.await;
        let _ = shutdown_started_tx.send(());
    };
    let server = axum::serve(listener, app).with_graceful_shutdown(graceful_shutdown);
    let result = await_server_with_grace_timeout(
        server.into_future(),
        shutdown_started_rx,
        graceful_timeout,
    )
    .await;
    let _ = crate::logging::flush(Duration::from_secs(2));
    result
}

async fn await_server_with_grace_timeout(
    server: impl Future<Output = std::io::Result<()>>,
    shutdown_started: tokio::sync::oneshot::Receiver<()>,
    graceful_timeout: Duration,
) -> anyhow::Result<()> {
    let timeout_after_shutdown = async move {
        if shutdown_started.await.is_ok() {
            tokio::time::sleep(graceful_timeout).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(server);
    tokio::pin!(timeout_after_shutdown);
    tokio::select! {
        biased;
        result = &mut server => result.map_err(anyhow::Error::from),
        () = &mut timeout_after_shutdown => Err(anyhow::anyhow!(
            "server graceful shutdown exceeded {} ms",
            graceful_timeout.as_millis()
        )),
    }
}

pub fn app(registry: Arc<Registry>) -> Router {
    app_with_monitor(registry, None)
}

pub fn app_with_monitor(registry: Arc<Registry>, monitor: Option<MonitorHandle>) -> Router {
    app_with_limits(registry, monitor, ServerLimits::default())
}

pub fn app_with_limits(
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    limits: ServerLimits,
) -> Router {
    initialize_process_identity();
    initialize_config_fingerprint(&limits, None, None);
    let state = Arc::new(AppState {
        registry,
        monitor,
        admission: AdmissionState::new(&limits),
        limits,
    });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/v1/models", get(models))
        .route("/v1/messages", post(handler_messages))
        .route("/v1/messages/count_tokens", post(handler_count_tokens))
        .fallback(fallback_handler)
        .with_state(state)
}

struct AppState {
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    admission: AdmissionState,
    limits: ServerLimits,
}

struct AdmissionState {
    global: Arc<Semaphore>,
    request_bytes: Arc<Semaphore>,
    providers: Mutex<HashMap<String, Arc<Semaphore>>>,
    sessions: Mutex<HashMap<String, Weak<Semaphore>>>,
    per_provider: usize,
    per_session: usize,
}

impl AdmissionState {
    fn new(limits: &ServerLimits) -> Self {
        Self {
            global: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
            request_bytes: Arc::new(Semaphore::new(limits.max_buffered_request_bytes)),
            providers: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            per_provider: limits.max_concurrent_per_provider,
            per_session: limits.max_concurrent_per_session,
        }
    }

    fn provider(&self, provider: &str) -> Arc<Semaphore> {
        let mut providers = self.providers.lock().expect("provider admission lock");
        providers
            .entry(provider.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_provider)))
            .clone()
    }

    fn session(&self, session_id: &str) -> Arc<Semaphore> {
        let mut sessions = self.sessions.lock().expect("session admission lock");
        if let Some(semaphore) = sessions.get(session_id).and_then(Weak::upgrade) {
            return semaphore;
        }
        if sessions.len() >= session::MAX_SESSIONS {
            sessions.retain(|_, semaphore| semaphore.strong_count() > 0);
        }
        let semaphore = Arc::new(Semaphore::new(self.per_session));
        sessions.insert(session_id.to_string(), Arc::downgrade(&semaphore));
        semaphore
    }

    fn acquire(&self, semaphore: Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
        semaphore.try_acquire_owned().ok()
    }
}

#[derive(Default)]
struct RequestPermits {
    global: Option<OwnedSemaphorePermit>,
    provider: Option<OwnedSemaphorePermit>,
    session: Option<OwnedSemaphorePermit>,
    request_bytes: Option<OwnedSemaphorePermit>,
}

enum ProviderRouteSelection {
    Admitted {
        provider: Arc<dyn Provider>,
        permit: OwnedSemaphorePermit,
    },
    Saturated(&'static str),
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn version() -> Json<serde_json::Value> {
    Json(version_info())
}

async fn models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let created_at = model_catalog_created_at();
    let mut catalog = state.registry.all_supported_models();
    catalog.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    catalog.dedup_by(|left, right| left.0 == right.0);

    let first_id = catalog.first().map(|(model, _)| model.clone());
    let last_id = catalog.last().map(|(model, _)| model.clone());
    let data: Vec<Value> = catalog
        .into_iter()
        .map(|(model, _provider)| {
            let display_name = model.clone();
            json!({
                "type": "model",
                "id": model,
                "display_name": display_name,
                "created_at": created_at.clone(),
            })
        })
        .collect();

    Json(json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    }))
}

fn model_catalog_created_at() -> String {
    let timestamp = env!("CCPROXY_BUILD_UNIX_EPOCH")
        .parse::<i64>()
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

struct ProcessIdentity {
    started_at_ms: u64,
    path: Option<String>,
    sha256: Option<String>,
}

static PROCESS_IDENTITY: OnceLock<ProcessIdentity> = OnceLock::new();
static CONFIG_FINGERPRINT: OnceLock<String> = OnceLock::new();
static ERROR_CAPTURE_GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();

// Cache the executable hash before serving. Re-reading current_exe for each
// request can hash a newly overwritten Cellar path while an old PID is alive.
pub fn initialize_process_identity() {
    PROCESS_IDENTITY.get_or_init(|| {
        let started_at_ms = current_millis();
        let executable = std::env::current_exe().ok();
        let sha256 = executable.as_deref().and_then(hash_file_sha256);
        ProcessIdentity {
            started_at_ms,
            path: executable.map(|path| path.display().to_string()),
            sha256,
        }
    });
}

fn hash_file_sha256(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Some(hex::encode(digest.finalize()))
}

fn initialize_config_fingerprint(
    limits: &ServerLimits,
    bind_address: Option<&str>,
    port: Option<u16>,
) {
    CONFIG_FINGERPRINT.get_or_init(|| {
        let config = crate::config::load_config();
        effective_config_fingerprint(
            limits,
            bind_address.unwrap_or(config.bind_address.as_str()),
            port.unwrap_or(config.port),
        )
    });
}

fn effective_config_fingerprint(limits: &ServerLimits, bind_address: &str, port: u16) -> String {
    let values = [
        format!("bindAddress={bind_address}"),
        format!("port={port}"),
        format!("maxRequestBodyBytes={}", limits.max_request_body_bytes),
        format!(
            "maxBufferedRequestBytes={}",
            limits.max_buffered_request_bytes
        ),
        format!("maxConcurrentRequests={}", limits.max_concurrent_requests),
        format!(
            "maxConcurrentPerProvider={}",
            limits.max_concurrent_per_provider
        ),
        format!(
            "maxConcurrentPerSession={}",
            limits.max_concurrent_per_session
        ),
        format!(
            "requestBodyIdleTimeoutMs={}",
            limits.request_body_idle_timeout.as_millis()
        ),
        format!(
            "requestBodyTotalTimeoutMs={}",
            limits.request_body_total_timeout.as_millis()
        ),
    ];
    hex::encode(Sha256::digest(values.join("\n").as_bytes()))
}

fn process_identity() -> &'static ProcessIdentity {
    initialize_process_identity();
    PROCESS_IDENTITY
        .get()
        .expect("process identity initialized")
}

pub fn version_info() -> Value {
    let identity = process_identity();
    initialize_config_fingerprint(&ServerLimits::configured(), None, None);
    let config_fingerprint = CONFIG_FINGERPRINT
        .get()
        .expect("configuration fingerprint initialized");

    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "gitSha": env!("CCPROXY_GIT_SHA"),
        "gitDirty": env!("CCPROXY_GIT_DIRTY") == "true",
        "buildTimestamp": env!("CCPROXY_BUILD_UNIX_EPOCH").parse::<u64>().ok(),
        "pid": std::process::id(),
        "startedAtMs": identity.started_at_ms,
        "executable": identity.path.as_deref(),
        "binarySha256": identity.sha256.as_deref(),
        "configFingerprint": config_fingerprint,
        "configFingerprintScope": "server-routing",
        "capabilities": {
            "codexOutputBudget": "unsupported_by_private_gateway",
            "codexZeroMaxTokens": "rejected_before_dispatch",
            "codexStructuredOutput": "validated_against_original_schema",
            "codexUnconfirmedToolCall": "fail_closed_by_default",
            "nonEmptyStopSequences": "rejected_before_dispatch",
            "samplingControls": "rejected_before_dispatch",
        },
    })
}

async fn handler_messages(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    dispatch_request(state, req, false).await
}

async fn handler_count_tokens(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    dispatch_request(state, req, true).await
}

struct BufferedRequestBody {
    bytes: Bytes,
    permit: Option<OwnedSemaphorePermit>,
}

enum RequestBodyReadError {
    TooLarge,
    ByteBudgetSaturated,
    TimedOut,
    Read(String),
}

async fn read_bounded_request_body(
    mut body: Body,
    request_limit: usize,
    byte_budget: Arc<Semaphore>,
    idle_timeout: Duration,
    total_timeout: Duration,
) -> Result<BufferedRequestBody, RequestBodyReadError> {
    let total_deadline = tokio::time::Instant::now() + total_timeout;
    let mut bytes = Vec::new();
    let mut byte_permit: Option<OwnedSemaphorePermit> = None;
    loop {
        let frame = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(total_deadline) => {
                return Err(RequestBodyReadError::TimedOut);
            }
            result = tokio::time::timeout(idle_timeout, body.frame()) => {
                match result {
                    Ok(frame) => frame,
                    Err(_) => return Err(RequestBodyReadError::TimedOut),
                }
            }
        };
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(|error| RequestBodyReadError::Read(error.to_string()))?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        if bytes.len().saturating_add(data.len()) > request_limit {
            return Err(RequestBodyReadError::TooLarge);
        }
        if !data.is_empty() {
            let amount = u32::try_from(data.len()).map_err(|_| RequestBodyReadError::TooLarge)?;
            let permit = byte_budget
                .clone()
                .try_acquire_many_owned(amount)
                .map_err(|_| RequestBodyReadError::ByteBudgetSaturated)?;
            if let Some(existing) = byte_permit.as_mut() {
                existing.merge(permit);
            } else {
                byte_permit = Some(permit);
            }
            bytes.extend_from_slice(data);
        }
    }
    Ok(BufferedRequestBody {
        bytes: Bytes::from(bytes),
        permit: byte_permit,
    })
}

async fn dispatch_request(
    state: Arc<AppState>,
    req: Request<Body>,
    count_tokens: bool,
) -> Response {
    let req_id = Uuid::new_v4().to_string();
    let mut response = dispatch_request_with_id(state, req, count_tokens, req_id.clone()).await;
    if let Ok(value) = http::HeaderValue::from_str(&req_id) {
        response
            .headers_mut()
            .insert(http::HeaderName::from_static("request-id"), value);
    }
    response
}

async fn dispatch_request_with_id(
    state: Arc<AppState>,
    req: Request<Body>,
    count_tokens: bool,
    req_id: String,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
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
    let mut request_guard = RequestMonitorGuard::new(
        state.monitor.clone(),
        req_id.clone(),
        log.clone(),
        started_at,
        count_tokens,
    );
    if headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > state.limits.max_request_body_bytes as u64)
    {
        return request_body_too_large(
            &log,
            &mut request_guard,
            &req_id,
            count_tokens,
            started_at,
            state.limits.max_request_body_bytes,
        )
        .await;
    }

    let mut permits = RequestPermits::default();
    if let Some(session_id) = session_id.as_deref() {
        permits.session = state.admission.acquire(state.admission.session(session_id));
        if permits.session.is_none() {
            return admission_rejection(
                &log,
                &mut request_guard,
                &req_id,
                None,
                None,
                count_tokens,
                started_at,
                "session request limit is saturated",
            )
            .await;
        }
    }
    permits.global = state.admission.acquire(state.admission.global.clone());
    if permits.global.is_none() {
        return admission_rejection(
            &log,
            &mut request_guard,
            &req_id,
            None,
            None,
            count_tokens,
            started_at,
            "global request limit is saturated",
        )
        .await;
    }

    let body_bytes = match read_bounded_request_body(
        req.into_body(),
        state.limits.max_request_body_bytes,
        state.admission.request_bytes.clone(),
        state.limits.request_body_idle_timeout,
        state.limits.request_body_total_timeout,
    )
    .await
    {
        Ok(buffered) => {
            permits.request_bytes = buffered.permit;
            buffered.bytes
        }
        Err(RequestBodyReadError::TooLarge) => {
            return request_body_too_large(
                &log,
                &mut request_guard,
                &req_id,
                count_tokens,
                started_at,
                state.limits.max_request_body_bytes,
            )
            .await;
        }
        Err(RequestBodyReadError::ByteBudgetSaturated) => {
            return admission_rejection(
                &log,
                &mut request_guard,
                &req_id,
                None,
                None,
                count_tokens,
                started_at,
                "global buffered request byte limit is saturated",
            )
            .await;
        }
        Err(RequestBodyReadError::TimedOut) => {
            let response = json_error(
                StatusCode::REQUEST_TIMEOUT,
                "invalid_request_error",
                "Request body exceeded its read timeout",
            );
            return finalize_immediate_failure(
                &log,
                &mut request_guard,
                &req_id,
                None,
                None,
                count_tokens,
                started_at,
                response,
            )
            .await;
        }
        Err(RequestBodyReadError::Read(error)) => {
            let response = json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid request body: {error}"),
            );
            return finalize_immediate_failure(
                &log,
                &mut request_guard,
                &req_id,
                None,
                None,
                count_tokens,
                started_at,
                response,
            )
            .await;
        }
    };

    let now = current_millis();

    let mut body: crate::anthropic::schema::MessagesRequest = match parse_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            let status = response.status();
            log_response_started(
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
            request_guard.failed(
                status,
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Invalid JSON")
                    .to_string(),
            );
            return response;
        }
    };

    if !count_tokens {
        log_client_tool_results(&log, &req_id, &body);
    }

    if let Some(monitor) = state.monitor.as_ref()
        && let Some(project) = project::name_from_request(
            body.extra.get("system"),
            body.messages.iter().rev().map(|message| &message.content),
        )
    {
        monitor.project_resolved(&req_id, project);
    }

    let requested_model = match body.model.clone() {
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
            log_response_started(
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
            request_guard.failed(
                response.status(),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Missing model")
                    .to_string(),
            );
            return response;
        }
    };

    let effective_model =
        compaction_model_override(&headers, &body).unwrap_or_else(|| requested_model.clone());
    let normalized_model = normalize_incoming_model(&effective_model);
    if normalized_model != normalize_incoming_model(&requested_model) {
        log.info(
            "internal request model override",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), json!(&req_id)),
                ("reason".to_string(), json!("claude_code_compaction")),
                ("requestedModel".to_string(), json!(&requested_model)),
                ("model".to_string(), json!(&normalized_model)),
            ])),
        );
    }
    request_guard.set_route(None, Some(&normalized_model));
    body.model = Some(normalized_model.clone());
    let (selection, current) =
        session::route_session_request(session_id.as_deref(), &normalized_model, now, |affinity| {
            state
                .registry
                .provider_for_model(&normalized_model, affinity)
                .map(|provider| {
                    let provider_name = provider.name();
                    match state
                        .admission
                        .acquire(state.admission.provider(provider_name))
                    {
                        Some(permit) => session::SessionRoute::new(
                            ProviderRouteSelection::Admitted { provider, permit },
                            provider_name,
                        ),
                        None => session::SessionRoute::without_commit(
                            ProviderRouteSelection::Saturated(provider_name),
                        ),
                    }
                })
        });

    let selection = match selection {
        Some(selection) => selection,
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
            log_response_started(
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
            request_guard.failed(
                response.status(),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Unknown model")
                    .to_string(),
            );
            return response;
        }
    };
    let provider = match selection {
        ProviderRouteSelection::Admitted { provider, permit } => {
            permits.provider = Some(permit);
            provider
        }
        ProviderRouteSelection::Saturated(provider_name) => {
            return admission_rejection(
                &log,
                &mut request_guard,
                &req_id,
                Some(provider_name),
                Some(&normalized_model),
                count_tokens,
                started_at,
                "provider request limit is saturated",
            )
            .await;
        }
    };
    if promote_codex_xhigh_as_max(&headers, &mut body, provider.name(), count_tokens) {
        log.info(
            "internal request effort override",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), json!(&req_id)),
                ("reason".to_string(), json!("codex_xhigh_as_max")),
                ("from".to_string(), json!("xhigh")),
                ("to".to_string(), json!("max")),
            ])),
        );
    }
    let effort = crate::providers::translate_shared::read_effort(&body)
        .ok()
        .flatten()
        .map(str::to_string);
    request_guard.set_route(Some(provider.name()), Some(&normalized_model));
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &normalized_model, effort);
    }

    let traffic = create_traffic_capture_async(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|s| s.seq),
        provider: Some(provider.name().to_string()),
        state_dir_override: None,
    })
    .await
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

    let request_byte_lease = permits
        .request_bytes
        .take()
        .map(|permit| RequestByteLease::new(permit, body_bytes.len()));
    let context = RequestContext {
        req_id: req_id.clone(),
        session_id,
        session_seq: current.map(|s| s.seq),
        provider: provider.name().to_string(),
        traffic,
        monitor: state.monitor.clone(),
        request_byte_lease: request_byte_lease.clone(),
    };

    let response = if count_tokens {
        provider.handle_count_tokens(body, context).await
    } else {
        provider.handle_messages(body, context).await
    };
    // Release the server's lease after dispatch. A provider may keep its clone
    // only through initial dispatch or replay; long-lived provider state uses
    // a separate bounded budget so request admission is not coupled to stream
    // duration.
    drop(request_byte_lease);
    log_response_started(
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
        return monitor_response_body(
            response,
            request_guard,
            ResponseLogContext {
                log,
                req_id,
                provider: Some(provider.name().to_string()),
                model: Some(normalized_model),
                count_tokens,
                started_at,
            },
            permits,
        );
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
    request_guard.failed(
        status,
        details
            .as_ref()
            .map(|details| details.message.clone())
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16())),
    );
    hold_response_permits(response, permits)
}

#[allow(clippy::too_many_arguments)]
async fn admission_rejection(
    log: &Logger,
    request_guard: &mut RequestMonitorGuard,
    req_id: &str,
    provider: Option<&str>,
    model: Option<&str>,
    count_tokens: bool,
    started_at: Instant,
    reason: &str,
) -> Response {
    let mut response = json_error(
        StatusCode::TOO_MANY_REQUESTS,
        "overloaded_error",
        format!("Proxy is busy: {reason}"),
    );
    response.headers_mut().insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("1"),
    );
    finalize_immediate_failure(
        log,
        request_guard,
        req_id,
        provider,
        model,
        count_tokens,
        started_at,
        response,
    )
    .await
}

async fn request_body_too_large(
    log: &Logger,
    request_guard: &mut RequestMonitorGuard,
    req_id: &str,
    count_tokens: bool,
    started_at: Instant,
    limit: usize,
) -> Response {
    let response = json_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_request_error",
        format!("Request body exceeds the {limit}-byte limit"),
    );
    finalize_immediate_failure(
        log,
        request_guard,
        req_id,
        None,
        None,
        count_tokens,
        started_at,
        response,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finalize_immediate_failure(
    log: &Logger,
    request_guard: &mut RequestMonitorGuard,
    req_id: &str,
    provider: Option<&str>,
    model: Option<&str>,
    count_tokens: bool,
    started_at: Instant,
    response: Response,
) -> Response {
    let status = response.status();
    log_response_started(
        log,
        RequestLogContext {
            req_id,
            provider,
            model,
            count_tokens,
            status,
            started_at,
        },
    );
    let (response, details) = record_failed_response(
        log,
        FailedResponseLogContext {
            req_id,
            provider,
            model,
            count_tokens,
            started_at,
        },
        response,
    )
    .await;
    request_guard.failed(
        status,
        details
            .map(|details| details.message)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16())),
    );
    response
}

fn monitor_response_body(
    response: Response,
    guard: RequestMonitorGuard,
    log_context: ResponseLogContext,
    permits: RequestPermits,
) -> Response {
    let status = response.status();
    let is_event_stream = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    let (parts, body) = response.into_parts();
    let lifecycle = ResponseBodyLifecycle {
        guard,
        log_context,
        status,
        sse_detector: is_event_stream.then(SseErrorDetector::default),
        open_tool_blocks: HashMap::new(),
        provisionally_closed_tool_blocks: HashMap::new(),
        _permits: permits,
        terminal: false,
    };
    let stream = futures_util::stream::unfold(
        (body, lifecycle),
        move |(mut body, mut lifecycle)| async move {
            if lifecycle.terminal {
                return None;
            }
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref()
                        && let Some(error) = lifecycle.detect_sse_error(data)
                    {
                        lifecycle.failed(error, true);
                    }
                    Some((Ok(frame), (body, lifecycle)))
                }
                Some(Err(err)) => {
                    lifecycle.failed(err.to_string(), false);
                    Some((Err(err), (body, lifecycle)))
                }
                None => {
                    lifecycle.completed();
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::new(StreamBody::new(stream)))
}

fn hold_response_permits(response: Response, permits: RequestPermits) -> Response {
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold((body, permits), |(mut body, permits)| async move {
        body.frame().await.map(|frame| (frame, (body, permits)))
    });
    Response::from_parts(parts, Body::new(StreamBody::new(stream)))
}

struct ResponseLogContext {
    log: Logger,
    req_id: String,
    provider: Option<String>,
    model: Option<String>,
    count_tokens: bool,
    started_at: Instant,
}

struct ResponseBodyLifecycle {
    guard: RequestMonitorGuard,
    log_context: ResponseLogContext,
    status: StatusCode,
    sse_detector: Option<SseErrorDetector>,
    open_tool_blocks: HashMap<u64, ToolBlockTrace>,
    provisionally_closed_tool_blocks: HashMap<u64, ToolBlockTrace>,
    _permits: RequestPermits,
    terminal: bool,
}

impl ResponseBodyLifecycle {
    fn detect_sse_error(&mut self, bytes: &[u8]) -> Option<String> {
        let (error, tool_events) = {
            let detector = self.sse_detector.as_mut()?;
            let error = detector.push(bytes);
            let tool_events = detector.take_tool_events();
            (error, tool_events)
        };
        for event in tool_events {
            self.observe_tool_event(event);
        }
        error
    }

    fn completed(&mut self) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        self.interrupt_open_tools("response_eof");
        self.guard.completed(self.status);
        log_request_completed(&self.log_context, self.status);
    }

    fn failed(&mut self, error: String, in_band_sse: bool) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        self.interrupt_open_tools(if in_band_sse {
            "in_band_sse_error"
        } else {
            "response_body_error"
        });
        self.guard.failed(self.status, error.clone());
        log_stream_failed(&self.log_context, self.status, &error, in_band_sse);
    }

    fn observe_tool_event(&mut self, event: SseToolEvent) {
        match event {
            SseToolEvent::Started(tool) => {
                if let Some(previous) = self.provisionally_closed_tool_blocks.remove(&tool.index) {
                    log_tool_block_event(
                        &self.log_context,
                        "tool_block_interrupted",
                        &previous,
                        Some("index_reused"),
                    );
                }
                if let Some(previous) = self.open_tool_blocks.insert(tool.index, tool.clone()) {
                    log_tool_block_event(
                        &self.log_context,
                        "tool_block_interrupted",
                        &previous,
                        Some("index_reused"),
                    );
                }
                log_tool_block_event(&self.log_context, "tool_block_started", &tool, None);
            }
            SseToolEvent::Stopped { index } => {
                if let Some(tool) = self.open_tool_blocks.remove(&index) {
                    self.provisionally_closed_tool_blocks.insert(index, tool);
                }
            }
            SseToolEvent::MessageStopped => {
                let completed = std::mem::take(&mut self.provisionally_closed_tool_blocks);
                for (_, tool) in completed {
                    log_tool_block_event(&self.log_context, "tool_block_completed", &tool, None);
                }
                self.interrupt_open_tools("message_stop_before_block_stop");
            }
        }
    }

    fn interrupt_open_tools(&mut self, reason: &'static str) {
        let open = std::mem::take(&mut self.open_tool_blocks);
        let provisional = std::mem::take(&mut self.provisionally_closed_tool_blocks);
        for (_, tool) in open.into_iter().chain(provisional) {
            log_tool_block_event(
                &self.log_context,
                "tool_block_interrupted",
                &tool,
                Some(reason),
            );
        }
    }
}

impl Drop for ResponseBodyLifecycle {
    fn drop(&mut self) {
        if !self.terminal {
            self.terminal = true;
            self.interrupt_open_tools("downstream_dropped");
            self.guard.abandoned("Downstream response body was dropped");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolBlockTrace {
    index: u64,
    tool_kind: String,
    tool_name: Option<String>,
    call_id_hash: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum SseToolEvent {
    Started(ToolBlockTrace),
    Stopped { index: u64 },
    MessageStopped,
}

fn log_tool_block_event(
    ctx: &ResponseLogContext,
    message: &'static str,
    tool: &ToolBlockTrace,
    reason: Option<&'static str>,
) {
    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), json!(ctx.req_id)),
        ("provider".to_string(), json!(ctx.provider)),
        ("model".to_string(), json!(ctx.model)),
        ("index".to_string(), json!(tool.index)),
        ("toolKind".to_string(), json!(tool.tool_kind)),
        ("toolName".to_string(), json!(tool.tool_name)),
        ("callIdHash".to_string(), json!(tool.call_id_hash)),
        (
            "elapsedMs".to_string(),
            json!(ctx.started_at.elapsed().as_millis()),
        ),
    ]);
    if let Some(reason) = reason {
        fields.insert("interruptReason".to_string(), json!(reason));
    }
    ctx.log.info(message, Some(fields));
}

fn safe_tool_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(
        value
            .chars()
            .take(128)
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':') {
                    character
                } else {
                    '_'
                }
            })
            .collect(),
    )
}

fn diagnostic_identifier_hash(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(hex::encode(Sha256::digest(value.as_bytes()))[..16].to_string())
}

#[derive(Default)]
struct DiagnosticByteCounter(u64);

impl Write for DiagnosticByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_value_size(value: &Value) -> u64 {
    let mut counter = DiagnosticByteCounter::default();
    if serde_json::to_writer(&mut counter, value).is_ok() {
        counter.0
    } else {
        0
    }
}

fn log_client_tool_results(
    log: &Logger,
    req_id: &str,
    request: &crate::anthropic::schema::MessagesRequest,
) {
    let Some((message_index, message)) = request.messages.iter().enumerate().next_back() else {
        return;
    };
    let Some(blocks) = message.content.as_array() else {
        return;
    };
    for (block_index, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(call_id_hash) = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(diagnostic_identifier_hash)
        else {
            continue;
        };
        let content = block.get("content").unwrap_or(&Value::Null);
        log.info(
            "client_tool_result",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), json!(req_id)),
                ("messageIndex".to_string(), json!(message_index)),
                ("blockIndex".to_string(), json!(block_index)),
                ("callIdHash".to_string(), json!(call_id_hash)),
                (
                    "isError".to_string(),
                    json!(
                        block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    ),
                ),
                (
                    "contentBytes".to_string(),
                    json!(serialized_value_size(content)),
                ),
            ])),
        );
    }
}

fn sse_tool_event(payload: &Value) -> Option<SseToolEvent> {
    match payload.get("type").and_then(Value::as_str)? {
        "content_block_start" => {
            let index = payload.get("index").and_then(Value::as_u64)?;
            let block = payload.get("content_block")?.as_object()?;
            let tool_kind = block.get("type").and_then(Value::as_str)?;
            if !matches!(tool_kind, "tool_use" | "server_tool_use") {
                return None;
            }
            Some(SseToolEvent::Started(ToolBlockTrace {
                index,
                tool_kind: tool_kind.to_string(),
                tool_name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(safe_tool_name),
                call_id_hash: block
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(diagnostic_identifier_hash),
            }))
        }
        "content_block_stop" => Some(SseToolEvent::Stopped {
            index: payload.get("index").and_then(Value::as_u64)?,
        }),
        "message_stop" => Some(SseToolEvent::MessageStopped),
        _ => None,
    }
}

const MAX_SSE_ERROR_EVENT_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct SseErrorDetector {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    event_bytes: usize,
    discard_line: bool,
    discard_event: bool,
    skip_lf: bool,
    tool_events: Vec<SseToolEvent>,
}

impl SseErrorDetector {
    fn push(&mut self, bytes: &[u8]) -> Option<String> {
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                if byte == b'\r' {
                    self.skip_lf = true;
                }
                if self.discard_line {
                    self.discard_line = false;
                    continue;
                }
                if self.discard_event {
                    if let Some(error) = self.finish_event() {
                        return Some(error);
                    }
                    continue;
                }
                let line = String::from_utf8_lossy(&self.line).into_owned();
                self.line.clear();
                if let Some(error) = self.process_line(&line) {
                    return Some(error);
                }
                continue;
            }

            if self.discard_event {
                self.discard_line = true;
                continue;
            }
            self.event_bytes = self.event_bytes.saturating_add(1);
            if self.event_bytes > MAX_SSE_ERROR_EVENT_BYTES {
                self.line.clear();
                self.data.clear();
                self.event = None;
                self.discard_line = true;
                self.discard_event = true;
                continue;
            }
            self.line.push(byte);
        }
        None
    }

    fn take_tool_events(&mut self) -> Vec<SseToolEvent> {
        std::mem::take(&mut self.tool_events)
    }

    fn process_line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            return self.finish_event();
        }
        if self.discard_event || line.starts_with(':') {
            return None;
        }

        let (field, value) = line
            .split_once(':')
            .map(|(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)))
            .unwrap_or((line, ""));
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                self.event_bytes = self
                    .event_bytes
                    .saturating_add(std::mem::size_of::<String>());
                if self.event_bytes > MAX_SSE_ERROR_EVENT_BYTES {
                    self.discard_event = true;
                    self.event = None;
                    self.data.clear();
                } else {
                    self.data.push(value.to_string());
                }
            }
            _ => {}
        }
        None
    }

    fn finish_event(&mut self) -> Option<String> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data).join("\n");
        let discarded = std::mem::take(&mut self.discard_event);
        self.event_bytes = 0;
        if discarded {
            return None;
        }

        let payload = serde_json::from_str::<Value>(&data).ok();
        if let Some(tool_event) = payload.as_ref().and_then(sse_tool_event) {
            self.tool_events.push(tool_event);
        }
        let is_error = event.as_deref() == Some("error")
            || payload
                .as_ref()
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("error");
        if !is_error {
            return None;
        }

        Some(
            payload
                .as_ref()
                .and_then(|value| value.pointer("/error/message"))
                .and_then(Value::as_str)
                .or_else(|| {
                    payload
                        .as_ref()
                        .and_then(|value| value.get("message"))
                        .and_then(Value::as_str)
                })
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("Upstream stream returned an error event")
                .to_string(),
        )
    }
}

struct RequestLogContext<'a> {
    req_id: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    count_tokens: bool,
    status: StatusCode,
    started_at: Instant,
}

fn log_response_started(log: &Logger, ctx: RequestLogContext<'_>) {
    log.info(
        "response_started",
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

fn response_log_fields(
    ctx: &ResponseLogContext,
    status: StatusCode,
    extra: Option<(&str, Value)>,
) -> serde_json::Map<String, Value> {
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
    ]);
    if let Some((key, value)) = extra {
        fields.insert(key.to_string(), value);
    }
    fields
}

fn log_request_completed(ctx: &ResponseLogContext, status: StatusCode) {
    ctx.log.info(
        "request_completed",
        Some(response_log_fields(ctx, status, None)),
    );
}

fn log_stream_failed(ctx: &ResponseLogContext, status: StatusCode, error: &str, in_band_sse: bool) {
    let mut fields = response_log_fields(ctx, status, Some(("message", json!(error))));
    fields.insert("phase".to_string(), json!("response_body"));
    fields.insert("inBandSse".to_string(), json!(in_band_sse));
    ctx.log.info("request_failed", Some(fields));
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
    let (mut parts, body) = response.into_parts();
    let read = read_bounded_error_body(body).await;
    let response_body = response_body_value(&read.bytes);
    let message = error_message_from_response(&response_body).unwrap_or_else(|| {
        if read.timed_out {
            "Upstream error response body timed out".to_string()
        } else if read.truncated {
            "Upstream error response exceeded the proxy limit".to_string()
        } else if let Some(error) = read.error.as_deref() {
            format!("Failed to read upstream error response: {error}")
        } else {
            format!("HTTP {}", status.as_u16())
        }
    });
    let document = json!({
        "reqId": ctx.req_id,
        "provider": ctx.provider,
        "model": ctx.model,
        "countTokens": ctx.count_tokens,
        "status": status.as_u16(),
        "elapsedMs": ctx.started_at.elapsed().as_millis(),
        "message": message,
        "response": response_body,
        "bodyTruncated": read.truncated,
        "bodyTimedOut": read.timed_out,
        "bodyReadError": read.error.as_deref(),
    });
    let error_file = if should_capture_error_response(ctx.provider, status) {
        write_error_capture(ctx.req_id, redact_error_value(document)).await
    } else {
        None
    };

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
        ("bodyTruncated".to_string(), json!(read.truncated)),
        ("bodyTimedOut".to_string(), json!(read.timed_out)),
    ]);
    if let Some(error) = read.error.as_deref() {
        fields.insert("bodyReadError".to_string(), json!(error));
    }
    if let Some(path) = error_file.as_ref() {
        fields.insert("errorFile".to_string(), json!(path.display().to_string()));
    }
    log.info("request_failed", Some(fields));

    let abnormal = read.truncated || read.timed_out || read.error.is_some();
    let bytes = if abnormal {
        parts.headers.remove(http::header::CONTENT_LENGTH);
        parts.headers.remove(http::header::CONTENT_ENCODING);
        parts.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        serde_json::to_vec(&json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": message,
            }
        }))
        .unwrap_or_else(|_| b"{\"type\":\"error\"}".to_vec())
    } else {
        read.bytes
    };

    (
        Response::from_parts(parts, Body::from(bytes)),
        Some(FailedResponseDetails { message }),
    )
}

struct BoundedErrorBody {
    bytes: Vec<u8>,
    truncated: bool,
    timed_out: bool,
    error: Option<String>,
}

async fn read_bounded_error_body(mut body: Body) -> BoundedErrorBody {
    read_bounded_error_body_with_timeouts(
        &mut body,
        ERROR_RESPONSE_BODY_IDLE_TIMEOUT,
        ERROR_RESPONSE_BODY_TOTAL_TIMEOUT,
    )
    .await
}

async fn read_bounded_error_body_with_timeouts(
    body: &mut Body,
    idle_timeout: Duration,
    total_timeout: Duration,
) -> BoundedErrorBody {
    let total_deadline = tokio::time::Instant::now() + total_timeout;
    let mut out = BoundedErrorBody {
        bytes: Vec::new(),
        truncated: false,
        timed_out: false,
        error: None,
    };
    loop {
        let frame = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(total_deadline) => {
                out.timed_out = true;
                break;
            }
            result = tokio::time::timeout(idle_timeout, body.frame()) => match result {
                Ok(frame) => frame,
                Err(_) => {
                    out.timed_out = true;
                    break;
                }
            }
        };
        match frame {
            None => break,
            Some(Err(error)) => {
                out.error = Some(error.to_string());
                break;
            }
            Some(Ok(frame)) => {
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                let remaining = MAX_ERROR_RESPONSE_BODY_BYTES.saturating_sub(out.bytes.len());
                if data.len() > remaining {
                    out.bytes.extend_from_slice(&data[..remaining]);
                    out.truncated = true;
                    break;
                }
                out.bytes.extend_from_slice(data);
            }
        }
    }
    out
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

fn should_capture_error_response(provider: Option<&str>, status: StatusCode) -> bool {
    provider.is_some() && status.is_server_error()
}

async fn write_error_capture(req_id: &str, document: Value) -> Option<PathBuf> {
    let gate = ERROR_CAPTURE_GATE
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone();
    let permit = gate.try_acquire_owned().ok()?;
    let req_id = req_id.to_string();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        write_error_capture_blocking(&req_id, &document)
    })
    .await
    .ok()
    .flatten()
}

fn write_error_capture_blocking(req_id: &str, document: &Value) -> Option<PathBuf> {
    let dir = crate::paths::state_dir().join("errors");
    fs::create_dir_all(&dir).ok()?;
    set_mode(&dir, 0o700);
    prune_error_captures(&dir);
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

fn prune_error_captures(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    files.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let remove = files
        .len()
        .saturating_add(1)
        .saturating_sub(MAX_ERROR_CAPTURE_FILES);
    for (_, path) in files.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
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
    log: Logger,
    started_at: Instant,
    count_tokens: bool,
    provider: Option<String>,
    model: Option<String>,
    terminal: bool,
}

impl RequestMonitorGuard {
    fn new(
        monitor: Option<MonitorHandle>,
        req_id: String,
        log: Logger,
        started_at: Instant,
        count_tokens: bool,
    ) -> Self {
        Self {
            monitor,
            req_id,
            log,
            started_at,
            count_tokens,
            provider: None,
            model: None,
            terminal: false,
        }
    }

    fn set_route(&mut self, provider: Option<&str>, model: Option<&str>) {
        self.provider = provider.map(str::to_string);
        self.model = model.map(str::to_string);
    }

    fn completed(&mut self, status: StatusCode) {
        self.terminal = true;
        if let Some(monitor) = self.monitor.take() {
            monitor.request_completed(&self.req_id, status.as_u16(), None, None);
        }
    }

    fn failed(&mut self, status: StatusCode, error: String) {
        self.terminal = true;
        if let Some(monitor) = self.monitor.take() {
            monitor.request_failed(&self.req_id, Some(status.as_u16()), error);
        }
    }

    fn abandoned(&mut self, error: &str) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        if let Some(monitor) = self.monitor.take() {
            monitor.request_abandoned(&self.req_id, error);
        }
        self.log.info(
            "request_abandoned",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), json!(&self.req_id)),
                ("provider".to_string(), json!(&self.provider)),
                ("model".to_string(), json!(&self.model)),
                ("countTokens".to_string(), json!(self.count_tokens)),
                (
                    "ms".to_string(),
                    json!(self.started_at.elapsed().as_millis()),
                ),
                ("message".to_string(), json!(error)),
            ])),
        );
    }
}

impl Drop for RequestMonitorGuard {
    fn drop(&mut self) {
        self.abandoned("Request future ended before completion");
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::stream;

    fn compaction_request() -> crate::anthropic::schema::MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "fable",
            "messages": [{
                "role": "user",
                "content": "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far."
            }]
        }))
        .unwrap()
    }

    #[test]
    fn compaction_override_requires_both_header_and_compaction_prompt() {
        let mut headers = HeaderMap::new();
        headers.insert(COMPACTION_MODEL_HEADER, "grok-4.5-high".parse().unwrap());

        assert_eq!(
            compaction_model_override(&headers, &compaction_request()).as_deref(),
            Some("grok-4.5-high")
        );

        let ordinary = serde_json::from_value(serde_json::json!({
            "model": "fable",
            "messages": [{"role": "user", "content": "normal work"}]
        }))
        .unwrap();
        assert!(compaction_model_override(&headers, &ordinary).is_none());
        assert!(compaction_model_override(&HeaderMap::new(), &compaction_request()).is_none());
    }

    fn effort_request(effort: &str) -> crate::anthropic::schema::MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "normal work"}],
            "output_config": {"effort": effort}
        }))
        .unwrap()
    }

    fn effort_of(request: &crate::anthropic::schema::MessagesRequest) -> Option<&str> {
        request
            .extra
            .get("output_config")
            .and_then(Value::as_object)
            .and_then(|output_config| output_config.get("effort"))
            .and_then(Value::as_str)
    }

    #[test]
    fn codex_xhigh_as_max_requires_exact_marker_and_messages_endpoint() {
        let mut enabled = HeaderMap::new();
        enabled.insert(CODEX_XHIGH_AS_MAX_HEADER, "1".parse().unwrap());

        let mut request = effort_request("xhigh");
        assert!(promote_codex_xhigh_as_max(
            &enabled,
            &mut request,
            "codex",
            false
        ));
        assert_eq!(effort_of(&request), Some("max"));

        for marker in [None, Some("true"), Some("01"), Some(" 1")] {
            let mut headers = HeaderMap::new();
            if let Some(marker) = marker {
                headers.insert(CODEX_XHIGH_AS_MAX_HEADER, marker.parse().unwrap());
            }
            let mut request = effort_request("xhigh");
            assert!(!promote_codex_xhigh_as_max(
                &headers,
                &mut request,
                "codex",
                false
            ));
            assert_eq!(effort_of(&request), Some("xhigh"));
        }

        let mut request = effort_request("xhigh");
        assert!(!promote_codex_xhigh_as_max(
            &enabled,
            &mut request,
            "codex",
            true
        ));
        assert_eq!(effort_of(&request), Some("xhigh"));

        let mut request = effort_request("xhigh");
        assert!(!promote_codex_xhigh_as_max(
            &enabled,
            &mut request,
            "grok",
            false
        ));
        assert_eq!(effort_of(&request), Some("xhigh"));
    }

    #[test]
    fn codex_xhigh_as_max_preserves_other_efforts_and_compaction() {
        let mut headers = HeaderMap::new();
        headers.insert(CODEX_XHIGH_AS_MAX_HEADER, "1".parse().unwrap());

        for effort in ["low", "medium", "high", "max", "ultra"] {
            let mut request = effort_request(effort);
            assert!(!promote_codex_xhigh_as_max(
                &headers,
                &mut request,
                "codex",
                false
            ));
            assert_eq!(effort_of(&request), Some(effort));
        }

        let mut compaction = compaction_request();
        compaction.extra.insert(
            "output_config".to_string(),
            serde_json::json!({"effort": "xhigh"}),
        );
        assert!(!promote_codex_xhigh_as_max(
            &headers,
            &mut compaction,
            "codex",
            false
        ));
        assert_eq!(effort_of(&compaction), Some("xhigh"));
    }

    fn response_log_context(req_id: &str) -> ResponseLogContext {
        ResponseLogContext {
            log: create_logger("server-test"),
            req_id: req_id.to_string(),
            provider: Some("test".to_string()),
            model: Some("test-model".to_string()),
            count_tokens: false,
            started_at: Instant::now(),
        }
    }

    fn started_monitor(req_id: &str) -> MonitorHandle {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(req_id, None, None, EndpointKind::Messages);
        monitor
    }

    fn request_guard(monitor: MonitorHandle, req_id: &str) -> RequestMonitorGuard {
        RequestMonitorGuard::new(
            Some(monitor),
            req_id.to_string(),
            create_logger("server-test"),
            Instant::now(),
            false,
        )
    }

    #[tokio::test]
    async fn response_body_stays_active_until_successful_eof() {
        let req_id = "stream-success";
        let monitor = started_monitor(req_id);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream::iter(vec![
                Ok::<_, std::io::Error>(Bytes::from_static(
                    b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
                )),
            ])))
            .unwrap();
        let response = monitor_response_body(
            response,
            request_guard(monitor.clone(), req_id),
            response_log_context(req_id),
            RequestPermits::default(),
        );

        let state = monitor.snapshot();
        assert_eq!(state.active.len(), 1);
        assert!(state.recent.is_empty());

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("event: ping"));
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(
            state.recent[0].status,
            crate::monitor::RequestStatus::Completed
        );
        assert_eq!(state.recent[0].http_status, Some(200));
    }

    #[tokio::test]
    async fn rejected_provider_response_holds_admission_permit_until_body_drop() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let response = hold_response_permits(
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("bounded error"))
                .unwrap(),
            RequestPermits {
                global: Some(permit),
                ..RequestPermits::default()
            },
        );
        assert!(semaphore.clone().try_acquire_owned().is_err());
        drop(response);
        assert!(semaphore.try_acquire_owned().is_ok());
    }

    #[test]
    fn buffered_request_budget_follows_request_sized_provider_material_only() {
        let request_bytes = Arc::new(Semaphore::new(1));
        let global = Arc::new(Semaphore::new(1));
        let mut permits = RequestPermits {
            global: Some(global.clone().try_acquire_owned().unwrap()),
            request_bytes: Some(request_bytes.clone().try_acquire_owned().unwrap()),
            ..RequestPermits::default()
        };

        let server_lease = RequestByteLease::new(permits.request_bytes.take().unwrap(), 1);
        let provider_material_lease = server_lease.clone();
        drop(server_lease);

        assert!(request_bytes.clone().try_acquire_owned().is_err());
        assert!(global.try_acquire_owned().is_err());
        drop(provider_material_lease);
        assert!(request_bytes.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn split_in_band_sse_error_is_a_failed_request() {
        let req_id = "stream-error";
        let monitor = started_monitor(req_id);
        let chunks = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"event: er")),
            Ok(Bytes::from_static(
                b"ror\r\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"deadline exceeded\"}}\r",
            )),
            Ok(Bytes::from_static(b"\n\r\n")),
            Ok(Bytes::from_static(
                b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            )),
        ];
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(
                http::header::CONTENT_TYPE,
                "text/event-stream; charset=utf-8",
            )
            .body(Body::from_stream(stream::iter(chunks)))
            .unwrap();
        let response = monitor_response_body(
            response,
            request_guard(monitor.clone(), req_id),
            response_log_context(req_id),
            RequestPermits::default(),
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("deadline exceeded"));
        assert!(!body.contains("message_stop"));
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(
            state.recent[0].status,
            crate::monitor::RequestStatus::Failed
        );
        assert_eq!(state.recent[0].http_status, Some(200));
        assert_eq!(state.recent[0].error.as_deref(), Some("deadline exceeded"));
    }

    #[tokio::test]
    async fn non_sse_error_shaped_json_remains_a_successful_body() {
        let req_id = "json-success";
        let monitor = started_monitor(req_id);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"message":"ordinary payload"}}"#,
            ))
            .unwrap();
        let response = monitor_response_body(
            response,
            request_guard(monitor.clone(), req_id),
            response_log_context(req_id),
            RequestPermits::default(),
        );

        let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        assert_eq!(
            state.recent[0].status,
            crate::monitor::RequestStatus::Completed
        );
    }

    #[test]
    fn cr_only_sse_error_is_detected() {
        let mut detector = SseErrorDetector::default();
        let error = detector
            .push(
                b"event: error\rdata: {\"type\":\"error\",\"error\":{\"message\":\"cr deadline\"}}\r\r",
            )
            .expect("CR-only SSE must dispatch an error event");

        assert_eq!(error, "cr deadline");
    }

    #[test]
    fn sse_tool_lifecycle_keeps_only_safe_metadata() {
        let mut detector = SseErrorDetector::default();
        assert!(
            detector
                .push(
                    b"event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-secret-123\",\"name\":\"Brave Search; bad\",\"input\":{\"query\":\"CANARY_PROMPT\"}}}\n\n",
                )
                .is_none()
        );

        let events = detector.take_tool_events();
        assert_eq!(events.len(), 1);
        let SseToolEvent::Started(tool) = &events[0] else {
            panic!("expected a tool start event");
        };
        assert_eq!(tool.index, 2);
        assert_eq!(tool.tool_kind, "tool_use");
        assert_eq!(tool.tool_name.as_deref(), Some("Brave_Search__bad"));
        assert_eq!(tool.call_id_hash.as_deref().map(str::len), Some(16));
        assert_ne!(tool.call_id_hash.as_deref(), Some("call-secret-123"));
        let serialized = serde_json::to_string(&format!("{tool:?}")).unwrap();
        assert!(!serialized.contains("CANARY_PROMPT"));

        assert!(
            detector
                .push(
                    b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"CANARY_ARGUMENT\\\"}\"}}\n\n",
                )
                .is_none()
        );
        assert!(detector.take_tool_events().is_empty());

        assert!(
            detector
                .push(
                    b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
                )
                .is_none()
        );
        assert_eq!(
            detector.take_tool_events(),
            vec![SseToolEvent::Stopped { index: 2 }]
        );

        assert!(
            detector
                .push(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
                .is_none()
        );
        assert_eq!(
            detector.take_tool_events(),
            vec![SseToolEvent::MessageStopped]
        );
    }

    #[test]
    fn tool_stop_is_provisional_until_a_clean_message_stop() {
        fn lifecycle(req_id: &str) -> ResponseBodyLifecycle {
            let monitor = started_monitor(req_id);
            ResponseBodyLifecycle {
                guard: request_guard(monitor, req_id),
                log_context: response_log_context(req_id),
                status: StatusCode::OK,
                sse_detector: Some(SseErrorDetector::default()),
                open_tool_blocks: HashMap::new(),
                provisionally_closed_tool_blocks: HashMap::new(),
                _permits: RequestPermits::default(),
                terminal: false,
            }
        }

        let tool = ToolBlockTrace {
            index: 2,
            tool_kind: "tool_use".to_string(),
            tool_name: Some("brave_search".to_string()),
            call_id_hash: Some("1234abcd".to_string()),
        };

        let mut failed = lifecycle("provisional-tool-failure");
        failed.observe_tool_event(SseToolEvent::Started(tool.clone()));
        failed.observe_tool_event(SseToolEvent::Stopped { index: 2 });
        assert!(failed.open_tool_blocks.is_empty());
        assert_eq!(failed.provisionally_closed_tool_blocks.get(&2), Some(&tool));
        failed.failed("upstream closed".to_string(), true);
        assert!(failed.provisionally_closed_tool_blocks.is_empty());

        let mut completed = lifecycle("provisional-tool-success");
        completed.observe_tool_event(SseToolEvent::Started(tool));
        completed.observe_tool_event(SseToolEvent::Stopped { index: 2 });
        completed.observe_tool_event(SseToolEvent::MessageStopped);
        assert!(completed.open_tool_blocks.is_empty());
        assert!(completed.provisionally_closed_tool_blocks.is_empty());
        completed.completed();
    }

    #[test]
    fn oversized_many_line_event_is_discarded_and_parser_recovers() {
        let mut detector = SseErrorDetector::default();
        for _ in 0..(MAX_SSE_ERROR_EVENT_BYTES / 5 + 10) {
            assert!(detector.push(b"data:\n").is_none());
        }
        assert!(detector.discard_event);
        assert!(detector.data.is_empty());
        assert!(detector.push(b"\n").is_none());

        let error = detector
            .push(
                b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"recovered\"}}\n\n",
            )
            .expect("the parser should recover after the oversized event boundary");
        assert_eq!(error, "recovered");
    }

    #[test]
    fn local_rejections_and_rate_limits_do_not_create_error_capture_files() {
        assert!(!should_capture_error_response(
            None,
            StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(!should_capture_error_response(
            Some("grok"),
            StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(!should_capture_error_response(
            Some("codex"),
            StatusCode::PAYLOAD_TOO_LARGE
        ));
        assert!(should_capture_error_response(
            Some("codex"),
            StatusCode::BAD_GATEWAY
        ));
    }

    #[tokio::test]
    async fn rejected_response_body_is_bounded_and_marks_truncation() {
        let mut exact = Body::from(vec![b'x'; MAX_ERROR_RESPONSE_BODY_BYTES]);
        let exact = read_bounded_error_body_with_timeouts(
            &mut exact,
            Duration::from_millis(20),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(exact.bytes.len(), MAX_ERROR_RESPONSE_BODY_BYTES);
        assert!(!exact.truncated);
        assert!(!exact.timed_out);

        let mut oversized = Body::from(vec![b'x'; MAX_ERROR_RESPONSE_BODY_BYTES + 1]);
        let oversized = read_bounded_error_body_with_timeouts(
            &mut oversized,
            Duration::from_millis(20),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(oversized.bytes.len(), MAX_ERROR_RESPONSE_BODY_BYTES);
        assert!(oversized.truncated);
    }

    #[tokio::test]
    async fn rejected_response_body_idle_timeout_does_not_wait_forever() {
        let body_stream = stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, std::io::Error>(Bytes::from_static(b"late"))
        });
        let mut body = Body::from_stream(body_stream);
        let result = read_bounded_error_body_with_timeouts(
            &mut body,
            Duration::from_millis(5),
            Duration::from_millis(50),
        )
        .await;
        assert!(result.timed_out);
        assert!(result.bytes.is_empty());
    }

    #[tokio::test]
    async fn rejected_response_body_total_timeout_stops_a_trickle() {
        let body_stream = stream::unfold(0_u8, |value| async move {
            tokio::time::sleep(Duration::from_millis(3)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from(vec![value])), value + 1))
        });
        let mut body = Body::from_stream(body_stream);
        let result = read_bounded_error_body_with_timeouts(
            &mut body,
            Duration::from_millis(10),
            Duration::from_millis(15),
        )
        .await;
        assert!(result.timed_out);
        assert!(!result.bytes.is_empty());
        assert!(result.bytes.len() < MAX_ERROR_RESPONSE_BODY_BYTES);
    }

    #[test]
    fn effective_config_fingerprint_changes_with_limit_values() {
        let first = ServerLimits::default();
        let mut second = first.clone();
        second.max_concurrent_requests += 1;

        assert_ne!(
            effective_config_fingerprint(&first, "127.0.0.1", 18765),
            effective_config_fingerprint(&second, "127.0.0.1", 18765)
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_timeout_bounds_a_stalled_server() {
        let (shutdown_started_tx, shutdown_started_rx) = tokio::sync::oneshot::channel();
        shutdown_started_tx.send(()).unwrap();
        let result = await_server_with_grace_timeout(
            std::future::pending::<std::io::Result<()>>(),
            shutdown_started_rx,
            Duration::from_millis(5),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("graceful shutdown exceeded")
        );
    }

    #[tokio::test]
    async fn server_completion_wins_before_shutdown_timeout() {
        let (_shutdown_started_tx, shutdown_started_rx) = tokio::sync::oneshot::channel();
        await_server_with_grace_timeout(
            std::future::ready(Ok(())),
            shutdown_started_rx,
            Duration::ZERO,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rejected_response_body_read_error_is_captured() {
        let body_stream = stream::iter([Err::<Bytes, _>(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ))]);
        let mut body = Body::from_stream(body_stream);
        let result = read_bounded_error_body_with_timeouts(
            &mut body,
            Duration::from_millis(20),
            Duration::from_millis(100),
        )
        .await;
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("reset"))
        );
    }

    #[test]
    fn dropping_pre_response_guard_records_abandonment_once() {
        let req_id = "pre-response-drop";
        let monitor = started_monitor(req_id);
        drop(request_guard(monitor.clone(), req_id));

        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(
            state.recent[0].status,
            crate::monitor::RequestStatus::Failed
        );
        assert_eq!(
            state.recent[0].error.as_deref(),
            Some("Request future ended before completion")
        );
    }
}
