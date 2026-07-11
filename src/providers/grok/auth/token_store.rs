use serde::{Deserialize, Serialize};

use crate::auth::{AuthStorage, FileAuthStore};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

pub struct GrokTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> GrokTokenStore<S> {
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

pub fn file_store() -> GrokTokenStore<FileAuthStore<StoredAuth>> {
    // Preferred path is `grok/`; still read old `xai/` auth if present.
    let primary = paths::provider_auth_file("grok");
    let legacy = paths::provider_auth_file("xai");
    let store = FileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
    );
    GrokTokenStore::new(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;

    #[test]
    fn roundtrip() {
        let store = GrokTokenStore::new(InMemoryAuthStore::new());
        let auth = StoredAuth {
            access: "a".into(),
            refresh: "r".into(),
            expires: 9999999999999,
            scope: Some("openid".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let loaded = store.load_auth().unwrap().unwrap();
        assert_eq!(loaded, auth);
    }
}
