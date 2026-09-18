//! Full-translation regression coverage for the compaction effort cap.
//!
//! These tests exercise the serialized Codex request body, not just the
//! helper value table, because the cap interacts with effort overrides and
//! with which reasoning artifacts get requested. Every case mutates process
//! environment variables, so it holds the shared `ENV_LOCK` and restores the
//! previous environment through `EnvGuard`. The test binary runs in its own
//! process, keeping this environment manipulation away from the unit tests.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use claude_code_proxy::MessagesRequest;
use claude_code_proxy::providers::codex::translate::request::{
    TranslateOptions, translate_request,
};
use serde_json::{Value, json};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
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

/// Clears the environment knobs that feed effort resolution and reasoning
/// summary selection, then points the config dir at an empty directory so a
/// real user config cannot leak into the test.
fn isolated_environment(config_dir: &Path) -> Vec<EnvGuard> {
    let mut guards = vec![
        EnvGuard::unset("CCP_CODEX_EFFORT"),
        EnvGuard::unset("CCP_CODEX_REASONING_SUMMARY"),
        EnvGuard::unset("CCP_COMPACT_EFFORT"),
    ];
    guards.push(EnvGuard::set("CCP_CONFIG_DIR", config_dir));
    guards
}

fn compact_request() -> MessagesRequest {
    serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "summarize"}],
        "system": "You are a helpful AI assistant tasked with summarizing conversations."
    }))
    .unwrap()
}

fn translate_compact() -> Value {
    let opts = TranslateOptions {
        session_id: None,
        service_tier: None,
        model: "gpt-5.5".to_string(),
        use_responses_lite: false,
    };
    let out = translate_request(&compact_request(), opts).unwrap();
    serde_json::to_value(out).unwrap()
}

#[test]
fn omitted_effort_compact_uses_default_low_cap() {
    let _guard = env_lock();
    let config = tempfile::TempDir::new().unwrap();
    let _env = isolated_environment(config.path());

    let wire = translate_compact();

    assert_eq!(
        wire["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
    assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn compact_cap_none_overrides_default_without_reasoning_artifacts() {
    let _guard = env_lock();
    let config = tempfile::TempDir::new().unwrap();
    let mut env = isolated_environment(config.path());
    env.push(EnvGuard::set("CCP_COMPACT_EFFORT", "none"));

    let wire = translate_compact();

    // `none` must reach the wire to displace the upstream default, but it
    // must not ask for a summary or encrypted continuation content.
    assert_eq!(wire["reasoning"], json!({"effort": "none"}));
    assert!(wire.get("include").is_none(), "unexpected include: {wire}");
}

#[test]
fn compact_cap_off_leaves_omitted_effort_unset() {
    let _guard = env_lock();
    let config = tempfile::TempDir::new().unwrap();
    let mut env = isolated_environment(config.path());
    env.push(EnvGuard::set("CCP_COMPACT_EFFORT", "off"));

    let wire = translate_compact();

    assert!(
        wire.get("reasoning").is_none(),
        "unexpected reasoning: {wire}"
    );
    assert!(wire.get("include").is_none(), "unexpected include: {wire}");
}

#[test]
fn global_high_is_lowered_by_default_compact_cap() {
    let _guard = env_lock();
    let config = tempfile::TempDir::new().unwrap();
    let mut env = isolated_environment(config.path());
    env.push(EnvGuard::set("CCP_CODEX_EFFORT", "high"));

    let wire = translate_compact();

    assert_eq!(
        wire["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
    assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn global_none_survives_compact_cap_without_reasoning_artifacts() {
    let _guard = env_lock();
    let config = tempfile::TempDir::new().unwrap();
    let mut env = isolated_environment(config.path());
    env.push(EnvGuard::set("CCP_CODEX_EFFORT", "none"));

    let wire = translate_compact();

    // `none` is at or below the cap, so it is preserved rather than raised.
    assert_eq!(wire["reasoning"], json!({"effort": "none"}));
    assert!(wire.get("include").is_none(), "unexpected include: {wire}");
}

#[test]
fn global_low_is_preserved_under_higher_compact_cap() {
    let _guard = env_lock();
    let config = tempfile::TempDir::new().unwrap();
    let mut env = isolated_environment(config.path());
    env.push(EnvGuard::set("CCP_CODEX_EFFORT", "low"));
    env.push(EnvGuard::set("CCP_COMPACT_EFFORT", "medium"));

    let wire = translate_compact();

    assert_eq!(
        wire["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
    assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
}
