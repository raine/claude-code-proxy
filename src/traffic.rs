use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};

use crate::logging::REDACT_KEYS;
use crate::paths;

#[derive(Debug)]
pub struct TrafficCapture {
    root: PathBuf,
    quota: Arc<TrafficQuota>,
    captured_charged_bytes: AtomicU64,
    captured_files: AtomicU64,
    disabled: AtomicBool,
    artifact_counter: Mutex<usize>,
    event_counter: Mutex<usize>,
}

pub const DEFAULT_TRAFFIC_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_TRAFFIC_MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_TRAFFIC_MAX_TOTAL_FILES: u64 = 65_536;
pub const DEFAULT_TRAFFIC_MAX_CAPTURE_FILES: u64 = 4_096;
const MAX_TRAFFIC_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_TRAFFIC_MAX_CAPTURE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_TRAFFIC_MAX_TOTAL_FILES: u64 = 1_048_576;
const MAX_TRAFFIC_MAX_CAPTURE_FILES: u64 = 65_536;
const TRAFFIC_FILE_CHARGE_BYTES: u64 = 4 * 1024;

pub const MAX_SSE_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STREAM_CAPTURE_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STREAM_CAPTURE_EVENTS: usize = 1_024;
pub const MAX_STREAM_CAPTURE_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct TrafficCaptureOptions {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: Option<String>,
    pub state_dir_override: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
struct TrafficQuotaLimits {
    total_bytes: u64,
    capture_bytes: u64,
    total_files: u64,
    capture_files: u64,
}

#[derive(Debug)]
struct TrafficQuota {
    used_charged_bytes: AtomicU64,
    used_files: AtomicU64,
    limits: TrafficQuotaLimits,
    warned: AtomicBool,
    scan_failed: bool,
}

#[derive(Debug, Default)]
struct TrafficQuotaCell {
    quota: OnceLock<Arc<TrafficQuota>>,
    async_initialization: tokio::sync::Mutex<()>,
}

static TRAFFIC_QUOTAS: LazyLock<Mutex<HashMap<PathBuf, Arc<TrafficQuotaCell>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn traffic_capture_enabled() -> bool {
    std::env::var_os("CCP_TRAFFIC_LOG").is_some_and(|value| {
        value
            .to_str()
            .is_some_and(|value| matches!(value, "1" | "true" | "yes"))
    })
}

pub fn traffic_capture_enabled_for_env(env: &std::collections::HashMap<String, String>) -> bool {
    match env.get("CCP_TRAFFIC_LOG").map(String::as_str) {
        Some(v) => matches!(v, "1" | "true" | "yes"),
        None => false,
    }
}

fn traffic_quota_cell(root: &Path) -> Arc<TrafficQuotaCell> {
    let mut quotas = TRAFFIC_QUOTAS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cell) = quotas.get(root) {
        return cell.clone();
    }

    let cell = Arc::new(TrafficQuotaCell::default());
    quotas.insert(root.to_path_buf(), cell.clone());
    cell
}

fn initialize_traffic_quota(root: &Path, cell: &TrafficQuotaCell) -> Arc<TrafficQuota> {
    cell.quota
        .get_or_init(|| Arc::new(TrafficQuota::from_existing(root, traffic_quota_limits())))
        .clone()
}

fn traffic_quota(root: &Path) -> Arc<TrafficQuota> {
    let cell = traffic_quota_cell(root);
    initialize_traffic_quota(root, &cell)
}

async fn traffic_quota_async(root: PathBuf) -> Arc<TrafficQuota> {
    let cell = traffic_quota_cell(&root);
    if let Some(quota) = cell.quota.get() {
        return quota.clone();
    }

    // Concurrent first requests wait asynchronously on this per-root gate. Only
    // the winner occupies a blocking worker; the global root registry is not
    // held while the recursive filesystem scan runs.
    let _initialization = cell.async_initialization.lock().await;
    if let Some(quota) = cell.quota.get() {
        return quota.clone();
    }

    let limits = traffic_quota_limits();
    let scan_cell = cell.clone();
    match tokio::task::spawn_blocking(move || {
        scan_cell
            .quota
            .get_or_init(|| Arc::new(TrafficQuota::from_existing(&root, limits)))
            .clone()
    })
    .await
    {
        Ok(quota) => quota,
        Err(_) => Arc::new(TrafficQuota::scan_failure(limits)),
    }
}

