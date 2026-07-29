//! Privacy-preserving support bundles for ccproxy.
//!
//! A bundle contains only a strict projection of the process JSON log. It
//! never copies traffic captures, error captures, configuration, credentials,
//! paths, host names, session identifiers, prompts, tool arguments, or tool
//! results.

use anyhow::{Context, Result, anyhow, bail, ensure};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::registry::{CODEX_MODELS, GROK_MODELS};

pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_EVENT_LINES: usize = 50_000;
pub const DEFAULT_EVENT_LINES: usize = 10_000;
pub const SFTP_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

const FORMAT_NAME: &str = "ccproxy-diagnostic-bundle";
const PRIVACY_LEVEL: &str = "metadata-only";
const SCHEMA_VERSION: u32 = 1;
const MAX_LOG_LINE_BYTES: usize = 64 * 1024;
const MAX_SOURCE_FILES: usize = 256;
const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EVENTS_BYTES: u64 = 32 * 1024 * 1024;
const MAX_METADATA_ENTRY_BYTES: u64 = 64 * 1024;
const MAX_VERIFIED_TOTAL_BYTES: u64 = MAX_EVENTS_BYTES + 2 * MAX_METADATA_ENTRY_BYTES;

static CURRENT_BINARY_SHA256: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct CollectOptions {
    /// Fixed state directory containing `proxy.log` and rotations.
    pub state_dir: PathBuf,
    /// New archive path. Existing files are never replaced.
    pub output: PathBuf,
    /// Optional exact request id filter. The raw id is never written; output
    /// events contain a short SHA-256 correlation fingerprint instead.
    pub request_id: Option<String>,
    /// Optional user-chosen, non-secret label such as `wsl-dev`.
    pub machine_label: Option<String>,
    pub max_input_bytes: u64,
    pub max_lines: usize,
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self {
            state_dir: crate::paths::state_dir(),
            output: crate::paths::state_dir().join("diagnostics.tar.gz"),
            request_id: None,
            machine_label: None,
            max_input_bytes: MAX_INPUT_BYTES,
            max_lines: DEFAULT_EVENT_LINES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CollectReport {
    pub bundle_path: PathBuf,
    pub bundle_id: String,
    pub event_count: usize,
    pub bundle_bytes: u64,
    pub sha256: String,
    pub source_file_count: usize,
    pub input_bytes: u64,
    pub skipped_count: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTarget {
    host: String,
    user: Option<String>,
    remote_dir: String,
}

impl UploadTarget {
    pub fn new(host: String, user: Option<String>, remote_dir: String) -> Result<Self> {
        validate_host(&host)?;
        if let Some(user) = user.as_deref() {
            validate_user(user)?;
        }
        validate_remote_dir(&remote_dir)?;
        Ok(Self {
            host,
            user,
            remote_dir,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
    pub fn remote_dir(&self) -> &str {
        &self.remote_dir
    }

    fn destination(&self) -> String {
        if IpAddr::from_str(&self.host).is_ok_and(|address| address.is_ipv6()) {
            self.user.as_deref().map_or_else(
                || format!("[{}]", self.host),
                |user| format!("{user}@[{}]", self.host),
            )
        } else {
            self.user
                .as_deref()
                .map_or_else(|| self.host.clone(), |user| format!("{user}@{}", self.host))
        }
    }
}

#[derive(Debug, Clone)]
pub struct UploadReport {
    pub bundle_id: String,
    pub remote_name: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct BundleSummary {
    pub bundle_id: String,
    pub privacy_level: String,
    pub event_count: usize,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    format: String,
    privacy_level: String,
    bundle_id: String,
    created_at: String,
    event_count: usize,
    source_file_count: usize,
    input_bytes: u64,
    skipped_lines: u64,
    dropped_by_line_limit: u64,
    truncated: bool,
    max_input_bytes: u64,
    max_lines: usize,
    request_id_hash: Option<String>,
    files: Vec<String>,
    excluded: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Environment {
    schema_version: u32,
    ccproxy_version: String,
    os: String,
    arch: String,
    runtime: String,
    machine_label: Option<String>,
    git_sha: String,
    git_dirty: bool,
    build_timestamp: Option<u64>,
    binary_sha256: Option<String>,
}

#[derive(Debug)]
struct SourceLog {
    path: PathBuf,
    current: bool,
    numeric_suffix: String,
}

#[derive(Debug)]
struct SourceChunk {
    bytes: Vec<u8>,
    starts_mid_line: bool,
}

/// Collect a metadata-only archive from the fixed `proxy.log` and
/// `proxy.<digits>` files directly inside `state_dir`.
pub fn collect_bundle(options: &CollectOptions) -> Result<CollectReport> {
    validate_collect_options(options)?;
    validate_state_dir(&options.state_dir)?;
    validate_output_path(&options.output)?;

    let sources = discover_source_logs(&options.state_dir)?;
    let (chunks, input_bytes, input_truncated) =
        read_recent_chunks(&sources, options.max_input_bytes)?;
    let mut events = VecDeque::with_capacity(options.max_lines.min(4096));
    let mut skipped_lines = 0_u64;
    let mut dropped_by_line_limit = 0_u64;

    for chunk in chunks {
        for (index, line) in chunk.bytes.split(|byte| *byte == b'\n').enumerate() {
            if (chunk.starts_mid_line && index == 0) || line.is_empty() {
                continue;
            }
            if line.len() > MAX_LOG_LINE_BYTES {
                skipped_lines += 1;
                continue;
            }
            let Ok(raw) = serde_json::from_slice::<Value>(line) else {
                skipped_lines += 1;
                continue;
            };
            if let Some(request_id) = options.request_id.as_deref()
                && raw.pointer("/fields/reqId").and_then(Value::as_str) != Some(request_id)
            {
                continue;
            }
            let Some(event) = sanitize_event(&raw) else {
                skipped_lines += 1;
                continue;
            };
            if events.len() == options.max_lines {
                events.pop_front();
                dropped_by_line_limit = dropped_by_line_limit.saturating_add(1);
            }
            events.push_back(event);
        }
    }

    let bundle_id = Uuid::new_v4().to_string();
    let truncated = input_truncated || dropped_by_line_limit > 0;
    let created_at = now_rfc3339();
    let event_count = events.len();
    let events_bytes = encode_events(&events)?;
    let environment = Environment {
        schema_version: SCHEMA_VERSION,
        ccproxy_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        runtime: runtime_kind().to_string(),
        machine_label: options.machine_label.clone(),
        git_sha: env!("CCPROXY_GIT_SHA").to_string(),
        git_dirty: env!("CCPROXY_GIT_DIRTY") == "true",
        build_timestamp: env!("CCPROXY_BUILD_UNIX_EPOCH").parse().ok(),
        binary_sha256: current_binary_sha256(),
    };
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        format: FORMAT_NAME.to_string(),
        privacy_level: PRIVACY_LEVEL.to_string(),
        bundle_id: bundle_id.clone(),
        created_at,
        event_count,
        source_file_count: sources.len(),
        input_bytes,
        skipped_lines,
        dropped_by_line_limit,
        truncated,
        max_input_bytes: options.max_input_bytes,
        max_lines: options.max_lines,
        request_id_hash: options.request_id.as_deref().map(identifier_hash),
        files: vec![
            "manifest.json".into(),
            "events.jsonl".into(),
            "environment.json".into(),
        ],
        excluded: vec![
            "traffic-captures".into(),
            "error-captures".into(),
            "configuration".into(),
            "authentication".into(),
            "home".into(),
            "hostname".into(),
            "paths".into(),
            "session-identifiers".into(),
            "prompts".into(),
            "tool-arguments".into(),
            "tool-results".into(),
            "raw-logs".into(),
        ],
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let environment_bytes = serde_json::to_vec_pretty(&environment)?;

    write_archive_atomic(
        &options.output,
        &manifest_bytes,
        &events_bytes,
        &environment_bytes,
    )?;
    let bundle_bytes = fs::metadata(&options.output)?.len();
    let sha256 = sha256_file(&options.output)?;
    Ok(CollectReport {
        bundle_path: options.output.clone(),
        bundle_id,
        event_count,
        bundle_bytes,
        sha256,
        source_file_count: sources.len(),
        input_bytes,
        skipped_count: skipped_lines.saturating_add(dropped_by_line_limit),
        truncated,
    })
}

pub fn inspect_bundle(path: &Path) -> Result<BundleSummary> {
    let verified = verify_bundle_for_upload(path)?;
    Ok(BundleSummary {
        bundle_id: verified.bundle_id,
        privacy_level: PRIVACY_LEVEL.to_string(),
        event_count: verified.event_count,
        bytes: fs::metadata(path)?.len(),
        sha256: sha256_file(path)?,
    })
}

/// Upload a verified ccproxy metadata-only bundle with OpenSSH `sftp`.
///
/// The remote file is first written under a random `.part` name and then
/// renamed. No shell is involved and all network behavior is batch-only.
pub fn upload_bundle_sftp(bundle_path: &Path, target: &UploadTarget) -> Result<UploadReport> {
    upload_bundle_sftp_with_program(bundle_path, target, Path::new("sftp"))
}

fn upload_bundle_sftp_with_program(
    bundle_path: &Path,
    target: &UploadTarget,
    sftp_program: &Path,
) -> Result<UploadReport> {
    let source_staging = stage_bundle_for_upload(bundle_path)?;
    let verified = verify_bundle_for_upload(&source_staging.path)?;
    let staging = stage_verified_bundle(&verified)?;
    drop(source_staging);
    let local = staging
        .path
        .canonicalize()
        .context("failed to resolve staged diagnostic bundle")?;
    let final_name = format!("ccproxy-diagnostic-{}.tar.gz", verified.bundle_id);
    let part_name = format!(".{}.{}.part", final_name, Uuid::new_v4());
    let remote_final = join_remote(&target.remote_dir, &final_name);
    let remote_part = join_remote(&target.remote_dir, &part_name);
    let batch = build_sftp_batch(&local, &remote_part, &remote_final)?;

    let mut child = Command::new(sftp_program)
        .args([
            "-b",
            "-",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg(target.destination())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start OpenSSH sftp")?;
    let batch_result = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open sftp batch input"))
        .and_then(|mut input| {
            input
                .write_all(batch.as_bytes())
                .context("failed to send sftp batch")
        });
    if let Err(error) = batch_result {
        terminate_child(&mut child);
        return Err(error);
    }

    let deadline = Instant::now() + SFTP_UPLOAD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_child(&mut child);
                return Err(error).context("failed to inspect sftp process");
            }
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            bail!(
                "sftp upload timed out after {} seconds",
                SFTP_UPLOAD_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(25));
    };
    ensure!(status.success(), "sftp upload failed with status {status}");
    Ok(UploadReport {
        bundle_id: verified.bundle_id,
        remote_name: final_name,
        size_bytes: fs::metadata(&staging.path)?.len(),
        sha256: sha256_file(&staging.path)?,
    })
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

struct UploadStaging {
    path: PathBuf,
}

impl Drop for UploadStaging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stage_bundle_for_upload(source_path: &Path) -> Result<UploadStaging> {
    let before = fs::symlink_metadata(source_path).context("diagnostic bundle is unavailable")?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "diagnostic bundle must be a regular non-symlink file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.nlink() == 1,
            "refusing hard-linked diagnostic bundle"
        );
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut source = options
        .open(source_path)
        .context("failed to open diagnostic bundle for upload")?;
    let opened = source.metadata()?;
    let after = fs::symlink_metadata(source_path)?;
    ensure!(
        opened.is_file() && after.is_file() && !after.file_type().is_symlink(),
        "diagnostic bundle changed while it was opened"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            opened.nlink() == 1
                && after.nlink() == 1
                && before.dev() == opened.dev()
                && before.ino() == opened.ino()
                && opened.dev() == after.dev()
                && opened.ino() == after.ino(),
            "diagnostic bundle changed while it was opened"
        );
    }

    let (path, mut destination) =
        create_temporary(&std::env::temp_dir(), "ccproxy-diagnostic-upload")?;
    let staging = UploadStaging { path };
    set_file_mode_600(&staging.path)?;
    let copied = std::io::copy(
        &mut Read::by_ref(&mut source).take(MAX_BUNDLE_BYTES + 1),
        &mut destination,
    )?;
    ensure!(
        copied <= MAX_BUNDLE_BYTES,
        "diagnostic bundle exceeds upload size limit"
    );
    destination.sync_all()?;
    drop(destination);
    Ok(staging)
}

fn stage_verified_bundle(verified: &VerifiedBundle) -> Result<UploadStaging> {
    let (path, file) = create_temporary(&std::env::temp_dir(), "ccproxy-diagnostic-canonical")?;
    let staging = UploadStaging { path };
    set_file_mode_600(&staging.path)?;
    write_archive_file(
        file,
        &verified.manifest,
        &verified.events,
        &verified.environment,
    )?;
    Ok(staging)
}

fn validate_collect_options(options: &CollectOptions) -> Result<()> {
    ensure!(
        (1..=MAX_EVENT_LINES).contains(&options.max_lines),
        "max_lines must be between 1 and {MAX_EVENT_LINES}"
    );
    ensure!(
        (1..=MAX_INPUT_BYTES).contains(&options.max_input_bytes),
        "max_input_bytes must be between 1 and {MAX_INPUT_BYTES}"
    );
    if let Some(request_id) = options.request_id.as_deref() {
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= 128
                && request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "request_id contains unsupported characters"
        );
    }
    if let Some(label) = options.machine_label.as_deref() {
        validate_machine_label(label)?;
    }
    Ok(())
}

fn validate_machine_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty()
            && label.len() <= 64
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "machine_label must use 1-64 ASCII letters, digits, '.', '_' or '-'"
    );
    Ok(())
}

fn validate_state_dir(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("diagnostic state directory is unavailable")?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "diagnostic state directory must not be a symlink"
    );
    ensure!(
        metadata.is_dir(),
        "diagnostic state directory is not a directory"
    );
    Ok(())
}

fn validate_output_path(path: &Path) -> Result<()> {
    ensure!(
        path.file_name().is_some(),
        "diagnostic output path must name a file"
    );
    ensure!(
        !is_log_name(path.file_name().and_then(|v| v.to_str()).unwrap_or("")),
        "diagnostic output must not use a proxy log filename"
    );
    if fs::symlink_metadata(path).is_ok() {
        bail!("diagnostic output already exists");
    }
    Ok(())
}

fn discover_source_logs(state_dir: &Path) -> Result<Vec<SourceLog>> {
    let mut sources = Vec::new();
    for entry in
        fs::read_dir(state_dir).context("failed to enumerate diagnostic state directory")?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_log_name(name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "refusing symlinked diagnostic log {name}"
        );
        ensure!(
            metadata.is_file(),
            "refusing non-file diagnostic log {name}"
        );
        sources.push(SourceLog {
            path: entry.path(),
            current: name == "proxy.log",
            numeric_suffix: name.strip_prefix("proxy.").unwrap_or("").to_string(),
        });
        ensure!(
            sources.len() <= MAX_SOURCE_FILES,
            "diagnostic state directory contains more than {MAX_SOURCE_FILES} proxy logs"
        );
    }
    sources.sort_by(|a, b| match (a.current, b.current) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        _ => numeric_string_cmp(&a.numeric_suffix, &b.numeric_suffix),
    });
    Ok(sources)
}

fn is_log_name(name: &str) -> bool {
    name == "proxy.log"
        || name
            .strip_prefix("proxy.")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

fn numeric_string_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn open_source(path: &Path) -> Result<File> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "diagnostic log is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(before.nlink() == 1, "refusing hard-linked diagnostic log");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).with_context(|| {
        format!(
            "failed to open diagnostic log {}",
            path.file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("<invalid>")
        )
    })?;
    let opened = file.metadata()?;
    let after = fs::symlink_metadata(path)?;
    ensure!(
        opened.is_file() && after.is_file() && !after.file_type().is_symlink(),
        "diagnostic log changed to a non-file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            opened.nlink() == 1 && after.nlink() == 1,
            "refusing hard-linked diagnostic log"
        );
        ensure!(
            before.dev() == opened.dev()
                && before.ino() == opened.ino()
                && opened.dev() == after.dev()
                && opened.ino() == after.ino(),
            "diagnostic log changed while it was opened"
        );
    }
    Ok(file)
}

