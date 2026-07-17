use crate::{anthropic::schema::MessagesRequest, logging::create_logger};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

const COMPACTION_ANCHOR: &str = "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions.";
const DEFAULT_EFFORT: &str = "medium";
const DEFAULT_MARKER_TTL_SECONDS: u64 = 60;
const MAX_MARKER_TTL_SECONDS: u64 = 86_400;
const MAX_SESSION_ID_LEN: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionRouteConfig {
    model: String,
    effort: String,
    marker_dir: PathBuf,
    marker_ttl_seconds: u64,
}

impl CompactionRouteConfig {
    /// Loads the optional route configuration. An absent or blank model disables the feature.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        Self::from_values(
            std::env::var("CCP_COMPACTION_MODEL").ok(),
            std::env::var("CCP_COMPACTION_EFFORT").ok(),
            std::env::var("CCP_COMPACTION_MARKER_DIR").ok(),
            std::env::var("CCP_COMPACTION_MARKER_TTL_SECONDS").ok(),
        )
    }

    pub fn from_values(
        model: Option<String>,
        effort: Option<String>,
        marker_dir: Option<String>,
        marker_ttl_seconds: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(model) = model.filter(|value| !value.trim().is_empty()) else {
            return Ok(None);
        };
        let marker_dir = marker_dir
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "CCP_COMPACTION_MARKER_DIR is required when compaction routing is enabled"
                )
            })?;
        let effort = effort.unwrap_or_else(|| DEFAULT_EFFORT.to_string());
        if !matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
            anyhow::bail!("CCP_COMPACTION_EFFORT must be one of low, medium, high, xhigh, or max");
        }
        let marker_ttl_seconds = match marker_ttl_seconds {
            Some(value) => value.parse::<u64>().map_err(|_| {
                anyhow::anyhow!(
                    "CCP_COMPACTION_MARKER_TTL_SECONDS must be a positive integer no greater than {MAX_MARKER_TTL_SECONDS}"
                )
            })?,
            None => DEFAULT_MARKER_TTL_SECONDS,
        };
        if !(1..=MAX_MARKER_TTL_SECONDS).contains(&marker_ttl_seconds) {
            anyhow::bail!(
                "CCP_COMPACTION_MARKER_TTL_SECONDS must be a positive integer no greater than {MAX_MARKER_TTL_SECONDS}"
            );
        }

        Ok(Some(Self {
            model: model.trim().to_string(),
            effort,
            marker_dir: PathBuf::from(marker_dir),
            marker_ttl_seconds,
        }))
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Clone, Debug)]
pub struct CompactionRoute {
    config: CompactionRouteConfig,
}

impl CompactionRoute {
    pub fn new(config: CompactionRouteConfig) -> Self {
        Self { config }
    }

    /// Attempts a one-shot compaction route. Every invalid or unavailable marker fails open.
    pub fn try_route(
        &self,
        body: &mut MessagesRequest,
        session_id: Option<&str>,
        request_id: &str,
        count_tokens: bool,
        now_ms: u64,
    ) -> bool {
        if count_tokens || !has_compaction_anchor(body) {
            return false;
        }
        let Some(session_id) = session_id.filter(|id| valid_session_id(id)) else {
            return false;
        };
        let Some(output_config) = routed_output_config(body, &self.config.effort) else {
            return false;
        };
        let marker_path = self.marker_path(session_id);
        let Some(claim_path) = self.claim_path(session_id, request_id) else {
            return false;
        };
        if fs::rename(&marker_path, &claim_path).is_err() {
            return false;
        }
        let _claim = ClaimGuard {
            path: claim_path.clone(),
        };
        if !valid_marker(
            &claim_path,
            session_id,
            now_ms,
            self.config.marker_ttl_seconds,
        ) {
            return false;
        }

        body.model = Some(self.config.model.clone());
        body.extra
            .insert("output_config".to_string(), Value::Object(output_config));
        create_logger("compaction_route").info(
            "compaction_routed",
            Some(Map::from_iter([
                ("reqId".to_string(), json!(request_id)),
                ("sessionId".to_string(), json!(session_id)),
                ("model".to_string(), json!(&self.config.model)),
                ("effort".to_string(), json!(&self.config.effort)),
            ])),
        );
        true
    }

