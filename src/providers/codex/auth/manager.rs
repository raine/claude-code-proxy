use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::AuthStorage;

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: CodexTokenStore<S>,
    cached: Arc<Mutex<Option<StoredAuth>>>,
    // Serializes token refreshes (single-flight). The `cached` mutex only
    // guards cache reads/writes; without this lock, N concurrent requests
    // hitting the expiry margin each POST /oauth/token with the SAME
    // (single-use, rotating) refresh token — the first rotates it, the rest
    // get 401 and used to clear_auth(), destroying the winner's fresh tokens.
    // Observed in production as minutes-long all-requests-401 windows during
    // agent fan-outs at token-expiry boundaries.
    refresh_flight: Arc<Mutex<()>>,
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self {
            store,
            cached: Arc::new(Mutex::new(None)),
            refresh_flight: Arc::new(Mutex::new(())),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let cached = {
            let guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            guard.clone()
        };
        let stored = match cached {
            Some(ref auth) => auth.clone(),
            None => {
                let loaded = self.store.load_auth()?;
                match loaded {
                    Some(auth) => {
                        let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                        *guard = Some(auth.clone());
                        auth
                    }
                    None => {
                        anyhow::bail!("Not authenticated. Run: claude-code-proxy codex auth login");
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
        // Single-flight: hold the flight lock for the whole refresh. Racing
        // callers block here, then discover the winner's tokens on re-check
        // below and return without touching the token endpoint.
        let _flight = self
            .refresh_flight
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Re-check under the lock: a concurrent flight (or another process
        // sharing the store) may have refreshed while we waited. The store is
        // the persisted truth; prefer it over both `current` and the cache.
        let current = match self.store.load_auth()? {
            Some(latest) => {
                if latest.expires > Self::now_ms() + REFRESH_MARGIN_MS {
                    let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                    *guard = Some(latest.clone());
                    return Ok(latest);
                }
                latest
            }
            None => current.clone(),
        };

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
            // Before destroying auth state: if the store's refresh token has
            // rotated since we read `current`, a concurrent writer (e.g.
            // another process sharing the Keychain entry) beat us — its
            // tokens are good; return them instead of clobbering the store.
            if let Ok(Some(latest)) = self.store.load_auth() {
                if latest.refresh != current.refresh && latest.expires > Self::now_ms() {
                    let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                    *guard = Some(latest.clone());
                    return Ok(latest);
                }
            }
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
    fn refresh_recheck_returns_concurrently_rotated_tokens_without_network() {
        // Cache holds an EXPIRED token; the store already holds a FRESH one
        // (as after a concurrent flight or another process refreshed). The
        // re-check under the flight lock must return the store's tokens and
        // never reach the token endpoint (no HTTP mock exists here — reaching
        // the network would fail the test with a refresh error).
        let store = test_store();
        let fresh = StoredAuth {
            access: "rotated_access".into(),
            refresh: "rotated_refresh".into(),
            expires: 9_999_999_999_999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(fresh.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        manager.set_cached(StoredAuth {
            access: "stale_access".into(),
            refresh: "stale_refresh".into(),
            expires: 0, // expired -> get_auth enters refresh_now
            account_id: Some("acct_1".into()),
        });
        let result = manager.get_auth().unwrap();
        assert_eq!(result.access, "rotated_access");
        assert_eq!(result.refresh, "rotated_refresh");
    }

    #[test]
    fn concurrent_get_auth_single_flights_to_rotated_tokens() {
        use std::sync::Arc as StdArc;
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "rotated_access".into(),
                refresh: "rotated_refresh".into(),
                expires: 9_999_999_999_999,
                account_id: None,
            })
            .unwrap();
        let manager = StdArc::new(CodexAuthManager::new(store));
        manager.set_cached(StoredAuth {
            access: "stale".into(),
            refresh: "stale".into(),
            expires: 0,
            account_id: None,
        });
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = StdArc::clone(&manager);
            handles.push(std::thread::spawn(move || m.get_auth().unwrap().access));
        }
        for h in handles {
            assert_eq!(h.join().unwrap(), "rotated_access");
        }
    }
}
