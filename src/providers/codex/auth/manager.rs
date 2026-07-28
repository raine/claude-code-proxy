use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::AuthStorage;
use crate::oauth_http::{
    MAX_OAUTH_ERROR_BYTES, MAX_OAUTH_JSON_BYTES, read_json_async, read_text_async,
};
use crate::oauth_rotation::{
    AuthMutationLock, clear_refresh_pending, generation_fingerprint, read_refresh_pending,
    write_refresh_pending,
};
use crate::providers::codex::dispatch_budget::CodexDispatchBudget;
use crate::timeutil::now_ms;

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: CodexTokenStore<S>,
    #[cfg(test)]
    test_auth: Arc<Mutex<Option<StoredAuth>>>,
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_safety: Arc<AsyncMutex<RefreshSafetyState>>,
    refresh_client: reqwest::Client,
    token_endpoint: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexAuthErrorKind {
    CredentialsInvalid,
    Temporary,
    RefreshOutcomeUnknown,
}

#[derive(Debug, thiserror::Error)]
pub enum CodexAuthError {
    #[error("{message}")]
    CredentialsInvalid { message: String },
    #[error("{message}")]
    Temporary { message: String },
    #[error("{message}")]
    RefreshOutcomeUnknown { message: String },
}

impl CodexAuthError {
    pub fn kind(&self) -> CodexAuthErrorKind {
        match self {
            Self::CredentialsInvalid { .. } => CodexAuthErrorKind::CredentialsInvalid,
            Self::Temporary { .. } => CodexAuthErrorKind::Temporary,
            Self::RefreshOutcomeUnknown { .. } => CodexAuthErrorKind::RefreshOutcomeUnknown,
        }
    }

    fn credentials_invalid(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self::CredentialsInvalid {
            message: message.into(),
        })
    }

    fn temporary(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self::Temporary {
            message: message.into(),
        })
    }

    fn refresh_outcome_unknown(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self::RefreshOutcomeUnknown {
            message: message.into(),
        })
    }
}

pub fn codex_auth_error_kind(error: &anyhow::Error) -> CodexAuthErrorKind {
    error
        .downcast_ref::<CodexAuthError>()
        .map(CodexAuthError::kind)
        .unwrap_or(CodexAuthErrorKind::Temporary)
}

