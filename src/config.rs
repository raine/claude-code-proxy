use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasProvider {
    Codex,
    Kimi,
}

impl AliasProvider {
    pub fn as_str(&self) -> &str {
        match self {
            AliasProvider::Codex => "codex",
            AliasProvider::Kimi => "kimi",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub bind_address: String,
    pub port: u16,
    pub alias_provider: AliasProvider,
    pub log_verbose: bool,
    pub log_stderr: bool,
    pub config_dir: PathBuf,
}

#[derive(Deserialize)]
struct FileConfig {
    #[serde(rename = "bindAddress")]
    pub bind_address: Option<String>,
    pub port: Option<u16>,
    #[serde(rename = "aliasProvider")]
    pub alias_provider: Option<String>,
    #[serde(rename = "autoReviewModel")]
    pub auto_review_model: Option<String>,
    pub log: Option<FileLog>,
    pub kimi: Option<KimiConfig>,
    pub codex: Option<CodexConfig>,
    pub cursor: Option<CursorConfig>,
    pub grok: Option<GrokConfig>,
    pub opencode: Option<OpenCodeConfig>,
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
    #[serde(rename = "serverCompaction")]
    pub server_compaction: Option<bool>,
    #[serde(rename = "responsesApi")]
    pub responses_api: Option<bool>,
    #[serde(rename = "imagesApi")]
    pub images_api: Option<bool>,
    #[serde(rename = "imagesBaseUrl")]
    pub images_base_url: Option<String>,
    #[serde(rename = "transcriptionsApi")]
    pub transcriptions_api: Option<bool>,
    #[serde(rename = "serviceTier")]
    pub service_tier: Option<String>,
    #[serde(rename = "reasoningSummary")]
    pub reasoning_summary: Option<String>,
    #[serde(rename = "effort")]
    pub effort: Option<String>,
    #[serde(rename = "model")]
    pub model: Option<String>,
    pub transport: Option<String>,
}

#[derive(Deserialize, Clone)]
struct CursorConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "clientVersion")]
    pub client_version: Option<String>,
    #[serde(rename = "agentBundle")]
    pub agent_bundle: Option<String>,
}

#[derive(Deserialize, Clone)]
struct KimiConfig {
    #[serde(rename = "userAgent")]
    pub user_agent: Option<String>,
    #[serde(rename = "oauthHost")]
    pub oauth_host: Option<String>,
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
}

#[derive(Deserialize, Clone)]
struct GrokConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "clientVersion")]
    pub client_version: Option<String>,
}

#[derive(Deserialize, Clone)]
struct OpenCodeConfig {
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
}

#[derive(Deserialize)]
struct FileLog {
    pub verbose: Option<bool>,
    pub stderr: Option<bool>,
}

fn parse_alias(raw: &str) -> Option<AliasProvider> {
    match raw {
        "codex" => Some(AliasProvider::Codex),
        "kimi" => Some(AliasProvider::Kimi),
        _ => None,
    }
}

