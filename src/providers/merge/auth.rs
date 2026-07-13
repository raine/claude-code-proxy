//! Bearer/token auth for the generic Anthropic-compatible upstream.
//!
//! Env wins over the on-disk auth file. Full 1Password injection is owned by
//! the macOS app; this provider only needs a configured token + base URL.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StoredMergeAuth {
    /// Bearer / Anthropic-style auth token for the upstream Messages API.
    pub access: String,
}

pub fn load_merge_token() -> Option<String> {
    if let Some(token) = env_merge_token() {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    load_auth_file()
        .ok()
        .flatten()
        .map(|auth| auth.access)
        .filter(|token| !token.trim().is_empty())
}

pub fn env_merge_token() -> Option<String> {
    std::env::var("CCP_MERGE_AUTH_TOKEN")
        .ok()
        .or_else(|| std::env::var("MERGE_AUTH_TOKEN").ok())
        .or_else(|| std::env::var("CCP_MERGE_API_KEY").ok())
}

pub fn auth_file_path() -> PathBuf {
    paths::provider_auth_file("merge")
}

pub fn load_auth_file() -> anyhow::Result<Option<StoredMergeAuth>> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let auth: StoredMergeAuth = serde_json::from_str(&raw)?;
    Ok(Some(auth))
}

pub fn save_auth_file(auth: &StoredMergeAuth) -> anyhow::Result<()> {
    if auth.access.trim().is_empty() {
        anyhow::bail!("merge auth token is required");
    }
    let path = auth_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    file.write_all(serde_json::to_string_pretty(auth)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn clear_auth_file() -> anyhow::Result<()> {
    let path = auth_file_path();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn missing_auth_message() -> String {
    [
        "Merge / Anthropic-compatible authentication was not found.",
        "Set CCP_MERGE_AUTH_TOKEN (or MERGE_AUTH_TOKEN), or write merge/auth.json with {\"access\":\"...\"}.",
        "Default upstream is Merge Gateway; any Anthropic Messages-compatible base URL works via CCP_MERGE_BASE_URL.",
    ]
    .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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

        fn clear(key: &'static str) -> Self {
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

    #[test]
    fn env_token_wins() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let _config = EnvGuard::set("CCP_CONFIG_DIR", dir.path());
        let _clear_merge = EnvGuard::clear("MERGE_AUTH_TOKEN");
        let _clear_key = EnvGuard::clear("CCP_MERGE_API_KEY");
        let _token = EnvGuard::set("CCP_MERGE_AUTH_TOKEN", "env-token");
        assert_eq!(load_merge_token().as_deref(), Some("env-token"));
    }
}