fn traffic_quota_limits() -> TrafficQuotaLimits {
    let total_bytes = positive_bounded_env(
        "CCP_TRAFFIC_MAX_TOTAL_BYTES",
        DEFAULT_TRAFFIC_MAX_TOTAL_BYTES,
        MAX_TRAFFIC_MAX_TOTAL_BYTES,
    );
    let capture_bytes = positive_bounded_env(
        "CCP_TRAFFIC_MAX_CAPTURE_BYTES",
        DEFAULT_TRAFFIC_MAX_CAPTURE_BYTES,
        MAX_TRAFFIC_MAX_CAPTURE_BYTES,
    )
    .min(total_bytes);
    let total_files = positive_bounded_env(
        "CCP_TRAFFIC_MAX_TOTAL_FILES",
        DEFAULT_TRAFFIC_MAX_TOTAL_FILES,
        MAX_TRAFFIC_MAX_TOTAL_FILES,
    );
    let capture_files = positive_bounded_env(
        "CCP_TRAFFIC_MAX_CAPTURE_FILES",
        DEFAULT_TRAFFIC_MAX_CAPTURE_FILES,
        MAX_TRAFFIC_MAX_CAPTURE_FILES,
    )
    .min(total_files);
    TrafficQuotaLimits {
        total_bytes,
        capture_bytes,
        total_files,
        capture_files,
    }
}

fn positive_bounded_env(name: &str, default: u64, maximum: u64) -> u64 {
    let value = std::env::var(name).ok();
    positive_bounded_value(value.as_deref(), default, maximum)
}

fn positive_bounded_value(value: Option<&str>, default: u64, maximum: u64) -> u64 {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .min(maximum)
}

impl TrafficQuota {
    fn from_existing(root: &Path, limits: TrafficQuotaLimits) -> Self {
        let (usage, scan_failed) = match existing_regular_file_usage(root) {
            Ok(usage) => (usage, false),
            Err(_) => (
                TrafficUsage {
                    charged_bytes: limits.total_bytes,
                    files: limits.total_files,
                },
                true,
            ),
        };
        Self {
            used_charged_bytes: AtomicU64::new(usage.charged_bytes),
            used_files: AtomicU64::new(usage.files),
            limits,
            warned: AtomicBool::new(false),
            scan_failed,
        }
    }

    fn scan_failure(limits: TrafficQuotaLimits) -> Self {
        Self {
            used_charged_bytes: AtomicU64::new(limits.total_bytes),
            used_files: AtomicU64::new(limits.total_files),
            limits,
            warned: AtomicBool::new(false),
            scan_failed: true,
        }
    }

    fn can_start_capture(&self) -> Result<(), TrafficQuotaRejection> {
        if self.scan_failed {
            return Err(TrafficQuotaRejection::InitialScan);
        }
        if self.used_charged_bytes.load(Ordering::Acquire) >= self.limits.total_bytes {
            return Err(TrafficQuotaRejection::TotalBytes);
        }
        if self.used_files.load(Ordering::Acquire) >= self.limits.total_files {
            return Err(TrafficQuotaRejection::TotalFiles);
        }
        Ok(())
    }

    fn reserve<'a>(
        &'a self,
        capture_charged_bytes: &'a AtomicU64,
        capture_files: &'a AtomicU64,
        value_len: u64,
    ) -> Result<TrafficReservation<'a>, TrafficQuotaRejection> {
        if self.scan_failed {
            return Err(TrafficQuotaRejection::InitialScan);
        }
        let charged_bytes = charged_bytes_for_file_len(value_len);
        if !try_reserve_units(
            capture_charged_bytes,
            self.limits.capture_bytes,
            charged_bytes,
        ) {
            return Err(TrafficQuotaRejection::CaptureBytes);
        }
        if !try_reserve_units(capture_files, self.limits.capture_files, 1) {
            capture_charged_bytes.fetch_sub(charged_bytes, Ordering::AcqRel);
            return Err(TrafficQuotaRejection::CaptureFiles);
        }
        if !try_reserve_units(
            &self.used_charged_bytes,
            self.limits.total_bytes,
            charged_bytes,
        ) {
            capture_files.fetch_sub(1, Ordering::AcqRel);
            capture_charged_bytes.fetch_sub(charged_bytes, Ordering::AcqRel);
            return Err(TrafficQuotaRejection::TotalBytes);
        }
        if !try_reserve_units(&self.used_files, self.limits.total_files, 1) {
            self.used_charged_bytes
                .fetch_sub(charged_bytes, Ordering::AcqRel);
            capture_files.fetch_sub(1, Ordering::AcqRel);
            capture_charged_bytes.fetch_sub(charged_bytes, Ordering::AcqRel);
            return Err(TrafficQuotaRejection::TotalFiles);
        }

        Ok(TrafficReservation {
            quota: self,
            capture_charged_bytes,
            capture_files,
            charged_bytes,
            retained_charged_bytes: 0,
            retained_files: 0,
        })
    }

    fn warn_once(&self, reason: &'static str) {
        if self
            .warned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let mut fields = Map::new();
        fields.insert("reason".into(), Value::String(reason.to_string()));
        fields.insert(
            "usedChargedBytes".into(),
            Value::Number(self.used_charged_bytes.load(Ordering::Acquire).into()),
        );
        fields.insert(
            "usedFiles".into(),
            Value::Number(self.used_files.load(Ordering::Acquire).into()),
        );
        fields.insert(
            "maxTotalBytes".into(),
            Value::Number(self.limits.total_bytes.into()),
        );
        fields.insert(
            "maxCaptureBytes".into(),
            Value::Number(self.limits.capture_bytes.into()),
        );
        fields.insert(
            "maxTotalFiles".into(),
            Value::Number(self.limits.total_files.into()),
        );
        fields.insert(
            "maxCaptureFiles".into(),
            Value::Number(self.limits.capture_files.into()),
        );
        crate::logging::create_logger("traffic").warn(
            "traffic_capture_quota_reached; skipping new diagnostic artifacts",
            Some(fields),
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrafficQuotaRejection {
    InitialScan,
    CaptureBytes,
    CaptureFiles,
    TotalBytes,
    TotalFiles,
}

impl TrafficQuotaRejection {
    fn as_str(self) -> &'static str {
        match self {
            Self::InitialScan => "initial_scan_failed",
            Self::CaptureBytes => "capture_charged_bytes_limit",
            Self::CaptureFiles => "capture_file_limit",
            Self::TotalBytes => "total_charged_bytes_limit",
            Self::TotalFiles => "total_file_limit",
        }
    }
}

struct TrafficReservation<'a> {
    quota: &'a TrafficQuota,
    capture_charged_bytes: &'a AtomicU64,
    capture_files: &'a AtomicU64,
    charged_bytes: u64,
    retained_charged_bytes: u64,
    retained_files: u64,
}