fn read_file_config(config_dir: &Path) -> Option<FileConfig> {
    let path = config_dir.join("config.json");
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn load_config() -> LoadedConfig {
    let env = paths::DirResolverEnv::default();
    let config_dir = paths::resolve_config_dir(&env);
    load_config_from_env(&env.env, config_dir)
}

pub fn load_config_for_env(env: &HashMap<String, String>) -> LoadedConfig {
    let home = env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .cloned()
        .unwrap_or_else(|| "/".to_string());
    let resolver_env = paths::DirResolverEnv {
        platform: std::env::consts::OS.to_string(),
        env: env.clone(),
        home,
    };
    let config_dir = paths::resolve_config_dir(&resolver_env);
    load_config_from_env(env, config_dir)
}

fn load_config_from_env(env: &HashMap<String, String>, config_dir: PathBuf) -> LoadedConfig {
    let file = read_file_config(&config_dir);

    let mut out = LoadedConfig {
        bind_address: "127.0.0.1".to_string(),
        port: 18765,
        alias_provider: AliasProvider::Codex,
        log_verbose: false,
        log_stderr: false,
        config_dir: config_dir.clone(),
    };

    if let Some(raw) = env.get("CCP_BIND_ADDRESS") {
        out.bind_address = raw.clone();
    } else if let Some(bind_address) = file.as_ref().and_then(|f| f.bind_address.clone()) {
        out.bind_address = bind_address;
    }

    if let Some(raw) = env.get("CCP_ALIAS_PROVIDER") {
        if let Some(alias) = parse_alias(raw) {
            out.alias_provider = alias;
        }
    } else if let Some(alias_provider) = file
        .as_ref()
        .and_then(|f| f.alias_provider.as_deref())
        .and_then(parse_alias)
    {
        out.alias_provider = alias_provider;
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

pub fn alias_provider() -> AliasProvider {
    load_config().alias_provider
}

pub fn log_verbose() -> bool {
    load_config().log_verbose
}

pub fn log_stderr() -> bool {
    load_config().log_stderr
}

pub fn config_override_summary_lines(cfg: &LoadedConfig) -> Vec<String> {
    let file = read_file_config(&cfg.config_dir);
    let env: HashMap<_, _> = std::env::vars().collect();
    let mut out = Vec::new();
    if env.contains_key("CCP_BIND_ADDRESS") {
        out.push("bindAddress (env)".to_string());
    }
    if env.contains_key("PORT") {
        out.push("port (env)".to_string());
    }
    if env.contains_key("CCP_ALIAS_PROVIDER") {
        out.push("aliasProvider (env)".to_string());
    }
    if env.contains_key("CCP_LOG_VERBOSE") {
        out.push("log.verbose (env)".to_string());
    }
    if env.contains_key("CCP_LOG_STDERR") {
        out.push("log.stderr (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_RESPONSES_API") {
        out.push("codex.responsesApi (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_IMAGES_API") {
        out.push("codex.imagesApi (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_IMAGES_BASE_URL") {
        out.push("codex.imagesBaseUrl (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_TRANSCRIPTIONS_API") {
        out.push("codex.transcriptionsApi (env)".to_string());
    }
    if env.contains_key("CCP_KIMI_OAUTH_HOST") {
        out.push("kimi.oauthHost (env)".to_string());
    }
    if env.contains_key("CCP_KIMI_BASE_URL") {
        out.push("kimi.baseUrl (env)".to_string());
    }
    if env.contains_key("CCP_CURSOR_BASE_URL") {
        out.push("cursor.baseUrl (env)".to_string());
    }
    if env.contains_key("CCP_CURSOR_CLIENT_VERSION") {
        out.push("cursor.clientVersion (env)".to_string());
    }
    if env.contains_key("CCP_KIMI_USER_AGENT") {
        out.push("kimi.userAgent (env)".to_string());
    }
    if env.contains_key("CCP_GROK_BASE_URL") {
        out.push("grok.baseUrl (env)".to_string());
    }
    if env.contains_key("CCP_GROK_CLIENT_VERSION") {
        out.push("grok.clientVersion (env)".to_string());
    }
    if env.contains_key("CCP_OPENCODE_API_KEY") {
        out.push("opencode.apiKey (env)".to_string());
    } else if env.contains_key("OPENCODE_API_KEY") {
        out.push("opencode.apiKey (OpenCode env)".to_string());
    }
    if env.contains_key("CCP_OPENCODE_BASE_URL") {
        out.push("opencode.baseUrl (env)".to_string());
    }
    if env
        .get("CCP_CODEX_REASONING_SUMMARY")
        .is_some_and(|raw| !raw.is_empty())
    {
        out.push("CCP_CODEX_REASONING_SUMMARY (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_SERVER_COMPACTION") {
        out.push("CCP_CODEX_SERVER_COMPACTION (env)".to_string());
    }
    if env
        .get("CCP_AUTO_REVIEW_MODEL")
        .is_some_and(|raw| !raw.is_empty())
    {
        out.push("CCP_AUTO_REVIEW_MODEL (env)".to_string());
    }
    if let Some(file_cfg) = file {
        if let Some(bind_address) = file_cfg.bind_address {
            out.push(format!("bindAddress: {bind_address}"));
        }
        if let Some(p) = file_cfg.port {
            out.push(format!("port: {p}"));
        }
        if let Some(alias) = file_cfg.alias_provider {
            out.push(format!("aliasProvider: {alias}"));
        }
        if file_cfg
            .auto_review_model
            .is_some_and(|model| !model.is_empty())
        {
            out.push("autoReviewModel (config)".to_string());
        }
        if let Some(log) = file_cfg.log {
            if let Some(v) = log.verbose {
                out.push(format!("log.verbose: {v}"));
            }
            if let Some(v) = log.stderr {
                out.push(format!("log.stderr: {v}"));
            }
        }
        if let Some(opencode) = file_cfg.opencode {
            if opencode.api_key.is_some_and(|raw| !raw.is_empty()) {
                out.push("opencode.apiKey (config)".to_string());
            }
            if let Some(url) = opencode.base_url.filter(|raw| !raw.is_empty()) {
                out.push(format!("opencode.baseUrl: {url}"));
            }
        }
        if let Some(codex) = file_cfg.codex {
            if codex
                .reasoning_summary
                .is_some_and(|value| !value.is_empty())
            {
                out.push("codex.reasoningSummary (config)".to_string());
            }
            if let Some(enabled) = codex.server_compaction {
                out.push(format!("codex.serverCompaction: {enabled}"));
            }
            if codex.responses_api == Some(true) {
                out.push("codex.responsesApi: true".to_string());
            }
            if codex.images_api == Some(true) {
                out.push("codex.imagesApi: true".to_string());
            }
            if codex.images_base_url.is_some() {
                out.push("codex.imagesBaseUrl (config)".to_string());
            }
            if codex.transcriptions_api == Some(true) {
                out.push("codex.transcriptionsApi: true".to_string());
            }
        }
    }
    out
}

pub fn grok_base_url() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
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
    let env: HashMap<_, _> = std::env::vars().collect();
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

// ---------------------------------------------------------------------------
// Grok tool-image policy (CCP_GROK_TOOL_IMAGE)
// ---------------------------------------------------------------------------

/// How the Grok translator treats Anthropic `image` blocks (tool results and
/// top-level user messages). `omit` is the safe default: degrade to the L1
/// placeholder string. `reattach` keeps the placeholder in the tool output and
/// additionally appends a user message carrying the images as `input_image`
/// data URLs. `inline` sends the tool output itself as an array of
/// `input_text` + `input_image` parts (string-only outputs still serialize as
/// plain strings). `reject` restores the pre-L1 hard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokToolImageMode {
    Omit,
    Reattach,
    Inline,
    Reject,
}

pub fn parse_grok_tool_image_mode(raw: Option<&str>) -> GrokToolImageMode {
    match raw.map(str::trim) {
        Some("reattach") => GrokToolImageMode::Reattach,
        Some("inline") => GrokToolImageMode::Inline,
        Some("reject") => GrokToolImageMode::Reject,
        // Any unknown/empty value degrades to the safe default.
        _ => GrokToolImageMode::Omit,
    }
}

pub fn grok_tool_image_mode() -> GrokToolImageMode {
    parse_grok_tool_image_mode(std::env::var("CCP_GROK_TOOL_IMAGE").ok().as_deref())
}

/// Warn once at startup when an unknown mode was requested. Called from the
/// Grok provider constructor rather than per request.
pub fn warn_grok_tool_image_mode_once(log: &crate::logging::Logger) {
    match std::env::var("CCP_GROK_TOOL_IMAGE")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some(other) if !matches!(other, "" | "omit" | "reattach" | "inline" | "reject") => {
            let mut fields = serde_json::Map::new();
            fields.insert(
                "value".to_string(),
                serde_json::Value::String(other.to_string()),
            );
            log.warn(
                "unrecognized CCP_GROK_TOOL_IMAGE value; falling back to omit",
                Some(fields),
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Grok hosted-search policy (CCP_GROK_HOSTED_SEARCH)
// ---------------------------------------------------------------------------

/// Whether the Grok translator replaces caller search tools with xAI-hosted
/// search and requires hosted tool use on explicit search turns.
///
/// The disabled policy preserves caller tools, instructions, and tool choice.
/// It adds `x_search` only to X-specific turns because the caller has no
/// equivalent access to xAI's X index.
///
/// The enabled policy favors xAI-hosted search and citations. Hosted tools
/// replace caller search implementations, matching turns receive search
/// guidance, and explicit search turns use `tool_choice: required`.
///
/// Set `CCP_GROK_HOSTED_SEARCH` to `1`, `on`, or `true` to enable this policy.
pub fn parse_grok_hosted_search(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1" | "on" | "true"))
}

pub fn grok_hosted_search() -> bool {
    parse_grok_hosted_search(std::env::var("CCP_GROK_HOSTED_SEARCH").ok().as_deref())
}

// ---------------------------------------------------------------------------
// Grok hosted-search block shape (CCP_GROK_SEARCH_BLOCKS)
// ---------------------------------------------------------------------------

/// How a hosted search that xAI ran is reported to the client.
///
/// `Text` projects the search query into a standard `text` block.
///
/// `Native` preserves the Anthropic server-tool shape: `server_tool_use`
/// followed by `web_search_tool_result` or `x_search_tool_result`. Select this
/// shape for clients that consume hosted-tool blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokSearchBlocks {
    Text,
    Native,
}

pub fn parse_grok_search_blocks(raw: Option<&str>) -> GrokSearchBlocks {
    match raw.map(str::trim) {
        Some("native") => GrokSearchBlocks::Native,
        // Text is the compatibility-safe fallback for empty or unknown values.
        _ => GrokSearchBlocks::Text,
    }
}

pub fn grok_search_blocks() -> GrokSearchBlocks {
    parse_grok_search_blocks(std::env::var("CCP_GROK_SEARCH_BLOCKS").ok().as_deref())
}

// ---------------------------------------------------------------------------
// Hosted-search constraints that a provider cannot enforce
// (CCP_SEARCH_CONSTRAINTS)
// ---------------------------------------------------------------------------

/// How the proxy treats Anthropic hosted-search options that the upstream
/// provider cannot enforce (`allowed_domains`, `blocked_domains`,
/// `user_location`).
///
/// Applies to providers that lack those fields. First provider: Grok. Codex
/// maps domain filters natively and does not use this policy.
///
/// `Soft` is the default: drop the fields and copy constraints into a prompt hint.
/// `Warning` drops them, logs, and continues with no hint. `Hard` is the
/// legacy 400. Unknown values fall back to `Soft`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchConstraints {
    Soft,
    Warning,
    Hard,
}

pub fn parse_search_constraints(raw: Option<&str>) -> SearchConstraints {
    match raw.map(str::trim) {
        Some("hard") => SearchConstraints::Hard,
        Some("warning") => SearchConstraints::Warning,
        _ => SearchConstraints::Soft,
    }
}

pub fn search_constraints() -> SearchConstraints {
    parse_search_constraints(std::env::var("CCP_SEARCH_CONSTRAINTS").ok().as_deref())
}

struct ResolvedOpenCodeConfig {
    api_key: Option<String>,
    api_key_source: Option<&'static str>,
    base_url: String,
}

fn resolve_opencode_config(
    env: &HashMap<String, String>,
    config_dir: &Path,
) -> ResolvedOpenCodeConfig {
    let file = read_file_config(config_dir).and_then(|file| file.opencode);
    let file_key = file
        .as_ref()
        .and_then(|config| config.api_key.as_ref())
        .filter(|value| !value.is_empty());
    let (api_key, api_key_source) = if let Some(value) = env
        .get("CCP_OPENCODE_API_KEY")
        .filter(|value| !value.is_empty())
    {
        (Some(value.clone()), Some("CCP_OPENCODE_API_KEY"))
    } else if let Some(value) = env
        .get("OPENCODE_API_KEY")
        .filter(|value| !value.is_empty())
    {
        (Some(value.clone()), Some("OPENCODE_API_KEY"))
    } else if let Some(value) = file_key {
        (Some(value.clone()), Some("config.json"))
    } else {
        (None, None)
    };
    let base_url = env
        .get("CCP_OPENCODE_BASE_URL")
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| {
            file.as_ref()
                .and_then(|config| config.base_url.as_ref())
                .filter(|value| !value.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| "https://opencode.ai/zen/go/v1".to_string());

    ResolvedOpenCodeConfig {
        api_key,
        api_key_source,
        base_url,
    }
}

pub fn opencode_api_key() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    resolve_opencode_config(&env, &paths::config_dir()).api_key
}

pub fn opencode_api_key_source() -> Option<&'static str> {
    let env: HashMap<_, _> = std::env::vars().collect();
    resolve_opencode_config(&env, &paths::config_dir()).api_key_source
}

pub fn opencode_base_url() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    resolve_opencode_config(&env, &paths::config_dir()).base_url
}

pub fn is_verbose() -> bool {
    log_verbose()
}

pub fn kimi_oauth_host() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_KIMI_OAUTH_HOST") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(kimi) = file.kimi
        && let Some(host) = kimi.oauth_host
    {
        return host;
    }
    "https://auth.kimi.com".to_string()
}

pub fn kimi_base_url() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_KIMI_BASE_URL") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(kimi) = file.kimi
        && let Some(url) = kimi.base_url
    {
        return url;
    }
    "https://api.kimi.com/coding/v1".to_string()
}

pub fn kimi_user_agent(default: &str) -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_KIMI_USER_AGENT") {
        return raw.clone();
    }
    if let Some(raw) = env.get("CCP_USER_AGENT") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(kimi) = file.kimi
        && let Some(ua) = kimi.user_agent
    {
        return ua;
    }
    default.to_string()
}

// ---------------------------------------------------------------------------
// Codex config
// ---------------------------------------------------------------------------

pub fn codex_base_url(default: &str) -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
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
    let env: HashMap<_, _> = std::env::vars().collect();
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
    let env: HashMap<_, _> = std::env::vars().collect();
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
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_PREVIOUS_RESPONSE_ID") {
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

pub fn codex_server_compaction() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_SERVER_COMPACTION") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.server_compaction
    {
        return enabled;
    }
    false
}

pub fn codex_responses_api() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_RESPONSES_API") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.responses_api
    {
        return enabled;
    }
    false
}

