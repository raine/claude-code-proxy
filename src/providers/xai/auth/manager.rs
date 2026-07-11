use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::constants::{CLIENT_ID, REFRESH_MARGIN_MS, TIER_DENIED_HINT, issuer};
use super::jwt::{TokenResponse, expires_at_ms, refresh_token_from, validate_token_response};
use super::token_store::{StoredAuth, XaiTokenStore};
use crate::auth::AuthStorage;

pub struct XaiAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: XaiTokenStore<S>,
    cached: Arc<Mutex<Option<StoredAuth>>>,
}

impl<S: AuthStorage<StoredAuth>> XaiAuthManager<S> {
    pub fn new(store: XaiTokenStore<S>) -> Self {
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
                        anyhow::bail!(
                            "Not authenticated. Run: claude-code-proxy xai auth login \
                             (or `xai auth device` on headless hosts)"
                        );
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
            anyhow::bail!("No refresh token stored; re-authenticate with `xai auth login`");
        }

        let client = reqwest::blocking::Client::new();
        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        let resp = client
            .post(format!("{}/oauth2/token", issuer()))
            .form(&form)
            .send()
            .map_err(|e| anyhow::anyhow!("refresh network error: {e}"))?;

        let status = resp.status().as_u16();
        let body_text = resp.text().unwrap_or_default();

        // Tier / entitlement gate: keep tokens. Re-login will not help (Hermes #26847).
        if status == 403 {
            anyhow::bail!("{TIER_DENIED_HINT}");
        }

        // Revoked / invalid grant: clear store so status shows Not authenticated.
        if status == 400 || status == 401 {
            {
                let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                *guard = None;
            }
            let _ = self.store.clear_auth();
            let detail = if body_text.trim().is_empty() {
                "invalid or revoked refresh token".to_string()
            } else {
                body_text
            };
            anyhow::bail!(
                "xAI token refresh failed ({status}): {detail}. \
                 Run `claude-code-proxy xai auth login` (or `xai auth device`)."
            );
        }

        if !(200..300).contains(&status) {
            anyhow::bail!("Token refresh failed: {status} {body_text}");
        }

        let tokens: TokenResponse = serde_json::from_str(&body_text)
            .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
        validate_token_response(&tokens)?;
        if tokens
            .refresh_token
            .as_ref()
            .map(|r| r.trim().is_empty())
            .unwrap_or(true)
            && current.refresh.is_empty()
        {
            anyhow::bail!("token response missing refresh token");
        }

        let next = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: refresh_token_from(&tokens, &current.refresh),
            expires: expires_at_ms(&tokens, Self::now_ms()),
            scope: tokens.scope.clone().or_else(|| current.scope.clone()),
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
        let refresh = tokens
            .refresh_token
            .clone()
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("token response missing refresh token"))?;
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh,
            expires: expires_at_ms(tokens, Self::now_ms()),
            scope: tokens.scope.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;

    fn test_store() -> XaiTokenStore<InMemoryAuthStore<StoredAuth>> {
        XaiTokenStore::new(InMemoryAuthStore::new())
    }

    #[test]
    fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            scope: Some("openid".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = XaiAuthManager::new(store);
        let result = manager.get_auth().unwrap();
        assert_eq!(result.access, "test_access");
    }

    #[test]
    fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = XaiAuthManager::new(store);
        assert!(manager.get_auth().is_err());
    }
}