fn refresh_outcome_unknown(message: impl Into<String>) -> anyhow::Error {
    CodexAuthError::refresh_outcome_unknown(format!(
        "the previous Codex OAuth token refresh outcome is unknown; {}; run `claude-code-proxy codex auth login` before retrying",
        message.into()
    ))
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self::new_with_token_endpoint(store, format!("{ISSUER}/oauth/token"))
    }

    pub(crate) fn new_with_token_endpoint(
        store: CodexTokenStore<S>,
        token_endpoint: String,
    ) -> Self {
        let mut client_builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30));
        if crate::oauth_http::is_loopback_url(&token_endpoint) {
            client_builder = client_builder.no_proxy();
        }
        Self {
            store,
            #[cfg(test)]
            test_auth: Arc::new(Mutex::new(None)),
            refresh_lock: Arc::new(AsyncMutex::new(())),
            refresh_safety: Arc::new(AsyncMutex::new(RefreshSafetyState::default())),
            refresh_client: client_builder
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        self.get_auth_inner(None).await
    }

    pub(crate) async fn get_auth_with_budget(
        &self,
        budget: &CodexDispatchBudget,
    ) -> Result<StoredAuth, anyhow::Error> {
        self.get_auth_inner(Some(budget)).await
    }

    async fn get_auth_inner(
        &self,
        budget: Option<&CodexDispatchBudget>,
    ) -> Result<StoredAuth, anyhow::Error> {
        let stored = match self.load_current_auth(false).await {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                return Err(CodexAuthError::credentials_invalid(
                    "Not authenticated. Run: claude-code-proxy codex auth login",
                ));
            }
            Err(error)
                if codex_auth_error_kind(&error) == CodexAuthErrorKind::RefreshOutcomeUnknown =>
            {
                // A pending marker can belong to another process that still
                // owns the mutation lock. Join it before deciding the marker
                // was orphaned by a crash or cancellation.
                return self.refresh(false, None, budget).await;
            }
            Err(error) => return Err(error),
        };

        if stored.expires > now_ms() + REFRESH_MARGIN_MS {
            return Ok(stored);
        }

        self.refresh(false, None, budget).await
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, anyhow::Error> {
        self.refresh(true, Some(rejected_access), None).await
    }

    pub(crate) async fn force_refresh_with_budget(
        &self,
        rejected_access: &str,
        budget: &CodexDispatchBudget,
    ) -> Result<StoredAuth, anyhow::Error> {
        self.refresh(true, Some(rejected_access), Some(budget))
            .await
    }

    fn load_stored_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(auth));
        }

        self.store.load_auth()
    }

    async fn load_current_auth(
        &self,
        mutation_locked: bool,
    ) -> Result<Option<StoredAuth>, anyhow::Error> {
        let stored = self.load_stored_auth();
        let volatile = self.refresh_safety.lock().await.volatile_auth.clone();
        if let Some(volatile) = volatile {
            match stored.as_ref() {
                Ok(Some(durable))
                    if generation_fingerprint(durable)? == volatile.base_generation =>
                {
                    // Never repair persistence from the lock-free get_auth
                    // path: doing so could overwrite a concurrent explicit
                    // login. The refresh path sets mutation_locked only after
                    // it owns the cross-process mutation lock.
                    if mutation_locked && self.store.save_auth(volatile.auth.clone()).is_ok() {
                        self.clear_refresh_pending_for(durable).await?;
                        self.refresh_safety.lock().await.volatile_auth = None;
                    }
                    return Ok(Some(volatile.auth));
                }
                Err(_) => return Ok(Some(volatile.auth)),
                _ => {
                    // Explicit login/logout or a completed durable write wins
                    // over an older in-memory fallback.
                    self.refresh_safety.lock().await.volatile_auth = None;
                }
            }
        }

        let Some(auth) = stored? else {
            return Ok(None);
        };
        let fingerprint = generation_fingerprint(&auth)?;
        {
            let mut safety = self.refresh_safety.lock().await;
            if safety.ambiguous_generation == Some(fingerprint) {
                return Err(refresh_outcome_unknown("a refresh marker is still pending"));
            }
            if safety.ambiguous_generation.is_some() {
                safety.ambiguous_generation = None;
            }
        }

        let coordination_path = self.store.coordination_path();
        if let Some(pending) = read_refresh_pending(coordination_path.as_deref())? {
            if pending == fingerprint {
                return Err(refresh_outcome_unknown(
                    "a durable refresh marker is still pending",
                ));
            }
            clear_refresh_pending(coordination_path.as_deref())?;
        }
        Ok(Some(auth))
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
        budget: Option<&CodexDispatchBudget>,
    ) -> Result<StoredAuth, anyhow::Error> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let coordination_path = self.store.coordination_path();
        let _mutation_guard = AuthMutationLock::acquire_async(coordination_path.as_deref()).await?;

        let current = self
            .load_current_auth(true)
            .await?
            .ok_or_else(|| CodexAuthError::credentials_invalid("Not authenticated"))?;

        if (!force && current.expires > now_ms() + REFRESH_MARGIN_MS)
            || rejected_access.is_some_and(|access| current.access != access)
        {
            return Ok(current);
        }

        if self.has_unpersisted_rotation(&current).await? {
            return Err(CodexAuthError::temporary(
                "rotated Codex credentials are usable in memory but are not durably persisted; refusing another refresh",
            ));
        }

        self.refresh_now(&current, budget).await
    }

    async fn refresh_now(
        &self,
        current: &StoredAuth,
        budget: Option<&CodexDispatchBudget>,
    ) -> Result<StoredAuth, anyhow::Error> {
        if current.refresh.is_empty() {
            return Err(CodexAuthError::credentials_invalid(
                "No refresh token stored; re-authenticate",
            ));
        }

        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        if let Some(budget) = budget
            && let Err(error) = budget.reserve_oauth()
        {
            return Err(anyhow::Error::new(error));
        }
        self.mark_refresh_pending(current).await?;
        let response = self
            .refresh_client
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await;
        let resp = match response {
            Ok(response) => response,
            Err(error) => {
                if error.is_connect() {
                    self.clear_refresh_pending_for(current).await?;
                    return Err(CodexAuthError::temporary(format!(
                        "refresh network error: {error}"
                    )));
                }
                return Err(refresh_outcome_unknown(format!(
                    "the token response was not received: {error}"
                )));
            }
        };

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            self.clear_refresh_pending_for(current).await?;
            let expected_generation = generation_fingerprint(current)?;
            if let Some(latest) = self.load_stored_auth()? {
                if generation_fingerprint(&latest)? != expected_generation {
                    return Ok(latest);
                }
                self.store.clear_auth()?;
            }
            let err_msg =
                read_text_async(resp, MAX_OAUTH_ERROR_BYTES, "Codex refresh error response")
                    .await
                    .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
            return Err(CodexAuthError::credentials_invalid(err_msg));
        }

        if !resp.status().is_success() {
            self.clear_refresh_pending_for(current).await?;
            return Err(CodexAuthError::temporary(format!(
                "Token refresh failed: {status}"
            )));
        }

        let tokens: TokenResponse =
            read_json_async(resp, MAX_OAUTH_JSON_BYTES, "Codex refresh response")
                .await
                .map_err(|error| {
                    refresh_outcome_unknown(format!("the token response was invalid: {error}"))
                })?;
        validate_token_response(&tokens).map_err(|error| {
            refresh_outcome_unknown(format!("the token response was incomplete: {error}"))
        })?;
        let account_id = extract_account_id(&tokens).or_else(|| current.account_id.clone());
        let expires = now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let next = StoredAuth {
            access: tokens.access_token,
            refresh: tokens.refresh_token,
            expires,
            account_id,
        };
        let base_generation = generation_fingerprint(current)?;
        self.refresh_safety.lock().await.volatile_auth = Some(VolatileAuth {
            auth: next.clone(),
            base_generation,
        });
        match self.store.save_auth(next.clone()) {
            Ok(()) => {
                self.clear_refresh_pending_for(current).await?;
                self.refresh_safety.lock().await.volatile_auth = None;
            }
            Err(error) => {
                crate::logging::create_logger("codex").warn(
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
        Ok(next)
    }

    async fn has_unpersisted_rotation(&self, auth: &StoredAuth) -> Result<bool, anyhow::Error> {
        let auth_generation = generation_fingerprint(auth)?;
        let safety = self.refresh_safety.lock().await;
        match safety.volatile_auth.as_ref() {
            Some(volatile) => Ok(generation_fingerprint(&volatile.auth)? == auth_generation),
            None => Ok(false),
        }
    }

    async fn mark_refresh_pending(&self, auth: &StoredAuth) -> Result<(), anyhow::Error> {
        let fingerprint = generation_fingerprint(auth)?;
        let coordination_path = self.store.coordination_path();
        write_refresh_pending(coordination_path.as_deref(), fingerprint)?;
        self.refresh_safety.lock().await.ambiguous_generation = Some(fingerprint);
        Ok(())
    }

    async fn clear_refresh_pending_for(&self, auth: &StoredAuth) -> Result<(), anyhow::Error> {
        let coordination_path = self.store.coordination_path();
        let fingerprint = generation_fingerprint(auth)?;
        let mut safety = self.refresh_safety.lock().await;
        // Once the durable marker is removed there must be no cancellation
        // point before the matching in-memory ambiguity is cleared.
        clear_refresh_pending(coordination_path.as_deref())?;
        if safety.ambiguous_generation == Some(fingerprint) {
            safety.ambiguous_generation = None;
        }
        Ok(())
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        validate_token_response(tokens)?;
        let account_id = extract_account_id(tokens);
        let expires = now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.refresh_token.clone(),
            expires,
            account_id,
        };
        self.store.save_auth_exclusive(auth.clone())?;
        Ok(auth)
    }

    #[cfg(test)]
    pub fn set_test_auth(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.test_auth.lock() {
            *guard = Some(auth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{FileAuthStore, InMemoryAuthStore};
    use crate::oauth_rotation::refresh_pending_path;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    fn test_store() -> CodexTokenStore<InMemoryAuthStore<StoredAuth>> {
        CodexTokenStore::new(InMemoryAuthStore::new())
    }

    fn expired_auth() -> StoredAuth {
        StoredAuth {
            access: "expired-access".into(),
            refresh: "old-refresh".into(),
            expires: 0,
            account_id: Some("acct_1".into()),
        }
    }

    async fn assert_refresh_post_does_not_follow_redirect(status: &'static str) {
        let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let target_server = thread::spawn(move || {
            let (mut stream, _) = target_listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            if request[..read].starts_with(b"STOP") {
                return None;
            }
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            let body = br#"{"access_token":"redirected","refresh_token":"redirected-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            Some(request)
        });

        let origin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_addr = origin_listener.local_addr().unwrap();
        let location = format!("http://{target_addr}/oauth/token/redirected");
        let origin_server = thread::spawn(move || {
            let (mut stream, _) = origin_listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });

        let store = test_store();
        store.save_auth(expired_auth()).unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            format!("http://{origin_addr}/oauth/token"),
        );
        let budget = CodexDispatchBudget::new();
        let result = manager.get_auth_with_budget(&budget).await;
        let origin_request = origin_server.join().unwrap();
        if let Ok(mut stop) = std::net::TcpStream::connect(target_addr) {
            let _ = stop.write_all(b"STOP");
        }
        let target_request = target_server.join().unwrap();

        assert!(origin_request.starts_with("POST /oauth/token "));
        assert!(origin_request.contains("refresh_token=old-refresh"));
        assert!(
            target_request.is_none(),
            "the redirected endpoint must not receive a replayed refresh-token POST"
        );
        let error = result.expect_err("Codex OAuth redirects must remain terminal responses");
        assert_eq!(codex_auth_error_kind(&error), CodexAuthErrorKind::Temporary);
        assert!(error.to_string().contains(&status[..3]));
        assert_eq!(budget.snapshot().oauth, 1);
        assert_eq!(budget.snapshot().total, 1);
    }

    #[derive(Clone)]
    struct FailingSaveStore {
        inner: InMemoryAuthStore<StoredAuth>,
        fail: Arc<AtomicBool>,
        coordination_path: PathBuf,
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
            "synthetic failing store".into()
        }

        fn coordination_path(&self) -> Option<PathBuf> {
            Some(self.coordination_path.clone())
        }
    }

    #[tokio::test]
    async fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        let result = manager.get_auth().await.unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.account_id.as_deref(), Some("acct_1"));
    }

    #[tokio::test]
    async fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = CodexAuthManager::new(store);
        let error = manager.get_auth().await.unwrap_err();
        assert_eq!(
            codex_auth_error_kind(&error),
            CodexAuthErrorKind::CredentialsInvalid
        );
        assert!(error.to_string().contains("Not authenticated"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_expired_auth_refreshes_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let server_refreshes = refreshes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("refresh_token=stale"));
            server_refreshes.fetch_add(1, Ordering::SeqCst);

            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "expired".into(),
                refresh: "stale".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            store,
            format!("http://{addr}/oauth/token"),
        ));
        let budget = CodexDispatchBudget::new();
        let (first, second) = tokio::join!(
            manager.get_auth_with_budget(&budget),
            manager.get_auth_with_budget(&budget)
        );
        let results = [first.unwrap(), second.unwrap()];
        server.join().unwrap();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(budget.snapshot().oauth, 1);
        assert_eq!(budget.snapshot().total, 1);
        assert!(results.iter().all(|auth| auth.access == "rotated"));
        assert!(results.iter().all(|auth| auth.refresh == "rotated-refresh"));
    }

    #[tokio::test]
    async fn refresh_post_does_not_follow_307_or_308_redirects() {
        assert_refresh_post_does_not_follow_redirect("307 Temporary Redirect").await;
        assert_refresh_post_does_not_follow_redirect("308 Permanent Redirect").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn independent_managers_join_one_file_locked_refresh() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let wire_refreshes = Arc::new(AtomicUsize::new(0));
        let server_refreshes = wire_refreshes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            server_refreshes.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(100));
            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let temp = tempfile::TempDir::new().unwrap();
        let auth_path = temp.path().join("codex/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        FileAuthStore::new(auth_path_string.clone(), auth_path_string.clone())
            .save(expired_auth())
            .unwrap();
        let endpoint = format!("http://{addr}/oauth/token");
        let first = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(FileAuthStore::new(
                auth_path_string.clone(),
                auth_path_string.clone(),
            )),
            endpoint.clone(),
        );
        let second = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(FileAuthStore::new(
                auth_path_string.clone(),
                auth_path_string,
            )),
            endpoint,
        );

        let (first, second) = tokio::join!(first.get_auth(), second.get_auth());
        server.join().unwrap();
        assert_eq!(wire_refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(first.unwrap().access, "rotated");
        assert_eq!(second.unwrap().access, "rotated");
        assert!(!refresh_pending_path(&auth_path).exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_refresh_future_leaves_durable_fail_closed_marker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (token_seen_tx, token_seen_rx) = tokio::sync::oneshot::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            assert!(read > 0);
            assert!(
                String::from_utf8_lossy(&request[..read]).contains("refresh_token=old-refresh")
            );
            let _ = token_seen_tx.send(());
            thread::sleep(Duration::from_millis(100));
        });

        let temp = tempfile::TempDir::new().unwrap();
        let auth_path = temp.path().join("codex/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let store = CodexTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string.clone(),
        ));
        store.save_auth(expired_auth()).unwrap();
        let manager =
            CodexAuthManager::new_with_token_endpoint(store, format!("http://{addr}/oauth/token"));
        let refresh = tokio::spawn(async move { manager.get_auth().await });
        token_seen_rx.await.unwrap();
        refresh.abort();
        let _ = refresh.await;
        server.join().unwrap();
        assert!(refresh_pending_path(&auth_path).exists());

        let restarted = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(FileAuthStore::new(
                auth_path_string.clone(),
                auth_path_string,
            )),
            "http://127.0.0.1:1/must-not-be-called".into(),
        );
        let error = restarted.get_auth().await.unwrap_err();
        assert!(error.to_string().contains("outcome is unknown"));
        assert!(refresh_pending_path(&auth_path).exists());
    }

    #[tokio::test]
    async fn cancelling_marker_clear_while_waiting_for_state_lock_keeps_marker() {
        let temp = tempfile::TempDir::new().unwrap();
        let auth_path = temp.path().join("codex/auth.json");
        let auth_path_string = auth_path.to_string_lossy().into_owned();
        let store = CodexTokenStore::new(FileAuthStore::new(
            auth_path_string.clone(),
            auth_path_string,
        ));
        let current = expired_auth();
        store.save_auth(current.clone()).unwrap();
        let manager = Arc::new(CodexAuthManager::new(store));
        manager.mark_refresh_pending(&current).await.unwrap();

        let state_guard = manager.refresh_safety.lock().await;
        let clear_manager = manager.clone();
        let clear_auth = current.clone();
        let mut clear =
            tokio::spawn(async move { clear_manager.clear_refresh_pending_for(&clear_auth).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut clear)
                .await
                .is_err(),
            "marker clear should be waiting for the in-memory state lock"
        );
        assert!(
            refresh_pending_path(&auth_path).exists(),
            "cancellation before the state lock is acquired must leave the durable marker"
        );
        clear.abort();
        drop(state_guard);
        let _ = clear.await;

        assert!(refresh_pending_path(&auth_path).exists());
        assert_eq!(
            manager.refresh_safety.lock().await.ambiguous_generation,
            Some(generation_fingerprint(&current).unwrap())
        );
    }

    #[tokio::test]
    async fn persistence_failure_keeps_rotated_auth_and_blocks_second_rotation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let temp = tempfile::TempDir::new().unwrap();
        let coordination_path = temp.path().join("codex/auth.json");
        let inner = InMemoryAuthStore::new();
        inner.save(expired_auth()).unwrap();
        let storage = FailingSaveStore {
            inner: inner.clone(),
            fail: Arc::new(AtomicBool::new(true)),
            coordination_path: coordination_path.clone(),
        };
        let manager = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(storage.clone()),
            format!("http://{addr}/oauth/token"),
        );

        let refreshed = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(refreshed.access, "rotated");
        assert_eq!(manager.get_auth().await.unwrap().access, "rotated");
        assert_eq!(inner.load().unwrap().unwrap().refresh, "old-refresh");
        let second = manager.force_refresh("rotated").await.unwrap_err();
        assert!(second.to_string().contains("not durably persisted"));
        assert!(refresh_pending_path(&coordination_path).exists());
        storage.fail.store(false, Ordering::SeqCst);
        assert_eq!(manager.get_auth().await.unwrap().access, "rotated");
        assert_eq!(
            inner.load().unwrap().unwrap().refresh,
            "old-refresh",
            "lock-free auth reads must not repair persistence and race an explicit login"
        );
        storage.fail.store(true, Ordering::SeqCst);
        drop(manager);

        let restarted = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(storage),
            "http://127.0.0.1:1/must-not-be-called".into(),
        );
        let error = restarted.get_auth().await.unwrap_err();
        assert!(error.to_string().contains("outcome is unknown"));
    }

    #[tokio::test]
    async fn stale_401_reuses_already_rotated_auth() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            "http://127.0.0.1:1/should-not-be-called".into(),
        );

        let budget = CodexDispatchBudget::new();
        let auth = manager
            .force_refresh_with_budget("rejected", &budget)
            .await
            .unwrap();
        assert_eq!(auth.access, "rotated");
        assert_eq!(auth.refresh, "rotated-refresh");
        assert_eq!(budget.snapshot().oauth, 0);
        assert_eq!(budget.snapshot().total, 0);
    }

    #[tokio::test]
    async fn unauthorized_refresh_preserves_changed_refresh_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let backing = InMemoryAuthStore::new();
        let server_backing = backing.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            server_backing
                .save(StoredAuth {
                    access: "same-access".into(),
                    refresh: "replacement-refresh".into(),
                    expires: u64::MAX,
                    account_id: Some("acct_1".into()),
                })
                .unwrap();
            let body = b"rejected refresh token";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = CodexTokenStore::new(backing);
        store
            .save_auth(StoredAuth {
                access: "same-access".into(),
                refresh: "rejected-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager =
            CodexAuthManager::new_with_token_endpoint(store, format!("http://{addr}/oauth/token"));

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, "same-access");
        assert_eq!(auth.refresh, "replacement-refresh");
        assert_eq!(manager.store.load_auth().unwrap(), Some(auth));
    }

    #[tokio::test]
    async fn durable_rotation_and_logout_are_observed_by_shared_manager() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "first".into(),
                refresh: "first-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);
        assert_eq!(manager.get_auth().await.unwrap().access, "first");

        manager
            .store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_2".into()),
            })
            .unwrap();
        let rotated = manager.get_auth().await.unwrap();
        assert_eq!(rotated.access, "rotated");
        assert_eq!(rotated.account_id.as_deref(), Some("acct_2"));

        manager.store.clear_auth().unwrap();
        assert!(manager.get_auth().await.is_err());
    }
}
