use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

use super::login::{CANONICAL_ISSUER, CLIENT_ID};
use super::token_store::{GrokTokenStore, StoredAuth};
use crate::auth::AuthStorage;
use crate::oauth_http::{MAX_OAUTH_ERROR_BYTES, MAX_OAUTH_JSON_BYTES, read_json_async};
#[cfg(test)]
use crate::oauth_rotation::refresh_pending_path;
use crate::oauth_rotation::{
    AuthMutationLock, clear_refresh_pending, generation_fingerprint, read_refresh_pending,
    write_refresh_pending,
};

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

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokAuthErrorKind {
    CredentialsInvalid,
    Temporary,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrokAuthError {
    #[error("Grok credentials are invalid: {message}")]
    CredentialsInvalid { message: String },
    #[error("Grok authentication is temporarily unavailable: {message}")]
    Temporary {
        message: String,
        safe_to_retry: bool,
    },
    #[error("Grok authentication is rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<String>,
        safe_to_retry: bool,
    },
    #[error(
        "Grok authentication is temporarily unavailable: the previous token refresh outcome is unknown; re-authenticate before retrying"
    )]
    RefreshOutcomeUnknown,
}

impl GrokAuthError {
    pub fn credentials_invalid(message: impl Into<String>) -> Self {
        Self::CredentialsInvalid {
            message: message.into(),
        }
    }

    fn temporary(message: impl Into<String>) -> Self {
        Self::Temporary {
            message: message.into(),
            safe_to_retry: false,
        }
    }

    fn retryable_temporary(message: impl Into<String>) -> Self {
        Self::Temporary {
            message: message.into(),
            safe_to_retry: true,
        }
    }

    fn rate_limited(message: impl Into<String>, retry_after: Option<String>) -> Self {
        Self::RateLimited {
            message: message.into(),
            retry_after,
            safe_to_retry: true,
        }
    }

    pub fn kind(&self) -> GrokAuthErrorKind {
        match self {
            Self::CredentialsInvalid { .. } => GrokAuthErrorKind::CredentialsInvalid,
            Self::Temporary { .. } | Self::RefreshOutcomeUnknown => GrokAuthErrorKind::Temporary,
            Self::RateLimited { .. } => GrokAuthErrorKind::RateLimited,
        }
    }

    pub fn retry_after(&self) -> Option<&str> {
        match self {
            Self::RateLimited { retry_after, .. } => retry_after.as_deref(),
            _ => None,
        }
    }

    pub fn safe_to_retry(&self) -> bool {
        match self {
            Self::CredentialsInvalid { .. } | Self::RefreshOutcomeUnknown => false,
            Self::Temporary { safe_to_retry, .. } | Self::RateLimited { safe_to_retry, .. } => {
                *safe_to_retry
            }
        }
    }
}

pub struct GrokAuthManager<S: AuthStorage<StoredAuth>> {
    store: GrokTokenStore<S>,
    client: reqwest::Client,
    refresh_lock: Arc<Mutex<()>>,
    refresh_safety: Arc<Mutex<RefreshSafetyState>>,
    issuer: String,
    require_https: bool,
}

#[derive(Default)]
struct RefreshSafetyState {
    volatile_auth: Option<VolatileAuth>,
    ambiguous_generation: Option<[u8; 32]>,
}

#[derive(Clone)]
struct VolatileAuth {
    auth: StoredAuth,
    base_generation: [u8; 32],
}

impl<S: AuthStorage<StoredAuth>> GrokAuthManager<S> {
    pub fn new(store: GrokTokenStore<S>) -> anyhow::Result<Self> {
        Self::with_issuer(store, CANONICAL_ISSUER.into(), true)
    }

