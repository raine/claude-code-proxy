use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{AuthStorage, KeychainFileAuthStore, SystemKeychain};
use crate::paths;

pub const KEYCHAIN_SERVICE: &str = "claude-code-proxy.codex";
pub const KEYCHAIN_ACCOUNT: &str = "auth";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(
        default,
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

pub struct CodexTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
    codex_cli_fallback: bool,
    reload_each_request: bool,
}

impl<S: AuthStorage<StoredAuth>> CodexTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            codex_cli_fallback: false,
            reload_each_request: false,
        }
    }

    pub fn new_with_codex_cli_fallback(store: S) -> Self {
        Self {
            store,
            codex_cli_fallback: true,
            reload_each_request: false,
        }
    }

    pub fn new_reloading(store: S) -> Self {
        Self {
            store,
            codex_cli_fallback: false,
            reload_each_request: true,
        }
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        match self.store.load() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => {
                if self.codex_cli_fallback {
                    load_codex_cli_auth()
                } else {
                    Ok(None)
                }
            }
            Err(err) => {
                if self.codex_cli_fallback
                    && let Some(auth) = load_codex_cli_auth()?
                {
                    return Ok(Some(auth));
                }
                Err(err)
            }
        }
    }

    pub fn save_auth(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        self.store.save(value)
    }

    pub fn clear_auth(&self) -> Result<(), anyhow::Error> {
        self.store.clear()
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }

    pub fn reload_each_request(&self) -> bool {
        self.reload_each_request
    }
}

pub type DefaultCodexAuthStore = KeychainFileAuthStore<StoredAuth, SystemKeychain>;

pub fn file_store() -> CodexTokenStore<DefaultCodexAuthStore> {
    let primary = paths::provider_auth_file("codex");
    let legacy = paths::provider_legacy_auth_file("codex");
    let store = KeychainFileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    );
    if std::env::var_os("CCP_CONFIG_DIR").is_none() {
        CodexTokenStore::new_with_codex_cli_fallback(store)
    } else {
        CodexTokenStore::new_reloading(store)
    }
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}

#[derive(Debug, Deserialize)]
struct CodexCliAuthFile {
    tokens: Option<CodexCliTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexCliTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

fn load_codex_cli_auth() -> Result<Option<StoredAuth>, anyhow::Error> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/".to_string());
    load_codex_cli_auth_from_path(&Path::new(&home).join(".codex").join("auth.json"))
}

fn load_codex_cli_auth_from_path(path: &Path) -> Result<Option<StoredAuth>, anyhow::Error> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(anyhow::Error::from(err)),
    };
    let parsed: CodexCliAuthFile = serde_json::from_str(&raw)?;
    let Some(tokens) = parsed.tokens else {
        return Ok(None);
    };
    let (Some(access), Some(refresh)) = (tokens.access_token, tokens.refresh_token) else {
        return Ok(None);
    };
    Ok(Some(StoredAuth {
        expires: jwt_exp_ms(&access).unwrap_or_else(|| now_ms() + 3600 * 1000),
        access,
        refresh,
        account_id: tokens.account_id,
    }))
}

fn jwt_exp_ms(token: &str) -> Option<u64> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
            base64::engine::general_purpose::URL_SAFE.decode(padded)
        })
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp")?.as_u64().map(|exp| exp * 1000)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn stored_auth_reads_account_id_alias() {
        let auth: StoredAuth = serde_json::from_value(json!({
            "access": "a",
            "refresh": "r",
            "expires": 123,
            "accountId": "acct"
        }))
        .unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("acct"));
    }

    #[test]
    fn stored_auth_writes_account_id_key() {
        let auth = StoredAuth {
            access: "a".into(),
            refresh: "r".into(),
            expires: 4102444800000,
            account_id: Some("acct_1".into()),
        };
        let value = serde_json::to_value(auth).unwrap();
        assert_eq!(value["accountId"], "acct_1");
        assert!(value.get("account_id").is_none());
    }

    #[test]
    fn stored_auth_roundtrip() {
        let store = CodexTokenStore::new(InMemoryAuthStore::new());
        let auth = StoredAuth {
            access: "token".into(),
            refresh: "refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let loaded = store.load_auth().unwrap().unwrap();
        assert_eq!(loaded.access, "token");
        assert_eq!(loaded.account_id.as_deref(), Some("acct_1"));
    }

    fn test_jwt_with_exp(exp: u64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn codex_cli_auth_file_maps_tokens_and_expiry() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "tokens": {
                    "access_token": test_jwt_with_exp(1234),
                    "refresh_token": "refresh",
                    "account_id": "acct_cli"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let auth = load_codex_cli_auth_from_path(&path).unwrap().unwrap();
        assert_eq!(auth.refresh, "refresh");
        assert_eq!(auth.account_id.as_deref(), Some("acct_cli"));
        assert_eq!(auth.expires, 1_234_000);
    }

    #[test]
    fn codex_cli_auth_file_returns_none_without_tokens() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        std::fs::write(&path, br#"{"tokens":{}}"#).unwrap();
        assert!(load_codex_cli_auth_from_path(&path).unwrap().is_none());
    }
}
