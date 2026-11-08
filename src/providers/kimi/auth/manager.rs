use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::constants::{CLIENT_ID, REFRESH_MARGIN_MS, oauth_host};
use super::headers::common_headers;
use super::jwt::extract_user_id;
use super::login::TokenResponse;
use super::token_store::{KimiTokenStore, StoredAuth};
use crate::auth::AuthStorage;

const MAX_REFRESH_ATTEMPTS: u32 = 3;
const RETRYABLE_STATUSES: &[u16] = &[429, 500, 502, 503, 504];

pub struct KimiAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: KimiTokenStore<S>,
    cached: Arc<Mutex<Option<StoredAuth>>>,
}

impl<S: AuthStorage<StoredAuth>> KimiAuthManager<S> {
    pub fn new(store: KimiTokenStore<S>) -> Self {
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
                        anyhow::bail!("Not authenticated. Run: claude-code-proxy kimi auth login");
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
        let auth_path = self.store.auth_path();
        let _lock = crate::auth::acquire_lock(&auth_path).ok();

        // Re-read inside critical section to avoid race with concurrent process
        if let Ok(Some(latest)) = self.store.load_auth()
            && (latest.expires > Self::now_ms() + REFRESH_MARGIN_MS || latest.access != current.access)
        {
            let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            *guard = Some(latest.clone());
            return Ok(latest);
        }

        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; re-authenticate");
        }

        let headers = common_headers()?;
        let client = reqwest::blocking::Client::new();

        for attempt in 0..MAX_REFRESH_ATTEMPTS {
            let form = [
                ("client_id", CLIENT_ID.to_string()),
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", current.refresh.clone()),
            ];

            let resp = match client
                .post(format!("{}/api/oauth/token", oauth_host()))
                .headers(build_headers_map(&headers))
                .form(&form)
                .send()
            {
                Ok(r) => r,
                Err(err) => {
                    if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                        let ms = 2u64.pow(attempt) * 1000;
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        continue;
                    }
                    anyhow::bail!("refresh network error: {err}");
                }
            };

            if resp.status().as_u16() == 200 {
                let tokens: TokenResponse = resp.json()?;
                let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(900) as u64 * 1000);
                let next = StoredAuth {
                    access: tokens.access_token.clone(),
                    refresh: tokens
                        .refresh_token
                        .unwrap_or_else(|| current.refresh.clone()),
                    expires,
                    scope: tokens.scope.clone(),
                    user_id: extract_user_id(&tokens.access_token)
                        .or_else(|| current.user_id.clone()),
                };
                self.store.save_auth(next.clone())?;
                {
                    let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                    *guard = Some(next.clone());
                }
                return Ok(next);
            }

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

            if !RETRYABLE_STATUSES.contains(&status) {
                anyhow::bail!("Token refresh failed: {status}");
            }

            if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                let ms = 2u64.pow(attempt) * 1000;
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }

        anyhow::bail!("Token refresh failed after {MAX_REFRESH_ATTEMPTS} attempts");
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(900) * 1000);
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.refresh_token.clone().unwrap_or_default(),
            expires,
            scope: tokens.scope.clone(),
            user_id: extract_user_id(&tokens.access_token),
        };
        self.store.save_auth(auth.clone())?;
        {
            let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            *guard = Some(auth.clone());
        }
        Ok(auth)
    }

    pub fn reset_cache(&self) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
    }
}

fn build_headers_map(
    headers: &std::collections::HashMap<String, String>,
) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
        {
            map.insert(name, value);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;

    fn test_store() -> KimiTokenStore<InMemoryAuthStore<StoredAuth>> {
        KimiTokenStore::new(InMemoryAuthStore::new())
    }

    #[test]
    fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            scope: Some("openid".into()),
            user_id: Some("user1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = KimiAuthManager::new(store);
        let result = manager.get_auth().unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.user_id.as_deref(), Some("user1"));
    }

    #[test]
    fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = KimiAuthManager::new(store);
        assert!(manager.get_auth().is_err());
    }
}
