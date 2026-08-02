use serde_json::{Map, Value};
use std::fs::{self, File, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::config::AliasProvider;
use crate::logging::REDACT_KEYS;
use crate::paths;

#[derive(Debug)]
pub struct TrafficCapture {
    root: PathBuf,
    artifact_counter: Mutex<usize>,
    event_counter: Mutex<usize>,
}

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

pub fn traffic_capture_enabled() -> bool {
    traffic_capture_enabled_for_env(&std::env::vars().collect())
}

pub fn traffic_capture_enabled_for_env(env: &std::collections::HashMap<String, String>) -> bool {
    match env.get("CCP_TRAFFIC_LOG").map(String::as_str) {
        Some(v) => matches!(v, "1" | "true" | "yes"),
        None => false,
    }
}

pub fn create_traffic_capture(opts: TrafficCaptureOptions) -> Option<TrafficCapture> {
    if !traffic_capture_enabled() {
        return None;
    }
    let state_root = opts
        .state_dir_override
        .unwrap_or_else(paths::state_dir)
        .join("traffic")
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
        let _ = write_bytes(path, &payload);
    }

    pub fn write_text(&self, name: &str, text: &str) {
        let file = if name.ends_with(".txt") {
            name.to_string()
        } else {
            format!("{name}.txt")
        };
        let path = self.next_artifact_path(&file, false);
        let _ = write_bytes(path, text.as_bytes());
    }

    pub fn write_bytes(&self, name: &str, value: &[u8]) {
        let path = self.next_artifact_path(name, false);
        let _ = write_bytes(path, value);
    }

    pub fn write_json_event(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_event_path(name, true);
        let _ = write_bytes(path, &payload);
    }

    pub fn stream_capture(&self) -> StreamTrafficCapture {
        StreamTrafficCapture::default()
    }

    fn next_artifact_path(&self, name: &str, ensure_ext_json: bool) -> PathBuf {
        let mut counter: MutexGuard<'_, usize> = self
            .artifact_counter
            .lock()
            .unwrap_or_else(|_| self.artifact_counter.lock().unwrap());
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
            .unwrap_or_else(|_| self.event_counter.lock().unwrap());
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
    TrafficCapture {
        root,
        artifact_counter: Mutex::new(0),
        event_counter: Mutex::new(0),
    }
}

fn write_bytes(path: PathBuf, value: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
        if let Ok(meta) = fs::metadata(parent) {
            set_mode(parent, 0o700);
            if meta.is_dir() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = meta.permissions();
                    perm.set_mode(0o700);
                    let _ = fs::set_permissions(parent, perm);
                }
            }
        }
    }
    let mut out = File::create(&path)?;
    out.write_all(value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = out.metadata()?.permissions();
        perm.set_mode(0o600);
        let _ = fs::set_permissions(&path, perm);
    }
    Ok(())
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
        self.finish_named(traffic, completion, "061-grok-stream-summary");
    }

    pub fn finish_named(self, traffic: &TrafficCapture, completion: Value, summary_name: &str) {
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
            summary_name,
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
    let cleaned: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    let truncated = if cleaned.len() > 160 {
        &cleaned[..160]
    } else {
        &cleaned
    };
    if truncated.is_empty() {
        "unknown".to_string()
    } else {
        truncated.to_string()
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
                if normalized == "image_url" {
                    // Grok/OpenAI `input_image` parts carry data URLs; never
                    // persist the payload in traffic captures.
                    out.insert(key.clone(), redact_traffic_value(value));
                } else if normalized == "data" && looks_like_image_source(map) {
                    // Anthropic image blocks carry raw base64 under
                    // `source.data`; redact it for the same reason.
                    out.insert(key.clone(), redact_traffic_value(value));
                } else if REDACT_KEYS.contains(&normalized.as_str())
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
        Value::String(text) if is_data_url(text) => {
            Value::String(format!("[redacted data-url len={}]", text.len()))
        }
        _ => value.clone(),
    }
}

fn is_data_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("data:image/") && lower.contains(";base64,")
}

/// An object shaped like an Anthropic image source: `type: "base64"` and
/// either an image `media_type` or none at all. Used to redact only image
/// payloads, not arbitrary `data` keys. Missing `media_type` is treated as
/// an image: the capture is written before translation, so a malformed block
/// that the translator would reject must still not persist its payload.
fn looks_like_image_source(map: &Map<String, Value>) -> bool {
    if !map
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|source_type| source_type.eq_ignore_ascii_case("base64"))
    {
        return false;
    }
    match map.get("media_type").and_then(Value::as_str) {
        Some(media_type) => media_type.to_ascii_lowercase().starts_with("image/"),
        None => true,
    }
}