fn read_recent_chunks(
    sources: &[SourceLog],
    max_bytes: u64,
) -> Result<(Vec<SourceChunk>, u64, bool)> {
    let mut remaining = max_bytes;
    let mut input_bytes = 0_u64;
    let mut truncated = false;
    let mut newest_first = Vec::new();
    for source in sources.iter().rev() {
        let mut file = open_source(&source.path)?;
        let len = file.metadata()?.len();
        if remaining == 0 {
            truncated |= len > 0;
            continue;
        }
        let take = len.min(remaining);
        if take == 0 {
            continue;
        }
        file.seek(SeekFrom::End(-(take as i64)))?;
        let mut bytes = Vec::with_capacity(take as usize);
        file.take(take).read_to_end(&mut bytes)?;
        input_bytes = input_bytes.saturating_add(bytes.len() as u64);
        remaining = remaining.saturating_sub(bytes.len() as u64);
        truncated |= take < len;
        newest_first.push(SourceChunk {
            bytes,
            starts_mid_line: take < len,
        });
    }
    newest_first.reverse();
    Ok((newest_first, input_bytes, truncated))
}

fn sanitize_event(raw: &Value) -> Option<Value> {
    let object = raw.as_object()?;
    let event = object.get("msg")?.as_str()?;
    if !allowed_event(event) {
        return None;
    }
    let level = object.get("level")?.as_str()?;
    if !matches!(level, "debug" | "info" | "warn" | "error") {
        return None;
    }
    let service = object.get("service")?.as_str()?;
    if !matches!(service, "server" | "codex" | "grok" | "logging") {
        return None;
    }
    let timestamp = sanitize_timestamp(object.get("t")?.as_str()?)?;
    let fields = object.get("fields").and_then(Value::as_object);
    let mut projected = Map::new();
    if let Some(fields) = fields {
        for (key, value) in fields {
            if let Some((out_key, out_value)) = sanitize_field(event, key, value) {
                projected.insert(out_key, out_value);
            }
        }
    }
    Some(
        json!({"t": timestamp, "level": level, "service": service, "event": event, "fields": projected}),
    )
}

