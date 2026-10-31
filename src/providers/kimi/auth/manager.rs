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

enum RefreshError {
    Unauthorized(String),
    Other(anyhow::Error),
}

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
        match self.refresh_with(current) {
            Ok(next) => Ok(next),
            Err(RefreshError::Other(err)) => Err(err),
            Err(RefreshError::Unauthorized(msg)) => {
                // The refresh token rotates on every refresh. When a peer
                // process sharing this credential (for example the Kimi CLI)
                // refreshes first, our copy is stale and the server rejects
                // it, but the file on disk already holds the newer token.
                // Reload once and retry with the peer credential before
                // declaring the login dead.
                let reloaded = self.store.load_auth()?;
                match reloaded {
                    Some(peer) if !peer.refresh.is_empty() && peer.refresh != current.refresh => {
                        match self.refresh_with(&peer) {
                            Ok(next) => Ok(next),
                            Err(RefreshError::Unauthorized(peer_msg)) => {
                                self.discard_auth();
                                anyhow::bail!(peer_msg)
                            }
                            Err(RefreshError::Other(err)) => Err(err),
                        }
                    }
                    _ => {
                        self.discard_auth();
                        anyhow::bail!(msg)
                    }
                }
            }
        }
    }

    fn discard_auth(&self) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
        let _ = self.store.clear_auth();
    }

    fn refresh_with(&self, current: &StoredAuth) -> Result<StoredAuth, RefreshError> {
        if current.refresh.is_empty() {
            return Err(RefreshError::Other(anyhow::anyhow!(
                "No refresh token stored; re-authenticate"
            )));
        }

        let headers = common_headers().map_err(RefreshError::Other)?;
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
                    return Err(RefreshError::Other(anyhow::anyhow!(
                        "refresh network error: {err}"
                    )));
                }
            };

            if resp.status().as_u16() == 200 {
                let tokens: TokenResponse =
                    resp.json().map_err(|e| RefreshError::Other(e.into()))?;
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
                self.store
                    .save_auth(next.clone())
                    .map_err(RefreshError::Other)?;
                {
                    let mut guard = self
                        .cached
                        .lock()
                        .map_err(|e| RefreshError::Other(anyhow::anyhow!("{e}")))?;
                    *guard = Some(next.clone());
                }
                return Ok(next);
            }

            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                let err_msg = resp
                    .text()
                    .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
                return Err(RefreshError::Unauthorized(err_msg));
            }

            if !RETRYABLE_STATUSES.contains(&status) {
                return Err(RefreshError::Other(anyhow::anyhow!(
                    "Token refresh failed: {status}"
                )));
            }

            if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                let ms = 2u64.pow(attempt) * 1000;
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }

        Err(RefreshError::Other(anyhow::anyhow!(
            "Token refresh failed after {MAX_REFRESH_ATTEMPTS} attempts"
        )))
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

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
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

    fn stale_auth() -> StoredAuth {
        StoredAuth {
            access: "stale_access".into(),
            refresh: "stale_refresh".into(),
            expires: 1,
            scope: None,
            user_id: None,
        }
    }

    #[test]
    fn refresh_adopts_peer_rotated_credential_on_unauthorized() {
        use crate::providers::codex::auth::test_http;

        let _lock = ENV_LOCK.lock().unwrap();
        let server = test_http::spawn_mock_server("kimi oauth mock ready", |request| {
            if request.contains("refresh_token=stale_refresh") {
                test_http::json_response(401, r#"{"error":"invalid_grant"}"#)
            } else if request.contains("refresh_token=peer_refresh") {
                test_http::json_response(
                    200,
                    r#"{"access_token":"peer_access","refresh_token":"rotated_refresh","expires_in":900}"#,
                )
            } else {
                test_http::json_response(400, r#"{"error":"unexpected_request"}"#)
            }
        });
        let _guard = EnvGuard::set("CCP_KIMI_OAUTH_HOST", &server.url);

        let store = test_store();
        let stale = stale_auth();
        store.save_auth(stale.clone()).unwrap();
        let manager = KimiAuthManager::new(store);

        manager
            .store
            .save_auth(StoredAuth {
                access: "peer_stale_access".into(),
                refresh: "peer_refresh".into(),
                expires: 1,
                scope: None,
                user_id: None,
            })
            .unwrap();

        let result = manager.refresh_now(&stale).unwrap();
        assert_eq!(result.access, "peer_access");
        assert_eq!(result.refresh, "rotated_refresh");

        let persisted = manager.store.load_auth().unwrap().unwrap();
        assert_eq!(persisted.refresh, "rotated_refresh");
    }

    #[test]
    fn refresh_clears_auth_only_when_disk_has_no_newer_credential() {
        use crate::providers::codex::auth::test_http;

        let _lock = ENV_LOCK.lock().unwrap();
        let server = test_http::spawn_mock_server("kimi oauth mock ready", |request| {
            if request.contains("refresh_token=stale_refresh") {
                test_http::json_response(401, r#"{"error":"invalid_grant"}"#)
            } else {
                test_http::json_response(400, r#"{"error":"unexpected_request"}"#)
            }
        });
        let _guard = EnvGuard::set("CCP_KIMI_OAUTH_HOST", &server.url);

        let store = test_store();
        let stale = stale_auth();
        store.save_auth(stale.clone()).unwrap();
        let manager = KimiAuthManager::new(store);

        let err = manager.refresh_now(&stale).unwrap_err();
        assert!(err.to_string().contains("invalid_grant"));
        assert!(manager.store.load_auth().unwrap().is_none());
    }
}
