use serde::{Deserialize, Serialize};

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
}

impl<S: AuthStorage<StoredAuth>> CodexTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        self.store.load()
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
}

pub struct DefaultCodexAuthStore {
    inner: Result<KeychainFileAuthStore<StoredAuth, SystemKeychain>, String>,
}

impl AuthStorage<StoredAuth> for DefaultCodexAuthStore {
    fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
        self.inner.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?.load()
    }

    fn save(&self, value: StoredAuth) -> anyhow::Result<()> {
        self.inner.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?.save(value)
    }

    fn clear(&self) -> anyhow::Result<()> {
        self.inner.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?.clear()
    }

    fn path(&self) -> String {
        self.inner.as_ref().map(|store| store.path()).unwrap_or_else(|_| "Invalid Codex profile".into())
    }
}

pub fn file_store() -> CodexTokenStore<DefaultCodexAuthStore> {
    let inner = crate::account_profiles::active_profile("codex")
        .and_then(|name| profile_storage(&crate::account_profiles::ProfileStore::configured(), &name))
        .map_err(|error| error.to_string());
    CodexTokenStore::new(DefaultCodexAuthStore { inner })
}

fn profile_storage(
    profiles: &crate::account_profiles::ProfileStore,
    name: &str,
) -> anyhow::Result<KeychainFileAuthStore<StoredAuth, SystemKeychain>> {
    crate::account_profiles::validate_name(name)?;
    if name != crate::account_profiles::DEFAULT_PROFILE {
        let path = profiles.credential_path("codex", name)?.to_string_lossy().into_owned();
        // A named profile must never read, refresh, or clear default/legacy credentials.
        return Ok(KeychainFileAuthStore::new(
            path.clone(), path, KEYCHAIN_SERVICE, format!("profile:{name}"), false, SystemKeychain,
        ));
    }
    let primary = paths::provider_auth_file("codex");
    let legacy = paths::provider_legacy_auth_file("codex");
    Ok(KeychainFileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    ))
}

/// Read-only local status lookup; never refreshes or changes credentials.
pub(crate) fn auth_for_profile(
    profiles: &crate::account_profiles::ProfileStore,
    name: &str,
) -> anyhow::Result<Option<StoredAuth>> {
    profile_storage(profiles, name)?.load()
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use serde_json::json;

    #[test]
    fn named_stores_isolate_load_refresh_save_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = crate::account_profiles::ProfileStore::new(dir.path().to_owned());
        let work = profile_storage(&profiles, "work").unwrap();
        let personal = profile_storage(&profiles, "personal").unwrap();
        let auth = StoredAuth {
            access: "work-access".into(), refresh: "work-refresh".into(),
            expires: 4102444800000, account_id: Some("work-account".into()),
        };
        work.save(auth.clone()).unwrap();
        assert!(personal.load().unwrap().is_none());
        let personal_auth = StoredAuth {
            access: "personal-access".into(), refresh: "personal-refresh".into(),
            account_id: Some("personal-account".into()), ..auth.clone()
        };
        personal.save(personal_auth.clone()).unwrap();
        let rotated = StoredAuth { access: "rotated".into(), refresh: "rotated-refresh".into(), ..auth };
        work.save(rotated.clone()).unwrap();
        assert_eq!(work.load().unwrap(), Some(rotated));
        assert_eq!(personal.load().unwrap(), Some(personal_auth.clone()));
        work.clear().unwrap();
        assert!(work.load().unwrap().is_none());
        assert_eq!(personal.load().unwrap(), Some(personal_auth));
        assert!(profile_storage(&profiles, "../default").is_err());
    }

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
}