    fn marker_path(&self, session_id: &str) -> PathBuf {
        self.config.marker_dir.join(format!("{session_id}.json"))
    }

    fn claim_path(&self, session_id: &str, request_id: &str) -> Option<PathBuf> {
        valid_request_id(request_id).then(|| {
            self.config
                .marker_dir
                .join(format!("{session_id}.json.claim-{request_id}"))
        })
    }
}

struct ClaimGuard {
    path: PathBuf,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u8,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "createdAtMs")]
    created_at_ms: u64,
}

fn valid_marker(path: &Path, session_id: &str, now_ms: u64, ttl_seconds: u64) -> bool {
    let marker = match fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
    {
        Some(marker) => marker,
        None => {
            let _ = fs::remove_file(path);
            return false;
        }
    };
    if marker.version != 1
        || marker.session_id != session_id
        || !valid_session_id(&marker.session_id)
    {
        let _ = fs::remove_file(path);
        return false;
    }
    if marker.created_at_ms > now_ms {
        let _ = fs::remove_file(path);
        return false;
    }
    let ttl_ms = ttl_seconds.saturating_mul(1_000);
    if now_ms.saturating_sub(marker.created_at_ms) > ttl_ms {
        let _ = fs::remove_file(path);
        return false;
    }
    true
}

fn has_compaction_anchor(body: &MessagesRequest) -> bool {
    body.messages.last().is_some_and(|message| {
        message.role == "user" && content_has_compaction_anchor(&message.content)
    })
}

fn content_has_compaction_anchor(content: &Value) -> bool {
    match content {
        Value::String(text) => text.contains(COMPACTION_ANCHOR),
        Value::Array(blocks) => blocks.iter().any(|block| match block {
            Value::String(text) => text.contains(COMPACTION_ANCHOR),
            Value::Object(block) => {
                block
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains(COMPACTION_ANCHOR))
            }
            _ => false,
        }),
        _ => false,
    }
}