impl TrafficReservation<'_> {
    fn commit(&mut self) {
        self.retained_charged_bytes = self.charged_bytes;
        self.retained_files = 1;
    }

    fn retain_partial(&mut self, bytes: u64, file_created: bool) {
        if file_created {
            self.retained_charged_bytes = charged_bytes_for_file_len(bytes).min(self.charged_bytes);
            self.retained_files = 1;
        }
    }
}

impl Drop for TrafficReservation<'_> {
    fn drop(&mut self) {
        let byte_refund = self
            .charged_bytes
            .saturating_sub(self.retained_charged_bytes);
        if byte_refund > 0 {
            self.quota
                .used_charged_bytes
                .fetch_sub(byte_refund, Ordering::AcqRel);
            self.capture_charged_bytes
                .fetch_sub(byte_refund, Ordering::AcqRel);
        }
        let file_refund = 1_u64.saturating_sub(self.retained_files);
        if file_refund > 0 {
            self.quota
                .used_files
                .fetch_sub(file_refund, Ordering::AcqRel);
            self.capture_files.fetch_sub(file_refund, Ordering::AcqRel);
        }
    }
}

fn try_reserve_units(counter: &AtomicU64, limit: u64, units: u64) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        let Some(next) = current.checked_add(units) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn charged_bytes_for_file_len(len: u64) -> u64 {
    let len = len.max(1);
    len.checked_add(TRAFFIC_FILE_CHARGE_BYTES - 1)
        .map(|rounded| (rounded / TRAFFIC_FILE_CHARGE_BYTES) * TRAFFIC_FILE_CHARGE_BYTES)
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TrafficUsage {
    charged_bytes: u64,
    files: u64,
}

impl TrafficUsage {
    fn add_file(&mut self, len: u64) {
        self.charged_bytes = self
            .charged_bytes
            .saturating_add(charged_bytes_for_file_len(len));
        self.files = self.files.saturating_add(1);
    }
}

fn existing_regular_file_usage(root: &Path) -> io::Result<TrafficUsage> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TrafficUsage::default());
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "traffic root must not be a symbolic link",
        ));
    }
    if metadata.is_file() {
        let mut usage = TrafficUsage::default();
        usage.add_file(metadata.len());
        return Ok(usage);
    }
    if !metadata.is_dir() {
        return Ok(TrafficUsage::default());
    }

    let mut usage = TrafficUsage::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_file() {
                usage.add_file(metadata.len());
            } else if file_type.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(usage)
}

pub fn create_traffic_capture(opts: TrafficCaptureOptions) -> Option<TrafficCapture> {
    if !traffic_capture_enabled() {
        return None;
    }
    let traffic_root = opts
        .state_dir_override
        .clone()
        .unwrap_or_else(paths::state_dir)
        .join("traffic");
    let quota = traffic_quota(&traffic_root);
    create_traffic_capture_with_quota(opts, traffic_root, quota)
}

pub async fn create_traffic_capture_async(opts: TrafficCaptureOptions) -> Option<TrafficCapture> {
    if !traffic_capture_enabled() {
        return None;
    }
    let traffic_root = opts
        .state_dir_override
        .clone()
        .unwrap_or_else(paths::state_dir)
        .join("traffic");
    let quota = traffic_quota_async(traffic_root.clone()).await;
    create_traffic_capture_with_quota(opts, traffic_root, quota)
}

