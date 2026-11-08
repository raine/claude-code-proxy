use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

use super::login::{CANONICAL_ISSUER, CLIENT_ID};
use super::token_store::{GrokTokenStore, StoredAuth};
use crate::auth::AuthStorage;

const REFRESH_SKEW_MS: u64 = 5 * 60 * 1000;

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

pub struct GrokAuthManager<S: AuthStorage<StoredAuth>> {
    store: GrokTokenStore<S>,
    client: reqwest::Client,
    refresh_lock: Arc<Mutex<()>>,
}

impl<S: AuthStorage<StoredAuth>> GrokAuthManager<S> {
    pub fn new(store: GrokTokenStore<S>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            store,
            client,
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn store(&self) -> &GrokTokenStore<S> {
        &self.store
    }

    pub async fn get_auth(&self) -> anyhow::Result<StoredAuth> {
        let auth = self
            .store
            .load_auth()?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
        if auth.expires_at_ms > now_ms().saturating_add(REFRESH_SKEW_MS) {
            return Ok(auth);
        }
        self.refresh(false, None).await
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> anyhow::Result<StoredAuth> {
        self.refresh(true, Some(rejected_access)).await
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
    ) -> anyhow::Result<StoredAuth> {
        let _guard = self.refresh_lock.lock().await;

        let auth_path = self.store.auth_path();
        let _file_lock = crate::auth::acquire_lock_async(&auth_path).await.ok();

        let auth = self
            .store
            .load_auth()?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
        if (!force && auth.expires_at_ms > now_ms().saturating_add(REFRESH_SKEW_MS))
            || rejected_access.is_some_and(|access| auth.access != access)
        {
            return Ok(auth);
        }
        if auth.issuer != CANONICAL_ISSUER || auth.client_id != CLIENT_ID {
            anyhow::bail!("Unsupported Grok OAuth session");
        }
        let issuer = Url::parse(CANONICAL_ISSUER)?;
        let discovery_url = issuer.join("/.well-known/openid-configuration")?;
        let discovery: Discovery = self
            .client
            .get(discovery_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if discovery.issuer != CANONICAL_ISSUER {
            anyhow::bail!("OIDC discovery issuer mismatch");
        }
        let endpoint = Url::parse(&discovery.token_endpoint)?;
        if endpoint.scheme() != "https" || endpoint.origin() != issuer.origin() {
            anyhow::bail!("OIDC token endpoint is outside the canonical issuer");
        }
        let refreshed: RefreshResponse = self
            .client
            .post(endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", auth.refresh.as_str()),
                ("client_id", auth.client_id.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if refreshed.access_token.is_empty() || refreshed.expires_in == 0 {
            anyhow::bail!("Invalid token refresh response");
        }
        let updated = StoredAuth {
            access: refreshed.access_token,
            refresh: refreshed
                .refresh_token
                .filter(|token| !token.is_empty())
                .unwrap_or(auth.refresh),
            expires_at_ms: now_ms().saturating_add(refreshed.expires_in.saturating_mul(1000)),
            issuer: auth.issuer,
            client_id: auth.client_id,
        };
        self.store.save_auth(updated.clone())?;
        Ok(updated)
    }
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

    fn auth(access: &str) -> StoredAuth {
        StoredAuth {
            access: access.into(),
            refresh: "synthetic-refresh".into(),
            expires_at_ms: now_ms().saturating_add(3_600_000),
            issuer: CANONICAL_ISSUER.into(),
            client_id: "synthetic-client".into(),
        }
    }

    #[test]
    fn discovery_accepts_standard_metadata_fields() {
        let discovery: Discovery = serde_json::from_value(serde_json::json!({
            "issuer": CANONICAL_ISSUER,
            "token_endpoint": "https://auth.x.ai/oauth/token",
            "authorization_endpoint": "https://auth.x.ai/oauth/authorize",
            "jwks_uri": "https://auth.x.ai/.well-known/jwks.json"
        }))
        .unwrap();

        assert_eq!(discovery.issuer, CANONICAL_ISSUER);
    }

    #[tokio::test]
    async fn concurrent_stale_401_refreshes_reuse_the_rotated_access_token() {
        let store = GrokTokenStore::new(InMemoryAuthStore::new());
        store.save_auth(auth("rotated-access")).unwrap();
        let manager = Arc::new(GrokAuthManager::new(store).unwrap());
        let (first, second) = tokio::join!(
            manager.force_refresh("rejected-access"),
            manager.force_refresh("rejected-access"),
        );
        assert_eq!(first.unwrap().access, "rotated-access");
        assert_eq!(second.unwrap().access, "rotated-access");
    }
}