pub fn codex_images_api() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_IMAGES_API") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.images_api
    {
        return enabled;
    }
    false
}

pub fn codex_transcriptions_api() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_TRANSCRIPTIONS_API") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.transcriptions_api
    {
        return enabled;
    }
    false
}

pub fn codex_images_base_url() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_IMAGES_BASE_URL") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(url) = codex.images_base_url
    {
        return url;
    }
    "https://chatgpt.com/backend-api/codex".to_string()
}

pub fn codex_service_tier() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_SERVICE_TIER") {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.service_tier;
    }
    None
}

pub fn codex_effort() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_EFFORT") {
        return Some(raw.clone());
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
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env
        .get("CCP_CODEX_REASONING_SUMMARY")
        .filter(|raw| !raw.is_empty())
    {
        return Some(raw.clone());
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
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_MODEL") {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.model;
    }
    None
}

pub fn auto_review_model() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env
        .get("CCP_AUTO_REVIEW_MODEL")
        .filter(|raw| !raw.is_empty())
    {
        return Some(raw.clone());
    }
    read_file_config(&paths::config_dir())
        .and_then(|file| file.auto_review_model)
        .filter(|model| !model.is_empty())
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

fn parse_codex_transport(raw: &str) -> Option<CodexTransport> {
    match raw {
        "http" => Some(CodexTransport::Http),
        "websocket" => Some(CodexTransport::WebSocket),
        "auto" => Some(CodexTransport::Auto),
        _ => None,
    }
}

