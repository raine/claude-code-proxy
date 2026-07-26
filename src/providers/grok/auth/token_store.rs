use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::auth::{AuthStorage, KeychainFileAuthStore, SystemKeychain};
use crate::oauth_rotation::{AuthMutationLock, clear_refresh_pending};
use crate::paths;

pub const KEYCHAIN_SERVICE: &str = "claude-code-proxy.grok";
pub const KEYCHAIN_ACCOUNT: &str = "auth";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
    pub issuer: String,
    pub client_id: String,
}

pub struct GrokTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> GrokTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
    pub fn load_auth(&self) -> anyhow::Result<Option<StoredAuth>> {
        self.store.load()
    }
    pub fn save_auth(&self, auth: StoredAuth) -> anyhow::Result<()> {
        self.store.save(auth)
    }
    pub fn save_auth_exclusive(&self, auth: StoredAuth) -> anyhow::Result<()> {
        let coordination_path = self.coordination_path();
        let _lock = AuthMutationLock::acquire(coordination_path.as_deref())?;
        self.save_auth(auth)?;
        clear_refresh_pending(coordination_path.as_deref())
    }
    pub fn clear_auth(&self) -> anyhow::Result<()> {
        self.store.clear()
    }
    pub fn clear_auth_exclusive(&self) -> anyhow::Result<()> {
        let coordination_path = self.coordination_path();
        let _lock = AuthMutationLock::acquire(coordination_path.as_deref())?;
        self.clear_auth()?;
        clear_refresh_pending(coordination_path.as_deref())
    }
    pub fn auth_path(&self) -> String {
        self.store.path()
    }

    pub fn coordination_path(&self) -> Option<PathBuf> {
        self.store.coordination_path()
    }
}

pub type DefaultGrokAuthStore = KeychainFileAuthStore<StoredAuth, SystemKeychain>;

impl GrokTokenStore<DefaultGrokAuthStore> {
    fn migrate_auth_exclusive(&self) -> anyhow::Result<bool> {
        let coordination_path = self.coordination_path();
        let _lock = AuthMutationLock::acquire(coordination_path.as_deref())?;
        self.store.migrate_file_to_keychain()
    }
}

pub fn file_store() -> GrokTokenStore<DefaultGrokAuthStore> {
    let primary = paths::provider_auth_file("grok");
    let legacy = paths::provider_legacy_auth_file("grok");
    let store = KeychainFileAuthStore::new(
        primary.to_string_lossy().into_owned(),
        legacy.to_string_lossy().into_owned(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    );
    GrokTokenStore::new(store)
}

pub fn file_store_with_migration() -> GrokTokenStore<DefaultGrokAuthStore> {
    let store = file_store();
    if let Err(error) = store.migrate_auth_exclusive() {
        crate::logging::create_logger("grok").warn(
            "auth_keychain_migration_failed",
            Some(serde_json::Map::from_iter([(
                "message".to_string(),
                serde_json::json!(error.to_string()),
            )])),
        );
    }
    store
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}

#[cfg(test)]
pub fn test_file_store(primary: String, legacy: String) -> GrokTokenStore<DefaultGrokAuthStore> {
    GrokTokenStore::new(KeychainFileAuthStore::new(
        primary,
        legacy,
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        false,
        SystemKeychain,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{FileAuthStore, StubKeychain};
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::TempDir;

    fn auth(access: &str) -> StoredAuth {
        StoredAuth {
            access: access.to_string(),
            refresh: format!("{access}-refresh"),
            expires_at_ms: 1,
            issuer: "https://auth.x.ai".to_string(),
            client_id: "client".to_string(),
        }
    }

    #[test]
    fn explicit_auth_mutation_waits_for_the_refresh_file_lock() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("grok/auth.json");
        let path = path.to_string_lossy().into_owned();
        let store = GrokTokenStore::new(FileAuthStore::new(path.clone(), path.clone()));
        store.save_auth(auth("old")).unwrap();
        let held = AuthMutationLock::acquire(Some(std::path::Path::new(&path))).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let thread_path = path.clone();
        let writer = std::thread::spawn(move || {
            let store =
                GrokTokenStore::new(FileAuthStore::new(thread_path.clone(), thread_path.clone()));
            started_tx.send(()).unwrap();
            let result = store.save_auth_exclusive(auth("new"));
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        writer.join().unwrap();
        assert_eq!(store.load_auth().unwrap().unwrap().access, "new");
    }

    #[test]
    fn keychain_display_path_keeps_filesystem_coordination_path() {
        let temp = TempDir::new().unwrap();
        let primary = temp.path().join("grok/auth.json");
        let legacy = temp.path().join("legacy-grok/auth.json");
        let store = GrokTokenStore::new(KeychainFileAuthStore::new(
            primary.to_string_lossy().into_owned(),
            legacy.to_string_lossy().into_owned(),
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
            true,
            StubKeychain,
        ));

        assert_eq!(store.auth_path(), "macOS Keychain");
        assert_eq!(
            store.coordination_path().as_deref(),
            Some(primary.as_path())
        );
    }

    #[test]
    fn keychain_store_exclusive_save_waits_for_filesystem_coordination_lock() {
        let temp = TempDir::new().unwrap();
        let primary = temp.path().join("grok/auth.json");
        let legacy = temp.path().join("legacy-grok/auth.json");
        let held = AuthMutationLock::acquire(Some(primary.as_path())).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let thread_primary = primary.clone();
        let writer = std::thread::spawn(move || {
            let store = GrokTokenStore::new(KeychainFileAuthStore::new(
                thread_primary.to_string_lossy().into_owned(),
                legacy.to_string_lossy().into_owned(),
                KEYCHAIN_SERVICE,
                KEYCHAIN_ACCOUNT,
                true,
                StubKeychain,
            ));
            assert_eq!(store.auth_path(), "macOS Keychain");
            started_tx.send(()).unwrap();
            done_tx
                .send(store.save_auth_exclusive(auth("new")))
                .unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        writer.join().unwrap();

        let stored: StoredAuth =
            crate::auth::load_auth_file(primary.to_string_lossy().as_ref()).unwrap();
        assert_eq!(stored.access, "new");
    }
}
