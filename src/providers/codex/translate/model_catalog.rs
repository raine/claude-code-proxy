//! Model knowledge from the Codex CLI's own model cache
//! (`$CODEX_HOME/models_cache.json`, default `~/.codex/models_cache.json`).
//!
//! The Codex CLI refreshes that file from the server (ETag fetch), so a model
//! that OpenAI ships to Codex shows up there before this proxy can ship a
//! release that lists it. The static allowlist in `model_allowlist` stays the
//! baseline; the catalog extends it at runtime.
//!
//! Only the fields the proxy needs are read. Each model object is decoded
//! leniently on its own, so one unexpected entry never discards the file.
//! The parsed result is cached per (mtime, len) of the file; a missing or
//! unreadable file keeps the last good parse (or yields nothing).

use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    /// `use_responses_lite` — the model exists behind the Responses Lite lane.
    pub use_responses_lite: bool,
    /// `supported_in_api` — false for TUI-only models the backend rejects.
    pub supported_in_api: bool,
    /// `visibility == "list"` — shown in the Codex picker (hidden models are
    /// still accepted when requested explicitly, just not advertised).
    pub listed: bool,
}

#[derive(Deserialize)]
struct CacheFile {
    #[serde(default)]
    models: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CacheModel {
    slug: String,
    #[serde(default)]
    use_responses_lite: bool,
    #[serde(default = "default_true")]
    supported_in_api: bool,
    #[serde(default = "default_visibility")]
    visibility: String,
}

fn default_true() -> bool {
    true
}

fn default_visibility() -> String {
    "list".to_string()
}

struct CacheState {
    key: Option<(SystemTime, u64)>,
    models: Vec<CatalogModel>,
}

static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();

pub fn catalog_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(dir).join("models_cache.json"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".codex").join("models_cache.json"))
}

/// Parses the cache file bytes. Returns `None` only if the top level is not
/// the expected object; individual malformed models are skipped.
pub fn parse_catalog(bytes: &[u8]) -> Option<Vec<CatalogModel>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if !value.is_object() {
        return None;
    }
    let file: CacheFile = serde_json::from_value(value).ok()?;
    let models = file
        .models
        .into_iter()
        .filter_map(|value| serde_json::from_value::<CacheModel>(value).ok())
        .filter(|model| !model.slug.is_empty())
        .map(|model| CatalogModel {
            listed: model.visibility == "list",
            slug: model.slug,
            use_responses_lite: model.use_responses_lite,
            supported_in_api: model.supported_in_api,
        })
        .collect();
    Some(models)
}

/// Current catalog snapshot (cached per file mtime/len).
pub fn catalog_models() -> Vec<CatalogModel> {
    let mutex = CACHE.get_or_init(|| {
        Mutex::new(CacheState {
            key: None,
            models: Vec::new(),
        })
    });
    let mut state = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let Some(path) = catalog_path() else {
        return state.models.clone();
    };
    let Some(key) = fs::metadata(&path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())))
    else {
        return state.models.clone();
    };
    if state.key == Some(key) {
        return state.models.clone();
    }
    if let Some(models) = fs::read(&path).ok().and_then(|bytes| parse_catalog(&bytes)) {
        state.key = Some(key);
        state.models = models;
    }
    state.models.clone()
}

pub fn catalog_model(slug: &str) -> Option<CatalogModel> {
    catalog_models()
        .into_iter()
        .find(|model| model.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "fetched_at": "2026-09-04T20:07:48Z",
        "models": [
            {"slug": "gpt-6-astra", "use_responses_lite": true, "supported_in_api": true, "visibility": "list"},
            {"slug": "gpt-reserve", "use_responses_lite": true, "supported_in_api": true, "visibility": "hide"},
            {"slug": "gpt-5.3-codex-spark", "use_responses_lite": false, "supported_in_api": false, "visibility": "list"},
            {"slug": "gpt-7-minimal"},
            {"nonsense": true},
            {"slug": ""}
        ]
    }"#;

    #[test]
    fn parses_only_needed_fields_and_skips_broken_entries() {
        let models = parse_catalog(FIXTURE.as_bytes()).expect("fixture parses");
        assert_eq!(models.len(), 4);
        assert_eq!(
            models[0],
            CatalogModel {
                slug: "gpt-6-astra".into(),
                use_responses_lite: true,
                supported_in_api: true,
                listed: true,
            }
        );
        assert!(!models[1].listed, "hidden models are kept but not listed");
        assert!(!models[2].supported_in_api);
        // Missing fields fall back to: full lane, API-supported, listed.
        assert_eq!(
            models[3],
            CatalogModel {
                slug: "gpt-7-minimal".into(),
                use_responses_lite: false,
                supported_in_api: true,
                listed: true,
            }
        );
    }

    #[test]
    fn garbage_yields_none_and_empty_models_yields_empty() {
        assert!(parse_catalog(b"not json").is_none());
        assert!(parse_catalog(b"[]").is_none());
        assert_eq!(parse_catalog(b"{}").unwrap(), Vec::<CatalogModel>::new());
    }
}