    fn with_issuer(
        store: GrokTokenStore<S>,
        issuer: String,
        require_https: bool,
    ) -> anyhow::Result<Self> {
        let issuer = issuer.trim_end_matches('/').to_string();
        let issuer_url = Url::parse(&issuer)?;
        if require_https && issuer_url.scheme() != "https" {
            anyhow::bail!("Grok OAuth issuer must use HTTPS");
        }
        let mut client_builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20));
        if crate::oauth_http::is_loopback_url(&issuer) {
            client_builder = client_builder.no_proxy();
        }
        let client = client_builder.build()?;
        Ok(Self {
            store,
            client,
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_safety: Arc::new(Mutex::new(RefreshSafetyState::default())),
            issuer,
            require_https,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(store: GrokTokenStore<S>, issuer: String) -> anyhow::Result<Self> {
        Self::with_issuer(store, issuer, false)
    }

    pub fn store(&self) -> &GrokTokenStore<S> {
        &self.store
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, GrokAuthError> {
        self.get_auth_with_status().await.map(|(auth, _)| auth)
    }

    pub async fn get_auth_with_status(&self) -> Result<(StoredAuth, bool), GrokAuthError> {
        let auth = match self.load_current_auth(None).await {
            Ok(auth) => auth,
            Err(GrokAuthError::RefreshOutcomeUnknown) => {
                // A pending marker may belong to another request currently holding
                // the refresh lock. Join that single-flight before deciding that it
                // is an orphan left by a crashed process.
                return self.refresh(false, None).await.map(|auth| (auth, true));
            }
            Err(error) => return Err(error),
        };
        if auth.expires_at_ms > now_ms().saturating_add(REFRESH_SKEW_MS) {
            return Ok((auth, false));
        }
        self.refresh(false, None).await.map(|auth| (auth, true))
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, GrokAuthError> {
        self.refresh(true, Some(rejected_access)).await
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, GrokAuthError> {
        let _guard = self.refresh_lock.lock().await;
        let file_lock = self.acquire_refresh_file_lock().await?;
        let auth = self.load_current_auth(Some(&file_lock)).await?;
        if (!force && auth.expires_at_ms > now_ms().saturating_add(REFRESH_SKEW_MS))
            || rejected_access.is_some_and(|access| auth.access != access)
        {
            return Ok(auth);
        }
        if self.has_unpersisted_rotation(&auth).await {
            return Err(GrokAuthError::temporary(
                "rotated credentials are usable in memory but are not durably persisted; refusing another refresh",
            ));
        }
        if auth.issuer.trim_end_matches('/') != self.issuer || auth.client_id != CLIENT_ID {
            return Err(GrokAuthError::credentials_invalid(
                "unsupported OAuth session; re-authenticate",
            ));
        }
        if auth.refresh.is_empty() {
            return Err(GrokAuthError::credentials_invalid(
                "no refresh token is stored; re-authenticate",
            ));
        }
        let issuer = Url::parse(&self.issuer).map_err(|error| {
            GrokAuthError::temporary(format!("configured OAuth issuer is invalid: {error}"))
        })?;
        let discovery_url = issuer
            .join("/.well-known/openid-configuration")
            .map_err(|error| {
                GrokAuthError::temporary(format!("failed to construct discovery URL: {error}"))
            })?;
        let discovery_response = self
            .client
            .get(discovery_url)
            .send()
            .await
            .map_err(|error| {
                GrokAuthError::retryable_temporary(format!(
                    "OIDC discovery request failed: {error}"
                ))
            })?;
        let discovery_status = discovery_response.status();
        if discovery_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(GrokAuthError::rate_limited(
                "OIDC discovery was rate limited",
                retry_after(discovery_response.headers()),
            ));
        }
        if !discovery_status.is_success() {
            let message = format!(
                "OIDC discovery failed with status {}",
                discovery_status.as_u16()
            );
            return Err(
                if matches!(discovery_status.as_u16(), 500 | 502 | 503 | 504) {
                    GrokAuthError::retryable_temporary(message)
                } else {
                    GrokAuthError::temporary(message)
                },
            );
        }
        let discovery: Discovery = read_json_async(
            discovery_response,
            MAX_OAUTH_JSON_BYTES,
            "Grok OIDC discovery response",
        )
        .await
        .map_err(|error| {
            GrokAuthError::temporary(format!("OIDC discovery response is invalid: {error}"))
        })?;
        if discovery.issuer.trim_end_matches('/') != self.issuer {
            return Err(GrokAuthError::temporary("OIDC discovery issuer mismatch"));
        }
        let endpoint = Url::parse(&discovery.token_endpoint).map_err(|error| {
            GrokAuthError::temporary(format!("OIDC token endpoint is invalid: {error}"))
        })?;
        if (self.require_https && endpoint.scheme() != "https")
            || endpoint.origin() != issuer.origin()
        {
            return Err(GrokAuthError::temporary(
                "OIDC token endpoint is outside the canonical issuer",
            ));
        }
        self.mark_refresh_pending(&auth).await?;
        let refresh_response = self
            .client
            .post(endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", auth.refresh.as_str()),
                ("client_id", auth.client_id.as_str()),
            ])
            .send()
            .await;
        let refresh_response = match refresh_response {
            Ok(response) => response,
            Err(error) => {
                if error.is_connect() {
                    self.clear_refresh_pending(&auth).await?;
                }
                let message = format!("token refresh request failed: {error}");
                return Err(if error.is_connect() {
                    GrokAuthError::retryable_temporary(message)
                } else {
                    GrokAuthError::temporary(message)
                });
            }
        };
        let refresh_status = refresh_response.status();
        if refresh_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.clear_refresh_pending(&auth).await?;
            return Err(GrokAuthError::rate_limited(
                "token refresh was rate limited",
                retry_after(refresh_response.headers()),
            ));
        }
        if !refresh_status.is_success() {
            self.clear_refresh_pending(&auth).await?;
            let oauth_error = if refresh_status == reqwest::StatusCode::BAD_REQUEST {
                read_json_async::<OAuthErrorResponse>(
                    refresh_response,
                    MAX_OAUTH_ERROR_BYTES,
                    "Grok token refresh error response",
                )
                .await
                .ok()
                .map(|response| response.error)
            } else {
                None
            };
            let credentials_invalid = matches!(refresh_status.as_u16(), 401 | 403)
                || (refresh_status == reqwest::StatusCode::BAD_REQUEST
                    && oauth_error.as_deref().is_some_and(is_credential_error));
            if credentials_invalid {
                return Err(GrokAuthError::credentials_invalid(format!(
                    "token refresh was rejected with status {}",
                    refresh_status.as_u16()
                )));
            }
            return Err(GrokAuthError::temporary(format!(
                "token refresh failed with status {}",
                refresh_status.as_u16()
            )));
        }
        let refreshed: RefreshResponse = match read_json_async(
            refresh_response,
            MAX_OAUTH_JSON_BYTES,
            "Grok token refresh response",
        )
        .await
        {
            Ok(refreshed) => refreshed,
            Err(error) => {
                return Err(GrokAuthError::temporary(format!(
                    "token refresh response is invalid: {error}"
                )));
            }
        };
        if refreshed.access_token.is_empty() || refreshed.expires_in == 0 {
            return Err(GrokAuthError::temporary(
                "token refresh response omitted required fields",
            ));
        }
        let base_generation = auth_generation_fingerprint(&auth);
        let updated = StoredAuth {
            access: refreshed.access_token,
            refresh: refreshed
                .refresh_token
                .filter(|token| !token.is_empty())
                .unwrap_or_else(|| auth.refresh.clone()),
            expires_at_ms: now_ms().saturating_add(refreshed.expires_in.saturating_mul(1000)),
            issuer: auth.issuer.clone(),
            client_id: auth.client_id.clone(),
        };
        {
            let mut safety = self.refresh_safety.lock().await;
            safety.volatile_auth = Some(VolatileAuth {
                auth: updated.clone(),
                base_generation,
            });
        }
        match self.store.save_auth(updated.clone()) {
            Ok(()) => {
                self.clear_refresh_pending(&auth).await?;
                self.refresh_safety.lock().await.volatile_auth = None;
            }
            Err(error) => {
                crate::logging::create_logger("grok").warn(
                    "auth_persistence_degraded",
                    Some(serde_json::Map::from_iter([
                        ("message".to_string(), serde_json::json!(error.to_string())),
                        (
                            "authPath".to_string(),
                            serde_json::json!(self.store.auth_path()),
                        ),
                    ])),
                );
            }
        }
        Ok(updated)
    }

    async fn load_current_auth(
        &self,
        mutation_lock: Option<&AuthMutationLock>,
    ) -> Result<StoredAuth, GrokAuthError> {
        let stored = self.store.load_auth();
        let volatile = self.refresh_safety.lock().await.volatile_auth.clone();
        if let Some(volatile) = volatile {
            match stored.as_ref() {
                Ok(Some(disk_auth))
                    if auth_generation_fingerprint(disk_auth) == volatile.base_generation =>
                {
                    // Lock-free auth reads may use the in-memory rotated
                    // credentials, but must never repair persistence: an
                    // explicit login or logout in another process could win
                    // between this durable read and the write. The refresh
                    // path supplies a live lock guard only after acquiring the
                    // cross-process mutation lock and rereading the durable
                    // generation above.
                    if mutation_lock.is_some()
                        && self.store.save_auth(volatile.auth.clone()).is_ok()
                    {
                        self.clear_refresh_pending(disk_auth).await?;
                        self.refresh_safety.lock().await.volatile_auth = None;
                    }
                    return Ok(volatile.auth);
                }
                Err(_) => return Ok(volatile.auth),
                _ => {
                    // A logout or explicit re-auth changed the durable generation;
                    // do not let an older volatile token shadow that user action.
                    self.refresh_safety.lock().await.volatile_auth = None;
                }
            }
        }

        let auth = stored
            .map_err(|error| {
                GrokAuthError::temporary(format!("failed to load stored credentials: {error}"))
            })?
            .ok_or_else(|| GrokAuthError::credentials_invalid("not authenticated"))?;
        let fingerprint = auth_generation_fingerprint(&auth);
        let mut safety = self.refresh_safety.lock().await;
        if safety.ambiguous_generation == Some(fingerprint) {
            return Err(GrokAuthError::RefreshOutcomeUnknown);
        }
        if safety.ambiguous_generation.is_some() {
            safety.ambiguous_generation = None;
        }
        drop(safety);

        let coordination_path = self.store.coordination_path();
        if let Some(pending) =
            read_refresh_pending(coordination_path.as_deref()).map_err(|error| {
                GrokAuthError::temporary(format!(
                    "failed to read the OAuth refresh pending marker: {error}"
                ))
            })?
        {
            if pending == fingerprint {
                return Err(GrokAuthError::RefreshOutcomeUnknown);
            }
            clear_refresh_pending(coordination_path.as_deref()).map_err(|error| {
                GrokAuthError::temporary(format!(
                    "failed to clear the OAuth refresh pending marker: {error}"
                ))
            })?;
        }
        Ok(auth)
    }

    async fn acquire_refresh_file_lock(&self) -> Result<AuthMutationLock, GrokAuthError> {
        let coordination_path = self.store.coordination_path();
        AuthMutationLock::acquire_async(coordination_path.as_deref())
            .await
            .map_err(|error| {
                GrokAuthError::temporary(format!(
                    "failed to acquire the OAuth refresh lock: {error}"
                ))
            })
    }

    async fn has_unpersisted_rotation(&self, auth: &StoredAuth) -> bool {
        self.refresh_safety
            .lock()
            .await
            .volatile_auth
            .as_ref()
            .is_some_and(|volatile| {
                auth_generation_fingerprint(&volatile.auth) == auth_generation_fingerprint(auth)
            })
    }

    async fn mark_refresh_pending(&self, auth: &StoredAuth) -> Result<(), GrokAuthError> {
        let fingerprint = auth_generation_fingerprint(auth);
        let coordination_path = self.store.coordination_path();
        write_refresh_pending(coordination_path.as_deref(), fingerprint).map_err(|error| {
            GrokAuthError::temporary(format!(
                "failed to persist the OAuth refresh pending marker: {error}"
            ))
        })?;
        self.refresh_safety.lock().await.ambiguous_generation = Some(fingerprint);
        Ok(())
    }

    async fn clear_refresh_pending(&self, auth: &StoredAuth) -> Result<(), GrokAuthError> {
        let fingerprint = auth_generation_fingerprint(auth);
        let mut safety = self.refresh_safety.lock().await;
        // Once the durable marker is removed there must be no cancellation
        // point before the matching in-memory ambiguity is cleared.
        let coordination_path = self.store.coordination_path();
        clear_refresh_pending(coordination_path.as_deref()).map_err(|error| {
            GrokAuthError::temporary(format!(
                "failed to clear the OAuth refresh pending marker: {error}"
            ))
        })?;
        if safety.ambiguous_generation == Some(fingerprint) {
            safety.ambiguous_generation = None;
        }
        Ok(())
    }
}

fn auth_generation_fingerprint(auth: &StoredAuth) -> [u8; 32] {
    generation_fingerprint(auth).expect("StoredAuth is serializable")
}

fn is_credential_error(error: &str) -> bool {
    matches!(
        error,
        "invalid_grant" | "invalid_client" | "unauthorized_client" | "invalid_token"
    )
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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
    use crate::auth::{AuthStorage, FileAuthStore, InMemoryAuthStore};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use tempfile::TempDir;

    fn auth(access: &str) -> StoredAuth {
        StoredAuth {
            access: access.into(),
            refresh: "synthetic-refresh".into(),
            expires_at_ms: now_ms().saturating_add(3_600_000),
            issuer: CANONICAL_ISSUER.into(),
            client_id: "synthetic-client".into(),
        }
    }

    fn read_request(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "expected an HTTP request");
            request.extend_from_slice(&buffer[..read]);
            if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while request.len().saturating_sub(header_end) < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "expected the complete HTTP request body");
            request.extend_from_slice(&buffer[..read]);
        }
    }

    fn spawn_refresh_server(
        token_status: u16,
        retry_after: Option<&str>,
        oauth_error: &str,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server_issuer = issuer.clone();
        let retry_after = retry_after.map(str::to_string);
        let oauth_error = oauth_error.to_string();
        let server = thread::spawn(move || {
            let (mut discovery, _) = listener.accept().unwrap();
            read_request(&mut discovery);
            let body = serde_json::json!({
                "issuer": server_issuer.clone(),
                "token_endpoint": format!("{server_issuer}/oauth/token")
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            discovery.write_all(response.as_bytes()).unwrap();

            let (mut token, _) = listener.accept().unwrap();
            read_request(&mut token);
            let reason = match token_status {
                400 => "Bad Request",
                429 => "Too Many Requests",
                503 => "Service Unavailable",
                _ => "Test Status",
            };
            let body = serde_json::json!({"error": oauth_error}).to_string();
            let retry_header = retry_after
                .as_deref()
                .map(|value| format!("retry-after: {value}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {token_status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{retry_header}connection: close\r\n\r\n{body}",
                body.len()
            );
            token.write_all(response.as_bytes()).unwrap();
        });
        (issuer, server)
    }

    fn spawn_successful_refresh_server(
        drop_token_response: bool,
    ) -> (String, thread::JoinHandle<()>) {
        spawn_successful_refresh_server_with_delay(drop_token_response, Duration::ZERO)
    }

    fn spawn_successful_refresh_server_with_delay(
        drop_token_response: bool,
        token_delay: Duration,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server_issuer = issuer.clone();
        let server = thread::spawn(move || {
            let (mut discovery, _) = listener.accept().unwrap();
            read_request(&mut discovery);
            let body = serde_json::json!({
                "issuer": server_issuer.clone(),
                "token_endpoint": format!("{server_issuer}/oauth/token")
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            discovery.write_all(response.as_bytes()).unwrap();

            let (mut token, _) = listener.accept().unwrap();
            read_request(&mut token);
            thread::sleep(token_delay);
            if drop_token_response {
                return;
            }
            let body = serde_json::json!({
                "access_token": "rotated-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            token.write_all(response.as_bytes()).unwrap();
        });
        (issuer, server)
    }

    #[derive(Clone)]
    struct FailingSaveStore {
        inner: InMemoryAuthStore<StoredAuth>,
        fail: Arc<AtomicBool>,
    }

    struct FailingFileSaveStore {
        inner: FileAuthStore<StoredAuth>,
        fail: Arc<AtomicBool>,
    }

    impl AuthStorage<StoredAuth> for FailingFileSaveStore {
        fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
            self.inner.load()
        }

        fn save(&self, value: StoredAuth) -> anyhow::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("synthetic persistence failure");
            }
            self.inner.save(value)
        }

        fn clear(&self) -> anyhow::Result<()> {
            self.inner.clear()
        }

        fn path(&self) -> String {
            self.inner.path()
        }

        fn coordination_path(&self) -> Option<std::path::PathBuf> {
            self.inner.coordination_path()
        }
    }

    impl AuthStorage<StoredAuth> for FailingSaveStore {
        fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
            self.inner.load()
        }

        fn save(&self, value: StoredAuth) -> anyhow::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("synthetic persistence failure");
            }
            self.inner.save(value)
        }

        fn clear(&self) -> anyhow::Result<()> {
            self.inner.clear()
        }

        fn path(&self) -> String {
            "synthetic-failing-store".to_string()
        }
    }

    async fn refresh_error(
        token_status: u16,
        retry_after: Option<&str>,
        oauth_error: &str,
    ) -> GrokAuthError {
        let (issuer, server) = spawn_refresh_server(token_status, retry_after, oauth_error);
        let store = GrokTokenStore::new(InMemoryAuthStore::new());
        store
            .save_auth(StoredAuth {
                access: "expired-access".into(),
                refresh: "expired-refresh".into(),
                expires_at_ms: 0,
                issuer: issuer.clone(),
                client_id: CLIENT_ID.into(),
            })
            .unwrap();
        let manager = GrokAuthManager::new_for_test(store, issuer).unwrap();
        let error = manager.get_auth().await.unwrap_err();
        server.join().unwrap();
        error
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

    #[tokio::test]
    async fn concurrent_expired_auth_requests_join_one_refresh() {
        let (issuer, server) =
            spawn_successful_refresh_server_with_delay(false, Duration::from_millis(100));
        let store = GrokTokenStore::new(InMemoryAuthStore::new());
        store
            .save_auth(StoredAuth {
                access: "expired-access".into(),
                refresh: "expired-refresh".into(),
                expires_at_ms: 0,
                issuer: issuer.clone(),
                client_id: CLIENT_ID.into(),
            })
            .unwrap();
        let manager = Arc::new(GrokAuthManager::new_for_test(store, issuer).unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut requests = Vec::new();
        for _ in 0..16 {
            let manager = manager.clone();
            let barrier = barrier.clone();
            requests.push(tokio::spawn(async move {
                barrier.wait().await;
                manager.get_auth().await
            }));
        }
        for request in requests {
            assert_eq!(request.await.unwrap().unwrap().access, "rotated-access");
        }
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejected_refresh_is_classified_as_invalid_credentials() {
        let error = refresh_error(400, None, "invalid_grant").await;

        assert_eq!(error.kind(), GrokAuthErrorKind::CredentialsInvalid);
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test]
    async fn unavailable_refresh_service_is_classified_as_temporary() {
        let error = refresh_error(503, None, "temporarily_unavailable").await;

        assert_eq!(error.kind(), GrokAuthErrorKind::Temporary);
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test]
    async fn refresh_rate_limit_preserves_retry_after() {
        let error = refresh_error(429, Some("17"), "slow_down").await;

        assert_eq!(error.kind(), GrokAuthErrorKind::RateLimited);
        assert_eq!(error.retry_after(), Some("17"));
    }

    #[tokio::test]
    async fn unrelated_bad_request_is_not_misclassified_as_invalid_credentials() {
        let error = refresh_error(400, None, "invalid_request").await;

        assert_eq!(error.kind(), GrokAuthErrorKind::Temporary);
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test]
    async fn ambiguous_token_response_is_not_replayed_by_a_later_request() {
        let (issuer, server) = spawn_successful_refresh_server(true);
        let store = GrokTokenStore::new(InMemoryAuthStore::new());
        store
            .save_auth(StoredAuth {
                access: "expired-access".into(),
                refresh: "possibly-rotated-refresh".into(),
                expires_at_ms: 0,
                issuer: issuer.clone(),
                client_id: CLIENT_ID.into(),
            })
            .unwrap();
        let manager = GrokAuthManager::new_for_test(store, issuer).unwrap();

        let first = manager.get_auth().await.unwrap_err();
        assert_eq!(first.kind(), GrokAuthErrorKind::Temporary);
        server.join().unwrap();

        let second = manager.get_auth().await.unwrap_err();
        assert!(second.to_string().contains("outcome is unknown"));
        assert!(!second.safe_to_retry());
    }

    #[tokio::test]
    async fn rotated_credentials_remain_usable_when_persistence_fails() {
        let (issuer, server) = spawn_successful_refresh_server(false);
        let inner = InMemoryAuthStore::new();
        inner
            .save(StoredAuth {
                access: "expired-access".into(),
                refresh: "old-refresh".into(),
                expires_at_ms: 0,
                issuer: issuer.clone(),
                client_id: CLIENT_ID.into(),
            })
            .unwrap();
        let fail = Arc::new(AtomicBool::new(true));
        let storage = FailingSaveStore {
            inner: inner.clone(),
            fail,
        };
        let manager = GrokAuthManager::new_for_test(GrokTokenStore::new(storage), issuer).unwrap();

        let refreshed = manager.get_auth().await.unwrap();
        assert_eq!(refreshed.access, "rotated-access");
        assert_eq!(refreshed.refresh, "rotated-refresh");
        server.join().unwrap();

        let reused = manager.get_auth().await.unwrap();
        assert_eq!(reused.access, "rotated-access");
        assert_eq!(inner.load().unwrap().unwrap().refresh, "old-refresh");
    }

    #[tokio::test]
    async fn volatile_rotation_is_repaired_only_from_the_mutation_locked_path() {
        let temp = TempDir::new().unwrap();
        let auth_path = temp.path().join("grok/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let durable = auth("durable-access");
        let rotated = auth("rotated-access");
        let store = GrokTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string.clone(),
        ));
        store.save_auth(durable.clone()).unwrap();
        let manager = GrokAuthManager::new(store).unwrap();
        manager.mark_refresh_pending(&durable).await.unwrap();
        manager.refresh_safety.lock().await.volatile_auth = Some(VolatileAuth {
            auth: rotated.clone(),
            base_generation: auth_generation_fingerprint(&durable),
        });

        assert_eq!(manager.get_auth().await.unwrap().access, rotated.access);
        assert_eq!(
            manager.store().load_auth().unwrap().unwrap().access,
            durable.access,
            "lock-free reads must not write the volatile generation"
        );
        assert!(refresh_pending_path(std::path::Path::new(&auth_path_string)).exists());

        let repaired = manager.force_refresh("rejected-access").await.unwrap();
        assert_eq!(repaired.access, rotated.access);
        assert_eq!(
            manager.store().load_auth().unwrap().unwrap().access,
            rotated.access
        );
        assert!(!refresh_pending_path(std::path::Path::new(&auth_path_string)).exists());
        assert!(manager.refresh_safety.lock().await.volatile_auth.is_none());
    }

    #[tokio::test]
    async fn explicit_login_and_logout_win_over_an_older_volatile_rotation() {
        let temp = TempDir::new().unwrap();
        let auth_path = temp.path().join("grok/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let original = auth("original-access");
        let volatile = auth("volatile-access");
        let store = GrokTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string,
        ));
        store.save_auth(original.clone()).unwrap();
        let manager = GrokAuthManager::new(store).unwrap();

        manager.refresh_safety.lock().await.volatile_auth = Some(VolatileAuth {
            auth: volatile.clone(),
            base_generation: auth_generation_fingerprint(&original),
        });
        let explicit_login = auth("explicit-login-access");
        manager
            .store()
            .save_auth_exclusive(explicit_login.clone())
            .unwrap();
        assert_eq!(
            manager.get_auth().await.unwrap().access,
            explicit_login.access
        );
        assert!(manager.refresh_safety.lock().await.volatile_auth.is_none());

        manager.store().save_auth(original.clone()).unwrap();
        manager.refresh_safety.lock().await.volatile_auth = Some(VolatileAuth {
            auth: volatile,
            base_generation: auth_generation_fingerprint(&original),
        });
        manager.store().clear_auth_exclusive().unwrap();
        assert!(matches!(
            manager.get_auth().await,
            Err(GrokAuthError::CredentialsInvalid { .. })
        ));
        assert!(manager.refresh_safety.lock().await.volatile_auth.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_explicit_mutations_win_before_volatile_repair() {
        let temp = TempDir::new().unwrap();
        let auth_path = temp.path().join("grok/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let original = auth("original-access");
        let volatile = auth("volatile-access");
        let file_store = FileAuthStore::new(auth_path_string.clone(), auth_path_string.clone());
        file_store.save(original.clone()).unwrap();
        let manager = Arc::new(
            GrokAuthManager::new(GrokTokenStore::new(FileAuthStore::new(
                auth_path_string.clone(),
                auth_path_string.clone(),
            )))
            .unwrap(),
        );

        manager.refresh_safety.lock().await.volatile_auth = Some(VolatileAuth {
            auth: volatile.clone(),
            base_generation: auth_generation_fingerprint(&original),
        });
        let held =
            AuthMutationLock::acquire(Some(std::path::Path::new(&auth_path_string))).unwrap();
        let refresh_manager = manager.clone();
        let mut refresh =
            tokio::spawn(async move { refresh_manager.force_refresh("rejected-access").await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut refresh)
                .await
                .is_err(),
            "volatile repair should wait for the cross-process mutation lock"
        );
        let explicit_login = auth("explicit-login-access");
        file_store.save(explicit_login.clone()).unwrap();
        drop(held);
        assert_eq!(
            refresh.await.unwrap().unwrap().access,
            explicit_login.access
        );
        assert_eq!(
            file_store.load().unwrap().unwrap().access,
            explicit_login.access
        );

        file_store.save(original.clone()).unwrap();
        manager.refresh_safety.lock().await.volatile_auth = Some(VolatileAuth {
            auth: volatile,
            base_generation: auth_generation_fingerprint(&original),
        });
        let held =
            AuthMutationLock::acquire(Some(std::path::Path::new(&auth_path_string))).unwrap();
        let refresh_manager = manager.clone();
        let mut refresh =
            tokio::spawn(async move { refresh_manager.force_refresh("rejected-access").await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut refresh)
                .await
                .is_err(),
            "volatile repair should wait for the cross-process mutation lock"
        );
        file_store.clear().unwrap();
        drop(held);
        assert!(matches!(
            refresh.await.unwrap(),
            Err(GrokAuthError::CredentialsInvalid { .. })
        ));
        assert!(file_store.load().unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_marker_clear_while_waiting_for_state_lock_keeps_marker() {
        let temp = TempDir::new().unwrap();
        let auth_path = temp.path().join("grok/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let current = auth("current-access");
        let store = GrokTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string.clone(),
        ));
        store.save_auth(current.clone()).unwrap();
        let manager = Arc::new(GrokAuthManager::new(store).unwrap());
        manager.mark_refresh_pending(&current).await.unwrap();

        let state_guard = manager.refresh_safety.lock().await;
        let clear_manager = manager.clone();
        let clear_auth = current.clone();
        let mut clear =
            tokio::spawn(async move { clear_manager.clear_refresh_pending(&clear_auth).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut clear)
                .await
                .is_err(),
            "marker clear should be waiting for the in-memory state lock"
        );
        assert!(
            refresh_pending_path(std::path::Path::new(&auth_path_string)).exists(),
            "cancellation before the state lock is acquired must leave the durable marker"
        );
        clear.abort();
        drop(state_guard);
        let _ = clear.await;

        assert!(refresh_pending_path(std::path::Path::new(&auth_path_string)).exists());
        assert_eq!(
            manager.refresh_safety.lock().await.ambiguous_generation,
            Some(auth_generation_fingerprint(&current))
        );
    }

    #[tokio::test]
    async fn persistence_degraded_state_blocks_a_second_rotation_and_stays_fail_closed() {
        let (issuer, server) = spawn_successful_refresh_server(false);
        let temp = TempDir::new().unwrap();
        let auth_path = temp.path().join("grok/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let initial = StoredAuth {
            access: "expired-access".into(),
            refresh: "durable-refresh".into(),
            expires_at_ms: 0,
            issuer: issuer.clone(),
            client_id: CLIENT_ID.into(),
        };
        let initial_store = FileAuthStore::new(auth_path_string.clone(), auth_path_string.clone());
        initial_store.save(initial.clone()).unwrap();
        let storage = FailingFileSaveStore {
            inner: FileAuthStore::new(auth_path_string.clone(), auth_path_string.clone()),
            fail: Arc::new(AtomicBool::new(true)),
        };
        let manager =
            GrokAuthManager::new_for_test(GrokTokenStore::new(storage), issuer.clone()).unwrap();

        assert_eq!(manager.get_auth().await.unwrap().access, "rotated-access");
        server.join().unwrap();
        let second = manager.force_refresh("rotated-access").await.unwrap_err();
        assert!(second.to_string().contains("not durably persisted"));
        assert!(!second.safe_to_retry());
        assert_eq!(
            read_refresh_pending(Some(std::path::Path::new(&auth_path_string))).unwrap(),
            Some(auth_generation_fingerprint(&initial))
        );
        drop(manager);

        let restarted_store = GrokTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string.clone(),
        ));
        let restarted = GrokAuthManager::new_for_test(restarted_store, issuer).unwrap();
        assert!(matches!(
            restarted.get_auth().await,
            Err(GrokAuthError::RefreshOutcomeUnknown)
        ));
    }

    #[tokio::test]
    async fn dropped_refresh_future_leaves_a_durable_fail_closed_marker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server_issuer = issuer.clone();
        let (token_seen_tx, token_seen_rx) = tokio::sync::oneshot::channel();
        let server = thread::spawn(move || {
            let (mut discovery, _) = listener.accept().unwrap();
            read_request(&mut discovery);
            let body = serde_json::json!({
                "issuer": server_issuer.clone(),
                "token_endpoint": format!("{server_issuer}/oauth/token")
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            discovery.write_all(response.as_bytes()).unwrap();

            let (mut token, _) = listener.accept().unwrap();
            read_request(&mut token);
            let _ = token_seen_tx.send(());
            thread::sleep(Duration::from_millis(100));
        });

        let temp = TempDir::new().unwrap();
        let auth_path = temp.path().join("grok/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let initial = StoredAuth {
            access: "expired-access".into(),
            refresh: "possibly-rotated-refresh".into(),
            expires_at_ms: 0,
            issuer: issuer.clone(),
            client_id: CLIENT_ID.into(),
        };
        let store = GrokTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string.clone(),
        ));
        store.save_auth(initial.clone()).unwrap();
        let manager = GrokAuthManager::new_for_test(store, issuer.clone()).unwrap();
        let refresh = tokio::spawn(async move { manager.get_auth().await });
        token_seen_rx.await.unwrap();
        refresh.abort();
        let _ = refresh.await;
        server.join().unwrap();

        let restarted_store = GrokTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string.clone(),
        ));
        let restarted = GrokAuthManager::new_for_test(restarted_store, issuer).unwrap();
        let blocked = restarted.get_auth().await.unwrap_err();
        assert!(blocked.to_string().contains("outcome is unknown"));
        assert!(refresh_pending_path(std::path::Path::new(&auth_path_string)).exists());

        restarted
            .store()
            .save_auth(StoredAuth {
                access: "explicitly-reimported-access".into(),
                expires_at_ms: now_ms().saturating_add(3_600_000),
                ..initial
            })
            .unwrap();
        let recovered = restarted.get_auth().await.unwrap();
        assert_eq!(recovered.access, "explicitly-reimported-access");
        assert!(!refresh_pending_path(std::path::Path::new(&auth_path_string)).exists());
    }
}
