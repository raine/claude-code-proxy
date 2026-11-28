//! Named credential profiles. Runtime selection changes only after inference drains.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

pub const DEFAULT_PROFILE: &str = "default";
const PROVIDERS: [&str; 2] = ["codex", "anthropic"];
static ACTIVE: OnceLock<RwLock<Result<BTreeMap<String, String>, String>>> = OnceLock::new();
thread_local! {
    // Constructors are synchronous. Pin each new auth store to the candidate
    // without publishing it to monitor readers before the backend swap commits.
    static CONSTRUCTING: RefCell<Option<BTreeMap<String, String>>> = const { RefCell::new(None) };
}

fn selection() -> &'static RwLock<Result<BTreeMap<String, String>, String>> {
    ACTIVE.get_or_init(|| {
        let env = std::env::vars().collect();
        RwLock::new(
            resolve_selection(&ProfileStore::configured(), &BTreeMap::new(), &env)
                .map_err(|error| error.to_string()),
        )
    })
}

pub(crate) fn candidate(provider: &str, name: &str) -> Result<BTreeMap<String, String>> {
    validate_provider(provider)?;
    validate_name(name)?;
    let store = ProfileStore::configured();
    let valid = if provider == "anthropic" {
        let key = if name == DEFAULT_PROFILE {
            crate::config::default_anthropic_api_key()
        } else {
            load_anthropic_key(&store, name).ok()
        };
        key.is_some_and(|key| !key.trim().is_empty() && http::HeaderValue::from_str(&key).is_ok())
    } else {
        crate::providers::codex::auth::token_store::auth_for_profile(&store, name)
            .ok()
            .flatten()
            .is_some_and(|auth| {
                !auth.access.trim().is_empty()
                    && !auth.refresh.trim().is_empty()
                    && http::HeaderValue::from_str(&format!("Bearer {}", auth.access)).is_ok()
                    && auth
                        .account_id
                        .as_ref()
                        .is_none_or(|id| http::HeaderValue::from_str(id).is_ok())
            })
    };
    ensure!(valid, "Profile credentials are missing or invalid");
    let mut selected = selection()
        .read()
        .unwrap()
        .as_ref()
        .map_err(|_| anyhow::anyhow!("Invalid account selection"))?
        .clone();
    selected.insert(provider.to_owned(), name.to_owned());
    Ok(selected)
}

pub(crate) fn construct_with<T>(
    selected: &BTreeMap<String, String>,
    build: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<BTreeMap<String, String>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            CONSTRUCTING.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let previous = CONSTRUCTING.with(|slot| slot.replace(Some(selected.clone())));
    let _reset = Reset(previous);
    build()
}

pub(crate) fn commit(selected: BTreeMap<String, String>) {
    *selection().write().unwrap() = Ok(selected);
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    #[serde(default)]
    defaults: BTreeMap<String, String>,
}

pub struct ProfileStore {
    root: PathBuf,
}