fn create_traffic_capture_with_quota(
    opts: TrafficCaptureOptions,
    traffic_root: PathBuf,
    quota: Arc<TrafficQuota>,
) -> Option<TrafficCapture> {
    if let Err(reason) = quota.can_start_capture() {
        quota.warn_once(reason.as_str());
        return None;
    }

    let state_root = traffic_root
        .join(sanitize_path_part(
            opts.session_id.as_deref().unwrap_or("no-session"),
        ))
        .join(format!(
            "{:06}-{}-{}",
            opts.session_seq.unwrap_or(0),
            sanitize_path_part(opts.provider.as_deref().unwrap_or("unknown-provider")),
            sanitize_path_part(&opts.req_id),
        ));

    Some(TrafficCapture {
        root: state_root,
        quota,
        captured_charged_bytes: AtomicU64::new(0),
        captured_files: AtomicU64::new(0),
        disabled: AtomicBool::new(false),
        artifact_counter: Mutex::new(0),
        event_counter: Mutex::new(0),
    })
}

impl TrafficCapture {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write_json(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_artifact_path(name, true);
        self.write_reserved(path, &payload);
    }

    pub fn write_text(&self, name: &str, text: &str) {
        let file = if name.ends_with(".txt") {
            name.to_string()
        } else {
            format!("{name}.txt")
        };
        let path = self.next_artifact_path(&file, false);
        self.write_reserved(path, text.as_bytes());
    }

    pub fn write_bytes(&self, name: &str, value: &[u8]) {
        let path = self.next_artifact_path(name, false);
        self.write_reserved(path, value);
    }

    pub fn write_json_event(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_event_path(name, true);
        self.write_reserved(path, &payload);
    }

    pub fn stream_capture(&self) -> StreamTrafficCapture {
        StreamTrafficCapture::default()
    }

    fn write_reserved(&self, path: PathBuf, value: &[u8]) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }

        let value_len = u64::try_from(value.len()).unwrap_or(u64::MAX);
        let mut reservation = match self.quota.reserve(
            &self.captured_charged_bytes,
            &self.captured_files,
            value_len,
        ) {
            Ok(reservation) => reservation,
            Err(reason) => {
                self.disabled.store(true, Ordering::Release);
                self.quota.warn_once(reason.as_str());
                return;
            }
        };

        match write_file_bytes(path, value) {
            Ok(()) => reservation.commit(),
            Err(error) => {
                reservation.retain_partial(error.residual_bytes, error.file_created);
            }
        }
    }

    fn next_artifact_path(&self, name: &str, ensure_ext_json: bool) -> PathBuf {
        let mut counter: MutexGuard<'_, usize> = self
            .artifact_counter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counter += 1;
        let file = if ensure_ext_json && !name.ends_with(".json") {
            format!("{name}.json")
        } else {
            name.to_string()
        };
        self.root
            .join(format!("{:03}-{}", *counter, sanitize_path_part(&file)))
    }

    fn next_event_path(&self, name: &str, ensure_ext_json: bool) -> PathBuf {
        let mut counter: MutexGuard<'_, usize> = self
            .event_counter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counter += 1;
        let file = if ensure_ext_json && !name.ends_with(".json") {
            format!("{name}.json")
        } else {
            name.to_string()
        };
        self.root
            .join("events")
            .join(format!("{:06}-{}", *counter, sanitize_path_part(&file)))
    }
}

#[cfg(test)]
pub(crate) fn test_capture(root: PathBuf) -> TrafficCapture {
    let quota = Arc::new(TrafficQuota::from_existing(
        &root,
        TrafficQuotaLimits {
            total_bytes: DEFAULT_TRAFFIC_MAX_TOTAL_BYTES,
            capture_bytes: DEFAULT_TRAFFIC_MAX_CAPTURE_BYTES,
            total_files: DEFAULT_TRAFFIC_MAX_TOTAL_FILES,
            capture_files: DEFAULT_TRAFFIC_MAX_CAPTURE_FILES,
        },
    ));
    TrafficCapture {
        root,
        quota,
        captured_charged_bytes: AtomicU64::new(0),
        captured_files: AtomicU64::new(0),
        disabled: AtomicBool::new(false),
        artifact_counter: Mutex::new(0),
        event_counter: Mutex::new(0),
    }
}

struct TrafficWriteFailure {
    residual_bytes: u64,
    file_created: bool,
}

fn write_file_bytes(path: PathBuf, value: &[u8]) -> Result<(), TrafficWriteFailure> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent).map_err(|_| TrafficWriteFailure {
            residual_bytes: 0,
            file_created: false,
        })?;
        set_mode(parent, 0o700);
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| TrafficWriteFailure {
            residual_bytes: 0,
            file_created: false,
        })?;
    let result = (|| -> io::Result<()> {
        out.write_all(value)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = out.metadata()?.permissions();
            perm.set_mode(0o600);
            let _ = fs::set_permissions(&path, perm);
        }
        Ok(())
    })();
    if result.is_ok() {
        return Ok(());
    }

    // Keep the no-deletion policy even for a newly-created partial artifact.
    // Account its length from the still-open file handle; if metadata is not
    // available, conservatively retain the full reservation.
    let residual_bytes = out
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_else(|_| u64::try_from(value.len()).unwrap_or(u64::MAX));
    drop(out);
    Err(TrafficWriteFailure {
        residual_bytes,
        file_created: true,
    })
}

