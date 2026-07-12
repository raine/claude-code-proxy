use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::AuthStorage;

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: CodexTokenStore<S>,
    cached: Arc<Mutex<Option<StoredAuth>>>,
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self {
            store,
            cached: Arc::new(Mutex::new(None)),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = if self.store.reload_each_request() {
            let loaded = self.store.load_auth()?;
            match loaded {
                Some(auth) => {
                    let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                    if guard.as_ref() != Some(&auth) {
                        *guard = Some(auth.clone());
                    }
                    auth
                }
                None => {
                    self.reset_cache();
                    anyhow::bail!("Not authenticated. Run: claude-code-proxy codex auth login");
                }
            }
        } else {
            let cached = {
                let guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                guard.clone()
            };
            match cached {
                Some(ref auth) => auth.clone(),
                None => {
                    let loaded = self.store.load_auth()?;
                    match loaded {
                        Some(auth) => {
                            let mut guard =
                                self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                            *guard = Some(auth.clone());
                            auth
                        }
                        None => {
                            anyhow::bail!(
                                "Not authenticated. Run: claude-code-proxy codex auth login"
                            );
                        }
                    }
                }
            }
        };

        if stored.expires > Self::now_ms() + REFRESH_MARGIN_MS {
            return Ok(stored);
        }

        self.refresh_now(&stored)
    }

    pub fn force_refresh(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = {
            let guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            guard.clone()
        };
        let stored = match stored {
            Some(auth) => auth,
            None => {
                let loaded = self.store.load_auth()?;
                loaded.ok_or_else(|| anyhow::anyhow!("Not authenticated"))?
            }
        };
        self.refresh_now(&stored)
    }

    fn refresh_now(&self, current: &StoredAuth) -> Result<StoredAuth, anyhow::Error> {
        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; re-authenticate");
        }

        let client = reqwest::blocking::Client::new();
        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        let resp = client
            .post(format!("{ISSUER}/oauth/token"))
            .form(&form)
            .send()
            .map_err(|e| anyhow::anyhow!("refresh network error: {e}"))?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            {
                let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                *guard = None;
            }
            let _ = self.store.clear_auth();
            let err_msg = resp
                .text()
                .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
            anyhow::bail!("{err_msg}");
        }

        if !resp.status().is_success() {
            anyhow::bail!("Token refresh failed: {status}");
        }

        let tokens: TokenResponse = resp
            .json()
            .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
        validate_token_response(&tokens)?;
        let account_id = extract_account_id(&tokens).or_else(|| current.account_id.clone());
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let next = StoredAuth {
            access: tokens.access_token,
            refresh: tokens.refresh_token,
            expires,
            account_id,
        };
        self.store.save_auth(next.clone())?;
        {
            let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            *guard = Some(next.clone());
        }
        Ok(next)
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        validate_token_response(tokens)?;
        let account_id = extract_account_id(tokens);
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.refresh_token.clone(),
            expires,
            account_id,
        };
        self.store.save_auth(auth.clone())?;
        {
            let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            *guard = Some(auth.clone());
        }
        Ok(auth)
    }

    pub fn set_cached(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some(auth);
        }
    }

    pub fn reset_cache(&self) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;

    fn test_store() -> CodexTokenStore<InMemoryAuthStore<StoredAuth>> {
        CodexTokenStore::new(InMemoryAuthStore::new())
    }

    #[test]
    fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        let result = manager.get_auth().unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.account_id.as_deref(), Some("acct_1"));
    }

    #[test]
    fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = CodexAuthManager::new(store);
        assert!(manager.get_auth().is_err());
        assert!(
            manager
                .get_auth()
                .unwrap_err()
                .to_string()
                .contains("Not authenticated")
        );
    }

    #[test]
    fn reloading_store_observes_profile_swap_on_next_request() {
        let store = CodexTokenStore::new_reloading(InMemoryAuthStore::new());
        store
            .save_auth(StoredAuth {
                access: "first_access".into(),
                refresh: String::new(),
                expires: 9999999999999,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);

        assert_eq!(manager.get_auth().unwrap().access, "first_access");
        manager
            .store
            .save_auth(StoredAuth {
                access: "second_access".into(),
                refresh: String::new(),
                expires: 9999999999999,
                account_id: Some("acct_2".into()),
            })
            .unwrap();

        let swapped = manager.get_auth().unwrap();
        assert_eq!(swapped.access, "second_access");
        assert_eq!(swapped.account_id.as_deref(), Some("acct_2"));
    }

    #[test]
    fn normal_store_keeps_cached_auth() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "first_access".into(),
                refresh: "first_refresh".into(),
                expires: 9999999999999,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);

        assert_eq!(manager.get_auth().unwrap().access, "first_access");
        manager
            .store
            .save_auth(StoredAuth {
                access: "second_access".into(),
                refresh: "second_refresh".into(),
                expires: 9999999999999,
                account_id: Some("acct_2".into()),
            })
            .unwrap();

        assert_eq!(manager.get_auth().unwrap().access, "first_access");
    }
}