impl ProfileStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn configured() -> Self {
        Self::new(crate::paths::config_dir())
    }

    fn defaults_path(&self) -> PathBuf {
        self.root.join("account-profiles.json")
    }

    fn read_defaults(&self) -> Result<Defaults> {
        match fs::read(self.defaults_path()) {
            Ok(raw) => {
                let defaults: Defaults =
                    serde_json::from_slice(&raw).context("Invalid account-profiles.json")?;
                for (provider, name) in &defaults.defaults {
                    validate_provider(provider)?;
                    validate_name(name)?;
                }
                Ok(defaults)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Defaults::default()),
            Err(error) => Err(error).context("Cannot read account-profiles.json"),
        }
    }

    pub fn default_profile(&self, provider: &str) -> Result<String> {
        validate_provider(provider)?;
        let defaults = self.read_defaults()?;
        let name = defaults
            .defaults
            .get(provider)
            .map(String::as_str)
            .unwrap_or(DEFAULT_PROFILE);
        validate_name(name)?;
        Ok(name.to_owned())
    }

    /// Named profiles never share a file or legacy/keychain fallback with default.
    pub fn credential_path(&self, provider: &str, name: &str) -> Result<PathBuf> {
        validate_provider(provider)?;
        validate_name(name)?;
        ensure!(
            name != DEFAULT_PROFILE,
            "The default profile uses existing provider configuration"
        );
        Ok(self
            .root
            .join("accounts")
            .join(provider)
            .join(format!("{name}.json")))
    }

    pub fn list(&self, provider: &str) -> Result<Vec<String>> {
        validate_provider(provider)?;
        let dir = self.root.join("accounts").join(provider);
        let mut names = vec![DEFAULT_PROFILE.to_owned()];
        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    if let Some(name) = path.file_stem().and_then(|name| name.to_str())
                        && validate_name(name).is_ok()
                        && name != DEFAULT_PROFILE
                    {
                        names.push(name.to_owned());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot list account profiles"),
        }
        names.sort();
        Ok(names)
    }

    pub fn use_profile(&self, provider: &str, name: &str) -> Result<()> {
        validate_provider(provider)?;
        validate_name(name)?;
        if name != DEFAULT_PROFILE {
            ensure!(
                self.credential_path(provider, name)?.is_file(),
                "Profile '{name}' does not exist; authenticate it first"
            );
        }
        let mut defaults = self.read_defaults()?;
        defaults
            .defaults
            .insert(provider.to_owned(), name.to_owned());
        crate::auth::write_atomically(&self.defaults_path().to_string_lossy(), &defaults)
    }

    pub fn remove(&self, provider: &str, name: &str) -> Result<()> {
        let path = self.credential_path(provider, name)?;
        ensure!(
            self.default_profile(provider)? != name,
            "Cannot remove the default profile; select another profile first"
        );
        // Removal is explicit and targets only the named file; never delete legacy auth.
        fs::remove_file(path).context("Cannot remove account profile")
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_')),
        "Profile names must contain 1-64 lowercase ASCII letters, digits, '-' or '_'"
    );
    let reserved = matches!(name, "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|prefix| {
            name.strip_prefix(*prefix).is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            })
        });
    ensure!(!reserved, "Profile name is reserved on Windows");
    Ok(())
}

fn validate_provider(provider: &str) -> Result<()> {
    ensure!(
        PROVIDERS.contains(&provider),
        "Profiles are supported only for codex and anthropic"
    );
    Ok(())
}

fn resolve_selection(
    store: &ProfileStore,
    overrides: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut selected = BTreeMap::new();
    for provider in PROVIDERS {
        let env_name = format!("CCP_{}_PROFILE", provider.to_ascii_uppercase());
        let name = overrides
            .get(provider)
            .cloned()
            .or_else(|| {
                env.get(&env_name)
                    .filter(|value| !value.is_empty())
                    .cloned()
            })
            .map(Ok)
            .unwrap_or_else(|| store.default_profile(provider))?;
        validate_name(&name)?;
        selected.insert(provider.to_owned(), name);
    }
    Ok(selected)
}

/// Called before constructing providers. A later `profile use` affects new
/// processes only, so refreshes and in-flight conversations keep their account.
pub fn initialize(overrides: BTreeMap<String, String>) -> Result<()> {
    let env = std::env::vars().collect();
    let selected = resolve_selection(&ProfileStore::configured(), &overrides, &env)?;
    ACTIVE
        .set(RwLock::new(Ok(selected)))
        .map_err(|_| anyhow::anyhow!("Account profiles are already initialized"))
}

pub fn active_profile(provider: &str) -> Result<String> {
    validate_provider(provider)?;
    if let Some(name) = CONSTRUCTING.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|map| map.get(provider).cloned())
    }) {
        return Ok(name);
    }
    let selected = selection().read().unwrap();
    match &*selected {
        Ok(selected) => Ok(selected.get(provider).expect("known provider").clone()),
        Err(error) => bail!("{error}"),
    }
}

#[derive(Serialize, Deserialize)]
struct ApiKeyProfile {
    #[serde(rename = "apiKey")]
    api_key: String,
}

pub fn save_anthropic_key(store: &ProfileStore, name: &str, key: &str) -> Result<()> {
    let path = store.credential_path("anthropic", name)?;
    ensure!(
        !key.trim().is_empty(),
        "Anthropic API key must not be empty"
    );
    let profile = ApiKeyProfile {
        api_key: key.trim().to_owned(),
    };
    crate::auth::write_atomically(&path.to_string_lossy(), &profile)
}

pub fn load_anthropic_key(store: &ProfileStore, name: &str) -> Result<String> {
    let path = store.credential_path("anthropic", name)?;
    load_key(&path)
}