pub struct StreamTrafficCapture {
    upstream_sse: Vec<u8>,
    upstream_events: Vec<Value>,
    downstream_events: Vec<Value>,
    malformed: Vec<Value>,
    upstream_event_bytes: usize,
    downstream_event_bytes: usize,
    upstream_sse_truncated: u64,
    upstream_events_truncated: u64,
    downstream_events_truncated: u64,
    malformed_truncated: u64,
    upstream_frames_truncated: u64,
}

impl Default for StreamTrafficCapture {
    fn default() -> Self {
        Self {
            upstream_sse: Vec::with_capacity(MAX_SSE_CAPTURE_BYTES.min(64 * 1024)),
            upstream_events: Vec::new(),
            downstream_events: Vec::new(),
            malformed: Vec::new(),
            upstream_event_bytes: 0,
            downstream_event_bytes: 0,
            upstream_sse_truncated: 0,
            upstream_events_truncated: 0,
            downstream_events_truncated: 0,
            malformed_truncated: 0,
            upstream_frames_truncated: 0,
        }
    }
}

impl StreamTrafficCapture {
    pub fn upstream_event(&mut self, event: Option<&str>, value: &Value) {
        let value = redact_traffic(value);
        let frame = serde_json::to_vec(&value).unwrap_or_default();
        let event = event.unwrap_or("message");
        let frame_len = event.len().saturating_add(frame.len()).saturating_add(16);
        if frame_len > MAX_STREAM_CAPTURE_FRAME_BYTES {
            self.upstream_frames_truncated = self.upstream_frames_truncated.saturating_add(1);
        } else if self.upstream_sse.len().saturating_add(frame_len) <= MAX_SSE_CAPTURE_BYTES {
            self.upstream_sse.extend_from_slice(b"event: ");
            self.upstream_sse.extend_from_slice(event.as_bytes());
            self.upstream_sse.extend_from_slice(b"\ndata: ");
            self.upstream_sse.extend_from_slice(&frame);
            self.upstream_sse.extend_from_slice(b"\n\n");
        } else {
            self.upstream_sse_truncated = self.upstream_sse_truncated.saturating_add(1);
        }
        self.push_event(true, serde_json::json!({"event":event,"data":value}));
    }

    pub fn malformed(&mut self, stage: &str, kind: &str) {
        if self.malformed.len() < MAX_STREAM_CAPTURE_EVENTS {
            self.malformed
                .push(serde_json::json!({"stage":stage,"kind":kind}));
        } else {
            self.malformed_truncated = self.malformed_truncated.saturating_add(1);
        }
    }

    pub fn downstream_event(&mut self, event: &str, data: Value) {
        self.push_event(
            false,
            serde_json::json!({"event":event,"data":redact_traffic(&data)}),
        );
    }

    fn push_event(&mut self, upstream: bool, value: Value) {
        let bytes = serde_json::to_vec(&value).map_or(0, |value| value.len());
        let (events, total, truncated) = if upstream {
            (
                &mut self.upstream_events,
                &mut self.upstream_event_bytes,
                &mut self.upstream_events_truncated,
            )
        } else {
            (
                &mut self.downstream_events,
                &mut self.downstream_event_bytes,
                &mut self.downstream_events_truncated,
            )
        };
        if events.len() < MAX_STREAM_CAPTURE_EVENTS
            && total.saturating_add(bytes) <= MAX_STREAM_CAPTURE_EVENT_BYTES
        {
            *total += bytes;
            events.push(value);
        } else {
            *truncated = truncated.saturating_add(1);
        }
    }

    pub fn finish(self, traffic: &TrafficCapture, completion: Value) {
        let upstream_event_count = self.upstream_events.len();
        let downstream_event_count = self.downstream_events.len();
        if !self.upstream_sse.is_empty() {
            traffic.write_bytes("032-upstream-response-body.sse", &self.upstream_sse);
        }
        traffic.write_json(
            "033-upstream-response-capture",
            &serde_json::json!({
                "truncated": self.upstream_sse_truncated > 0 || self.upstream_frames_truncated > 0 || self.upstream_events_truncated > 0,
                "captured_bytes": self.upstream_sse.len(),
                "truncated_frames": self.upstream_sse_truncated,
                "oversized_frames": self.upstream_frames_truncated,
                "captured_events": self.upstream_events.len(),
                "captured_event_bytes": self.upstream_event_bytes,
                "truncated_events": self.upstream_events_truncated,
                "malformed": self.malformed,
                "truncated_malformed": self.malformed_truncated,
            }),
        );
        for value in self.upstream_events {
            traffic.write_json_event("040-upstream-event", &value);
        }
        for value in self.downstream_events {
            traffic.write_json_event("050-downstream-event", &value);
        }
        traffic.write_json(
            "061-grok-stream-summary",
            &serde_json::json!({
                "completion": completion,
                "upstream_sse": {
                    "captured_bytes": self.upstream_sse.len(),
                    "truncated_frames": self.upstream_sse_truncated,
                    "oversized_frames": self.upstream_frames_truncated,
                },
                "upstream_events": {
                    "captured": upstream_event_count,
                    "captured_bytes": self.upstream_event_bytes,
                    "truncated": self.upstream_events_truncated,
                },
                "downstream_events": {
                    "captured": downstream_event_count,
                    "captured_bytes": self.downstream_event_bytes,
                    "truncated": self.downstream_events_truncated,
                },
            }),
        );
    }
}