fn allowed_event(event: &str) -> bool {
    matches!(
        event,
        "response_started"
            | "request_completed"
            | "request_failed"
            | "request_abandoned"
            | "client_tool_result"
            | "tool_block_started"
            | "tool_block_completed"
            | "tool_block_interrupted"
            | "request_configuration"
            | "native_web_search_phase"
            | "upstream_first_event"
            | "stream_committed_before_semantic"
            | "terminal_resolution"
            | "buffered_transport_retry"
            | "buffered_transport_retry_exhausted"
            | "live_transport_retry"
            | "live_transport_retry_exhausted"
            | "post_ping_replay_scheduled"
            | "post_ping_replay_rejected"
            | "auto_transport_fallback"
            | "websocket_circuit_fallback"
            | "websocket_circuit_opened"
            | "websocket_pool_refresh"
            | "websocket_terminal_pool_probe_failed"
            | "upstream_retry"
            | "upstream_retry_exhausted"
            | "stream_reducer_error"
            | "stream_terminal"
            | "log_records_dropped"
    )
}

fn sanitize_field(event: &str, key: &str, value: &Value) -> Option<(String, Value)> {
    let allowed = allowed_fields(event);
    if !allowed.contains(&key) {
        return None;
    }
    if matches!(key, "message" | "reason" | "detail" | "bodyReadError") {
        let text = value.as_str()?;
        let prefix = match key {
            "message" => "message",
            "reason" => "reason",
            "detail" => "detail",
            _ => "bodyReadError",
        };
        return Some((
            format!("{prefix}Diagnostic"),
            json!({
                "class": classify_dynamic_text(text), "fingerprint": dynamic_fingerprint(text)
            }),
        ));
    }
    match key {
        "reqId" => value
            .as_str()
            .map(|s| ("reqIdHash".into(), json!(identifier_hash(s)))),
        "model" => value
            .as_str()
            .map(|s| ("model".into(), json!(diagnostic_model(s)))),
        "provider" => enum_string(value, &["codex", "grok"]).map(|v| (key.into(), v)),
        "transport" | "transportRequested" | "fallbackTransport" => {
            enum_string(value, &["http", "websocket", "auto"]).map(|v| (key.into(), v))
        }
        "reasoningEffort" => enum_string(value, &["none", "low", "medium", "high", "xhigh", "max"])
            .map(|v| (key.into(), v)),
        "phase" => enum_string(
            value,
            &[
                "request",
                "response_body",
                "added",
                "in_progress",
                "searching",
                "completed",
                "done",
            ],
        )
        .map(|v| (key.into(), v)),
        "callIdHash" => value
            .as_str()
            .filter(|v| v.len() == 16 && is_short_hex(v))
            .map(|v| (key.into(), json!(v))),
        "toolName" => sanitize_tool_name(value.as_str()?).map(|name| (key.into(), json!(name))),
        "toolKind" => enum_string(value, &["tool_use", "server_tool_use"]).map(|v| (key.into(), v)),
        "interruptReason" => enum_string(
            value,
            &[
                "index_reused",
                "response_eof",
                "in_band_sse_error",
                "response_body_error",
                "downstream_dropped",
                "message_stop_before_block_stop",
            ],
        )
        .map(|value| (key.into(), value)),
        "event" | "sourceEvent" => value
            .as_str()
            .map(|v| (key.into(), json!(protocol_event_category(v)))),
        "toolChoice" => Some((
            "toolChoiceCategory".into(),
            json!(tool_choice_category(value)),
        )),
        _ => sanitize_scalar_field(key, value).map(|value| (key.into(), value)),
    }
}