fn load_key(path: &Path) -> Result<String> {
    let raw = fs::read(path).context("Cannot read Anthropic profile credentials")?;
    // Do not include serde's error: malformed input can contain secret values.
    let profile: ApiKeyProfile = serde_json::from_slice(&raw)
        .map_err(|_| anyhow::anyhow!("Invalid Anthropic profile credentials"))?;
    ensure!(
        !profile.api_key.trim().is_empty(),
        "Anthropic profile API key is empty"
    );
    Ok(profile.api_key.trim().to_owned())
}

/// Safe, read-only metadata suitable for monitor snapshots. Never include secrets,
/// upstream account identifiers, file paths, or unfiltered error messages here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileStatus {
    pub provider: String,
    pub name: String,
    pub selected: bool,
    pub is_default: bool,
    pub status: String,
    pub expires_at_ms: Option<u64>,
}

pub fn profile_statuses() -> Vec<ProfileStatus> {
    let store = ProfileStore::configured();
    let now = time::OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .max(0)
        .saturating_div(1_000_000) as u64;
    collect_statuses(&store, now, |provider| active_profile(provider).ok())
}

fn collect_statuses(
    store: &ProfileStore,
    now: u64,
    selected: impl Fn(&str) -> Option<String>,
) -> Vec<ProfileStatus> {
    let mut result = Vec::new();
    for provider in PROVIDERS {
        let default = store.default_profile(provider);
        let selected = selected(provider);
        let names = match store.list(provider) {
            Ok(mut names) => {
                // Include selected/default profiles even if their credential file disappeared.
                for name in [default.as_ref().ok(), selected.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                names.sort();
                names
            }
            Err(_) => {
                result.push(ProfileStatus {
                    provider: provider.to_owned(),
                    name: "(profiles)".to_owned(),
                    selected: false,
                    is_default: false,
                    status: "Cannot read profile directory".to_owned(),
                    expires_at_ms: None,
                });
                continue;
            }
        };
        for name in names {
            let (mut status, expires_at_ms) = if provider == "anthropic" {
                anthropic_status(store, &name)
            } else {
                codex_status(store, &name, now)
            };
            if default.is_err() {
                status = "Invalid profile configuration".to_owned();
            }
            result.push(ProfileStatus {
                provider: provider.to_owned(),
                selected: selected.as_ref() == Some(&name),
                is_default: default.as_ref().ok() == Some(&name),
                name,
                status,
                expires_at_ms,
            });
        }
    }
    result
}

fn anthropic_status(store: &ProfileStore, name: &str) -> (String, Option<u64>) {
    let configured = if name == DEFAULT_PROFILE {
        crate::config::default_anthropic_api_key().is_some()
    } else {
        match load_anthropic_key(store, name) {
            Ok(_) => true,
            Err(_) => {
                let exists = store
                    .credential_path("anthropic", name)
                    .is_ok_and(|path| path.exists());
                return (
                    if exists {
                        "Invalid/unreadable credentials"
                    } else {
                        "Not configured"
                    }
                    .to_owned(),
                    None,
                );
            }
        }
    };
    (
        if configured {
            "Configured (unverified)"
        } else {
            "Not configured"
        }
        .to_owned(),
        None,
    )
}

fn codex_status(store: &ProfileStore, name: &str, now: u64) -> (String, Option<u64>) {
    match crate::providers::codex::auth::token_store::auth_for_profile(store, name) {
        Ok(Some(auth)) => {
            let status = if auth.access.trim().is_empty() || auth.refresh.trim().is_empty() {
                "Invalid credentials"
            } else if auth.expires <= now {
                "Expired (refresh available)"
            } else if auth.expires.saturating_sub(now) <= 300_000 {
                "Refresh due"
            } else {
                "Authenticated (local)"
            };
            (status.to_owned(), Some(auth.expires))
        }
        Ok(None) => {
            let exists = name != DEFAULT_PROFILE
                && store
                    .credential_path("codex", name)
                    .is_ok_and(|path| path.exists());
            (
                if exists {
                    "Invalid/unreadable credentials"
                } else {
                    "Not authenticated"
                }
                .to_owned(),
                None,
            )
        }
        Err(_) => ("Cannot read credentials".to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_cannot_escape_or_collide_on_windows() {
        for name in [
            "",
            "../work",
            "a/b",
            "a\\b",
            ".",
            "..",
            "Work",
            "work.json",
            "x:y",
            "é",
            "con",
            "nul",
            "com1",
            "lpt9",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name(&"x".repeat(65)).is_err());
        for name in ["default", "work", "personal-2", "account_1"] {
            assert!(validate_name(name).is_ok());
        }
    }

    #[test]
    fn credentials_defaults_and_removal_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().to_owned());
        assert_eq!(store.default_profile("anthropic").unwrap(), "default");
        save_anthropic_key(&store, "work", " work-secret ").unwrap();
        save_anthropic_key(&store, "personal", "personal-secret").unwrap();
        assert_eq!(load_anthropic_key(&store, "work").unwrap(), "work-secret");
        assert_eq!(
            load_anthropic_key(&store, "personal").unwrap(),
            "personal-secret"
        );
        assert!(store.use_profile("anthropic", "missing").is_err());
        store.use_profile("anthropic", "work").unwrap();
        assert!(store.remove("anthropic", "work").is_err());
        assert!(store.remove("anthropic", "default").is_err());
        store.remove("anthropic", "personal").unwrap();
        assert_eq!(store.list("anthropic").unwrap(), ["default", "work"]);
        assert_eq!(store.default_profile("codex").unwrap(), "default");
        let defaults = fs::read_to_string(store.defaults_path()).unwrap();
        assert!(!defaults.contains("secret"));
        assert!(load_anthropic_key(&store, "missing").is_err());
    }

    #[test]
    fn explicit_selection_then_env_then_saved_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().to_owned());
        save_anthropic_key(&store, "work", "key").unwrap();
        store.use_profile("anthropic", "work").unwrap();
        let env = BTreeMap::from([("CCP_ANTHROPIC_PROFILE".into(), "personal".into())]);
        let overrides = BTreeMap::from([("anthropic".into(), "explicit".into())]);
        assert_eq!(
            resolve_selection(&store, &BTreeMap::new(), &BTreeMap::new()).unwrap()["anthropic"],
            "work"
        );
        assert_eq!(
            resolve_selection(&store, &BTreeMap::new(), &env).unwrap()["anthropic"],
            "personal"
        );
        assert_eq!(
            resolve_selection(&store, &overrides, &env).unwrap()["anthropic"],
            "explicit"
        );
        let bad = BTreeMap::from([("CCP_CODEX_PROFILE".into(), "../bad".into())]);
        assert!(resolve_selection(&store, &BTreeMap::new(), &bad).is_err());
    }

    #[test]
    fn named_profile_statuses_are_local_and_secret_free() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().to_owned());
        save_anthropic_key(&store, "work", "never-show-api-key").unwrap();
        assert_eq!(
            anthropic_status(&store, "work").0,
            "Configured (unverified)"
        );
        assert_eq!(anthropic_status(&store, "missing").0, "Not configured");
        let path = store.credential_path("codex", "work").unwrap();
        crate::auth::write_atomically(
            &path.to_string_lossy(),
            &serde_json::json!({
                "access": "never-show-access", "refresh": "never-show-refresh",
                "expires": 1000, "accountId": "never-show-account-id"
            }),
        )
        .unwrap();
        let (status, expires_at_ms) = codex_status(&store, "work", 2000);
        assert_eq!(status, "Expired (refresh available)");
        assert_eq!(expires_at_ms, Some(1000));
        let row = ProfileStatus {
            provider: "codex".into(),
            name: "work".into(),
            selected: true,
            is_default: false,
            status,
            expires_at_ms,
        };
        let serialized = serde_json::to_string(&row).unwrap();
        assert!(!serialized.contains("never-show"));
        assert!(!serialized.contains("accountId"));
        assert!(!serialized.contains(&dir.path().to_string_lossy().to_string()));
        assert_eq!(codex_status(&store, "work", 0).0, "Refresh due");
        assert_eq!(codex_status(&store, "missing", 0).0, "Not authenticated");
    }

    #[test]
    fn malformed_default_selection_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().to_owned());
        for raw in [
            "not json",
            r#"{"defauts":{"codex":"work"}}"#,
            r#"{"defaults":{"codxe":"work"}}"#,
            r#"{"defaults":{"codex":"../work"}}"#,
        ] {
            fs::write(store.defaults_path(), raw).unwrap();
            assert!(store.default_profile("codex").is_err());
        }
    }
}
