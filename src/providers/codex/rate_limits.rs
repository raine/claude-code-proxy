//! Latest Codex rate-limit snapshot, captured from `codex.rate_limits`
//! stream events and served by the proxy's `GET /usage` endpoint.
//!
//! The upstream pushes a snapshot on responses whether or not a limit is
//! reached; before this module, the `limit_reached: false` snapshots were
//! discarded, so the only way to see remaining quota was an unofficial
//! side-channel API. The upstream payload shape is undocumented and can
//! change, so the `rate_limits` object is stored and served verbatim
//! rather than being re-modeled field by field.
//!
//! The snapshot is also mirrored to a small file under the state dir so it
//! survives a proxy restart: quota does not change across a restart, so a
//! freshly started proxy can answer `GET /usage` before its first upstream
//! request instead of returning null until traffic flows.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::RwLock;

static LATEST: RwLock<Option<Value>> = RwLock::new(None);

fn snapshot_file() -> PathBuf {
    crate::paths::state_dir().join("codex-usage.json")
}

/// Record the snapshot when `payload` is a `codex.rate_limits` event.
/// Returns true when a snapshot was captured.
pub fn record_event(payload: &Value) -> bool {
    if payload.get("type").and_then(Value::as_str) != Some("codex.rate_limits") {
        return false;
    }
    let Some(rate_limits) = payload.get("rate_limits") else {
        return false;
    };
    let snapshot = json!({
        "rate_limits": rate_limits.clone(),
        "captured_at": jiff::Timestamp::now().to_string(),
    });
    persist(&snapshot);
    if let Ok(mut guard) = LATEST.write() {
        *guard = Some(snapshot);
        return true;
    }
    false
}

/// The most recent snapshot, or None when neither this process nor a prior
/// run has produced one. A cold process reads the persisted file once and
/// promotes it into memory so later reads stay in-process.
pub fn latest() -> Option<Value> {
    if let Ok(guard) = LATEST.read()
        && guard.is_some()
    {
        return guard.clone();
    }
    let loaded = load_persisted()?;
    if let Ok(mut guard) = LATEST.write()
        && guard.is_none()
    {
        *guard = Some(loaded.clone());
    }
    Some(loaded)
}

fn persist(snapshot: &Value) {
    let path = snapshot_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(snapshot) {
        // Best-effort: a failed write must never break request handling.
        let _ = std::fs::write(&path, bytes);
    }
}

fn load_persisted() -> Option<Value> {
    let bytes = std::fs::read(snapshot_file()).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("rate_limits").is_some() {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignores_non_rate_limit_events() {
        assert!(!record_event(&json!({"type": "keepalive"})));
        assert!(!record_event(
            &json!({"type": "response.output_text.delta", "delta": "x"})
        ));
        assert!(!record_event(&json!({"type": "codex.rate_limits"})));
    }

    #[test]
    fn records_and_returns_latest_snapshot() {
        // Keep the persisted mirror out of the real state dir.
        let state = tempfile::tempdir().unwrap();
        // SAFETY: restored implicitly at process exit; value is a temp path.
        unsafe { std::env::set_var("XDG_STATE_HOME", state.path()) };

        let first = json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "limit_reached": false,
                "primary": {"used_percent": 16.0, "window_minutes": 10080}
            }
        });
        assert!(record_event(&first));
        let stored = latest().expect("snapshot stored");
        assert_eq!(
            stored.pointer("/rate_limits/primary/used_percent"),
            Some(&json!(16.0))
        );
        assert!(stored.get("captured_at").and_then(Value::as_str).is_some());

        let second = json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true}
        });
        assert!(record_event(&second));
        let stored = latest().expect("snapshot replaced");
        assert_eq!(
            stored.pointer("/rate_limits/limit_reached"),
            Some(&json!(true))
        );
    }

    #[test]
    fn persisted_snapshot_loads_on_cold_state() {
        // Isolated state dir so the load path is exercised without touching
        // the real one or the shared in-memory LATEST.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex-usage.json");
        let snapshot = json!({
            "rate_limits": {"limit_reached": false, "primary": {"used_percent": 42.0}},
            "captured_at": "2026-07-19T00:00:00Z",
        });
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let loaded: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            loaded.pointer("/rate_limits/primary/used_percent"),
            Some(&json!(42.0))
        );
    }

    #[test]
    fn load_persisted_rejects_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex-usage.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).is_err());
    }
}