fn sanitize_scalar_field(key: &str, value: &Value) -> Option<Value> {
    if matches!(
        key,
        "countTokens"
            | "isError"
            | "responsesLite"
            | "nativeWebSearch"
            | "parallelToolCalls"
            | "promptCacheKeyPresent"
            | "generationStarted"
            | "semanticEmitted"
            | "inBandSse"
            | "bodyTruncated"
            | "bodyTimedOut"
            | "downstreamEmitted"
    ) {
        return value.as_bool().map(Value::Bool);
    }

    let numeric_limit = match key {
        "status" => 999,
        "ms"
        | "elapsedMs"
        | "delayMs"
        | "cooldownMs"
        | "waitMs"
        | "deadlineRemainingMs"
        | "heartbeatIntervalMs" => 7 * 24 * 60 * 60 * 1_000,
        "bytes" | "contentBytes" => 1_u64 << 40,
        "anthropicMaxTokens" | "estimatedInputTokens" => 1_000_000_000,
        "index"
        | "messageIndex"
        | "blockIndex"
        | "functionToolCount"
        | "hostedToolCount"
        | "inputFunctionToolCount"
        | "failedAttempt"
        | "nextAttempt"
        | "maxAttempts"
        | "attempt"
        | "attempts"
        | "transientFailures"
        | "failures"
        | "chunks"
        | "count"
        | "queueCapacity" => 1_000_000_000,
        _ => 0,
    };
    if numeric_limit > 0 {
        return value
            .as_u64()
            .filter(|number| *number <= numeric_limit)
            .map(|number| json!(number));
    }

    if value.is_null()
        && matches!(
            key,
            "serviceTier" | "stopReason" | "incompleteReason" | "upstreamIncompleteReason"
        )
    {
        return Some(Value::Null);
    }
    let raw = value.as_str()?;
    let choices: &[&str] = match key {
        "provider" => &["codex", "grok"],
        "transport" | "transportRequested" => &["http", "websocket", "auto"],
        "fallbackTransport" => &["http"],
        "reasoningEffort" => &["none", "low", "medium", "high", "xhigh", "max"],
        "phase" => &[
            "request",
            "response_body",
            "added",
            "in_progress",
            "searching",
            "completed",
            "done",
        ],
        "toolKind" => &["tool_use", "server_tool_use"],
        "serviceTier" => &["priority", "flex"],
        "transportReason" => &[
            "system_proxy",
            "environment_proxy_unsupported",
            "automatic",
            "explicit",
        ],
        "laneReason" => &[
            "responses_lite",
            "hosted_web_search",
            "parallel_functions",
            "full_responses",
        ],
        "outputBudgetEnforcement" => &["unsupported_by_private_codex_gateway"],
        "replaySafety" => &[
            "definitely_not_dispatched",
            "explicitly_retryable_response",
            "outcome_unknown",
        ],
        "authority" => &[
            "authoritative",
            "unsafe_salvaged_after_transport_error",
            "failed_closed_after_transport_error",
            "unsafe_salvaged_after_stream_close",
            "failed_closed_after_stream_close",
        ],
        "action" => &["fallback_http"],
        "origin" => &[
            "http",
            "upstream_http",
            "websocket",
            "websocket_handshake",
            "auth",
            "buffered_http",
            "buffered_websocket",
            "serialization",
            "request_transport",
            "stream_transport",
            "response_limit",
            "deadline",
        ],
        "stage" | "errorStage" => &[
            "auth",
            "serialize",
            "connect",
            "header",
            "status",
            "body",
            "stream",
            "deadline",
            "downstream",
            "transport",
            "decoder",
            "json",
            "reducer",
            "render",
            "upstream",
        ],
        "kind" => &[
            "total_timeout",
            "consumer_stalled",
            "event_size_limit",
            "response_size_limit",
            "malformed_sse",
            "malformed_event",
            "invalid_event",
            "incomplete_stream",
            "upstream_stream",
            "response_failed",
            "response_error",
            "error_event",
            "outcome_unknown",
            "retry_exhausted",
            "retry_after_exceeds_budget",
            "retry_delay_exceeds_deadline",
        ],
        "outcome" => &["completed", "incomplete", "failed"],
        "stopReason" => &["end_turn", "tool_use", "max_tokens", "refusal"],
        "incompleteReason" => &["max_output_tokens", "content_filter", "missing", "other"],
        _ => &[],
    };
    if key == "upstreamIncompleteReason" {
        let category = match raw {
            "max_output_tokens" | "max_tokens" | "length" => "max_output_tokens",
            "content_filter" => "content_filter",
            _ => "other",
        };
        return Some(json!(category));
    }
    choices.contains(&raw).then(|| json!(raw))
}

fn allowed_fields(event: &str) -> &'static [&'static str] {
    match event {
        "response_started" | "request_completed" => {
            &["reqId", "provider", "model", "countTokens", "status", "ms"]
        }
        "request_failed" => &[
            "reqId",
            "provider",
            "model",
            "countTokens",
            "status",
            "ms",
            "message",
            "phase",
            "inBandSse",
            "bodyTruncated",
            "bodyTimedOut",
            "bodyReadError",
        ],
        "request_abandoned" => &["reqId", "provider", "model", "countTokens", "ms", "message"],
        "client_tool_result" => &[
            "reqId",
            "messageIndex",
            "blockIndex",
            "callIdHash",
            "isError",
            "contentBytes",
        ],
        "tool_block_started" | "tool_block_completed" | "tool_block_interrupted" => &[
            "reqId",
            "provider",
            "model",
            "index",
            "toolKind",
            "toolName",
            "callIdHash",
            "elapsedMs",
            "interruptReason",
        ],
        "request_configuration" => &[
            "reqId",
            "model",
            "serviceTier",
            "reasoningEffort",
            "transport",
            "transportRequested",
            "transportReason",
            "responsesLite",
            "nativeWebSearch",
            "parallelToolCalls",
            "promptCacheKeyPresent",
            "toolChoice",
            "functionToolCount",
            "hostedToolCount",
            "inputFunctionToolCount",
            "laneReason",
            "anthropicMaxTokens",
            "outputBudgetEnforcement",
            "estimatedInputTokens",
        ],
        "native_web_search_phase" => &["reqId", "phase", "elapsedMs"],
        "upstream_first_event" => &["reqId", "event", "elapsedMs"],
        "stream_committed_before_semantic" => &[
            "reqId",
            "generationStarted",
            "semanticEmitted",
            "heartbeatIntervalMs",
        ],
        "terminal_resolution" => &["reqId", "authority", "sourceEvent"],
        "buffered_transport_retry" | "live_transport_retry" => &[
            "reqId",
            "transport",
            "failedAttempt",
            "nextAttempt",
            "maxAttempts",
            "delayMs",
            "status",
            "origin",
            "reason",
        ],
        "buffered_transport_retry_exhausted" => &[
            "reqId",
            "transport",
            "attempts",
            "status",
            "origin",
            "reason",
        ],
        "live_transport_retry_exhausted" => &[
            "reqId",
            "transport",
            "attempts",
            "maxAttempts",
            "status",
            "replaySafety",
        ],
        "post_ping_replay_scheduled" | "post_ping_replay_rejected" => {
            &["reqId", "transport", "status", "replaySafety", "delayMs"]
        }
        "auto_transport_fallback" => &["reqId", "action", "origin", "status", "reason", "detail"],
        "websocket_circuit_fallback" => &["reqId", "cooldownMs", "fallbackTransport"],
        "websocket_circuit_opened" => &["reqId", "failures", "cooldownMs"],
        "websocket_pool_refresh" => &["reqId", "reason"],
        "websocket_terminal_pool_probe_failed" => &["reason", "detail"],
        "upstream_retry" => &[
            "reqId",
            "attempt",
            "transientFailures",
            "waitMs",
            "replaySafety",
            "deadlineRemainingMs",
            "status",
            "origin",
            "stage",
            "message",
        ],
        "upstream_retry_exhausted" => &[
            "reqId",
            "attempt",
            "replaySafety",
            "deadlineRemainingMs",
            "status",
            "origin",
            "stage",
            "message",
        ],
        "stream_reducer_error" => &[
            "reqId",
            "model",
            "stage",
            "kind",
            "message",
            "bytes",
            "chunks",
            "downstreamEmitted",
        ],
        "stream_terminal" => &[
            "reqId",
            "model",
            "outcome",
            "stopReason",
            "bytes",
            "chunks",
            "downstreamEmitted",
            "incompleteReason",
            "upstreamIncompleteReason",
            "stage",
            "kind",
            "status",
            "origin",
            "errorStage",
        ],
        "log_records_dropped" => &["count", "queueCapacity"],
        _ => &[],
    }
}

fn sanitize_timestamp(value: &str) -> Option<String> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    parsed.format(&Rfc3339).ok()
}

fn enum_string(value: &Value, choices: &[&str]) -> Option<Value> {
    value
        .as_str()
        .filter(|v| choices.contains(v))
        .map(|v| json!(v))
}

fn is_short_hex(value: &str) -> bool {
    value.len() <= 64 && !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn diagnostic_model(value: &str) -> &'static str {
    CODEX_MODELS
        .iter()
        .chain(GROK_MODELS.iter())
        .copied()
        .find(|model| *model == value)
        .unwrap_or("other")
}

