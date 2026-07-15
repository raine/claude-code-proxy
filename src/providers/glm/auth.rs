use serde::{Deserialize, Serialize};

use crate::auth::{AuthStorage, KeychainFileAuthStore, SystemKeychain};
use crate::paths;

pub const KEYCHAIN_SERVICE: &str = "claude-code-proxy.glm";
pub const KEYCHAIN_ACCOUNT: &str = "auth";

/// Stored GLM (z.ai) credential. z.ai uses a single static API key (no OAuth,
/// no refresh tokens), so this is intentionally minimal compared with the
/// OAuth-backed providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StoredGlmAuth {
    pub api_key: String,
}

pub type DefaultGlmAuthStore = KeychainFileAuthStore<StoredGlmAuth, SystemKeychain>;

pub fn file_store() -> DefaultGlmAuthStore {
    let primary = paths::provider_auth_file("glm");
    let legacy = paths::provider_legacy_auth_file("glm");
    KeychainFileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    )
}

pub(crate) fn env_glm_api_key() -> Option<String> {
    env_glm_api_key_from(|key| std::env::var(key).ok())
}

fn env_glm_api_key_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    get("CCP_GLM_API_KEY")
        .filter(|k| !k.trim().is_empty())
        .or_else(|| get("GLM_API_KEY").filter(|k| !k.trim().is_empty()))
}

/// Load the GLM API key, preferring the environment over stored credentials.
pub fn load_glm_api_key() -> Option<String> {
    if let Some(key) = env_glm_api_key() {
        return Some(key);
    }
    let stored = file_store().load().ok().flatten()?;
    if stored.api_key.trim().is_empty() {
        return None;
    }
    Some(stored.api_key)
}

pub fn save_glm_api_key(api_key: String) -> anyhow::Result<()> {
    if api_key.trim().is_empty() {
        anyhow::bail!("GLM API key must not be empty");
    }
    file_store().save(StoredGlmAuth { api_key })
}

pub fn clear_glm_auth() -> anyhow::Result<()> {
    file_store().clear()
}

pub fn auth_location() -> String {
    file_store().path()
}

pub fn missing_auth_message() -> String {
    [
        "GLM (z.ai) API key not found.",
        "Run `claude-code-proxy glm auth login`, or set CCP_GLM_API_KEY / GLM_API_KEY.",
    ]
    .join(" ")
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_prefers_ccp_over_glm() {
        let key = env_glm_api_key_from(|k| match k {
            "CCP_GLM_API_KEY" => Some("ccp".into()),
            "GLM_API_KEY" => Some("glm".into()),
            _ => None,
        });
        assert_eq!(key.as_deref(), Some("ccp"));
    }

    #[test]
    fn env_returns_none_when_unset() {
        assert!(env_glm_api_key_from(|_| None).is_none());
    }

    #[test]
    fn env_ignores_blank_values_and_falls_through() {
        let key = env_glm_api_key_from(|k| match k {
            "CCP_GLM_API_KEY" => Some("   ".into()),
            "GLM_API_KEY" => Some("real".into()),
            _ => None,
        });
        assert_eq!(key.as_deref(), Some("real"));
    }

    #[test]
    fn stored_auth_round_trips_camel_case() {
        let auth = StoredGlmAuth {
            api_key: "sk-test".to_string(),
        };
        let value = serde_json::to_value(&auth).unwrap();
        assert_eq!(value["apiKey"], "sk-test");
        assert!(value.get("api_key").is_none());
        let back: StoredGlmAuth = serde_json::from_value(value).unwrap();
        assert_eq!(back, auth);
    }
}
