//! Advisory model metadata from `$CODEX_HOME/models_cache.json` (default
//! `~/.codex/models_cache.json`). No credentials are read and no network requests
//! are made. Cache age, client version and provider identity do not establish
//! authorization: the upstream still decides access. Compiled models remain the
//! baseline even when this optional cache is absent or invalid.
//!
//! Snapshots are checked at most once per second and parsed only on metadata
//! changes. Invalid or missing files clear the extension, not the baseline.

use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use serde::Deserialize;

const MAX_CACHE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MODELS: usize = 4096;
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    pub use_responses_lite: bool,
    pub supported_in_api: bool,
    pub listed: bool,
}

#[derive(Deserialize)]
struct CacheFile {
    models: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CacheModel {
    slug: String,
    #[serde(default)]
    use_responses_lite: bool,
    supported_in_api: bool,
    visibility: Visibility,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Visibility {
    List,
    Hide,
    None,
}

#[derive(Default)]
struct CacheState {
    path: Option<PathBuf>,
    checked_at: Option<Instant>,
    key: Option<(SystemTime, u64)>,
    models: Vec<CatalogModel>,
}

static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();

fn catalog_path() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty());
    let dir = home.or_else(|| {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|dir| !dir.is_empty())
            .map(|home| PathBuf::from(home).join(".codex").into_os_string())
    })?;
    Some(PathBuf::from(dir).join("models_cache.json"))
}

/// Slugs must not impersonate local aliases, tier suffixes or provider syntax.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug.as_bytes()[0].is_ascii_alphanumeric()
        && slug
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        && !slug.ends_with("-fast")
        && !slug.ends_with("-ultrafast")
        && !crate::registry::is_anthropic_alias(slug)
        && !crate::registry::is_cursor_model(slug)
}

/// Malformed entries are skipped; required metadata must not grant access by
/// default. The optional Lite field defaults to false, as it does in Codex.
fn parse_catalog(bytes: &[u8]) -> Option<Vec<CatalogModel>> {
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    let file: CacheFile = serde_json::from_slice(bytes).ok()?;
    if file.models.len() > MAX_MODELS {
        return None;
    }
    let mut seen = HashSet::new();
    Some(
        file.models
            .into_iter()
            .filter_map(|value| serde_json::from_value::<CacheModel>(value).ok())
            .filter(|model| valid_slug(&model.slug) && seen.insert(model.slug.clone()))
            .map(|model| CatalogModel {
                listed: matches!(model.visibility, Visibility::List),
                slug: model.slug,
                use_responses_lite: model.use_responses_lite,
                supported_in_api: model.supported_in_api,
            })
            .collect(),
    )
}