fn sanitize_tool_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn tool_choice_category(value: &Value) -> &'static str {
    match value {
        Value::String(value) => match value.as_str() {
            "auto" => "auto",
            "none" => "none",
            "required" => "required",
            _ => "unknown",
        },
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("auto") => "auto",
            Some("none") => "none",
            Some("required") => "required",
            Some("function") => "function",
            Some("tool") => "tool",
            _ => "object",
        },
        _ => "unknown",
    }
}

fn protocol_event_category(value: &str) -> &'static str {
    match value {
        "response.created" => "response_created",
        "response.completed" => "response_completed",
        "response.incomplete" => "response_incomplete",
        "response.failed" => "response_failed",
        "response.error" => "response_error",
        "response.output_item.added" => "output_item_added",
        "response.output_item.done" => "output_item_done",
        "response.output_text.delta" => "output_text_delta",
        "response.function_call_arguments.delta" => "function_arguments_delta",
        "response.function_call_arguments.done" => "function_arguments_done",
        _ => "other",
    }
}

fn classify_dynamic_text(value: &str) -> &'static str {
    let lower = value.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") || lower.contains("etimedout") {
        "timeout"
    } else if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
    {
        "rate_limit"
    } else if lower.contains("connection reset")
        || lower.contains("econnreset")
        || lower.contains("broken pipe")
    {
        "connection_reset"
    } else if lower.contains("connection closed")
        || lower.contains("stream closed")
        || lower.contains("unexpected eof")
    {
        "connection_closed"
    } else if lower.contains("dns") || lower.contains("name resolution") {
        "dns"
    } else if lower.contains("tls") || lower.contains("certificate") {
        "tls"
    } else if lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("401")
        || lower.contains("403")
    {
        "authorization"
    } else if lower.contains("overload")
        || lower.contains("unavailable")
        || lower.contains("503")
        || lower.contains("529")
    {
        "upstream_unavailable"
    } else if lower.contains("json")
        || lower.contains("malformed")
        || lower.contains("protocol")
        || lower.contains("invalid event")
    {
        "protocol"
    } else if lower.contains("limit") || lower.contains("too large") || lower.contains("exceeded") {
        "limit"
    } else if lower.contains("cancel") || lower.contains("abandon") || lower.contains("dropped") {
        "cancelled"
    } else {
        "other"
    }
}

fn dynamic_fingerprint(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len().min(512));
    let mut variable = false;
    for character in value.to_ascii_lowercase().chars().take(4096) {
        if character.is_ascii_digit()
            || character == '/'
            || character == '\\'
            || character == ':'
            || character == '@'
        {
            if !variable {
                normalized.push('#');
                variable = true;
            }
        } else if character.is_ascii_alphabetic() || matches!(character, ' ' | '_' | '-' | '.') {
            normalized.push(character);
            variable = false;
        } else if !variable {
            normalized.push('#');
            variable = true;
        }
    }
    identifier_hash(&normalized)
}

fn identifier_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..16].to_string()
}

fn encode_events(events: &VecDeque<Value>) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for event in events {
        serde_json::to_writer(&mut output, event)?;
        output.push(b'\n');
        ensure!(
            output.len() as u64 <= MAX_EVENTS_BYTES,
            "sanitized events exceed the bundle limit"
        );
    }
    Ok(output)
}