pub fn codex_transport() -> CodexTransport {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_TRANSPORT")
        && let Some(transport) = parse_codex_transport(raw)
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
    CodexTransport::WebSocket
}

// ---------------------------------------------------------------------------
// Cursor config
// ---------------------------------------------------------------------------

pub fn cursor_base_url() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CURSOR_BASE_URL") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(cursor) = file.cursor
        && let Some(url) = cursor.base_url
    {
        return url;
    }
    "https://api2.cursor.sh".to_string()
}

pub fn cursor_client_version() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CURSOR_CLIENT_VERSION") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(cursor) = file.cursor
        && let Some(version) = cursor.client_version
    {
        return version;
    }
    detect_cursor_agent_version().unwrap_or_else(|| "cli-2026.07.23-e383d2b".to_string())
}

fn detect_cursor_agent_version() -> Option<String> {
    let output = std::process::Command::new("cursor-agent")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?;
    let version = version.lines().next()?.trim();
    if version.is_empty() {
        return None;
    }
    Some(if version.starts_with("cli-") {
        version.to_string()
    } else {
        format!("cli-{version}")
    })
}

pub fn cursor_agent_bundle() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CURSOR_AGENT_BUNDLE") {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(cursor) = file.cursor
        && let Some(bundle) = cursor.agent_bundle
    {
        return Some(bundle);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn clear_env() {
        unsafe {
            std::env::remove_var("CCP_BIND_ADDRESS");
            std::env::remove_var("CCP_CODEX_TRANSPORT");
            std::env::remove_var("CCP_CONFIG_DIR");
            std::env::remove_var("CCP_LOG_VERBOSE");
            std::env::remove_var("CCP_LOG_STDERR");
            std::env::remove_var("CCP_CODEX_REASONING_SUMMARY");
            std::env::remove_var("CCP_CODEX_SERVER_COMPACTION");
            std::env::remove_var("CCP_CODEX_RESPONSES_API");
            std::env::remove_var("CCP_CODEX_IMAGES_API");
            std::env::remove_var("CCP_CODEX_IMAGES_BASE_URL");
            std::env::remove_var("CCP_CODEX_TRANSCRIPTIONS_API");
            std::env::remove_var("CCP_AUTO_REVIEW_MODEL");
        }
    }

    fn config_env(config: &tempfile::TempDir) -> HashMap<String, String> {
        HashMap::from([(
            "CCP_CONFIG_DIR".to_string(),
            config.path().to_string_lossy().into_owned(),
        )])
    }

    #[test]
    fn opencode_config_reads_file_and_env_precedence() {
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"opencode":{"apiKey":"file-key","baseUrl":"https://file.example/v1"}}"#,
        )
        .unwrap();
        let mut env = HashMap::new();
        let resolved = resolve_opencode_config(&env, config.path());
        assert_eq!(resolved.api_key.as_deref(), Some("file-key"));
        assert_eq!(resolved.api_key_source, Some("config.json"));
        assert_eq!(resolved.base_url, "https://file.example/v1");

        env.insert("OPENCODE_API_KEY".into(), "standard-key".into());
        let resolved = resolve_opencode_config(&env, config.path());
        assert_eq!(resolved.api_key.as_deref(), Some("standard-key"));
        assert_eq!(resolved.api_key_source, Some("OPENCODE_API_KEY"));

        env.insert("CCP_OPENCODE_API_KEY".into(), "ccp-key".into());
        env.insert(
            "CCP_OPENCODE_BASE_URL".into(),
            "https://env.example/v1".into(),
        );
        let resolved = resolve_opencode_config(&env, config.path());
        assert_eq!(resolved.api_key.as_deref(), Some("ccp-key"));
        assert_eq!(resolved.api_key_source, Some("CCP_OPENCODE_API_KEY"));
        assert_eq!(resolved.base_url, "https://env.example/v1");
    }

    #[test]
    fn bind_address_defaults_to_loopback() {
        let config = tempfile::TempDir::new().unwrap();
        let env = config_env(&config);

        assert_eq!(load_config_for_env(&env).bind_address, "127.0.0.1");
    }

    #[test]
    fn bind_address_reads_config_and_env_takes_precedence() {
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"bindAddress":"192.0.2.10"}"#,
        )
        .unwrap();
        let mut env = config_env(&config);

        assert_eq!(load_config_for_env(&env).bind_address, "192.0.2.10");
        env.insert("CCP_BIND_ADDRESS".to_string(), "0.0.0.0".to_string());
        assert_eq!(load_config_for_env(&env).bind_address, "0.0.0.0");
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
    fn codex_transport_defaults_to_websocket() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let result = codex_transport();
        assert_eq!(result, CodexTransport::WebSocket);
    }

    #[test]
    fn codex_transport_reads_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "auto");
        }
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_env_websocket() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "websocket");
        }
        assert_eq!(codex_transport(), CodexTransport::WebSocket);
    }

    #[test]
    fn codex_transport_invalid_env_falls_back_to_websocket() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "invalid");
        }
        assert_eq!(codex_transport(), CodexTransport::WebSocket);
    }

    #[test]
    fn codex_transport_empty_env_falls_back_to_websocket() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("CCP_CODEX_TRANSPORT", "");
        }
        assert_eq!(codex_transport(), CodexTransport::WebSocket);
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
        let config = tempfile::TempDir::new().unwrap();
        let mut env = config_env(&config);
        env.insert("CCP_LOG_VERBOSE".to_string(), "0".to_string());
        env.insert("CCP_LOG_STDERR".to_string(), String::new());

        let loaded = load_config_for_env(&env);
        assert!(loaded.log_verbose);
        assert!(loaded.log_stderr);
    }

    #[test]
    fn log_config_values_apply_without_env() {
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"log":{"verbose":true,"stderr":true}}"#,
        )
        .unwrap();
        let env = config_env(&config);

        let loaded = load_config_for_env(&env);
        assert!(loaded.log_verbose);
        assert!(loaded.log_stderr);
    }

    #[test]
    fn codex_responses_api_defaults_to_disabled() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(!codex_responses_api());
    }

    #[test]
    fn codex_responses_api_reads_config_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"responsesApi":true}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(codex_responses_api());
        let _responses_env = EnvGuard::set("CCP_CODEX_RESPONSES_API", "false");
        assert!(!codex_responses_api());
    }

    #[test]
    fn codex_responses_api_accepts_enabled_env_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        for value in ["1", "true", "TRUE", "yes"] {
            let _responses_env = EnvGuard::set("CCP_CODEX_RESPONSES_API", value);
            assert!(codex_responses_api(), "{value}");
        }
    }

    #[test]
    fn codex_images_api_defaults_to_disabled_and_env_overrides_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"imagesApi":true,"imagesBaseUrl":"https://chatgpt.com/backend-api/codex-custom"}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(codex_images_api());
        assert_eq!(
            codex_images_base_url(),
            "https://chatgpt.com/backend-api/codex-custom"
        );
        let _enabled_env = EnvGuard::set("CCP_CODEX_IMAGES_API", "false");
        let _base_env = EnvGuard::set(
            "CCP_CODEX_IMAGES_BASE_URL",
            "https://chatgpt.com/backend-api/codex",
        );
        assert!(!codex_images_api());
        assert_eq!(
            codex_images_base_url(),
            "https://chatgpt.com/backend-api/codex"
        );
    }

    #[test]
    fn codex_transcriptions_api_defaults_to_disabled_and_env_overrides_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"transcriptionsApi":true}}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(codex_transcriptions_api());
        let _enabled_env = EnvGuard::set("CCP_CODEX_TRANSCRIPTIONS_API", "false");
        assert!(!codex_transcriptions_api());
    }

    #[test]
    fn codex_reasoning_summary_reads_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
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
    fn codex_reasoning_summary_env_overrides_config_and_empty_falls_through() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
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
    fn auto_review_model_reads_top_level_config_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"autoReviewModel":"grok-4.5"}"#,
        )
        .unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert_eq!(auto_review_model().as_deref(), Some("grok-4.5"));
        {
            let _model_env = EnvGuard::set("CCP_AUTO_REVIEW_MODEL", "gpt-5.6-terra");
            assert_eq!(auto_review_model().as_deref(), Some("gpt-5.6-terra"));
        }
        {
            let _model_env = EnvGuard::set("CCP_AUTO_REVIEW_MODEL", "");
            assert_eq!(auto_review_model().as_deref(), Some("grok-4.5"));
        }
    }

    #[test]
    fn codex_server_compaction_defaults_and_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = tempfile::TempDir::new().unwrap();
        let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

        assert!(!codex_server_compaction());
        {
            let _enabled_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "on");
            assert!(codex_server_compaction());
        }
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"serverCompaction":true}}"#,
        )
        .unwrap();
        assert!(codex_server_compaction());
        let _disabled_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "false");
        assert!(!codex_server_compaction());
    }
}
