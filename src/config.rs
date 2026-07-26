use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::paths;

fn environment() -> HashMap<String, String> {
    std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn environment_value(key: &str) -> Option<String> {
    std::env::var_os(key).map(|value| value.to_string_lossy().into_owned())
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub bind_address: String,
    pub port: u16,
    pub log_verbose: bool,
    pub log_stderr: bool,
    pub config_dir: PathBuf,
    /// Monotonic generation of the process-local file-config snapshot.
    ///
    /// Environment overrides are intentionally not part of this generation:
    /// process environment is expected to be immutable outside tests.
    pub config_generation: u64,
}

#[derive(Clone, Deserialize)]
struct FileConfig {
    #[serde(rename = "bindAddress")]
    pub bind_address: Option<String>,
    pub port: Option<u16>,
    pub log: Option<FileLog>,
    pub codex: Option<CodexConfig>,
    pub grok: Option<GrokConfig>,
    pub server: Option<ServerResourceConfig>,
}

#[derive(Deserialize, Clone)]
struct ServerResourceConfig {
    #[serde(rename = "allowRemoteUnauthenticated")]
    pub allow_remote_unauthenticated: Option<bool>,
    #[serde(rename = "maxRequestBodyBytes")]
    pub max_request_body_bytes: Option<u64>,
    #[serde(rename = "maxBufferedRequestBytes")]
    pub max_buffered_request_bytes: Option<u64>,
    #[serde(rename = "maxConcurrentRequests")]
    pub max_concurrent_requests: Option<u64>,
    #[serde(rename = "maxConcurrentPerProvider")]
    pub max_concurrent_per_provider: Option<u64>,
    #[serde(rename = "maxConcurrentPerSession")]
    pub max_concurrent_per_session: Option<u64>,
    #[serde(rename = "requestBodyIdleTimeoutMs")]
    pub request_body_idle_timeout_ms: Option<u64>,
    #[serde(rename = "requestBodyTotalTimeoutMs")]
    pub request_body_total_timeout_ms: Option<u64>,
}

#[derive(Deserialize, Clone)]
struct CodexConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "originator")]
    pub originator: Option<String>,
    #[serde(rename = "userAgent")]
    pub user_agent: Option<String>,
    #[serde(rename = "previousResponseId")]
    pub previous_response_id: Option<bool>,
    #[serde(rename = "unsafeSalvageToolCallOnClose")]
    pub unsafe_salvage_tool_call_on_close: Option<bool>,
    #[serde(rename = "serviceTier")]
    pub service_tier: Option<String>,
    #[serde(rename = "responsesLite")]
    pub responses_lite: Option<bool>,
    #[serde(rename = "parallelTools")]
    pub parallel_tools: Option<bool>,
    #[serde(rename = "reasoningSummary")]
    pub reasoning_summary: Option<String>,
    #[serde(rename = "effort")]
    pub effort: Option<String>,
    #[serde(rename = "model")]
    pub model: Option<String>,
    pub transport: Option<String>,
    #[serde(rename = "connectTimeoutMs")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(rename = "headerTimeoutMs")]
    pub header_timeout_ms: Option<u64>,
    #[serde(rename = "httpFirstByteTimeoutMs")]
    pub http_first_byte_timeout_ms: Option<u64>,
    #[serde(rename = "bodyIdleTimeoutMs")]
    pub body_idle_timeout_ms: Option<u64>,
    #[serde(rename = "totalTimeoutMs")]
    pub total_timeout_ms: Option<u64>,
    #[serde(rename = "streamHeartbeatMs")]
    pub stream_heartbeat_ms: Option<u64>,
    #[serde(rename = "websocketResponseStartTimeoutMs")]
    pub websocket_response_start_timeout_ms: Option<u64>,
    #[serde(rename = "websocketIdleTimeoutMs")]
    pub websocket_idle_timeout_ms: Option<u64>,
    #[serde(rename = "maxIdleWebSockets")]
    pub max_idle_websockets: Option<u64>,
    #[serde(rename = "idleWebSocketTtlMs")]
    pub idle_websocket_ttl_ms: Option<u64>,
}

#[derive(Deserialize, Clone)]
struct GrokConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "clientVersion")]
    pub client_version: Option<String>,
    #[serde(rename = "connectTimeoutMs")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(rename = "headerTimeoutMs")]
    pub header_timeout_ms: Option<u64>,
    #[serde(rename = "firstByteTimeoutMs")]
    pub first_byte_timeout_ms: Option<u64>,
    #[serde(rename = "bodyIdleTimeoutMs")]
    pub body_idle_timeout_ms: Option<u64>,
    #[serde(rename = "totalTimeoutMs")]
    pub total_timeout_ms: Option<u64>,
    #[serde(rename = "streamHeartbeatMs")]
    pub stream_heartbeat_ms: Option<u64>,
}

#[derive(Clone, Deserialize)]
struct FileLog {
    pub verbose: Option<bool>,
    pub stderr: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileConfigFingerprint {
    Missing,
    Present {
        len: u64,
        modified: Option<SystemTime>,
    },
}

#[derive(Clone)]
struct CachedFileConfig {
    path: PathBuf,
    fingerprint: FileConfigFingerprint,
    value: Option<FileConfig>,
    checked_at: Instant,
    generation: u64,
}

#[derive(Default)]
struct FileConfigCache {
    current: Option<CachedFileConfig>,
    #[cfg(test)]
    filesystem_checks: u64,
    #[cfg(test)]
    parse_attempts: u64,
}

impl FileConfigCache {
    fn load(&mut self, path: &Path, now: Instant, recheck_interval: Duration) -> CachedFileConfig {
        if let Some(cached) = self.current.as_ref()
            && cached.path == path
            && now.saturating_duration_since(cached.checked_at) < recheck_interval
        {
            return cached.clone();
        }

        #[cfg(test)]
        {
            self.filesystem_checks += 1;
        }
        let fingerprint = file_config_fingerprint(path);
        if let Some(cached) = self.current.as_mut()
            && cached.path == path
            && cached.fingerprint == fingerprint
        {
            cached.checked_at = now;
            return cached.clone();
        }

        #[cfg(test)]
        {
            self.parse_attempts += 1;
        }
        let value = fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let generation = self
            .current
            .as_ref()
            .map_or(1, |cached| cached.generation.saturating_add(1));
        let loaded = CachedFileConfig {
            path: path.to_path_buf(),
            fingerprint,
            value,
            checked_at: now,
            generation,
        };
        self.current = Some(loaded.clone());
        loaded
    }
}

// File edits remain hot-reloadable, but request and logging hot paths only
// perform a metadata check at this cadence. Every lookup inside the interval
// observes one coherent generation. Unit tests use an immediate recheck so
// their existing write-then-resolve behaviour remains deterministic.
#[cfg(not(test))]
const FILE_CONFIG_RECHECK_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(test)]
const FILE_CONFIG_RECHECK_INTERVAL: Duration = Duration::ZERO;

static FILE_CONFIG_CACHE: OnceLock<Mutex<FileConfigCache>> = OnceLock::new();

fn file_config_fingerprint(path: &Path) -> FileConfigFingerprint {
    match fs::metadata(path) {
        Ok(metadata) => FileConfigFingerprint::Present {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        },
        Err(_) => FileConfigFingerprint::Missing,
    }
}