fn routed_output_config(body: &MessagesRequest, effort: &str) -> Option<Map<String, Value>> {
    let mut config = match body.extra.get("output_config") {
        Some(Value::Object(config)) => config.clone(),
        Some(_) => return None,
        None => Map::new(),
    };
    config.insert("effort".to_string(), Value::String(effort.to_string()));
    Some(config)
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_LEN
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_LEN
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    fn route(dir: &Path) -> CompactionRoute {
        CompactionRoute::new(
            CompactionRouteConfig::from_values(
                Some("gpt-5.4".to_string()),
                Some("high".to_string()),
                Some(dir.display().to_string()),
                Some("60".to_string()),
            )
            .unwrap()
            .unwrap(),
        )
    }

    fn request() -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "original-model",
            "messages": [{
                "role": "user",
                "content": format!("CRITICAL: Respond with TEXT ONLY. {COMPACTION_ANCHOR} Continue with the detailed requirements.")
            }]
        }))
        .unwrap()
    }

    fn write_marker(dir: &Path, session_id: &str, created_at_ms: u64) -> PathBuf {
        let path = dir.join(format!("{session_id}.json"));
        fs::write(
            &path,
            json!({
                "version": 1,
                "sessionId": session_id,
                "createdAtMs": created_at_ms,
            })
            .to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn disabled_without_model_and_no_marker_do_not_route() {
        assert!(
            CompactionRouteConfig::from_values(None, None, None, None)
                .unwrap()
                .is_none()
        );
        let defaulted = CompactionRouteConfig::from_values(
            Some("gpt-5.4".to_string()),
            None,
            Some("markers".to_string()),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(defaulted.effort, "medium");
        assert_eq!(defaulted.marker_ttl_seconds, 60);

        let dir = tempfile::tempdir().unwrap();
        let mut body = request();
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "request", false, 1));
        assert_eq!(body.model.as_deref(), Some("original-model"));
    }

    #[test]
    fn count_tokens_retains_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "session", 1_000);
        let mut body = request();
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "request", true, 1_000));
        assert!(marker.exists());
    }

    #[test]
    fn wrong_session_and_noncompaction_requests_retain_marker() {
        let dir = tempfile::tempdir().unwrap();
        let wrong_session = write_marker(dir.path(), "other", 1_000);
        let mut body = request();
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "request", false, 1_000));
        assert!(wrong_session.exists());

        let marker = write_marker(dir.path(), "session", 1_000);
        body.messages[0].content = json!("not a compaction request");
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "second", false, 1_000));
        assert!(marker.exists());
    }

    #[test]
    fn historical_anchor_and_noncanonical_session_do_not_route() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "session", 1_000);
        let mut body = request();
        body.messages.push(
            serde_json::from_value(json!({
                "role": "assistant",
                "content": "previous response"
            }))
            .unwrap(),
        );
        body.messages.push(
            serde_json::from_value(json!({
                "role": "user",
                "content": "ordinary terminal prompt"
            }))
            .unwrap(),
        );
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "request", false, 1_000));
        assert!(marker.exists());

        let upper_marker = write_marker(dir.path(), "UPPER", 1_000);
        let mut compact = request();
        assert!(
            !route(dir.path()).try_route(&mut compact, Some("UPPER"), "request", false, 1_000,)
        );
        assert!(upper_marker.exists());
    }

    #[test]
    fn routes_once_then_leaves_second_request_normal() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "session", 1_000);
        let router = route(dir.path());
        let mut first = request();
        assert!(router.try_route(&mut first, Some("session"), "first", false, 1_000));
        assert_eq!(first.model.as_deref(), Some("gpt-5.4"));
        assert!(!marker.exists());
        assert!(!dir.path().join("session.json.claim-first").exists());

        let mut second = request();
        assert!(!router.try_route(&mut second, Some("session"), "second", false, 1_000));
        assert_eq!(second.model.as_deref(), Some("original-model"));
    }

    #[test]
    fn stale_malformed_and_future_markers_fail_open() {
        let dir = tempfile::tempdir().unwrap();
        let stale = write_marker(dir.path(), "session", 1);
        let mut body = request();
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "request", false, 61_002));
        assert!(!stale.exists());

        let malformed = dir.path().join("session.json");
        fs::write(&malformed, "not json").unwrap();
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "again", false, 1_000));
        assert!(!malformed.exists());

        let future = write_marker(dir.path(), "session", 1_001);
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "future", false, 1_000));
        assert!(!future.exists());
    }

    #[test]
    fn malformed_output_config_leaves_marker_unclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "session", 1_000);
        let mut body = request();
        body.extra.insert(
            "output_config".to_string(),
            Value::String("invalid".to_string()),
        );
        assert!(!route(dir.path()).try_route(&mut body, Some("session"), "request", false, 1_000));
        assert!(marker.exists());
    }

    #[test]
    fn routing_preserves_unknown_output_config_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), "session", 1_000);
        let mut body = request();
        body.extra.insert(
            "output_config".to_string(),
            json!({"format": {"type": "json_schema"}, "effort": "low"}),
        );
        body.extra
            .insert("future_feature".to_string(), json!({"preserved": true}));
        assert!(route(dir.path()).try_route(&mut body, Some("session"), "request", false, 1_000));
        assert_eq!(
            body.extra["output_config"],
            json!({"format": {"type": "json_schema"}, "effort": "high"}),
        );
        assert_eq!(body.extra["future_feature"], json!({"preserved": true}));
    }

    #[test]
    fn concurrent_claims_route_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), "session", 1_000);
        let router = Arc::new(route(dir.path()));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut joins = Vec::new();
        for request_id in ["first", "second"] {
            let router = Arc::clone(&router);
            let barrier = Arc::clone(&barrier);
            joins.push(thread::spawn(move || {
                let mut body = request();
                barrier.wait();
                router.try_route(&mut body, Some("session"), request_id, false, 1_000)
            }));
        }
        let routed = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .filter(|routed| *routed)
            .count();
        assert_eq!(routed, 1);
    }
}