fn redact_traffic_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        // Structured values (e.g. Kimi's `{url: ...}` image_url) keep their
        // shape so captures stay parseable; only the payload leaves.
        Value::Object(_) | Value::Array(_) => redact_traffic(value),
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

#[allow(dead_code)]
fn _provider_alias(_provider: &str) -> Option<AliasProvider> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_traffic_strips_input_image_data_urls() {
        let value = serde_json::json!({
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "what color?"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"}
                ]}
            ]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "base64 payload leaked: {rendered}"
        );
        assert!(
            !rendered.contains("data:image/png;base64,iVBOR"),
            "data URL payload leaked: {rendered}"
        );
        // The image part is still structurally visible (redacted marker).
        assert!(rendered.contains("input_image"));
    }

    #[test]
    fn redact_traffic_strips_inline_base64_image_values() {
        let value = serde_json::json!({
            "output": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(!rendered.contains("iVBORw0KGgo"));
    }

    #[test]
    fn redact_traffic_strips_anthropic_image_source_data() {
        // The incoming Anthropic request body carries raw base64 under
        // `source.data` (no data: URL wrapper) — it must not persist either.
        let value = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
                    }}
                ]
            }]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "source.data base64 leaked: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        // Structure is preserved.
        assert!(rendered.contains("image/png"));
    }

    #[test]
    fn redact_traffic_keeps_unrelated_data_keys() {
        let value = serde_json::json!({"data": "some-non-image-payload", "count": 3});
        let redacted = redact_traffic(&value);
        assert_eq!(redacted["data"], "some-non-image-payload");
    }

    #[test]
    fn redact_traffic_strips_data_urls_in_array_shaped_tool_output() {
        // L2b inline mode: function_call_output.output is an array of content
        // parts — the image_url inside must be redacted just like message parts.
        let value = serde_json::json!({
            "input": [
                {"type": "function_call_output", "call_id": "call_1", "output": [
                    {"type": "input_text", "text": "screenshot"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"}
                ]}
            ]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "base64 payload leaked: {rendered}"
        );
        assert!(
            !rendered.contains("data:image/png;base64,iVBOR"),
            "data URL payload leaked: {rendered}"
        );
        // Structure and text survive.
        assert!(rendered.contains("input_image"));
        assert!(rendered.contains("screenshot"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn redact_traffic_strips_uppercase_media_type_image_source() {
        // Captures are written before translation validates anything, so a
        // non-standard casing must still not persist its payload.
        let value = serde_json::json!({
            "source": {"type": "base64", "media_type": "IMAGE/PNG", "data": "QUJDREVGRw=="}
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("QUJDREVGRw"),
            "payload leaked: {rendered}"
        );
        assert!(
            redacted["source"]["data"]
                .as_str()
                .unwrap()
                .contains("redacted")
        );
    }

    #[test]
    fn redact_traffic_strips_uppercase_image_source_type() {
        let value = serde_json::json!({
            "source": {"type": "BASE64", "media_type": "image/png", "data": "QUJDREVGRw=="}
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("QUJDREVGRw"),
            "payload leaked: {rendered}"
        );
    }

    #[test]
    fn redact_traffic_strips_image_source_without_media_type() {
        // A malformed block the translator would reject must still be redacted
        // in the pre-translation capture.
        let value = serde_json::json!({
            "source": {"type": "base64", "data": "QUJDREVGRw=="}
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("QUJDREVGRw"),
            "payload leaked: {rendered}"
        );
    }

    #[test]
    fn redact_traffic_strips_uppercase_data_url_scheme() {
        let value = serde_json::json!({
            "image_url": "DATA:IMAGE/PNG;base64,iVBORw0KGgoAAAANSUhEUg"
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "data URL payload leaked: {rendered}"
        );
    }

    #[test]
    fn redact_traffic_keeps_structured_image_url_shape() {
        // Kimi-style object image_url: redact the payload but keep the object
        // structure so captures stay parseable.
        let value = serde_json::json!({
            "image_url": {"url": "DATA:IMAGE/PNG;BASE64,iVBORw0KGgoAAAANSUhEUg"}
        });
        let redacted = redact_traffic(&value);
        assert!(redacted["image_url"].is_object(), "shape lost: {redacted}");
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "nested payload leaked: {rendered}"
        );
    }

    #[test]
    fn traffic_redacts_proxy_authorization() {
        let redacted = redact_traffic(&serde_json::json!({
            "headers": {
                "proxy-authorization": "Basic dXNlcjpwYXNz",
                "x-safe": "kept"
            }
        }));

        assert_eq!(redacted["headers"]["x-safe"], "kept");
        assert_eq!(
            redacted["headers"]["proxy-authorization"],
            "[redacted len=18]"
        );
    }
}