fn read_file_config_snapshot(config_dir: &Path) -> CachedFileConfig {
    let path = config_dir.join("config.json");
    FILE_CONFIG_CACHE
        .get_or_init(|| Mutex::new(FileConfigCache::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .load(&path, Instant::now(), FILE_CONFIG_RECHECK_INTERVAL)
}

fn read_file_config(config_dir: &Path) -> Option<FileConfig> {
    read_file_config_snapshot(config_dir).value
}

pub fn load_config() -> LoadedConfig {
    let config_dir = paths::config_dir();
    let snapshot = read_file_config_snapshot(&config_dir);
    let file = snapshot.value;
    let env = environment();

    let mut out = LoadedConfig {
        bind_address: "127.0.0.1".to_string(),
        port: 18765,
        log_verbose: false,
        log_stderr: false,
        config_dir: config_dir.clone(),
        config_generation: snapshot.generation,
    };

    if let Some(raw) = env.get("CCP_BIND_ADDRESS") {
        out.bind_address = raw.clone();
    } else if let Some(bind_address) = file.as_ref().and_then(|f| f.bind_address.clone()) {
        out.bind_address = bind_address;
    }

    if let Some(raw) = env.get("PORT") {
        if let Ok(port) = raw.parse::<u16>() {
            out.port = port;
        }
    } else if let Some(port) = file.as_ref().and_then(|f| f.port) {
        out.port = port;
    }

    if env.contains_key("CCP_LOG_VERBOSE") {
        out.log_verbose = true;
    } else if let Some(value) = file
        .as_ref()
        .and_then(|f| f.log.as_ref().and_then(|v| v.verbose))
    {
        out.log_verbose = value;
    }

    if env.contains_key("CCP_LOG_STDERR") {
        out.log_stderr = true;
    } else if let Some(value) = file
        .as_ref()
        .and_then(|f| f.log.as_ref().and_then(|v| v.stderr))
    {
        out.log_stderr = value;
    }

    out
}

pub fn config_path() -> PathBuf {
    paths::config_dir().join("config.json")
}

pub fn port() -> u16 {
    load_config().port
}

pub fn bind_address() -> String {
    load_config().bind_address
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogConfigSnapshot {
    pub verbose: bool,
    pub stderr: bool,
    pub generation: u64,
}

/// Load the two logging flags from one file-config generation.
///
/// This deliberately avoids collecting the entire process environment on the
/// log emission path. As with the legacy behaviour, presence of either env var
/// enables its flag regardless of the variable's value.
pub fn load_log_config() -> LogConfigSnapshot {
    let config_dir = paths::config_dir();
    let snapshot = read_file_config_snapshot(&config_dir);
    let file_log = snapshot.value.as_ref().and_then(|file| file.log.as_ref());
    LogConfigSnapshot {
        verbose: if std::env::var_os("CCP_LOG_VERBOSE").is_some() {
            true
        } else {
            file_log.and_then(|log| log.verbose).unwrap_or(false)
        },
        stderr: if std::env::var_os("CCP_LOG_STDERR").is_some() {
            true
        } else {
            file_log.and_then(|log| log.stderr).unwrap_or(false)
        },
        generation: snapshot.generation,
    }
}

pub fn log_verbose() -> bool {
    load_log_config().verbose
}

pub fn log_stderr() -> bool {
    load_log_config().stderr
}

pub fn config_override_summary_lines(cfg: &LoadedConfig) -> Vec<String> {
    let file = read_file_config(&cfg.config_dir);
    let env = environment();
    let mut out = Vec::new();
    if env.contains_key("CCP_BIND_ADDRESS") {
        out.push("bindAddress (env)".to_string());
    }
    if env.contains_key("PORT") {
        out.push("port (env)".to_string());
    }
    if env.contains_key("CCP_LOG_VERBOSE") {
        out.push("log.verbose (env)".to_string());
    }
    if env.contains_key("CCP_LOG_STDERR") {
        out.push("log.stderr (env)".to_string());
    }
    if env.contains_key("CCP_GROK_BASE_URL") {
        out.push("grok.baseUrl (env)".to_string());
    }
    if env.contains_key("CCP_GROK_CLIENT_VERSION") {
        out.push("grok.clientVersion (env)".to_string());
    }
    if env.contains_key("CCP_GROK_CONNECT_TIMEOUT_MS") {
        out.push("CCP_GROK_CONNECT_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_GROK_HEADER_TIMEOUT_MS") {
        out.push("CCP_GROK_HEADER_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_GROK_FIRST_BYTE_TIMEOUT_MS") {
        out.push("CCP_GROK_FIRST_BYTE_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_GROK_BODY_IDLE_TIMEOUT_MS") {
        out.push("CCP_GROK_BODY_IDLE_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_GROK_TOTAL_TIMEOUT_MS") {
        out.push("CCP_GROK_TOTAL_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_GROK_STREAM_HEARTBEAT_MS") {
        out.push("CCP_GROK_STREAM_HEARTBEAT_MS (env)".to_string());
    }
    if env.contains_key("CCP_MAX_REQUEST_BODY_BYTES") {
        out.push("CCP_MAX_REQUEST_BODY_BYTES (env)".to_string());
    }
    if env.contains_key("CCP_MAX_BUFFERED_REQUEST_BYTES") {
        out.push("CCP_MAX_BUFFERED_REQUEST_BYTES (env)".to_string());
    }
    if env.contains_key("CCP_MAX_CONCURRENT_REQUESTS") {
        out.push("CCP_MAX_CONCURRENT_REQUESTS (env)".to_string());
    }
    if env.contains_key("CCP_MAX_CONCURRENT_PER_PROVIDER") {
        out.push("CCP_MAX_CONCURRENT_PER_PROVIDER (env)".to_string());
    }
    if env.contains_key("CCP_MAX_CONCURRENT_PER_SESSION") {
        out.push("CCP_MAX_CONCURRENT_PER_SESSION (env)".to_string());
    }
    if env.contains_key("CCP_REQUEST_BODY_IDLE_TIMEOUT_MS") {
        out.push("CCP_REQUEST_BODY_IDLE_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_REQUEST_BODY_TOTAL_TIMEOUT_MS") {
        out.push("CCP_REQUEST_BODY_TOTAL_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_ALLOW_REMOTE_UNAUTHENTICATED") {
        out.push("CCP_ALLOW_REMOTE_UNAUTHENTICATED (env)".to_string());
    }
    if env
        .get("CCP_CODEX_REASONING_SUMMARY")
        .is_some_and(|raw| !raw.is_empty())
    {
        out.push("CCP_CODEX_REASONING_SUMMARY (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_RESPONSES_LITE") {
        out.push("CCP_CODEX_RESPONSES_LITE (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_PARALLEL_TOOLS") {
        out.push("CCP_CODEX_PARALLEL_TOOLS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE") {
        out.push("CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_CONNECT_TIMEOUT_MS") {
        out.push("CCP_CODEX_CONNECT_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_HEADER_TIMEOUT_MS") {
        out.push("CCP_CODEX_HEADER_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS") {
        out.push("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_BODY_IDLE_TIMEOUT_MS") {
        out.push("CCP_CODEX_BODY_IDLE_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_TOTAL_TIMEOUT_MS") {
        out.push("CCP_CODEX_TOTAL_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_STREAM_HEARTBEAT_MS") {
        out.push("CCP_CODEX_STREAM_HEARTBEAT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS") {
        out.push("CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_WEBSOCKET_IDLE_TIMEOUT_MS") {
        out.push("CCP_CODEX_WEBSOCKET_IDLE_TIMEOUT_MS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_MAX_IDLE_WEBSOCKETS") {
        out.push("CCP_CODEX_MAX_IDLE_WEBSOCKETS (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_IDLE_WEBSOCKET_TTL_MS") {
        out.push("CCP_CODEX_IDLE_WEBSOCKET_TTL_MS (env)".to_string());
    }
    if let Some(file_cfg) = file {
        if let Some(bind_address) = file_cfg.bind_address {
            out.push(format!("bindAddress: {bind_address}"));
        }
        if let Some(p) = file_cfg.port {
            out.push(format!("port: {p}"));
        }
        if let Some(log) = file_cfg.log {
            if let Some(v) = log.verbose {
                out.push(format!("log.verbose: {v}"));
            }
            if let Some(v) = log.stderr {
                out.push(format!("log.stderr: {v}"));
            }
        }
        if let Some(codex) = file_cfg.codex {
            if let Some(responses_lite) = codex.responses_lite {
                out.push(format!("codex.responsesLite: {responses_lite}"));
            }
            if let Some(parallel_tools) = codex.parallel_tools {
                out.push(format!("codex.parallelTools: {parallel_tools}"));
            }
            if let Some(unsafe_salvage) = codex.unsafe_salvage_tool_call_on_close {
                out.push(format!(
                    "codex.unsafeSalvageToolCallOnClose: {unsafe_salvage}"
                ));
            }
            if codex
                .reasoning_summary
                .as_deref()
                .is_some_and(|summary| !summary.is_empty())
            {
                out.push("codex.reasoningSummary (config)".to_string());
            }
            if let Some(timeout_ms) = codex.connect_timeout_ms {
                out.push(format!("codex.connectTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = codex.header_timeout_ms {
                out.push(format!("codex.headerTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = codex.http_first_byte_timeout_ms {
                out.push(format!("codex.httpFirstByteTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = codex.body_idle_timeout_ms {
                out.push(format!("codex.bodyIdleTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = codex.total_timeout_ms {
                out.push(format!("codex.totalTimeoutMs: {timeout_ms}"));
            }
            if let Some(heartbeat_ms) = codex.stream_heartbeat_ms {
                out.push(format!("codex.streamHeartbeatMs: {heartbeat_ms}"));
            }
            if let Some(timeout_ms) = codex.websocket_response_start_timeout_ms {
                out.push(format!(
                    "codex.websocketResponseStartTimeoutMs: {timeout_ms}"
                ));
            }
            if let Some(timeout_ms) = codex.websocket_idle_timeout_ms {
                out.push(format!("codex.websocketIdleTimeoutMs: {timeout_ms}"));
            }
            if let Some(max_idle) = codex.max_idle_websockets {
                out.push(format!("codex.maxIdleWebSockets: {max_idle}"));
            }
            if let Some(ttl_ms) = codex.idle_websocket_ttl_ms {
                out.push(format!("codex.idleWebSocketTtlMs: {ttl_ms}"));
            }
        }
        if let Some(grok) = file_cfg.grok {
            if let Some(timeout_ms) = grok.connect_timeout_ms {
                out.push(format!("grok.connectTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = grok.header_timeout_ms {
                out.push(format!("grok.headerTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = grok.first_byte_timeout_ms {
                out.push(format!("grok.firstByteTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = grok.body_idle_timeout_ms {
                out.push(format!("grok.bodyIdleTimeoutMs: {timeout_ms}"));
            }
            if let Some(timeout_ms) = grok.total_timeout_ms {
                out.push(format!("grok.totalTimeoutMs: {timeout_ms}"));
            }
            if let Some(heartbeat_ms) = grok.stream_heartbeat_ms {
                out.push(format!("grok.streamHeartbeatMs: {heartbeat_ms}"));
            }
        }
        if let Some(server) = file_cfg.server {
            if let Some(value) = server.allow_remote_unauthenticated {
                out.push(format!("server.allowRemoteUnauthenticated: {value}"));
            }
            if let Some(value) = server.max_request_body_bytes {
                out.push(format!("server.maxRequestBodyBytes: {value}"));
            }
            if let Some(value) = server.max_buffered_request_bytes {
                out.push(format!("server.maxBufferedRequestBytes: {value}"));
            }
            if let Some(value) = server.max_concurrent_requests {
                out.push(format!("server.maxConcurrentRequests: {value}"));
            }
            if let Some(value) = server.max_concurrent_per_provider {
                out.push(format!("server.maxConcurrentPerProvider: {value}"));
            }
            if let Some(value) = server.max_concurrent_per_session {
                out.push(format!("server.maxConcurrentPerSession: {value}"));
            }
            if let Some(value) = server.request_body_idle_timeout_ms {
                out.push(format!("server.requestBodyIdleTimeoutMs: {value}"));
            }
            if let Some(value) = server.request_body_total_timeout_ms {
                out.push(format!("server.requestBodyTotalTimeoutMs: {value}"));
            }
        }
    }
    out
}

pub fn allow_remote_unauthenticated() -> bool {
    if let Ok(raw) = std::env::var("CCP_ALLOW_REMOTE_UNAUTHENTICATED") {
        return matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        );
    }
    read_file_config(&paths::config_dir())
        .and_then(|file| file.server)
        .and_then(|server| server.allow_remote_unauthenticated)
        .unwrap_or(false)
}

fn server_positive_u64(
    env_key: &str,
    file_value: impl FnOnce(&ServerResourceConfig) -> Option<u64>,
    default: u64,
    maximum: u64,
) -> u64 {
    if let Ok(raw) = std::env::var(env_key)
        && let Ok(value) = raw.parse::<u64>()
        && value > 0
    {
        return value.min(maximum);
    }
    if let Some(server) = read_file_config(&paths::config_dir()).and_then(|file| file.server)
        && let Some(value) = file_value(&server)
        && value > 0
    {
        return value.min(maximum);
    }
    default
}

pub fn max_request_body_bytes(default: usize) -> usize {
    server_positive_u64(
        "CCP_MAX_REQUEST_BODY_BYTES",
        |server| server.max_request_body_bytes,
        default as u64,
        256 * 1024 * 1024,
    ) as usize
}

pub fn max_buffered_request_bytes(default: usize) -> usize {
    server_positive_u64(
        "CCP_MAX_BUFFERED_REQUEST_BYTES",
        |server| server.max_buffered_request_bytes,
        default as u64,
        1024 * 1024 * 1024,
    ) as usize
}

pub fn max_concurrent_requests(default: usize) -> usize {
    server_positive_u64(
        "CCP_MAX_CONCURRENT_REQUESTS",
        |server| server.max_concurrent_requests,
        default as u64,
        4096,
    ) as usize
}

pub fn max_concurrent_per_provider(default: usize) -> usize {
    server_positive_u64(
        "CCP_MAX_CONCURRENT_PER_PROVIDER",
        |server| server.max_concurrent_per_provider,
        default as u64,
        4096,
    ) as usize
}

pub fn max_concurrent_per_session(default: usize) -> usize {
    server_positive_u64(
        "CCP_MAX_CONCURRENT_PER_SESSION",
        |server| server.max_concurrent_per_session,
        default as u64,
        4096,
    ) as usize
}

pub fn request_body_idle_timeout_ms(default: u64) -> u64 {
    server_positive_u64(
        "CCP_REQUEST_BODY_IDLE_TIMEOUT_MS",
        |server| server.request_body_idle_timeout_ms,
        default,
        120_000,
    )
}

pub fn request_body_total_timeout_ms(default: u64) -> u64 {
    server_positive_u64(
        "CCP_REQUEST_BODY_TOTAL_TIMEOUT_MS",
        |server| server.request_body_total_timeout_ms,
        default,
        300_000,
    )
}

pub fn grok_base_url() -> String {
    let env = environment();
    if let Some(raw) = env.get("CCP_GROK_BASE_URL") {
        return raw.clone();
    }
    if let Some(grok) = read_file_config(&paths::config_dir()).and_then(|f| f.grok)
        && let Some(url) = grok.base_url
    {
        return url;
    }
    "https://cli-chat-proxy.grok.com/v1".to_string()
}

pub fn grok_client_version() -> String {
    let env = environment();
    if let Some(raw) = env.get("CCP_GROK_CLIENT_VERSION") {
        return raw.clone();
    }
    if let Some(grok) = read_file_config(&paths::config_dir()).and_then(|f| f.grok)
        && let Some(version) = grok.client_version
    {
        return version;
    }
    "0.2.93".to_string()
}

fn grok_positive_u64(
    env_key: &str,
    file_value: impl FnOnce(&GrokConfig) -> Option<u64>,
    default: u64,
) -> u64 {
    if let Ok(raw) = std::env::var(env_key)
        && let Ok(value) = raw.parse::<u64>()
        && value > 0
    {
        return value;
    }
    if let Some(grok) = read_file_config(&paths::config_dir()).and_then(|file| file.grok)
        && let Some(value) = file_value(&grok)
        && value > 0
    {
        return value;
    }
    default
}

pub fn grok_connect_timeout_ms(default: u64) -> u64 {
    grok_positive_u64(
        "CCP_GROK_CONNECT_TIMEOUT_MS",
        |grok| grok.connect_timeout_ms,
        default,
    )
}

pub fn grok_header_timeout_ms(default: u64) -> u64 {
    grok_positive_u64(
        "CCP_GROK_HEADER_TIMEOUT_MS",
        |grok| grok.header_timeout_ms,
        default,
    )
}

pub fn grok_first_byte_timeout_ms(default: u64) -> u64 {
    grok_positive_u64(
        "CCP_GROK_FIRST_BYTE_TIMEOUT_MS",
        |grok| grok.first_byte_timeout_ms,
        default,
    )
}

pub fn grok_body_idle_timeout_ms(default: u64) -> u64 {
    grok_positive_u64(
        "CCP_GROK_BODY_IDLE_TIMEOUT_MS",
        |grok| grok.body_idle_timeout_ms,
        default,
    )
}

pub fn grok_total_timeout_ms(default: u64) -> u64 {
    grok_positive_u64(
        "CCP_GROK_TOTAL_TIMEOUT_MS",
        |grok| grok.total_timeout_ms,
        default,
    )
}

pub fn grok_stream_heartbeat_ms(default: u64) -> u64 {
    grok_positive_u64(
        "CCP_GROK_STREAM_HEARTBEAT_MS",
        |grok| grok.stream_heartbeat_ms,
        default,
    )
}

pub fn is_verbose() -> bool {
    log_verbose()
}

// ---------------------------------------------------------------------------
// Codex config
// ---------------------------------------------------------------------------

pub fn codex_base_url(default: &str) -> String {
    let env = environment();
    if let Some(raw) = env.get("CCP_CODEX_BASE_URL") {
        return raw.clone();
    }
    if let Some(raw) = env.get("CLAUDE_CODE_PROXY_CODEX_BASE_URL") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(url) = codex.base_url
    {
        return url;
    }
    default.to_string()
}

pub fn codex_originator(default: &str) -> String {
    let env = environment();
    if let Some(raw) = env.get("CCP_CODEX_ORIGINATOR") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(val) = codex.originator
    {
        return val;
    }
    default.to_string()
}

pub fn codex_user_agent(default: &str) -> String {
    let env = environment();
    if let Some(raw) = env.get("CCP_CODEX_USER_AGENT") {
        return raw.clone();
    }
    if let Some(raw) = env.get("CCP_USER_AGENT") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(ua) = codex.user_agent
    {
        return ua;
    }
    default.to_string()
}

pub fn codex_previous_response_id() -> bool {
    if let Some(raw) = environment_value("CCP_CODEX_PREVIOUS_RESPONSE_ID") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(val) = codex.previous_response_id
    {
        return val;
    }
    false
}

/// Re-enables the legacy heuristic that turns complete-looking tool JSON into
/// a successful tool-use response after the upstream closes without an
/// authoritative terminal. This is intentionally strict opt-in: only the
/// literal environment value `true` enables it.
pub fn codex_unsafe_salvage_tool_call_on_close() -> bool {
    if let Some(raw) = environment_value("CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE") {
        return raw.eq_ignore_ascii_case("true");
    }
    read_file_config(&paths::config_dir())
        .and_then(|file| file.codex)
        .and_then(|codex| codex.unsafe_salvage_tool_call_on_close)
        .unwrap_or(false)
}

pub fn codex_service_tier() -> Option<String> {
    if let Some(raw) = environment_value("CCP_CODEX_SERVICE_TIER") {
        return Some(raw);
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.service_tier;
    }
    None
}

pub fn codex_responses_lite() -> bool {
    if let Ok(raw) = std::env::var("CCP_CODEX_RESPONSES_LITE") {
        match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
    }
    if let Some(codex) = read_file_config(&paths::config_dir()).and_then(|file| file.codex)
        && let Some(value) = codex.responses_lite
    {
        return value;
    }
    true
}

pub fn codex_parallel_tools() -> bool {
    if let Ok(raw) = std::env::var("CCP_CODEX_PARALLEL_TOOLS") {
        match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
    }
    if let Some(codex) = read_file_config(&paths::config_dir()).and_then(|file| file.codex)
        && let Some(value) = codex.parallel_tools
    {
        return value;
    }
    false
}

pub fn codex_effort() -> Option<String> {
    if let Some(raw) = environment_value("CCP_CODEX_EFFORT") {
        return Some(raw);
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.effort;
    }
    None
}

pub fn codex_reasoning_summary() -> Option<String> {
    if let Some(raw) =
        environment_value("CCP_CODEX_REASONING_SUMMARY").filter(|raw| !raw.is_empty())
    {
        return Some(raw);
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(summary) = codex.reasoning_summary.filter(|raw| !raw.is_empty())
    {
        return Some(summary);
    }
    None
}

pub fn codex_model() -> Option<String> {
    if let Some(raw) = environment_value("CCP_CODEX_MODEL") {
        return Some(raw);
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.model;
    }
    None
}

fn codex_positive_u64(
    env_key: &str,
    file_value: impl FnOnce(&CodexConfig) -> Option<u64>,
    default: u64,
) -> u64 {
    if let Ok(raw) = std::env::var(env_key)
        && let Ok(value) = raw.parse::<u64>()
        && value > 0
    {
        return value;
    }
    if let Some(codex) = read_file_config(&paths::config_dir()).and_then(|file| file.codex)
        && let Some(value) = file_value(&codex)
        && value > 0
    {
        return value;
    }
    default
}

pub fn codex_websocket_response_start_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS",
        |codex| codex.websocket_response_start_timeout_ms,
        default,
    )
}

pub fn codex_connect_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_CONNECT_TIMEOUT_MS",
        |codex| codex.connect_timeout_ms,
        default,
    )
}

pub fn codex_header_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_HEADER_TIMEOUT_MS",
        |codex| codex.header_timeout_ms,
        default,
    )
}

pub fn codex_http_first_byte_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS",
        |codex| codex.http_first_byte_timeout_ms,
        default,
    )
}

pub fn codex_body_idle_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_BODY_IDLE_TIMEOUT_MS",
        |codex| codex.body_idle_timeout_ms,
        default,
    )
}

pub fn codex_total_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_TOTAL_TIMEOUT_MS",
        |codex| codex.total_timeout_ms,
        default,
    )
}

pub fn codex_stream_heartbeat_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_STREAM_HEARTBEAT_MS",
        |codex| codex.stream_heartbeat_ms,
        default,
    )
}

pub fn codex_websocket_idle_timeout_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_WEBSOCKET_IDLE_TIMEOUT_MS",
        |codex| codex.websocket_idle_timeout_ms,
        default,
    )
}

pub fn codex_max_idle_websockets(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_MAX_IDLE_WEBSOCKETS",
        |codex| codex.max_idle_websockets,
        default,
    )
}

pub fn codex_idle_websocket_ttl_ms(default: u64) -> u64 {
    codex_positive_u64(
        "CCP_CODEX_IDLE_WEBSOCKET_TTL_MS",
        |codex| codex.idle_websocket_ttl_ms,
        default,
    )
}

// ---------------------------------------------------------------------------
// Codex transport config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexTransport {
    Http,
    WebSocket,
    Auto,
}

impl CodexTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            CodexTransport::Http => "http",
            CodexTransport::WebSocket => "websocket",
            CodexTransport::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexTransportDecision {
    requested: CodexTransport,
    effective: CodexTransport,
    reason: &'static str,
    explicit_websocket_environment_proxy_unsupported: bool,
}

impl CodexTransportDecision {
    pub(crate) fn resolve(
        requested: CodexTransport,
        system_proxy_intercepts_upstream: bool,
        environment_proxy_intercepts_upstream: bool,
    ) -> Self {
        let explicit_websocket_environment_proxy_unsupported =
            requested == CodexTransport::WebSocket && environment_proxy_intercepts_upstream;
        if requested == CodexTransport::Auto && system_proxy_intercepts_upstream {
            Self {
                requested,
                effective: CodexTransport::Http,
                reason: "system_proxy",
                explicit_websocket_environment_proxy_unsupported,
            }
        } else {
            Self {
                requested,
                effective: requested,
                reason: if explicit_websocket_environment_proxy_unsupported {
                    "environment_proxy_unsupported"
                } else if requested == CodexTransport::Auto {
                    "automatic"
                } else {
                    "explicit"
                },
                explicit_websocket_environment_proxy_unsupported,
            }
        }
    }

    pub fn requested(self) -> CodexTransport {
        self.requested
    }

    pub fn effective(self) -> CodexTransport {
        self.effective
    }

    pub fn reason(self) -> &'static str {
        self.reason
    }

    pub fn explicit_websocket_environment_proxy_unsupported(self) -> bool {
        self.explicit_websocket_environment_proxy_unsupported
    }
}

fn parse_codex_transport(raw: &str) -> Option<CodexTransport> {
    match raw {
        "http" => Some(CodexTransport::Http),
        "websocket" => Some(CodexTransport::WebSocket),
        "auto" => Some(CodexTransport::Auto),
        _ => None,
    }
}

fn codex_proxy_match_uri(upstream: &str) -> Option<http::Uri> {
    let mut url = url::Url::parse(upstream).ok()?;
    let http_scheme = match url.scheme() {
        "ws" => Some("http"),
        "wss" => Some("https"),
        _ => None,
    };
    if let Some(http_scheme) = http_scheme {
        url.set_scheme(http_scheme).ok()?;
    }
    url.as_str().parse().ok()
}

fn codex_system_proxy_intercepts_upstream(upstream: &str) -> bool {
    if crate::oauth_http::is_loopback_url(upstream) {
        return false;
    }
    let Some(uri) = codex_proxy_match_uri(upstream) else {
        return false;
    };
    hyper_util::client::proxy::matcher::Matcher::from_system()
        .intercept(&uri)
        .is_some()
}

fn codex_environment_proxy_intercepts_upstream(upstream: &str) -> bool {
    if crate::oauth_http::is_loopback_url(upstream) {
        return false;
    }
    let Some(uri) = codex_proxy_match_uri(upstream) else {
        return false;
    };
    hyper_util::client::proxy::matcher::Matcher::from_env()
        .intercept(&uri)
        .is_some()
}

fn requested_codex_transport() -> CodexTransport {
    if let Some(raw) = environment_value("CCP_CODEX_TRANSPORT")
        && let Some(transport) = parse_codex_transport(&raw)
    {
        return transport;
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(transport) = codex.transport.as_deref().and_then(parse_codex_transport)
    {
        return transport;
    }
    CodexTransport::Auto
}

/// Capture the Codex transport and system-proxy decision for one client lifetime.
///
/// Reqwest snapshots its system proxy matcher when a client is built, so callers
/// should store this decision next to that client instead of probing again for
/// every request.
pub fn codex_transport_decision() -> CodexTransportDecision {
    const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
    let upstream = codex_base_url(DEFAULT_CODEX_BASE_URL);
    codex_transport_decision_for_url(&upstream)
}

pub(crate) fn codex_transport_decision_for_url(upstream: &str) -> CodexTransportDecision {
    let requested = requested_codex_transport();
    let system_proxy_intercepts_upstream =
        requested == CodexTransport::Auto && codex_system_proxy_intercepts_upstream(upstream);
    let environment_proxy_intercepts_upstream = requested == CodexTransport::WebSocket
        && codex_environment_proxy_intercepts_upstream(upstream);
    CodexTransportDecision::resolve(
        requested,
        system_proxy_intercepts_upstream,
        environment_proxy_intercepts_upstream,
    )
}

pub fn codex_transport() -> CodexTransport {
    codex_transport_decision().effective()
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    const TEST_ENV_KEYS: &[&str] = &[
        "CCP_ALLOW_REMOTE_UNAUTHENTICATED",
        "CCP_BIND_ADDRESS",
        "CCP_CODEX_TRANSPORT",
        "CCP_CONFIG_DIR",
        "CCP_LOG_VERBOSE",
        "CCP_LOG_STDERR",
        "CCP_CODEX_MODEL",
        "CCP_CODEX_SERVICE_TIER",
        "CCP_CODEX_EFFORT",
        "CCP_CODEX_PREVIOUS_RESPONSE_ID",
        "CCP_CODEX_REASONING_SUMMARY",
        "CCP_CODEX_RESPONSES_LITE",
        "CCP_CODEX_PARALLEL_TOOLS",
        "CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE",
        "CCP_CODEX_CONNECT_TIMEOUT_MS",
        "CCP_CODEX_HEADER_TIMEOUT_MS",
        "CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS",
        "CCP_CODEX_BODY_IDLE_TIMEOUT_MS",
        "CCP_CODEX_TOTAL_TIMEOUT_MS",
        "CCP_CODEX_STREAM_HEARTBEAT_MS",
        "CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS",
        "CCP_CODEX_WEBSOCKET_IDLE_TIMEOUT_MS",
        "CCP_CODEX_MAX_IDLE_WEBSOCKETS",
        "CCP_CODEX_IDLE_WEBSOCKET_TTL_MS",
        "CCP_GROK_CONNECT_TIMEOUT_MS",
        "CCP_GROK_HEADER_TIMEOUT_MS",
        "CCP_GROK_FIRST_BYTE_TIMEOUT_MS",
        "CCP_GROK_BODY_IDLE_TIMEOUT_MS",
        "CCP_GROK_TOTAL_TIMEOUT_MS",
        "CCP_GROK_STREAM_HEARTBEAT_MS",
        "CCP_MAX_REQUEST_BODY_BYTES",
        "CCP_MAX_BUFFERED_REQUEST_BYTES",
        "CCP_MAX_CONCURRENT_REQUESTS",
        "CCP_MAX_CONCURRENT_PER_PROVIDER",
        "CCP_MAX_CONCURRENT_PER_SESSION",
        "CCP_REQUEST_BODY_IDLE_TIMEOUT_MS",
        "CCP_REQUEST_BODY_TOTAL_TIMEOUT_MS",
    ];

    struct ClearedEnvGuard {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    fn clear_env() -> ClearedEnvGuard {
        let previous = TEST_ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        unsafe {
            for key in TEST_ENV_KEYS {
                std::env::remove_var(key);
            }
        }
        ClearedEnvGuard { previous }
    }

    impl Drop for ClearedEnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in self.previous.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    fn bind_address_defaults_to_loopback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(load_config().bind_address, "127.0.0.1");
    }

    #[test]
    fn bind_address_reads_config_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"bindAddress":"192.0.2.10"}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(load_config().bind_address, "192.0.2.10");
        let _bind_env = EnvGuard::set("CCP_BIND_ADDRESS", "0.0.0.0");
        assert_eq!(load_config().bind_address, "0.0.0.0");
    }

    #[test]
    fn remote_unauthenticated_ack_is_explicit_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"server":{"allowRemoteUnauthenticated":true}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(allow_remote_unauthenticated());
        let _ack_env = EnvGuard::set("CCP_ALLOW_REMOTE_UNAUTHENTICATED", "false");
        assert!(!allow_remote_unauthenticated());
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn file_config_cache_avoids_hot_path_io_and_refreshes_as_one_generation() {
        let config = tempfile::TempDir::new().unwrap();
        let path = config.path().join("config.json");
        std::fs::write(&path, r#"{"port":18765}"#).unwrap();

        let mut cache = FileConfigCache::default();
        let start = Instant::now();
        let refresh_interval = Duration::from_secs(1);
        let first = cache.load(&path, start, refresh_interval);
        assert_eq!(first.value.as_ref().and_then(|file| file.port), Some(18765));
        assert_eq!(first.generation, 1);
        assert_eq!(cache.filesystem_checks, 1);
        assert_eq!(cache.parse_attempts, 1);

        // A resolver burst observes the same immutable generation without
        // metadata calls or JSON parsing, even if an editor writes meanwhile.
        std::fs::write(&path, r#"{"port":2876}"#).unwrap();
        for elapsed_ms in [1, 10, 100, 999] {
            let cached = cache.load(
                &path,
                start + Duration::from_millis(elapsed_ms),
                refresh_interval,
            );
            assert_eq!(
                cached.value.as_ref().and_then(|file| file.port),
                Some(18765)
            );
            assert_eq!(cached.generation, first.generation);
        }
        assert_eq!(cache.filesystem_checks, 1);
        assert_eq!(cache.parse_attempts, 1);

        let refreshed = cache.load(&path, start + refresh_interval, refresh_interval);
        assert_eq!(
            refreshed.value.as_ref().and_then(|file| file.port),
            Some(2876)
        );
        assert_eq!(refreshed.generation, first.generation + 1);
        assert_eq!(cache.filesystem_checks, 2);
        assert_eq!(cache.parse_attempts, 2);
    }

    #[test]
    fn logging_config_uses_one_file_generation() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"log":{"verbose":true,"stderr":false}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        let loaded = load_log_config();
        assert!(loaded.verbose);
        assert!(!loaded.stderr);
        assert!(loaded.generation > 0);
    }

    #[test]
    fn cleared_env_restores_existing_config_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let sentinel = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", sentinel.path());

        {
            let _cleared_env = clear_env();
            assert!(std::env::var_os("CCP_CONFIG_DIR").is_none());
        }

        assert_eq!(
            std::env::var_os("CCP_CONFIG_DIR").as_deref(),
            Some(sentinel.path().as_os_str())
        );
    }

    #[test]
    fn codex_transport_defaults_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _no_proxy = EnvGuard::set("NO_PROXY", "*");
        let result = codex_transport();
        assert_eq!(result, CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_uses_http_when_system_proxy_intercepts() {
        let automatic = CodexTransportDecision::resolve(CodexTransport::Auto, true, false);
        assert_eq!(automatic.requested(), CodexTransport::Auto);
        assert_eq!(automatic.effective(), CodexTransport::Http);
        assert_eq!(automatic.reason(), "system_proxy");
        assert!(!automatic.explicit_websocket_environment_proxy_unsupported());

        let explicit = CodexTransportDecision::resolve(CodexTransport::WebSocket, true, false);
        assert_eq!(explicit.effective(), CodexTransport::WebSocket);
        assert_eq!(explicit.reason(), "explicit");
        assert!(!explicit.explicit_websocket_environment_proxy_unsupported());
    }

    #[test]
    fn codex_explicit_websocket_detects_environment_proxy() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
        let _proxy = EnvGuard::set("HTTPS_PROXY", "http://127.0.0.1:18080");
        let _no_proxy = EnvGuard::set("NO_PROXY", "");

        let decision = codex_transport_decision();
        assert_eq!(decision.requested(), CodexTransport::WebSocket);
        assert_eq!(decision.effective(), CodexTransport::WebSocket);
        assert_eq!(decision.reason(), "environment_proxy_unsupported");
        assert!(decision.explicit_websocket_environment_proxy_unsupported());

        let websocket_decision =
            codex_transport_decision_for_url("wss://chatgpt.com/backend-api/codex/responses");
        assert_eq!(websocket_decision.reason(), "environment_proxy_unsupported");
        assert!(websocket_decision.explicit_websocket_environment_proxy_unsupported());
    }

    #[test]
    fn codex_explicit_websocket_environment_proxy_honors_no_proxy_and_loopback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
        let _proxy = EnvGuard::set("HTTPS_PROXY", "http://127.0.0.1:18080");
        let _no_proxy = EnvGuard::set("NO_PROXY", "chatgpt.com");

        let excluded = codex_transport_decision();
        assert_eq!(excluded.reason(), "explicit");
        assert!(!excluded.explicit_websocket_environment_proxy_unsupported());

        let websocket_excluded =
            codex_transport_decision_for_url("wss://chatgpt.com/backend-api/codex/responses");
        assert_eq!(websocket_excluded.reason(), "explicit");
        assert!(!websocket_excluded.explicit_websocket_environment_proxy_unsupported());

        let _all_proxy = EnvGuard::set("ALL_PROXY", "http://127.0.0.1:18081");
        let _no_proxy = EnvGuard::set("NO_PROXY", "");
        let loopback = codex_transport_decision_for_url("ws://127.0.0.1:18765/responses");
        assert_eq!(loopback.reason(), "explicit");
        assert!(!loopback.explicit_websocket_environment_proxy_unsupported());
    }

    #[test]
    fn codex_auto_transport_honors_https_proxy_environment() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "auto");
        let _proxy = EnvGuard::set("HTTPS_PROXY", "http://127.0.0.1:18080");
        let _no_proxy = EnvGuard::set("NO_PROXY", "");

        let decision = codex_transport_decision();
        assert_eq!(decision.requested(), CodexTransport::Auto);
        assert_eq!(decision.effective(), CodexTransport::Http);
        assert_eq!(decision.reason(), "system_proxy");
    }

    #[test]
    fn codex_auto_transport_honors_no_proxy_exclusion() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "auto");
        let _proxy = EnvGuard::set("HTTPS_PROXY", "http://127.0.0.1:18080");
        let _no_proxy = EnvGuard::set("NO_PROXY", "chatgpt.com");

        let decision = codex_transport_decision();
        assert_eq!(decision.effective(), CodexTransport::Auto);
        assert_eq!(decision.reason(), "automatic");
    }

    #[test]
    fn codex_transport_reads_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _no_proxy = EnvGuard::set("NO_PROXY", "*");
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "auto");
        }
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_env_websocket() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "websocket");
        }
        assert_eq!(codex_transport(), CodexTransport::WebSocket);
    }

    #[test]
    fn codex_transport_invalid_env_falls_back_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _no_proxy = EnvGuard::set("NO_PROXY", "*");
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "invalid");
        }
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_empty_env_falls_back_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _no_proxy = EnvGuard::set("NO_PROXY", "*");
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "");
        }
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn parse_codex_transport_variants() {
        assert_eq!(parse_codex_transport("http"), Some(CodexTransport::Http));
        assert_eq!(
            parse_codex_transport("websocket"),
            Some(CodexTransport::WebSocket)
        );
        assert_eq!(parse_codex_transport("auto"), Some(CodexTransport::Auto));
        assert_eq!(parse_codex_transport(""), None);
        assert_eq!(parse_codex_transport("HTTP"), None);
        assert_eq!(parse_codex_transport("ws"), None);
    }

    #[test]
    fn codex_transport_as_str() {
        assert_eq!(CodexTransport::Http.as_str(), "http");
        assert_eq!(CodexTransport::WebSocket.as_str(), "websocket");
        assert_eq!(CodexTransport::Auto.as_str(), "auto");
    }

    #[test]
    fn log_env_presence_enables_legacy_verbose_and_stderr() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _verbose_env = EnvGuard::set("CCP_LOG_VERBOSE", "0");
        let _stderr_env = EnvGuard::set("CCP_LOG_STDERR", "");

        let loaded = load_config();
        assert!(loaded.log_verbose);
        assert!(loaded.log_stderr);
    }

    #[test]
    fn log_config_values_apply_without_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"log":{"verbose":true,"stderr":true}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        let loaded = load_config();
        assert!(loaded.log_verbose);
        assert!(loaded.log_stderr);
    }

    #[test]
    fn codex_reasoning_summary_reads_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"reasoningSummary":"off"}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(codex_reasoning_summary().as_deref(), Some("off"));
    }

    #[test]
    fn codex_responses_lite_defaults_true_and_reads_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(codex_responses_lite());
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"responsesLite":false}}"#,
        )
        .unwrap();
        assert!(!codex_responses_lite());

        let _lite_env = EnvGuard::set("CCP_CODEX_RESPONSES_LITE", "true");
        assert!(codex_responses_lite());
    }

    #[test]
    fn codex_parallel_tools_defaults_false_and_reads_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(!codex_parallel_tools());
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"parallelTools":true}}"#,
        )
        .unwrap();
        assert!(codex_parallel_tools());

        let _parallel_env = EnvGuard::set("CCP_CODEX_PARALLEL_TOOLS", "false");
        assert!(!codex_parallel_tools());
    }

    #[test]
    fn unsafe_tool_close_salvage_is_strict_opt_in_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(!codex_unsafe_salvage_tool_call_on_close());
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"unsafeSalvageToolCallOnClose":true}}"#,
        )
        .unwrap();
        assert!(codex_unsafe_salvage_tool_call_on_close());

        {
            let _unsafe_env = EnvGuard::set("CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE", "1");
            assert!(!codex_unsafe_salvage_tool_call_on_close());
        }
        {
            let _unsafe_env = EnvGuard::set("CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE", "true");
            assert!(codex_unsafe_salvage_tool_call_on_close());
            let summary = config_override_summary_lines(&load_config());
            assert!(summary.iter().any(|line| {
                line.contains("CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE (env)")
            }));
        }
    }

    #[test]
    fn codex_reasoning_summary_env_overrides_config_and_empty_falls_through() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"reasoningSummary":"off"}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        {
            let _summary_env = EnvGuard::set("CCP_CODEX_REASONING_SUMMARY", "auto");
            assert_eq!(codex_reasoning_summary().as_deref(), Some("auto"));
        }
        {
            let _summary_env = EnvGuard::set("CCP_CODEX_REASONING_SUMMARY", "");
            assert_eq!(codex_reasoning_summary().as_deref(), Some("off"));
        }
    }

    #[test]
    fn codex_hot_path_environment_values_remain_dynamic_and_override_file_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"model":"file-model","serviceTier":"flex","effort":"medium","reasoningSummary":"auto","previousResponseId":true,"transport":"http"}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(codex_model().as_deref(), Some("file-model"));
        assert_eq!(codex_service_tier().as_deref(), Some("flex"));
        assert_eq!(codex_effort().as_deref(), Some("medium"));
        assert_eq!(codex_reasoning_summary().as_deref(), Some("auto"));
        assert!(codex_previous_response_id());
        assert_eq!(requested_codex_transport(), CodexTransport::Http);

        {
            let _model = EnvGuard::set("CCP_CODEX_MODEL", "env-model");
            let _tier = EnvGuard::set("CCP_CODEX_SERVICE_TIER", "priority");
            let _effort = EnvGuard::set("CCP_CODEX_EFFORT", "high");
            let _summary = EnvGuard::set("CCP_CODEX_REASONING_SUMMARY", "detailed");
            let _continuation = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "false");
            let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

            assert_eq!(codex_model().as_deref(), Some("env-model"));
            assert_eq!(codex_service_tier().as_deref(), Some("priority"));
            assert_eq!(codex_effort().as_deref(), Some("high"));
            assert_eq!(codex_reasoning_summary().as_deref(), Some("detailed"));
            assert!(!codex_previous_response_id());
            assert_eq!(requested_codex_transport(), CodexTransport::WebSocket);
        }

        assert_eq!(codex_model().as_deref(), Some("file-model"));
        assert_eq!(codex_service_tier().as_deref(), Some("flex"));
        assert_eq!(codex_effort().as_deref(), Some("medium"));
        assert_eq!(codex_reasoning_summary().as_deref(), Some("auto"));
        assert!(codex_previous_response_id());
        assert_eq!(requested_codex_transport(), CodexTransport::Http);
    }

    #[test]
    fn codex_timeouts_read_config_and_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"connectTimeoutMs":11000,"headerTimeoutMs":22000,"httpFirstByteTimeoutMs":33000,"bodyIdleTimeoutMs":130000,"totalTimeoutMs":150000,"streamHeartbeatMs":6000,"websocketResponseStartTimeoutMs":45000,"websocketIdleTimeoutMs":180000,"maxIdleWebSockets":64,"idleWebSocketTtlMs":240000}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(codex_connect_timeout_ms(1), 11_000);
        assert_eq!(codex_header_timeout_ms(1), 22_000);
        assert_eq!(codex_http_first_byte_timeout_ms(1), 33_000);
        assert_eq!(codex_body_idle_timeout_ms(1), 130_000);
        assert_eq!(codex_total_timeout_ms(1), 150_000);
        assert_eq!(codex_stream_heartbeat_ms(1), 6_000);
        assert_eq!(codex_websocket_response_start_timeout_ms(1), 45_000);
        assert_eq!(codex_websocket_idle_timeout_ms(1), 180_000);
        assert_eq!(codex_max_idle_websockets(1), 64);
        assert_eq!(codex_idle_websocket_ttl_ms(1), 240_000);

        let _connect_env = EnvGuard::set("CCP_CODEX_CONNECT_TIMEOUT_MS", "9000");
        let _header_env = EnvGuard::set("CCP_CODEX_HEADER_TIMEOUT_MS", "18000");
        let _first_byte_env = EnvGuard::set("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS", "27000");
        let _body_idle_env = EnvGuard::set("CCP_CODEX_BODY_IDLE_TIMEOUT_MS", "100000");
        let _total_env = EnvGuard::set("CCP_CODEX_TOTAL_TIMEOUT_MS", "120000");
        let _heartbeat_env = EnvGuard::set("CCP_CODEX_STREAM_HEARTBEAT_MS", "7000");
        let _start_env = EnvGuard::set("CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS", "12000");
        let _idle_env = EnvGuard::set("CCP_CODEX_WEBSOCKET_IDLE_TIMEOUT_MS", "90000");
        let _pool_cap_env = EnvGuard::set("CCP_CODEX_MAX_IDLE_WEBSOCKETS", "32");
        let _pool_ttl_env = EnvGuard::set("CCP_CODEX_IDLE_WEBSOCKET_TTL_MS", "120000");
        assert_eq!(codex_connect_timeout_ms(1), 9_000);
        assert_eq!(codex_header_timeout_ms(1), 18_000);
        assert_eq!(codex_http_first_byte_timeout_ms(1), 27_000);
        assert_eq!(codex_body_idle_timeout_ms(1), 100_000);
        assert_eq!(codex_total_timeout_ms(1), 120_000);
        assert_eq!(codex_stream_heartbeat_ms(1), 7_000);
        assert_eq!(codex_websocket_response_start_timeout_ms(1), 12_000);
        assert_eq!(codex_websocket_idle_timeout_ms(1), 90_000);
        assert_eq!(codex_max_idle_websockets(1), 32);
        assert_eq!(codex_idle_websocket_ttl_ms(1), 120_000);
    }

    #[test]
    fn codex_http_first_byte_timeout_can_inherit_body_idle_timeout() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"bodyIdleTimeoutMs":130000}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        let body_idle_timeout_ms = codex_body_idle_timeout_ms(300_000);
        assert_eq!(body_idle_timeout_ms, 130_000);
        assert_eq!(
            codex_http_first_byte_timeout_ms(body_idle_timeout_ms),
            body_idle_timeout_ms
        );
    }

    #[test]
    fn codex_timeouts_ignore_zero_and_invalid_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"connectTimeoutMs":0,"headerTimeoutMs":0,"httpFirstByteTimeoutMs":0,"bodyIdleTimeoutMs":0,"totalTimeoutMs":0,"streamHeartbeatMs":0,"websocketResponseStartTimeoutMs":0,"websocketIdleTimeoutMs":0,"maxIdleWebSockets":0,"idleWebSocketTtlMs":0}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _connect_env = EnvGuard::set("CCP_CODEX_CONNECT_TIMEOUT_MS", "invalid");
        let _header_env = EnvGuard::set("CCP_CODEX_HEADER_TIMEOUT_MS", "invalid");
        let _first_byte_env = EnvGuard::set("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS", "invalid");
        let _body_idle_env = EnvGuard::set("CCP_CODEX_BODY_IDLE_TIMEOUT_MS", "invalid");
        let _start_env = EnvGuard::set("CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS", "invalid");
        let _total_env = EnvGuard::set("CCP_CODEX_TOTAL_TIMEOUT_MS", "invalid");
        let _heartbeat_env = EnvGuard::set("CCP_CODEX_STREAM_HEARTBEAT_MS", "invalid");
        let _pool_cap_env = EnvGuard::set("CCP_CODEX_MAX_IDLE_WEBSOCKETS", "invalid");
        let _pool_ttl_env = EnvGuard::set("CCP_CODEX_IDLE_WEBSOCKET_TTL_MS", "invalid");

        assert_eq!(codex_connect_timeout_ms(15_000), 15_000);
        assert_eq!(codex_header_timeout_ms(60_000), 60_000);
        assert_eq!(codex_http_first_byte_timeout_ms(60_000), 60_000);
        assert_eq!(codex_body_idle_timeout_ms(300_000), 300_000);
        assert_eq!(codex_total_timeout_ms(540_000), 540_000);
        assert_eq!(codex_stream_heartbeat_ms(5_000), 5_000);
        assert_eq!(codex_websocket_response_start_timeout_ms(45_000), 45_000);
        assert_eq!(codex_websocket_idle_timeout_ms(180_000), 180_000);
        assert_eq!(codex_max_idle_websockets(128), 128);
        assert_eq!(codex_idle_websocket_ttl_ms(300_000), 300_000);
    }

    #[test]
    fn codex_http_timeouts_appear_in_override_summary() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"connectTimeoutMs":15000,"headerTimeoutMs":60000,"httpFirstByteTimeoutMs":60000,"bodyIdleTimeoutMs":300000,"streamHeartbeatMs":5000}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let loaded = load_config();
        let summary = config_override_summary_lines(&loaded);
        assert!(
            summary
                .iter()
                .any(|line| line.contains("codex.connectTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("codex.headerTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("codex.httpFirstByteTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("codex.bodyIdleTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("codex.streamHeartbeatMs"))
        );

        let _connect_env = EnvGuard::set("CCP_CODEX_CONNECT_TIMEOUT_MS", "12000");
        let _first_byte_env = EnvGuard::set("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS", "30000");
        let _heartbeat_env = EnvGuard::set("CCP_CODEX_STREAM_HEARTBEAT_MS", "4000");
        let summary = config_override_summary_lines(&loaded);
        assert!(
            summary
                .iter()
                .any(|line| line.contains("CCP_CODEX_CONNECT_TIMEOUT_MS (env)"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS (env)"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("CCP_CODEX_STREAM_HEARTBEAT_MS (env)"))
        );
    }

    #[test]
    fn grok_timeouts_read_config_and_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"grok":{"connectTimeoutMs":11000,"headerTimeoutMs":22000,"firstByteTimeoutMs":33000,"bodyIdleTimeoutMs":44000,"totalTimeoutMs":55000,"streamHeartbeatMs":6000}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(grok_connect_timeout_ms(1), 11_000);
        assert_eq!(grok_header_timeout_ms(1), 22_000);
        assert_eq!(grok_first_byte_timeout_ms(1), 33_000);
        assert_eq!(grok_body_idle_timeout_ms(1), 44_000);
        assert_eq!(grok_total_timeout_ms(1), 55_000);
        assert_eq!(grok_stream_heartbeat_ms(1), 6_000);

        let _connect_env = EnvGuard::set("CCP_GROK_CONNECT_TIMEOUT_MS", "15000");
        let _header_env = EnvGuard::set("CCP_GROK_HEADER_TIMEOUT_MS", "25000");
        let _first_env = EnvGuard::set("CCP_GROK_FIRST_BYTE_TIMEOUT_MS", "35000");
        let _idle_env = EnvGuard::set("CCP_GROK_BODY_IDLE_TIMEOUT_MS", "45000");
        let _total_env = EnvGuard::set("CCP_GROK_TOTAL_TIMEOUT_MS", "56000");
        let _heartbeat_env = EnvGuard::set("CCP_GROK_STREAM_HEARTBEAT_MS", "7000");
        assert_eq!(grok_connect_timeout_ms(1), 15_000);
        assert_eq!(grok_header_timeout_ms(1), 25_000);
        assert_eq!(grok_first_byte_timeout_ms(1), 35_000);
        assert_eq!(grok_body_idle_timeout_ms(1), 45_000);
        assert_eq!(grok_total_timeout_ms(1), 56_000);
        assert_eq!(grok_stream_heartbeat_ms(1), 7_000);
    }

    #[test]
    fn grok_timeouts_ignore_zero_and_invalid_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"grok":{"connectTimeoutMs":0,"headerTimeoutMs":0,"firstByteTimeoutMs":0,"bodyIdleTimeoutMs":0,"totalTimeoutMs":0,"streamHeartbeatMs":0}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let _connect_env = EnvGuard::set("CCP_GROK_CONNECT_TIMEOUT_MS", "invalid");
        let _total_env = EnvGuard::set("CCP_GROK_TOTAL_TIMEOUT_MS", "invalid");

        assert_eq!(grok_connect_timeout_ms(10_000), 10_000);
        assert_eq!(grok_header_timeout_ms(60_000), 60_000);
        assert_eq!(grok_first_byte_timeout_ms(60_000), 60_000);
        assert_eq!(grok_body_idle_timeout_ms(300_000), 300_000);
        assert_eq!(grok_total_timeout_ms(540_000), 540_000);
        assert_eq!(grok_stream_heartbeat_ms(5_000), 5_000);
    }

    #[test]
    fn grok_timeouts_appear_in_override_summary() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"grok":{"connectTimeoutMs":10000,"headerTimeoutMs":60000,"firstByteTimeoutMs":60000,"bodyIdleTimeoutMs":300000,"totalTimeoutMs":540000,"streamHeartbeatMs":5000}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
        let loaded = load_config();
        let summary = config_override_summary_lines(&loaded);
        assert!(
            summary
                .iter()
                .any(|line| line.contains("grok.connectTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("grok.headerTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("grok.firstByteTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("grok.bodyIdleTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("grok.totalTimeoutMs"))
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("grok.streamHeartbeatMs"))
        );

        let _connect_env = EnvGuard::set("CCP_GROK_CONNECT_TIMEOUT_MS", "12000");
        let summary = config_override_summary_lines(&loaded);
        assert!(
            summary
                .iter()
                .any(|line| line.contains("CCP_GROK_CONNECT_TIMEOUT_MS (env)"))
        );
    }

    #[test]
    fn server_resource_limits_read_config_env_and_clamp() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _cleared_env = clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"server":{"maxRequestBodyBytes":1048576,"maxBufferedRequestBytes":8388608,"maxConcurrentRequests":64,"maxConcurrentPerProvider":32,"maxConcurrentPerSession":8,"requestBodyIdleTimeoutMs":1500,"requestBodyTotalTimeoutMs":5000}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(max_request_body_bytes(1), 1_048_576);
        assert_eq!(max_buffered_request_bytes(1), 8_388_608);
        assert_eq!(max_concurrent_requests(1), 64);
        assert_eq!(max_concurrent_per_provider(1), 32);
        assert_eq!(max_concurrent_per_session(1), 8);
        assert_eq!(request_body_idle_timeout_ms(1), 1_500);
        assert_eq!(request_body_total_timeout_ms(1), 5_000);

        let _global = EnvGuard::set("CCP_MAX_CONCURRENT_REQUESTS", "999999");
        let _wait = EnvGuard::set("CCP_REQUEST_BODY_IDLE_TIMEOUT_MS", "999999");
        assert_eq!(max_concurrent_requests(1), 4096);
        assert_eq!(request_body_idle_timeout_ms(1), 120_000);
    }
}