pub fn sanitize_path_part(input: &str) -> String {
    let mut cleaned: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    cleaned.truncate(160);
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

pub fn redact_traffic(value: &Value) -> Value {
    redact_traffic_with_depth(value, 0)
}

fn redact_traffic_with_depth(value: &Value, depth: u16) -> Value {
    if depth > 100 {
        return Value::String("[depth-limit]".to_string());
    }

    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let normalized = key.to_lowercase();
                if REDACT_KEYS.contains(&normalized.as_str())
                    || matches!(
                        normalized.as_str(),
                        "token"
                            | "bearer_token"
                            | "oauth_token"
                            | "oauth_access_token"
                            | "oauth_refresh_token"
                            | "client_secret"
                            | "secret"
                            | "password"
                            | "email"
                            | "user_id"
                            | "account_id"
                            | "identity"
                            | "identity_id"
                            | "subject"
                            | "sub"
                    )
                {
                    out.insert(key.clone(), redact_traffic_value(value));
                } else {
                    out.insert(key.clone(), redact_traffic_with_depth(value, depth + 1));
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_traffic_with_depth(value, depth + 1))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn redact_traffic_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        _ => Value::String("[redacted]".to_string()),
    }
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
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;
    use tempfile::TempDir;

    fn limits(total_bytes: u64, capture_bytes: u64) -> TrafficQuotaLimits {
        TrafficQuotaLimits {
            total_bytes,
            capture_bytes,
            total_files: u64::MAX,
            capture_files: u64::MAX,
        }
    }

    fn limits_with_files(
        total_bytes: u64,
        capture_bytes: u64,
        total_files: u64,
        capture_files: u64,
    ) -> TrafficQuotaLimits {
        TrafficQuotaLimits {
            total_bytes,
            capture_bytes,
            total_files,
            capture_files,
        }
    }

    fn quota(root: &Path, total_bytes: u64, capture_bytes: u64) -> Arc<TrafficQuota> {
        quota_with_limits(root, limits(total_bytes, capture_bytes))
    }

    fn quota_with_limits(root: &Path, limits: TrafficQuotaLimits) -> Arc<TrafficQuota> {
        let quota = Arc::new(TrafficQuota::from_existing(root, limits));
        // Unit tests intentionally avoid writing quota warnings to the user's
        // process log. Production quotas start with this flag cleared.
        quota.warned.store(true, Ordering::Release);
        quota
    }

    fn capture(root: PathBuf, quota: Arc<TrafficQuota>) -> TrafficCapture {
        TrafficCapture {
            root,
            quota,
            captured_charged_bytes: AtomicU64::new(0),
            captured_files: AtomicU64::new(0),
            disabled: AtomicBool::new(false),
            artifact_counter: Mutex::new(0),
            event_counter: Mutex::new(0),
        }
    }

    #[test]
    fn existing_files_consume_total_quota_without_being_removed() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        fs::create_dir_all(&traffic_root).unwrap();
        let sentinel = traffic_root.join("sentinel.capture");
        fs::write(&sentinel, b"1234567890").unwrap();

        let quota = quota(
            &traffic_root,
            TRAFFIC_FILE_CHARGE_BYTES,
            TRAFFIC_FILE_CHARGE_BYTES,
        );
        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(quota.used_files.load(Ordering::Acquire), 1);
        assert_eq!(
            quota.can_start_capture(),
            Err(TrafficQuotaRejection::TotalBytes)
        );
        let capture = capture(traffic_root.join("request"), quota.clone());
        capture.write_bytes("too-large.bin", b"x");

        assert_eq!(fs::read(&sentinel).unwrap(), b"1234567890");
        assert!(!capture.root().exists());
        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(quota.used_files.load(Ordering::Acquire), 1);
    }

    #[test]
    fn each_traffic_root_is_scanned_only_once_per_process() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        fs::create_dir_all(&traffic_root).unwrap();
        fs::write(traffic_root.join("before.capture"), b"1234").unwrap();

        let first = traffic_quota(&traffic_root);
        assert_eq!(
            first.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(first.used_files.load(Ordering::Acquire), 1);
        fs::write(traffic_root.join("after.capture"), b"12345").unwrap();
        let second = traffic_quota(&traffic_root);

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            second.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(second.used_files.load(Ordering::Acquire), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_async_initialization_shares_one_background_scan() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        fs::create_dir_all(&traffic_root).unwrap();
        fs::write(traffic_root.join("before.capture"), b"x").unwrap();

        let (first, second) = tokio::join!(
            traffic_quota_async(traffic_root.clone()),
            traffic_quota_async(traffic_root)
        );

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            first.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(first.used_files.load(Ordering::Acquire), 1);
    }

    #[test]
    fn concurrent_reservations_never_exceed_charged_byte_quota() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let max_charged_bytes = 10 * TRAFFIC_FILE_CHARGE_BYTES;
        let quota = quota_with_limits(
            &traffic_root,
            limits_with_files(max_charged_bytes, TRAFFIC_FILE_CHARGE_BYTES, 64, 1),
        );
        let barrier = Arc::new(Barrier::new(64));
        let mut workers = Vec::new();
        for index in 0..64 {
            let capture = capture(traffic_root.join(format!("request-{index}")), quota.clone());
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                capture.write_bytes("artifact.bin", &[b'x'; 37]);
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let disk_usage = existing_regular_file_usage(&traffic_root).unwrap();
        let reserved_bytes = quota.used_charged_bytes.load(Ordering::Acquire);
        assert!(
            disk_usage.charged_bytes <= max_charged_bytes,
            "charged bytes exceeded quota: {}",
            disk_usage.charged_bytes
        );
        assert_eq!(reserved_bytes, disk_usage.charged_bytes);
        assert_eq!(quota.used_files.load(Ordering::Acquire), disk_usage.files);
    }

    #[test]
    fn concurrent_reservations_never_exceed_total_file_quota() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let quota = quota_with_limits(
            &traffic_root,
            limits_with_files(
                64 * TRAFFIC_FILE_CHARGE_BYTES,
                TRAFFIC_FILE_CHARGE_BYTES,
                7,
                1,
            ),
        );
        let barrier = Arc::new(Barrier::new(64));
        let mut workers = Vec::new();
        for index in 0..64 {
            let capture = capture(traffic_root.join(format!("request-{index}")), quota.clone());
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                capture.write_bytes("artifact.bin", b"x");
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let disk_usage = existing_regular_file_usage(&traffic_root).unwrap();
        assert_eq!(disk_usage.files, 7);
        assert_eq!(quota.used_files.load(Ordering::Acquire), 7);
        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            7 * TRAFFIC_FILE_CHARGE_BYTES
        );
    }

    #[cfg(unix)]
    #[test]
    fn initial_scan_does_not_follow_file_or_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&traffic_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(traffic_root.join("local.capture"), b"abc").unwrap();
        fs::write(outside.join("large.capture"), vec![b'x'; 4_096]).unwrap();
        symlink(
            outside.join("large.capture"),
            traffic_root.join("linked-file"),
        )
        .unwrap();
        symlink(&outside, traffic_root.join("linked-directory")).unwrap();

        let quota = quota(
            &traffic_root,
            100 * TRAFFIC_FILE_CHARGE_BYTES,
            100 * TRAFFIC_FILE_CHARGE_BYTES,
        );
        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(quota.used_files.load(Ordering::Acquire), 1);
    }

    #[test]
    fn single_capture_charged_byte_limit_stops_later_artifacts_only() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let quota = quota(
            &traffic_root,
            5 * TRAFFIC_FILE_CHARGE_BYTES,
            TRAFFIC_FILE_CHARGE_BYTES,
        );
        let rejected = capture(traffic_root.join("rejected"), quota.clone());
        rejected.write_bytes(
            "oversized.bin",
            &vec![b'x'; usize::try_from(TRAFFIC_FILE_CHARGE_BYTES + 1).unwrap()],
        );
        rejected.write_bytes("later-small.bin", b"x");
        assert!(!rejected.root().exists());
        assert_eq!(quota.used_charged_bytes.load(Ordering::Acquire), 0);
        assert_eq!(quota.used_files.load(Ordering::Acquire), 0);

        let accepted = capture(traffic_root.join("accepted"), quota.clone());
        accepted.write_bytes("within-limit.bin", b"12345678");
        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(quota.used_files.load(Ordering::Acquire), 1);
        assert_eq!(
            existing_regular_file_usage(&traffic_root).unwrap(),
            TrafficUsage {
                charged_bytes: TRAFFIC_FILE_CHARGE_BYTES,
                files: 1,
            }
        );
    }

    #[test]
    fn single_capture_file_limit_stops_a_small_file_fanout() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let quota = quota_with_limits(
            &traffic_root,
            limits_with_files(
                10 * TRAFFIC_FILE_CHARGE_BYTES,
                10 * TRAFFIC_FILE_CHARGE_BYTES,
                10,
                2,
            ),
        );
        let capture = capture(traffic_root.join("request"), quota.clone());

        capture.write_bytes("one.bin", b"1");
        capture.write_bytes("two.bin", b"2");
        capture.write_bytes("three.bin", b"3");

        assert!(capture.disabled.load(Ordering::Acquire));
        assert_eq!(capture.captured_files.load(Ordering::Acquire), 2);
        assert_eq!(quota.used_files.load(Ordering::Acquire), 2);
        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            2 * TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(existing_regular_file_usage(&traffic_root).unwrap().files, 2);
    }

    #[test]
    fn restart_scan_reconstructs_charged_bytes_and_file_count() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let limits = limits_with_files(
            32 * TRAFFIC_FILE_CHARGE_BYTES,
            32 * TRAFFIC_FILE_CHARGE_BYTES,
            32,
            32,
        );
        let first_quota = quota_with_limits(&traffic_root, limits);
        let capture = capture(traffic_root.join("request"), first_quota.clone());
        capture.write_bytes("empty.bin", b"");
        capture.write_bytes("tiny.bin", b"x");
        capture.write_bytes(
            "rounded.bin",
            &vec![b'x'; usize::try_from(TRAFFIC_FILE_CHARGE_BYTES + 1).unwrap()],
        );

        let expected = TrafficUsage {
            charged_bytes: 4 * TRAFFIC_FILE_CHARGE_BYTES,
            files: 3,
        };
        assert_eq!(
            existing_regular_file_usage(&traffic_root).unwrap(),
            expected
        );

        let restarted = TrafficQuota::from_existing(&traffic_root, limits);
        assert_eq!(
            restarted.used_charged_bytes.load(Ordering::Acquire),
            expected.charged_bytes
        );
        assert_eq!(restarted.used_files.load(Ordering::Acquire), expected.files);
        assert_eq!(
            first_quota.used_charged_bytes.load(Ordering::Acquire),
            restarted.used_charged_bytes.load(Ordering::Acquire)
        );
        assert_eq!(
            first_quota.used_files.load(Ordering::Acquire),
            restarted.used_files.load(Ordering::Acquire)
        );
    }

    #[test]
    fn failed_write_returns_both_reservations_and_preserves_existing_file() {
        let temp = TempDir::new().unwrap();
        let traffic_root = temp.path().join("traffic");
        let quota = quota(
            &traffic_root,
            10 * TRAFFIC_FILE_CHARGE_BYTES,
            10 * TRAFFIC_FILE_CHARGE_BYTES,
        );
        let request_root = traffic_root.join("request");
        fs::create_dir_all(&request_root).unwrap();
        let existing = request_root.join("001-artifact.bin");
        fs::write(&existing, b"sentinel").unwrap();
        let capture = capture(request_root, quota.clone());

        capture.write_bytes("artifact.bin", b"replacement");

        assert_eq!(fs::read(existing).unwrap(), b"sentinel");
        assert_eq!(quota.used_charged_bytes.load(Ordering::Acquire), 0);
        assert_eq!(quota.used_files.load(Ordering::Acquire), 0);
        assert_eq!(capture.captured_charged_bytes.load(Ordering::Acquire), 0);
        assert_eq!(capture.captured_files.load(Ordering::Acquire), 0);
    }

    #[test]
    fn failed_write_can_retain_observed_partial_file_charge() {
        let temp = TempDir::new().unwrap();
        let quota = quota(
            temp.path(),
            10 * TRAFFIC_FILE_CHARGE_BYTES,
            10 * TRAFFIC_FILE_CHARGE_BYTES,
        );
        let captured_charged_bytes = AtomicU64::new(0);
        let captured_files = AtomicU64::new(0);
        let mut reservation = quota
            .reserve(&captured_charged_bytes, &captured_files, 20)
            .unwrap();
        reservation.retain_partial(7, true);
        drop(reservation);

        assert_eq!(
            quota.used_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(quota.used_files.load(Ordering::Acquire), 1);
        assert_eq!(
            captured_charged_bytes.load(Ordering::Acquire),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(captured_files.load(Ordering::Acquire), 1);
    }

    #[test]
    fn file_charge_is_at_least_one_block_and_rounds_up() {
        assert_eq!(charged_bytes_for_file_len(0), TRAFFIC_FILE_CHARGE_BYTES);
        assert_eq!(charged_bytes_for_file_len(1), TRAFFIC_FILE_CHARGE_BYTES);
        assert_eq!(
            charged_bytes_for_file_len(TRAFFIC_FILE_CHARGE_BYTES),
            TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(
            charged_bytes_for_file_len(TRAFFIC_FILE_CHARGE_BYTES + 1),
            2 * TRAFFIC_FILE_CHARGE_BYTES
        );
        assert_eq!(charged_bytes_for_file_len(u64::MAX), u64::MAX);
    }

    #[test]
    fn quota_environment_values_are_positive_and_bounded() {
        assert_eq!(positive_bounded_value(Some("12"), 5, 20), 12);
        assert_eq!(positive_bounded_value(Some("0"), 5, 20), 5);
        assert_eq!(positive_bounded_value(Some("invalid"), 5, 20), 5);
        assert_eq!(positive_bounded_value(Some("200"), 5, 20), 20);
    }
}