impl CacheState {
    fn load(&mut self, path: Option<&Path>, now: Instant) -> Vec<CatalogModel> {
        if self.path.as_deref() != path {
            *self = Self {
                path: path.map(Path::to_path_buf),
                ..Self::default()
            };
        }
        if self
            .checked_at
            .is_some_and(|last| now.duration_since(last) < CHECK_INTERVAL)
        {
            return self.models.clone();
        }
        self.checked_at = Some(now);
        let metadata = path
            .and_then(|p| fs::metadata(p).ok())
            .filter(|meta| meta.is_file() && meta.len() <= MAX_CACHE_BYTES);
        let key = metadata.and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if key.is_some() && key == self.key {
            return self.models.clone();
        }
        self.key = None;
        self.models = path
            .filter(|_| key.is_some())
            .and_then(|p| {
                let file = fs::File::open(p).ok()?;
                let mut bytes = Vec::new();
                file.take(MAX_CACHE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                self.key = key;
                parse_catalog(&bytes)
            })
            .unwrap_or_default();
        self.models.clone()
    }
}

pub fn catalog_models() -> Vec<CatalogModel> {
    CACHE
        .get_or_init(|| Mutex::new(CacheState::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .load(catalog_path().as_deref(), Instant::now())
}

pub fn catalog_model(slug: &str) -> Option<CatalogModel> {
    catalog_models()
        .into_iter()
        .find(|model| model.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(slug: &str) -> Vec<u8> {
        serde_json::to_vec(
            &json!({"fetched_at":"2020-01-01T00:00:00Z", "client_version":"old", "models":[
                {"slug":slug,"supported_in_api":true,"visibility":"list","use_responses_lite":true}
            ]}),
        )
        .unwrap()
    }

    #[test]
    fn parses_metadata_without_trusting_incomplete_entries() {
        let models = parse_catalog(br#"{"models":[
            {"slug":"gpt-future","supported_in_api":true,"visibility":"list","use_responses_lite":true},
            {"slug":"gpt-hidden","supported_in_api":true,"visibility":"hide"},
            {"slug":"gpt-denied","supported_in_api":false,"visibility":"list"},
            {"slug":"gpt-incomplete"},
            {"slug":"gpt-bad","supported_in_api":"true","visibility":"list"},
            {"slug":"gpt-future","supported_in_api":false,"visibility":"hide"},
            {"slug":"gpt-unknown-visibility","supported_in_api":true,"visibility":"unknown"}
        ]}"#).unwrap();
        assert_eq!(models.len(), 3);
        assert!(models[0].use_responses_lite);
        assert!(!models[1].listed);
        assert!(!models[1].use_responses_lite);
        assert!(!models[2].supported_in_api);
    }

    #[test]
    fn rejects_reserved_or_unsafe_slugs() {
        for slug in [
            "",
            "../gpt-test",
            "gpt-test\n",
            "gpt-test[1m]",
            "gpt-test-fast",
            "gpt-test-ultrafast",
            "opus",
            "claude-sonnet-5",
            "cursor",
            "cursor:test",
            "-test",
        ] {
            assert!(!valid_slug(slug), "{slug:?}");
        }
        assert!(!valid_slug(&"a".repeat(129)));
        assert!(valid_slug("gpt-7.2-codex"));
    }

    #[test]
    fn invalid_or_oversized_catalog_is_not_an_extension() {
        for bytes in [b"not json".as_slice(), b"[]", b"{}", b"{\"models\":null}"] {
            assert!(parse_catalog(bytes).is_none());
        }
        assert!(parse_catalog(&vec![b' '; MAX_CACHE_BYTES as usize + 1]).is_none());
        let many = serde_json::to_vec(&json!({"models":vec![json!({}); MAX_MODELS + 1]})).unwrap();
        assert!(parse_catalog(&many).is_none());
        assert_eq!(parse_catalog(b"{\"models\":[]}").unwrap(), vec![]);
    }

    #[test]
    fn snapshots_refresh_and_never_cross_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        fs::write(&a, fixture("gpt-a")).unwrap();
        fs::write(&b, fixture("gpt-b")).unwrap();
        let now = Instant::now();
        let later = now + CHECK_INTERVAL;
        let mut state = CacheState::default();
        assert_eq!(state.load(Some(&a), now)[0].slug, "gpt-a");
        // Switching paths bypasses the polling interval, even at equal size.
        assert_eq!(state.load(Some(&b), now)[0].slug, "gpt-b");
        fs::write(&b, b"broken").unwrap();
        assert_eq!(state.load(Some(&b), now)[0].slug, "gpt-b");
        assert!(state.load(Some(&b), later).is_empty());
        fs::write(&b, fixture("gpt-restored")).unwrap();
        assert_eq!(
            state.load(Some(&b), later + CHECK_INTERVAL)[0].slug,
            "gpt-restored"
        );
        fs::remove_file(&b).unwrap();
        assert!(state.load(Some(&b), later + CHECK_INTERVAL * 2).is_empty());
        assert!(state.load(None, later + CHECK_INTERVAL * 3).is_empty());
        assert!(
            state
                .load(Some(dir.path()), later + CHECK_INTERVAL * 4)
                .is_empty()
        );
    }
}