fn write_archive_atomic(
    output: &Path,
    manifest: &[u8],
    events: &[u8],
    environment: &[u8],
) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let (temporary, file) = create_temporary(
        parent,
        output
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("diagnostics"),
    )?;
    let result = (|| -> Result<()> {
        set_file_mode_600(&temporary)?;
        write_archive_file(file, manifest, events, environment)?;
        fs::hard_link(&temporary, output)
            .context("failed to atomically publish diagnostic bundle")?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn write_archive_file(
    file: File,
    manifest: &[u8],
    events: &[u8],
    environment: &[u8],
) -> Result<()> {
    let encoder = GzEncoder::new(file, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    append_archive_bytes(&mut archive, "manifest.json", manifest)?;
    append_archive_bytes(&mut archive, "events.jsonl", events)?;
    append_archive_bytes(&mut archive, "environment.json", environment)?;
    let encoder = archive.into_inner()?;
    let file = encoder.finish()?;
    file.sync_all()?;
    ensure!(
        file.metadata()?.len() <= MAX_BUNDLE_BYTES,
        "compressed diagnostic bundle exceeds {MAX_BUNDLE_BYTES} bytes"
    );
    Ok(())
}

fn create_temporary(parent: &Path, stem: &str) -> Result<(PathBuf, File)> {
    for _ in 0..16 {
        let path = parent.join(format!(".{stem}.{}.part", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("failed to allocate a unique diagnostic temporary file")
}

fn set_file_mode_600(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn append_archive_bytes<W: Write>(
    archive: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, name, bytes)?;
    Ok(())
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn runtime_kind() -> &'static str {
    let wsl_kernel = cfg!(target_os = "linux")
        && fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .is_some_and(|release| {
                let release = release.to_ascii_lowercase();
                release.contains("microsoft") || release.contains("wsl")
            });
    if cfg!(target_os = "linux")
        && (wsl_kernel
            || std::env::var_os("WSL_INTEROP").is_some()
            || std::env::var_os("WSL_DISTRO_NAME").is_some())
    {
        "wsl"
    } else {
        "native"
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn current_binary_sha256() -> Option<String> {
    CURRENT_BINARY_SHA256
        .get_or_init(|| {
            let executable = std::env::current_exe().ok()?;
            sha256_file(&executable).ok()
        })
        .clone()
}

struct VerifiedBundle {
    bundle_id: String,
    event_count: usize,
    manifest: Vec<u8>,
    events: Vec<u8>,
    environment: Vec<u8>,
}

fn verify_bundle_for_upload(path: &Path) -> Result<VerifiedBundle> {
    let metadata = fs::symlink_metadata(path).context("diagnostic bundle is unavailable")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "diagnostic bundle must be a regular non-symlink file"
    );
    ensure!(
        metadata.len() <= MAX_BUNDLE_BYTES,
        "diagnostic bundle exceeds upload size limit"
    );
    let decoder = GzDecoder::new(File::open(path)?);
    let mut archive = tar::Archive::new(decoder);
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut total = 0_u64;
    for entry in archive.entries().context("invalid diagnostic archive")? {
        let mut entry = entry?;
        ensure!(
            entry.header().entry_type().is_file(),
            "diagnostic archive contains a non-file entry"
        );
        let path = entry.path()?;
        let name = path
            .to_str()
            .ok_or_else(|| anyhow!("diagnostic archive contains a non-UTF-8 path"))?
            .to_string();
        ensure!(
            matches!(
                name.as_str(),
                "manifest.json" | "events.jsonl" | "environment.json"
            ),
            "diagnostic archive contains unexpected entry {name}"
        );
        ensure!(
            !files.contains_key(&name),
            "diagnostic archive contains duplicate entry {name}"
        );
        let size = entry.size();
        let entry_limit = if name == "events.jsonl" {
            MAX_EVENTS_BYTES
        } else {
            MAX_METADATA_ENTRY_BYTES
        };
        ensure!(size <= entry_limit, "diagnostic archive entry is too large");
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow!("diagnostic archive size overflow"))?;
        ensure!(
            total <= MAX_VERIFIED_TOTAL_BYTES,
            "diagnostic archive expands beyond the privacy verification limit"
        );
        let mut bytes = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut bytes)?;
        files.insert(name, bytes);
    }
    ensure!(
        files.len() == 3,
        "diagnostic archive must contain exactly three files"
    );
    let manifest: Manifest = serde_json::from_slice(files.get("manifest.json").unwrap())
        .context("invalid diagnostic manifest")?;
    validate_manifest(&manifest)?;
    let environment: Environment = serde_json::from_slice(files.get("environment.json").unwrap())
        .context("invalid diagnostic environment")?;
    validate_environment(&environment)?;
    let events = files.get("events.jsonl").unwrap();
    let mut event_count = 0_usize;
    let mut canonical_events = Vec::new();
    for line in events
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        ensure!(
            line.len() <= MAX_LOG_LINE_BYTES,
            "diagnostic event line is too large"
        );
        let value: Value = serde_json::from_slice(line).context("invalid diagnostic event")?;
        ensure!(
            is_sanitized_event(&value),
            "diagnostic archive contains an event outside the metadata-only schema"
        );
        serde_json::to_writer(&mut canonical_events, &value)?;
        canonical_events.push(b'\n');
        event_count += 1;
        ensure!(
            event_count <= MAX_EVENT_LINES,
            "diagnostic archive contains too many events"
        );
    }
    ensure!(
        event_count == manifest.event_count,
        "diagnostic manifest event count does not match events.jsonl"
    );
    ensure!(
        canonical_events.len() as u64 <= MAX_EVENTS_BYTES,
        "canonical diagnostic events exceed the bundle limit"
    );
    let bundle_id = manifest.bundle_id.clone();
    let canonical_manifest = serde_json::to_vec_pretty(&manifest)?;
    let canonical_environment = serde_json::to_vec_pretty(&environment)?;
    ensure!(
        canonical_manifest.len() as u64 <= MAX_METADATA_ENTRY_BYTES
            && canonical_environment.len() as u64 <= MAX_METADATA_ENTRY_BYTES,
        "canonical diagnostic metadata exceeds the bundle limit"
    );
    Ok(VerifiedBundle {
        bundle_id,
        event_count,
        manifest: canonical_manifest,
        events: canonical_events,
        environment: canonical_environment,
    })
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.schema_version == SCHEMA_VERSION
            && manifest.format == FORMAT_NAME
            && manifest.privacy_level == PRIVACY_LEVEL,
        "archive is not a supported metadata-only diagnostic bundle"
    );
    Uuid::parse_str(&manifest.bundle_id).context("invalid diagnostic bundle id")?;
    sanitize_timestamp(&manifest.created_at)
        .ok_or_else(|| anyhow!("invalid diagnostic creation time"))?;
    ensure!(
        (1..=MAX_INPUT_BYTES).contains(&manifest.max_input_bytes)
            && (1..=MAX_EVENT_LINES).contains(&manifest.max_lines)
            && manifest.input_bytes <= manifest.max_input_bytes
            && manifest.event_count <= manifest.max_lines
            && manifest.source_file_count <= MAX_SOURCE_FILES,
        "diagnostic manifest limits are invalid"
    );
    ensure!(
        manifest.dropped_by_line_limit == 0 || manifest.truncated,
        "diagnostic manifest truncation counters are inconsistent"
    );
    ensure!(
        manifest.files == ["manifest.json", "events.jsonl", "environment.json"],
        "diagnostic manifest file list is invalid"
    );
    ensure!(
        manifest.excluded
            == [
                "traffic-captures",
                "error-captures",
                "configuration",
                "authentication",
                "home",
                "hostname",
                "paths",
                "session-identifiers",
                "prompts",
                "tool-arguments",
                "tool-results",
                "raw-logs",
            ],
        "diagnostic manifest exclusion policy is invalid"
    );
    if let Some(hash) = manifest.request_id_hash.as_deref() {
        ensure!(
            hash.len() == 16 && is_short_hex(hash),
            "invalid request id fingerprint"
        );
    }
    Ok(())
}

fn validate_environment(environment: &Environment) -> Result<()> {
    ensure!(
        environment.schema_version == SCHEMA_VERSION,
        "unsupported environment schema"
    );
    ensure!(
        matches!(
            environment.os.as_str(),
            "linux"
                | "macos"
                | "windows"
                | "freebsd"
                | "openbsd"
                | "netbsd"
                | "dragonfly"
                | "android"
                | "ios"
        ) && matches!(
            environment.arch.as_str(),
            "x86"
                | "x86_64"
                | "arm"
                | "aarch64"
                | "mips"
                | "mips64"
                | "powerpc"
                | "powerpc64"
                | "riscv32"
                | "riscv64"
                | "s390x"
                | "wasm32"
                | "wasm64"
        ) && matches!(environment.runtime.as_str(), "native" | "wsl"),
        "invalid diagnostic environment metadata"
    );
    ensure!(
        is_supported_version_metadata(&environment.ccproxy_version),
        "invalid ccproxy version metadata"
    );
    if let Some(label) = environment.machine_label.as_deref() {
        validate_machine_label(label)?;
    }
    ensure!(
        environment.git_sha == "unknown"
            || (matches!(environment.git_sha.len(), 40 | 64)
                && environment
                    .git_sha
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())),
        "invalid git build identity"
    );
    if let Some(timestamp) = environment.build_timestamp {
        let now = OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
        ensure!(
            (1_577_836_800..=now.saturating_add(86_400)).contains(&timestamp),
            "invalid build timestamp"
        );
    }
    if let Some(hash) = environment.binary_sha256.as_deref() {
        ensure!(
            hash.len() == 64 && is_short_hex(hash),
            "invalid binary build identity"
        );
    }
    Ok(())
}

fn is_supported_version_metadata(value: &str) -> bool {
    let (base, suffix) = value
        .split_once('-')
        .map_or((value, None), |(base, suffix)| (base, Some(suffix)));
    let mut parts = base.split('.');
    let valid_base = (0..3).all(|_| {
        parts.next().is_some_and(|part| {
            !part.is_empty() && part.len() <= 10 && part.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) && parts.next().is_none();
    if !valid_base {
        return false;
    }
    match suffix {
        None => true,
        Some(suffix) => ["preview", "alpha", "beta", "rc"]
            .into_iter()
            .any(|prefix| {
                suffix.strip_prefix(prefix).is_some_and(|tail| {
                    tail.strip_prefix('.').is_some_and(|number| {
                        !number.is_empty()
                            && number.len() <= 10
                            && number.bytes().all(|byte| byte.is_ascii_digit())
                    })
                })
            }),
    }
}

fn is_sanitized_event(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "t" | "level" | "service" | "event" | "fields"))
    {
        return false;
    }
    let Some(event) = object.get("event").and_then(Value::as_str) else {
        return false;
    };
    if !allowed_event(event)
        || sanitize_timestamp(object.get("t").and_then(Value::as_str).unwrap_or("")).is_none()
    {
        return false;
    }
    if !matches!(
        object.get("level").and_then(Value::as_str),
        Some("debug" | "info" | "warn" | "error")
    ) {
        return false;
    }
    if !matches!(
        object.get("service").and_then(Value::as_str),
        Some("server" | "codex" | "grok" | "logging")
    ) {
        return false;
    }
    let Some(fields) = object.get("fields").and_then(Value::as_object) else {
        return false;
    };
    let allowed: BTreeSet<String> = allowed_fields(event)
        .iter()
        .map(|key| {
            if *key == "reqId" {
                "reqIdHash".into()
            } else if *key == "model" {
                "model".into()
            } else if *key == "toolChoice" {
                "toolChoiceCategory".into()
            } else if matches!(*key, "message" | "reason" | "detail" | "bodyReadError") {
                format!("{key}Diagnostic")
            } else {
                (*key).into()
            }
        })
        .collect();
    fields
        .iter()
        .all(|(key, value)| allowed.contains(key) && sanitized_field_value(key, value))
}

fn sanitized_field_value(key: &str, value: &Value) -> bool {
    if key.ends_with("Diagnostic") {
        let Some(object) = value.as_object() else {
            return false;
        };
        return object.len() == 2
            && object
                .get("class")
                .and_then(Value::as_str)
                .is_some_and(|class| {
                    matches!(
                        class,
                        "timeout"
                            | "rate_limit"
                            | "connection_reset"
                            | "connection_closed"
                            | "dns"
                            | "tls"
                            | "authorization"
                            | "upstream_unavailable"
                            | "protocol"
                            | "limit"
                            | "cancelled"
                            | "other"
                    )
                })
            && object
                .get("fingerprint")
                .and_then(Value::as_str)
                .is_some_and(|v| v.len() == 16 && is_short_hex(v));
    }
    match key {
        "reqIdHash" => value
            .as_str()
            .is_some_and(|v| v.len() == 16 && is_short_hex(v)),
        "callIdHash" => value
            .as_str()
            .is_some_and(|v| v.len() == 16 && is_short_hex(v)),
        "model" => value
            .as_str()
            .is_some_and(|model| diagnostic_model(model) == model),
        "toolName" => value.as_str().is_some_and(|name| {
            sanitize_tool_name(name).as_deref() == Some(name) && name.len() <= 128
        }),
        "interruptReason" => matches!(
            value.as_str(),
            Some(
                "index_reused"
                    | "response_eof"
                    | "in_band_sse_error"
                    | "response_body_error"
                    | "downstream_dropped"
                    | "message_stop_before_block_stop"
            )
        ),
        "event" | "sourceEvent" => matches!(
            value.as_str(),
            Some(
                "response_created"
                    | "response_completed"
                    | "response_incomplete"
                    | "response_failed"
                    | "response_error"
                    | "output_item_added"
                    | "output_item_done"
                    | "output_text_delta"
                    | "function_arguments_delta"
                    | "function_arguments_done"
                    | "other"
            )
        ),
        "toolChoiceCategory" => matches!(
            value.as_str(),
            Some("auto" | "none" | "required" | "function" | "tool" | "object" | "unknown")
        ),
        _ => sanitize_scalar_field(key, value).as_ref() == Some(value),
    }
}

fn validate_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty() && host.len() <= 253 && host.is_ascii() && !host.starts_with('-'),
        "invalid upload host"
    );
    if IpAddr::from_str(host).is_ok() {
        return Ok(());
    }
    ensure!(
        host.split('.').all(|label| !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')),
        "invalid upload host"
    );
    Ok(())
}

fn validate_user(user: &str) -> Result<()> {
    ensure!(
        !user.is_empty()
            && user.len() <= 64
            && !user.starts_with('-')
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
        "invalid upload user"
    );
    Ok(())
}

fn validate_remote_dir(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && path.len() <= 512 && !path.contains("//") && !path.ends_with('/'),
        "remote_dir must be an absolute normalized POSIX directory"
    );
    ensure!(
        path[1..].split('/').all(|component| {
            !component.is_empty()
                && !matches!(component, "." | "..")
                && component.len() <= 128
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        }),
        "remote_dir contains an unsafe path component"
    );
    Ok(())
}

fn join_remote(directory: &str, name: &str) -> String {
    format!("{directory}/{name}")
}

fn build_sftp_batch(local: &Path, remote_part: &str, remote_final: &str) -> Result<String> {
    let local = local
        .to_str()
        .ok_or_else(|| anyhow!("local bundle path is not UTF-8"))?;
    Ok(format!(
        "put {} {}\nrename {} {}\n",
        sftp_quote(local)?,
        sftp_quote(remote_part)?,
        sftp_quote(remote_part)?,
        sftp_quote(remote_final)?
    ))
}

fn sftp_quote(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control),
        "sftp path contains control characters"
    );
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn log(event: &str, fields: Value) -> String {
        json!({"t":"2026-07-22T12:00:00Z","level":"warn","service":"codex","msg":event,"fields":fields}).to_string()
    }

    #[test]
    fn options_and_upload_target_are_strict() {
        assert!(
            UploadTarget::new(
                "dev.example".into(),
                Some("alice".into()),
                "/srv/ccproxy/diagnostics".into()
            )
            .is_ok()
        );
        assert!(
            UploadTarget::new(
                "-oProxyCommand=bad".into(),
                Some("alice".into()),
                "/srv/x".into()
            )
            .is_err()
        );
        assert!(
            UploadTarget::new(
                "dev.example".into(),
                Some("a\nput bad".into()),
                "/srv/x".into()
            )
            .is_err()
        );
        assert!(
            UploadTarget::new(
                "dev.example".into(),
                Some("alice".into()),
                "relative/path".into()
            )
            .is_err()
        );
        assert!(
            UploadTarget::new(
                "dev.example".into(),
                Some("alice".into()),
                "/srv/../secret".into()
            )
            .is_err()
        );
        let options = CollectOptions {
            max_lines: 0,
            ..CollectOptions::default()
        };
        assert!(validate_collect_options(&options).is_err());
    }

    #[test]
    fn dynamic_errors_are_classified_and_fingerprinted_without_raw_text() {
        let first = sanitize_event(
            &serde_json::from_str(&log(
                "request_failed",
                json!({"reqId":"one","message":"connection reset at /home/alice/a:123"}),
            ))
            .unwrap(),
        )
        .unwrap();
        let encoded = first.to_string();
        assert_eq!(
            first["fields"]["messageDiagnostic"]["class"],
            "connection_reset"
        );
        assert!(!encoded.contains("alice"));
        assert!(!encoded.contains("connection reset"));
    }

    #[test]
    fn collection_contains_only_metadata_files_and_redacts_dynamic_fields() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("proxy.100"),
            format!(
                "{}\n",
                log(
                    "request_failed",
                    json!({
                        "reqId":"req-old",
                        "provider":"codex",
                        "model":"gpt-5.6-sol",
                        "status":502,
                        "message":"timeout reading /home/alice/secret Bearer AUTH_CANARY",
                        "errorFile":"/home/alice/error.json",
                        "toolArguments":{"query":"TOOL_ARGUMENT_CANARY"}
                    })
                )
            ),
        )
        .unwrap();
        fs::write(
            root.path().join("proxy.log"),
            format!(
                "{}\n",
                log(
                    "client_tool_result",
                    json!({
                        "reqId":"req-new",
                        "callIdHash":"1234abcd5678ef90",
                        "isError":false,
                        "contentBytes":42,
                        "message":"TOOL_RESULT_CANARY",
                        "arguments":"TOOL_ARGUMENT_CANARY"
                    })
                )
            ),
        )
        .unwrap();
        fs::write(root.path().join("traffic.json"), "TOP_SECRET").unwrap();
        let output = root.path().join("bundle.tar.gz");
        let report = collect_bundle(&CollectOptions {
            state_dir: root.path().into(),
            output: output.clone(),
            machine_label: Some("wsl-dev".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(report.event_count, 2);
        assert_eq!(report.sha256.len(), 64);
        verify_bundle_for_upload(&output).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(File::open(&output).unwrap()));
        let mut unpacked = Vec::new();
        for entry in archive.entries().unwrap() {
            entry.unwrap().read_to_end(&mut unpacked).unwrap();
        }
        let unpacked = String::from_utf8_lossy(&unpacked);
        assert!(!unpacked.contains("TOP_SECRET"));
        assert!(!unpacked.contains("/home/alice"));
        assert!(!unpacked.contains("AUTH_CANARY"));
        assert!(!unpacked.contains("TOOL_ARGUMENT_CANARY"));
        assert!(!unpacked.contains("TOOL_RESULT_CANARY"));
        assert!(!unpacked.contains("toolArguments"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn request_filter_never_writes_the_raw_request_id() {
        let root = tempdir().unwrap();
        let secret_id = "request-private-123";
        fs::write(
            root.path().join("proxy.log"),
            format!(
                "{}\n{}\n",
                log("request_completed", json!({"reqId":secret_id,"status":200})),
                log("request_completed", json!({"reqId":"other","status":200}))
            ),
        )
        .unwrap();
        let output = root.path().join("filtered.tar.gz");
        let report = collect_bundle(&CollectOptions {
            state_dir: root.path().into(),
            output: output.clone(),
            request_id: Some(secret_id.into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(report.event_count, 1);
        let mut archive = tar::Archive::new(GzDecoder::new(File::open(output).unwrap()));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(secret_id));
        }
    }

    #[test]
    fn publish_is_no_clobber() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("proxy.log"), "").unwrap();
        let output = root.path().join("bundle.tar.gz");
        fs::write(&output, "keep").unwrap();
        assert!(
            collect_bundle(&CollectOptions {
                state_dir: root.path().into(),
                output: output.clone(),
                ..Default::default()
            })
            .is_err()
        );
        assert_eq!(fs::read_to_string(output).unwrap(), "keep");
    }

    #[test]
    fn line_limit_reports_real_truncation_and_preserves_latest_event() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("proxy.log"),
            format!(
                "{}\n{}\n",
                log("request_completed", json!({"reqId":"older","status":200})),
                log("request_completed", json!({"reqId":"newer","status":201}))
            ),
        )
        .unwrap();
        let report = collect_bundle(&CollectOptions {
            state_dir: root.path().into(),
            output: root.path().join("limited.tar.gz"),
            max_lines: 1,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(report.event_count, 1);
        assert_eq!(report.skipped_count, 1);
        assert!(report.truncated);
        inspect_bundle(&report.bundle_path).unwrap();
    }

    #[test]
    fn tool_block_keeps_safe_lowercase_name_and_fixed_interrupt_reason() {
        let event = sanitize_event(
            &serde_json::from_str(&log(
                "tool_block_interrupted",
                json!({
                    "reqId":"request-1",
                    "provider":"codex",
                    "model":"gpt-5.6-sol",
                    "toolKind":"tool_use",
                    "toolName":"Brave_Search:Query",
                    "interruptReason":"downstream_dropped"
                }),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(event["fields"]["model"], "gpt-5.6-sol");
        assert_eq!(event["fields"]["toolName"], "brave_search:query");
        assert_eq!(event["fields"]["interruptReason"], "downstream_dropped");
        assert!(is_sanitized_event(&event));
    }

    #[test]
    fn upstream_incomplete_reason_is_categorized_and_forged_enums_are_rejected() {
        let event = sanitize_event(
            &serde_json::from_str(&log(
                "stream_terminal",
                json!({
                    "reqId":"request-1",
                    "model":"grok-4.5",
                    "outcome":"incomplete",
                    "stopReason":"refusal",
                    "incompleteReason":"other",
                    "upstreamIncompleteReason":"private_secret"
                }),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(event["fields"]["upstreamIncompleteReason"], "other");
        assert!(!event.to_string().contains("private_secret"));
        assert!(is_sanitized_event(&event));

        let mut forged = event;
        forged["fields"]["stage"] = json!("private_secret");
        assert!(!is_sanitized_event(&forged));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_log() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, root.path().join("proxy.log")).unwrap();
        assert!(
            collect_bundle(&CollectOptions {
                state_dir: root.path().into(),
                output: root.path().join("bundle.tar.gz"),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_hard_linked_log() {
        let root = tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, "secret").unwrap();
        fs::hard_link(&outside, root.path().join("proxy.log")).unwrap();
        assert!(
            collect_bundle(&CollectOptions {
                state_dir: root.path().into(),
                output: root.path().join("bundle.tar.gz"),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn sftp_batch_uses_part_then_rename_and_quotes_paths() {
        let batch = build_sftp_batch(
            Path::new("/tmp/a b\"c.tar.gz"),
            "/srv/.x.part",
            "/srv/x.tar.gz",
        )
        .unwrap();
        assert!(batch.starts_with("put \"/tmp/a b\\\"c.tar.gz\" \"/srv/.x.part\""));
        assert!(batch.ends_with("rename \"/srv/.x.part\" \"/srv/x.tar.gz\"\n"));
    }

    #[cfg(unix)]
    #[test]
    fn upload_executes_sftp_directly_with_a_private_staging_copy() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        fs::write(
            root.path().join("proxy.log"),
            format!(
                "{}\n",
                log(
                    "request_completed",
                    json!({"reqId":"request-1","status":200})
                )
            ),
        )
        .unwrap();
        let bundle = root.path().join("bundle.tar.gz");
        collect_bundle(&CollectOptions {
            state_dir: root.path().into(),
            output: bundle.clone(),
            ..Default::default()
        })
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&bundle)
            .unwrap()
            .write_all(b"PRIVATE_TRAILING_CANARY")
            .unwrap();

        let fake_sftp = root.path().join("fake-sftp");
        fs::write(
            &fake_sftp,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$0.args"
cat > "$0.batch"
staged=$(sed -n '1s/^put "\([^"]*\)".*/\1/p' "$0.batch")
cp "$staged" "$0.uploaded"
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_sftp, fs::Permissions::from_mode(0o700)).unwrap();
        let target = UploadTarget::new(
            "ccproxy-dev".into(),
            Some("developer".into()),
            "/srv/ccproxy-diagnostics".into(),
        )
        .unwrap();

        let report = upload_bundle_sftp_with_program(&bundle, &target, &fake_sftp).unwrap();
        assert!(report.size_bytes < fs::metadata(&bundle).unwrap().len());
        let arguments = fs::read_to_string(format!("{}.args", fake_sftp.display())).unwrap();
        assert!(arguments.contains("BatchMode=yes"));
        assert!(arguments.contains("developer@ccproxy-dev"));
        let batch = fs::read_to_string(format!("{}.batch", fake_sftp.display())).unwrap();
        assert!(batch.contains(".part\"\nrename "));
        assert!(!batch.contains(bundle.to_str().unwrap()));
        let staged_path = batch.split('"').nth(1).unwrap();
        assert!(!Path::new(staged_path).exists());
        let uploaded = PathBuf::from(format!("{}.uploaded", fake_sftp.display()));
        assert_eq!(report.size_bytes, fs::metadata(&uploaded).unwrap().len());
        assert!(
            !fs::read(&uploaded)
                .unwrap()
                .windows(b"PRIVATE_TRAILING_CANARY".len())
                .any(|window| window == b"PRIVATE_TRAILING_CANARY")
        );
        inspect_bundle(&uploaded).unwrap();
    }

    #[test]
    fn verification_rewrites_json_and_drops_hidden_duplicate_field_data() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("proxy.log"),
            format!(
                "{}\n",
                log(
                    "stream_reducer_error",
                    json!({
                        "reqId":"request-1",
                        "model":"grok-4.5",
                        "stage":"reducer",
                        "kind":"invalid_event",
                        "bytes":1,
                        "chunks":1,
                        "downstreamEmitted":false
                    })
                )
            ),
        )
        .unwrap();
        let source = root.path().join("source.tar.gz");
        collect_bundle(&CollectOptions {
            state_dir: root.path().into(),
            output: source.clone(),
            ..Default::default()
        })
        .unwrap();
        let verified = verify_bundle_for_upload(&source).unwrap();

        let forged = root.path().join("forged.tar.gz");
        let forged_event = concat!(
            "{\"t\":\"2026-07-22T12:00:00Z\",\"level\":\"warn\",",
            "\"service\":\"grok\",\"event\":\"stream_reducer_error\",\"fields\":{",
            "\"reqIdHash\":\"0123456789abcdef\",\"model\":\"grok-4.5\",",
            "\"stage\":\"DUPLICATE_FIELD_CANARY\",\"stage\":\"reducer\",",
            "\"kind\":\"invalid_event\",\"bytes\":1,\"chunks\":1,",
            "\"downstreamEmitted\":false}}\n"
        );
        write_archive_file(
            File::create(&forged).unwrap(),
            &verified.manifest,
            forged_event.as_bytes(),
            &verified.environment,
        )
        .unwrap();

        let canonical = verify_bundle_for_upload(&forged).unwrap();
        assert!(
            !String::from_utf8(canonical.events)
                .unwrap()
                .contains("DUPLICATE_FIELD_CANARY")
        );
    }

    #[test]
    fn arbitrary_archive_is_not_uploadable() {
        let root = tempdir().unwrap();
        let path = root.path().join("fake.tar.gz");
        let file = File::create(&path).unwrap();
        let mut archive = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_archive_bytes(
            &mut archive,
            "manifest.json",
            br#"{"privacyLevel":"metadata-only"}"#,
        )
        .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        assert!(verify_bundle_for_upload(&path).is_err());
    }
}
