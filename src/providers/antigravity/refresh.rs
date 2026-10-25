//! Manager-owned credential maintenance, never model generation. Request
//! cancellation releases only its waiter. The stable lock is acquired BEFORE
//! the bounded network phase; a complete validated rotation is then persisted
//! outside that timer, with no cancellation point in the filesystem phase.
use super::{
    auth::{Credential, Lock, Store, bounded_json, now, update_tokens},
    config::{CONNECT_TIMEOUT, Config},
};
use std::{
    error::Error as StdError,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

const RETRY_BACKOFF: Duration = Duration::from_millis(200);
const CONNECT_COOLDOWN: Duration = Duration::from_secs(30);
const RETRY_RESPONSE_RESERVE: Duration = Duration::from_secs(1);
const MAX_ERROR_NODES: usize = 16;

type ErrorRef<'a> = &'a (dyn StdError + 'static);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SendFailure {
    Ambiguous,
    PreSendTransientRetryable,
    PreSendNonRetryableSafe,
}

// Complete both source() and io.get_ref() paths before granting pre-send proof.
// The latter is essential: io::Error::source can skip its contained error.
// Distinguish an active-path cycle from a shared child in the bounded graph.
fn error_graph<'a>(root: ErrorRef<'a>) -> Option<Vec<ErrorRef<'a>>> {
    fn visit<'a>(
        error: ErrorRef<'a>,
        nodes: &mut Vec<ErrorRef<'a>>,
        active: &mut Vec<ErrorRef<'a>>,
    ) -> Option<()> {
        if active.iter().any(|seen| std::ptr::eq(*seen, error)) {
            return None;
        }
        if nodes.iter().any(|seen| std::ptr::eq(*seen, error)) {
            return Some(());
        }
        if nodes.len() == MAX_ERROR_NODES {
            return None;
        }
        nodes.push(error);
        active.push(error);
        if let Some(source) = error.source() {
            visit(source, nodes, active)?;
        }
        if let Some(inner) = error
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
        {
            visit(inner, nodes, active)?;
        }
        active.pop();
        Some(())
    }
    let mut nodes = Vec::with_capacity(MAX_ERROR_NODES);
    visit(root, &mut nodes, &mut Vec::with_capacity(MAX_ERROR_NODES))?;
    Some(nodes)
}

fn classify_connect_graph(root: ErrorRef<'_>) -> SendFailure {
    let Some(nodes) = error_graph(root) else {
        return SendFailure::Ambiguous;
    };
    let Some((index, first_hyper)) = nodes.iter().enumerate().find_map(|(index, error)| {
        error
            .downcast_ref::<hyper_util::client::legacy::Error>()
            .map(|error| (index, error))
    }) else {
        return SendFailure::Ambiguous;
    };
    if !first_hyper.is_connect() || nodes[..index].iter().any(|error| error.is::<io::Error>()) {
        return SendFailure::Ambiguous;
    }
    if nodes.iter().any(|error| error.is::<rustls::Error>()) {
        return SendFailure::PreSendNonRetryableSafe;
    }
    let first_io = nodes[index + 1..]
        .iter()
        .find_map(|error| error.downcast_ref::<io::Error>());
    if first_io.is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
        )
    }) {
        SendFailure::PreSendTransientRetryable
    } else {
        SendFailure::PreSendNonRetryableSafe
    }
}

// Pinned reqwest 0.12.28 / hyper-util 0.1.20: Connect is constructed before
// request dispatch, including proxy CONNECT and TLS. This proof is specific to
// our buffered form, disabled redirects and disabled reqwest retry policy.
// Hyper may reconnect an unstarted pooled request internally: two .send() calls
// are not a limit of two sockets. Sent requests are never returned for that retry.
fn classify_send_error(error: &reqwest::Error) -> SendFailure {
    let classification = classify_connect_graph(error);
    // Only call reqwest's unbounded source walker after a complete acyclic graph.
    if classification != SendFailure::Ambiguous && error.is_connect() {
        classification
    } else {
        SendFailure::Ambiguous
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NetworkPhase {
    NotSent,
    InFlight,
    ProvenPreSend,
}

fn retry_budget(remaining: Duration, before_backoff: bool) -> bool {
    remaining
        >= CONNECT_TIMEOUT
            + RETRY_RESPONSE_RESERVE
            + if before_backoff {
                RETRY_BACKOFF
            } else {
                Duration::ZERO
            }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessError {
    Storage(&'static str),
    Lock(&'static str),
    Missing,
    LoginRequired,
    ClientConfiguration,
    ConnectFailure,
    ConnectCooldown,
    StoppedBeforeSend,
    Network,
    NetworkTimeout,
    InvalidResponse,
    PendingPersistence,
    ProfileChanged,
    UncertainRotation,
    Cancelled,
    Closed,
}
impl AccessError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Storage(_) => "Antigravity credential storage is invalid or unreadable",
            Self::Lock(_) => "Antigravity credential lock is unavailable or timed out",
            Self::Missing => "Antigravity authentication missing; run antigravity auth login",
            Self::LoginRequired => "Antigravity login required",
            Self::ClientConfiguration => "configure CCP_ANTIGRAVITY_CLIENT_ID before refreshing",
            Self::ConnectFailure => {
                "Antigravity refresh connection failed before token submission; credentials retained"
            }
            Self::ConnectCooldown => {
                "Antigravity refresh connection retry temporarily deferred; credentials retained"
            }
            Self::StoppedBeforeSend => {
                "Antigravity refresh stopped before token submission; credentials retained"
            }
            Self::Network => {
                "Antigravity refresh failed; its outcome may be unknown, no obsolete-token retry attempted"
            }
            Self::NetworkTimeout => {
                "Antigravity refresh network deadline exceeded; no obsolete-token retry attempted"
            }
            Self::InvalidResponse => {
                "Antigravity refresh response invalid; no obsolete-token retry attempted"
            }
            Self::PendingPersistence => {
                "Rotated Antigravity credentials await persistence; repair local storage and retry without starting a new login"
            }
            Self::ProfileChanged => {
                "Antigravity profile or epoch changed; obsolete credentials were not saved"
            }
            Self::UncertainRotation => {
                "Antigravity refresh outcome is unknown; explicit login required instead of retrying the old refresh token"
            }
            Self::Cancelled => "Antigravity credential waiter cancelled before refresh began",
            Self::Closed => "Antigravity credential manager is shutting down",
        }
    }
}
impl std::fmt::Display for AccessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}
impl std::error::Error for AccessError {}
type AccessResult = std::result::Result<Credential, AccessError>;

#[derive(Clone)]
struct Scope {
    epoch: String,
    profile_id: String,
    refresh_token: Option<String>,
}
impl Scope {
    fn matches(&self, epoch: &str, credential: &Credential) -> bool {
        // Incidental expiry/project/access metadata is not a new login or a
        // replacement for the known-obsolete refresh token. A changed epoch,
        // profile, or committed refresh token invalidates this recovery scope.
        self.epoch == epoch
            && self.profile_id == credential.profile_id
            && self.refresh_token == credential.refresh_token
    }
}
#[derive(Clone)]
struct Pending {
    scope: Scope,
    credential: Credential,
}
struct Flight {
    reply: watch::Receiver<Option<AccessResult>>,
    waiters: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
struct Cooldown {
    scope: Scope,
    until: Instant,
}
#[derive(Default)]
struct State {
    flight: Option<Flight>,
    pending: Option<Pending>,
    uncertain: Option<Scope>,
    cooldown: Option<Cooldown>,
    closed: bool,
}
struct Waiter(Arc<AtomicUsize>);
impl Drop for Waiter {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct RefreshManager {
    store: Store,
    client: reqwest::Client,
    token_url: url::Url,
    client_id: Option<String>,
    client_secret: Option<String>,
    lock_timeout: Duration,
    network_timeout: Duration,
    state: Mutex<State>,
    #[cfg(test)]
    first_failure_barrier: Option<Arc<tests::RetryBarrier>>,
    #[cfg(test)]
    send_attempts: AtomicUsize,
}
impl RefreshManager {
    pub fn new(store: Store, client: reqwest::Client, config: &Config) -> Arc<Self> {
        Arc::new(Self {
            store,
            client,
            token_url: config.token_url.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            lock_timeout: Duration::from_secs(10),
            network_timeout: Duration::from_secs(30),
            state: Mutex::new(State::default()),
            #[cfg(test)]
            first_failure_barrier: None,
            #[cfg(test)]
            send_attempts: AtomicUsize::new(0),
        })
    }
    #[cfg(test)]
    pub fn with_timeouts(mut self: Arc<Self>, lock: Duration, network: Duration) -> Arc<Self> {
        let manager = Arc::get_mut(&mut self).expect("unused test manager");
        manager.lock_timeout = lock;
        manager.network_timeout = network;
        self
    }

    pub async fn access(self: &Arc<Self>) -> AccessResult {
        let (mut reply, _waiter) = {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            if state.closed {
                return Err(AccessError::Closed);
            }
            // A reply is published only AFTER all filesystem work is complete.
            // Reap completed work; never reuse its credential across a new login.
            if state
                .flight
                .as_ref()
                .is_some_and(|flight| flight.reply.borrow().is_some())
            {
                state.flight.take();
            }
            if let Some(flight) = state.flight.as_ref() {
                flight.waiters.fetch_add(1, Ordering::AcqRel);
                (flight.reply.clone(), Waiter(flight.waiters.clone()))
            } else {
                let waiters = Arc::new(AtomicUsize::new(1));
                let (sender, reply) = watch::channel(None);
                let manager = self.clone();
                let subscribers = waiters.clone();
                let task = tokio::spawn(async move {
                    let result = manager.transaction(&subscribers).await;
                    let _ = sender.send(Some(result));
                });
                state.flight = Some(Flight {
                    reply: reply.clone(),
                    waiters: waiters.clone(),
                    task,
                });
                (reply, Waiter(waiters))
            }
        };
        loop {
            let completed = reply.borrow().clone();
            if let Some(result) = completed {
                return result;
            }
            reply.changed().await.map_err(|_| AccessError::Closed)?;
        }
    }

    fn clear_recovery(&self) -> std::result::Result<(), AccessError> {
        let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
        state.pending = None;
        state.uncertain = None;
        state.cooldown = None;
        Ok(())
    }

    /// Called with the stable OS lock held. A pending replacement is never
    /// overwritten with, or retried against, the obsolete refresh credential.
    fn recover(&self, lock: &mut Lock) -> std::result::Result<Option<Credential>, AccessError> {
        let (pending, uncertain) = {
            let state = self.state.lock().map_err(|_| AccessError::Closed)?;
            (state.pending.clone(), state.uncertain.clone())
        };
        let epoch = lock.epoch().map_err(|error| {
            if pending.is_some() {
                AccessError::PendingPersistence
            } else {
                AccessError::Storage(error)
            }
        })?;
        let mut stored = self.store.load().map_err(|error| {
            if pending.is_some() {
                AccessError::PendingPersistence
            } else {
                AccessError::Storage(error)
            }
        })?;
        if let Some(pending) = pending {
            if stored
                .as_ref()
                .is_some_and(|credential| pending.scope.matches(&epoch, credential))
            {
                stored = Some(self.persist(lock, pending)?);
            } else {
                // Logout/new login/another completed refresh owns the new state.
                // Missing storage is not permission to resurrect the old profile.
                self.clear_recovery()?;
            }
        } else if let Some(scope) = uncertain {
            if stored
                .as_ref()
                .is_some_and(|credential| scope.matches(&epoch, credential))
            {
                return Err(AccessError::UncertainRotation);
            }
            self.clear_recovery()?;
        }
        Ok(stored)
    }

    /// No await/timer occurs after a validated replacement is retained. The
    /// filesystem calls are synchronous and are NOT claimed to be cancellable.
    fn persist(&self, lock: &mut Lock, pending: Pending) -> AccessResult {
        {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.pending = Some(pending.clone());
        }
        let epoch = lock.epoch().map_err(|_| AccessError::PendingPersistence)?;
        let stored = self
            .store
            .load()
            .map_err(|_| AccessError::PendingPersistence)?;
        if !stored
            .as_ref()
            .is_some_and(|credential| pending.scope.matches(&epoch, credential))
        {
            self.clear_recovery()?;
            return Err(AccessError::ProfileChanged);
        }
        self.store
            .save(lock, &pending.credential)
            .map_err(|_| AccessError::PendingPersistence)?;
        self.clear_recovery()?;
        Ok(pending.credential)
    }

    async fn transaction(&self, waiters: &AtomicUsize) -> AccessResult {
        // Lock acquisition has its own bound and precedes the network side effect.
        let mut lock = self
            .store
            .lock(self.lock_timeout)
            .await
            .map_err(AccessError::Lock)?;
        let credential = self.recover(&mut lock)?.ok_or(AccessError::Missing)?;
        if credential.expires_at > now().saturating_add(60) {
            return Ok(credential);
        }
        if waiters.load(Ordering::Acquire) == 0 {
            return Err(AccessError::Cancelled);
        }
        let refresh = credential
            .refresh_token
            .as_deref()
            .ok_or(AccessError::LoginRequired)?;
        let client_id = self
            .client_id
            .as_deref()
            .ok_or(AccessError::ClientConfiguration)?;
        let scope = Scope {
            epoch: lock.epoch().map_err(AccessError::Storage)?,
            profile_id: credential.profile_id.clone(),
            refresh_token: credential.refresh_token.clone(),
        };
        {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            if state.closed {
                return Err(AccessError::Closed);
            }
            if state.cooldown.as_ref().is_some_and(|cooldown| {
                cooldown.scope.matches(&scope.epoch, &credential) && Instant::now() < cooldown.until
            }) {
                return Err(AccessError::ConnectCooldown);
            }
            state.cooldown = None;
        }
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id),
        ];
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let deadline = Instant::now() + self.network_timeout;
        let mut phase = NetworkPhase::NotSent;
        let result = tokio::time::timeout_at(deadline, async {
            for attempt in 0..2 {
                {
                    let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
                    if state.closed {
                        return Err(AccessError::StoppedBeforeSend);
                    }
                    if attempt > 0
                        && !retry_budget(deadline.saturating_duration_since(Instant::now()), false)
                    {
                        return Err(AccessError::ConnectFailure);
                    }
                    // No cancellation point between declaring uncertainty and
                    // starting this attempt. An unresolved send/body stays unknown.
                    state.uncertain = Some(scope.clone());
                    phase = NetworkPhase::InFlight;
                }
                #[cfg(test)]
                self.send_attempts.fetch_add(1, Ordering::SeqCst);
                let sent = self
                    .client
                    .post(self.token_url.clone())
                    .timeout(deadline.saturating_duration_since(Instant::now()))
                    .form(&form)
                    .send()
                    .await;
                let response = match sent {
                    Ok(response) => response,
                    Err(error) => {
                        let classification = classify_send_error(&error);
                        if classification == SendFailure::Ambiguous {
                            return Err(AccessError::Network);
                        }
                        phase = NetworkPhase::ProvenPreSend;
                        {
                            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
                            state.uncertain = None;
                            if state.closed {
                                return Err(AccessError::StoppedBeforeSend);
                            }
                        }
                        if attempt == 1 || classification == SendFailure::PreSendNonRetryableSafe {
                            return Err(AccessError::ConnectFailure);
                        }
                        #[cfg(test)]
                        if let Some(barrier) = &self.first_failure_barrier {
                            barrier.pause().await;
                        }
                        if self.state.lock().map_err(|_| AccessError::Closed)?.closed {
                            return Err(AccessError::StoppedBeforeSend);
                        }
                        if !retry_budget(deadline.saturating_duration_since(Instant::now()), true) {
                            return Err(AccessError::ConnectFailure);
                        }
                        tokio::time::sleep(RETRY_BACKOFF).await;
                        // Loop entry rechecks shutdown and the 16s reserve AFTER
                        // backoff; scheduler delay cannot renew or steal the budget.
                        continue;
                    }
                };
                let token = bounded_json(response)
                    .await
                    .map_err(|_| AccessError::InvalidResponse)?;
                let mut replacement = credential.clone();
                update_tokens(&mut replacement, &token)
                    .map_err(|_| AccessError::InvalidResponse)?;
                return Ok(replacement);
            }
            Err(AccessError::ConnectFailure)
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) if phase == NetworkPhase::InFlight => Err(AccessError::NetworkTimeout),
            Err(_) => Err(AccessError::ConnectFailure),
        };
        let replacement = match result {
            Err(AccessError::ConnectFailure) => {
                let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
                if state.closed {
                    return Err(AccessError::StoppedBeforeSend);
                }
                // Only complete pre-send proof (or never starting a send) reaches
                // this cooldown. An ambiguous second attempt must never clear or
                // hide uncertainty. This rate bound is manager-local, not durable.
                state.cooldown = Some(Cooldown {
                    scope: scope.clone(),
                    until: Instant::now() + CONNECT_COOLDOWN,
                });
                return Err(AccessError::ConnectFailure);
            }
            other => other?,
        };
        // The manager's network deadline ended above. A disconnected waiter or
        // elapsed network timer cannot discard the now validated replacement.
        self.persist(
            &mut lock,
            Pending {
                scope,
                credential: replacement,
            },
        )
    }

    /// Quiesce new waiters, join owned maintenance, and retry ONLY a pending local
    /// write. No refresh or inference is initiated during graceful shutdown.
    pub async fn shutdown(&self) -> std::result::Result<(), AccessError> {
        let reply = {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.closed = true;
            state.flight.as_ref().map(|flight| flight.reply.clone())
        };
        // Keep the task handle owned by the manager if a shutdown waiter itself
        // is cancelled. Only reap after the transaction publishes completion.
        if let Some(mut reply) = reply {
            while reply.borrow().is_none() {
                if reply.changed().await.is_err() {
                    break;
                }
            }
        }
        let flight = self
            .state
            .lock()
            .map_err(|_| AccessError::Closed)?
            .flight
            .take();
        if let Some(flight) = flight {
            flight.task.await.map_err(|_| AccessError::Closed)?;
        }
        let recovery = {
            let state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.pending.is_some() || state.uncertain.is_some()
        };
        if recovery {
            let mut lock = self
                .store
                .lock(self.lock_timeout)
                .await
                .map_err(AccessError::Lock)?;
            self.recover(&mut lock)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{error::Error, io, net::SocketAddr};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Notify,
    };

    #[derive(Default)]
    pub(super) struct RetryBarrier {
        entered: Notify,
        release: Notify,
    }
    impl RetryBarrier {
        pub(super) async fn pause(&self) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        async fn wait(&self) {
            tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
                .await
                .expect("first refusal reached retry barrier");
        }
    }

    fn credential() -> Credential {
        Credential {
            profile_id: "synthetic-profile".into(),
            access_token: "synthetic-expired-access".into(),
            refresh_token: Some("synthetic-refresh".into()),
            project_id: Some("synthetic-project".into()),
            expires_at: 0,
        }
    }

    async fn fixture(
        url: url::Url,
        client: Option<reqwest::Client>,
        barrier: Option<Arc<RetryBarrier>>,
        budget: Duration,
    ) -> (tempfile::TempDir, Arc<RefreshManager>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("auth.json"));
        let lock = store.lock(Duration::from_secs(1)).await.unwrap();
        store.save(&lock, &credential()).unwrap();
        drop(lock);
        let config = Config {
            token_url: url,
            client_id: Some("synthetic-client".into()),
            ..Config::default()
        };
        let client = client.unwrap_or_else(|| config.client_builder().no_proxy().build().unwrap());
        let mut manager = RefreshManager::new(store, client, &config)
            .with_timeouts(Duration::from_secs(1), budget);
        Arc::get_mut(&mut manager).unwrap().first_failure_barrier = barrier;
        (directory, manager)
    }

    async fn closed_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }
    fn token_url(address: SocketAddr) -> url::Url {
        format!("http://{address}/token").parse().unwrap()
    }
    fn attempts(manager: &RefreshManager) -> usize {
        manager.send_attempts.load(Ordering::SeqCst)
    }
    async fn failure(manager: &Arc<RefreshManager>) -> AccessError {
        manager.access().await.err().expect("expected fixed error")
    }

    async fn read_request<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
        let mut raw = Vec::new();
        let header_end = loop {
            let mut byte = [0u8];
            stream.read_exact(&mut byte).await.unwrap();
            raw.push(byte[0]);
            assert!(raw.len() < 8192);
            if raw.ends_with(b"\r\n\r\n") {
                break raw.len();
            }
        };
        let headers = std::str::from_utf8(&raw).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(length < 8192);
        raw.resize(header_end + length, 0);
        stream.read_exact(&mut raw[header_end..]).await.unwrap();
        raw
    }

    async fn successful_server(listener: TcpListener, calls: Arc<AtomicUsize>) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request.starts_with(b"POST /token HTTP/1.1\r\n"));
        assert!(
            request
                .windows(b"refresh_token=synthetic-refresh".len())
                .any(|part| part == b"refresh_token=synthetic-refresh")
        );
        calls.fetch_add(1, Ordering::SeqCst);
        let body = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600,"token_type":"Bearer"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn refresh_connect_real_refusal_then_success_coalesces_and_survives_cancelled_waiter() {
        let address = closed_address().await;
        let client = Config::default()
            .client_builder()
            .no_proxy()
            .build()
            .unwrap();
        let error = client
            .post(token_url(address))
            .form(&[("refresh_token", "synthetic")])
            .send()
            .await
            .err()
            .unwrap();
        // Positive transport guard: never widen this if dependency wrapping changes.
        assert_eq!(
            classify_send_error(&error),
            SendFailure::PreSendTransientRetryable
        );
        let barrier = Arc::new(RetryBarrier::default());
        let (_directory, manager) = fixture(
            token_url(address),
            Some(client),
            Some(barrier.clone()),
            Duration::from_secs(30),
        )
        .await;
        let leader = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.access().await })
        };
        barrier.wait().await;
        let mut followers = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            followers.push(tokio::spawn(async move { manager.access().await }));
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if manager
                    .state
                    .lock()
                    .unwrap()
                    .flight
                    .as_ref()
                    .unwrap()
                    .waiters
                    .load(Ordering::Acquire)
                    == 9
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        leader.abort();
        let _ = leader.await;
        let listener = TcpListener::bind(address).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(successful_server(listener, calls.clone()));
        barrier.release.notify_one();
        for follower in followers {
            let result = follower.await.unwrap().unwrap();
            assert_eq!(result.refresh_token.as_deref(), Some("rotated-refresh"));
        }
        server.await.unwrap();
        assert_eq!(attempts(&manager), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            manager
                .store
                .load()
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh")
        );
        assert!(manager.state.lock().unwrap().cooldown.is_none());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_connect_two_refusals_cooldown_expiry_and_scope_rules() {
        let (_directory, manager) = fixture(
            token_url(closed_address().await),
            None,
            None,
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(failure(&manager).await, AccessError::ConnectFailure);
        assert_eq!(attempts(&manager), 2);
        for _ in 0..8 {
            assert_eq!(failure(&manager).await, AccessError::ConnectCooldown);
        }
        assert_eq!(attempts(&manager), 2);
        assert!(manager.state.lock().unwrap().uncertain.is_none());
        let mut same_scope = manager.store.load().unwrap().unwrap();
        same_scope.expires_at = 1;
        same_scope.access_token = "incidental-access".into();
        same_scope.project_id = Some("incidental-project".into());
        let lock = manager.store.lock(Duration::from_secs(1)).await.unwrap();
        manager.store.save(&lock, &same_scope).unwrap();
        drop(lock);
        assert_eq!(failure(&manager).await, AccessError::ConnectCooldown);
        // Expire only cooldown, without pausing Tokio's real-socket clock.
        manager
            .state
            .lock()
            .unwrap()
            .cooldown
            .as_mut()
            .unwrap()
            .until = tokio::time::Instant::now();
        assert_eq!(failure(&manager).await, AccessError::ConnectFailure);
        assert_eq!(attempts(&manager), 4);
        for change in 0..3 {
            let mut lock = manager.store.lock(Duration::from_secs(1)).await.unwrap();
            match change {
                0 => {
                    lock.advance().unwrap();
                }
                1 => {
                    same_scope.profile_id = "new-profile".into();
                }
                _ => {
                    same_scope.refresh_token = Some("new-refresh".into());
                }
            }
            manager.store.save(&lock, &same_scope).unwrap();
            drop(lock);
            assert_eq!(failure(&manager).await, AccessError::ConnectFailure);
            assert_eq!(attempts(&manager), 6 + change * 2);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_connect_insufficient_budget_and_shutdown_before_retry_are_safe() {
        let (_directory, manager) = fixture(
            token_url(closed_address().await),
            None,
            None,
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(failure(&manager).await, AccessError::ConnectFailure);
        assert_eq!(attempts(&manager), 1);
        assert!(manager.state.lock().unwrap().uncertain.is_none());
        manager.shutdown().await.unwrap();

        let barrier = Arc::new(RetryBarrier::default());
        let (_directory, manager) = fixture(
            token_url(closed_address().await),
            None,
            Some(barrier.clone()),
            Duration::from_secs(30),
        )
        .await;
        let waiter = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.access().await })
        };
        barrier.wait().await;
        let closing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.shutdown().await })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !manager.state.lock().unwrap().closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        barrier.release.notify_one();
        assert_eq!(
            waiter.await.unwrap().err(),
            Some(AccessError::StoppedBeforeSend)
        );
        closing.await.unwrap().unwrap();
        assert_eq!(attempts(&manager), 1);
        assert!(manager.state.lock().unwrap().uncertain.is_none());
        assert_eq!(failure(&manager).await, AccessError::Closed);
    }

    #[tokio::test]
    async fn refresh_connect_deadline_during_proven_presend_pause_is_not_uncertain() {
        let barrier = Arc::new(RetryBarrier::default());
        let (_directory, manager) = fixture(
            token_url(closed_address().await),
            None,
            Some(barrier.clone()),
            Duration::from_millis(150),
        )
        .await;
        let waiter = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.access().await })
        };
        barrier.wait().await;
        assert_eq!(
            waiter.await.unwrap().err(),
            Some(AccessError::ConnectFailure)
        );
        assert_eq!(attempts(&manager), 1);
        assert!(manager.state.lock().unwrap().uncertain.is_none());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_connect_sent_body_drop_partial_response_timeout_and_status_never_retry() {
        for mode in 0..4 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = token_url(listener.local_addr().unwrap());
            let received = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let server = {
                let received = received.clone();
                let release = release.clone();
                tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = read_request(&mut stream).await;
                    assert!(request.starts_with(b"POST "));
                    received.notify_one();
                    match mode {
                        1 => {
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"access_token\":").await.unwrap();
                        }
                        2 => {
                            release.notified().await;
                        }
                        3 => {
                            stream
                                .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n")
                                .await
                                .unwrap();
                        }
                        _ => {}
                    }
                })
            };
            let (_directory, manager) = fixture(url, None, None, Duration::from_millis(500)).await;
            let result = failure(&manager).await;
            assert!(matches!(
                result,
                AccessError::Network | AccessError::NetworkTimeout | AccessError::InvalidResponse
            ));
            received.notified().await;
            assert_eq!(attempts(&manager), 1);
            assert_eq!(failure(&manager).await, AccessError::UncertainRotation);
            assert!(manager.state.lock().unwrap().cooldown.is_none());
            release.notify_one();
            server.await.unwrap();
            assert_eq!(
                manager.shutdown().await,
                Err(AccessError::UncertainRotation)
            );
        }
    }

    #[tokio::test]
    async fn refresh_connect_safe_first_then_ambiguous_second_never_reuses_token() {
        let address = closed_address().await;
        let barrier = Arc::new(RetryBarrier::default());
        let (_directory, manager) = fixture(
            token_url(address),
            None,
            Some(barrier.clone()),
            Duration::from_secs(30),
        )
        .await;
        let waiter = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.access().await })
        };
        barrier.wait().await;
        let listener = TcpListener::bind(address).await.unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(read_request(&mut stream).await.starts_with(b"POST "));
        });
        barrier.release.notify_one();
        assert_eq!(waiter.await.unwrap().err(), Some(AccessError::Network));
        server.await.unwrap();
        assert_eq!(attempts(&manager), 2);
        assert_eq!(failure(&manager).await, AccessError::UncertainRotation);
        assert_eq!(attempts(&manager), 2);
        assert!(manager.state.lock().unwrap().cooldown.is_none());
    }

    #[tokio::test]
    async fn refresh_connect_shutdown_during_second_send_joins_and_commits() {
        let address = closed_address().await;
        let barrier = Arc::new(RetryBarrier::default());
        let (_directory, manager) = fixture(
            token_url(address),
            None,
            Some(barrier.clone()),
            Duration::from_secs(30),
        )
        .await;
        let waiter = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.access().await })
        };
        barrier.wait().await;
        let listener = TcpListener::bind(address).await.unwrap();
        let issued = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = {
            let issued = issued.clone();
            let release = release.clone();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(read_request(&mut stream).await.starts_with(b"POST "));
                issued.notify_one();
                release.notified().await;
                let body = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            })
        };
        barrier.release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), issued.notified())
            .await
            .unwrap();
        waiter.abort();
        let _ = waiter.await;
        let closing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.shutdown().await })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !manager.state.lock().unwrap().closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!closing.is_finished());
        release.notify_one();
        closing.await.unwrap().unwrap();
        server.await.unwrap();
        assert_eq!(attempts(&manager), 2);
        assert_eq!(
            manager
                .store
                .load()
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh")
        );
        assert!(manager.state.lock().unwrap().uncertain.is_none());
    }

    #[derive(Debug)]
    struct UnknownDns;
    impl reqwest::dns::Resolve for UnknownDns {
        fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async {
                Err(Box::new(io::Error::other("synthetic-private-DNS"))
                    as Box<dyn Error + Send + Sync>)
            })
        }
    }

    #[tokio::test]
    async fn refresh_connect_private_connector_timeout_and_unknown_dns_are_not_retried() {
        let client = Config::default()
            .client_builder()
            .no_proxy()
            .dns_resolver(Arc::new(UnknownDns))
            .build()
            .unwrap();
        let (_directory, manager) = fixture(
            "https://synthetic.invalid/token".parse().unwrap(),
            Some(client),
            None,
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(failure(&manager).await, AccessError::ConnectFailure);
        assert_eq!(attempts(&manager), 1);
        assert_eq!(failure(&manager).await, AccessError::ConnectCooldown);
        manager.shutdown().await.unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let release = Arc::new(Notify::new());
        let server = {
            let release = release.clone();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(read_request(&mut stream).await.starts_with(b"CONNECT "));
                release.notified().await;
            })
        };
        // The shortened connector deadline is a test client setting only. It
        // yields reqwest's private TimedOut, NOT an allowlisted io::TimedOut.
        let client = Config::default()
            .client_builder()
            .no_proxy()
            .proxy(reqwest::Proxy::https(proxy).unwrap())
            .connect_timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let (_directory, manager) = fixture(
            "https://localhost/token".parse().unwrap(),
            Some(client),
            None,
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(failure(&manager).await, AccessError::ConnectFailure);
        assert_eq!(attempts(&manager), 1);
        assert_eq!(failure(&manager).await, AccessError::ConnectCooldown);
        release.notify_one();
        server.await.unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_connect_nested_connect_under_send_request_is_ambiguous() {
        let nested = hyper_connect_error(io::Error::from(io::ErrorKind::ConnectionRefused)).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = token_url(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes)).await;
        });
        let body =
            reqwest::Body::wrap_stream(futures_util::stream::iter([Err::<bytes::Bytes, _>(
                nested,
            )]));
        let error = Config::default()
            .client_builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .body(body)
            .send()
            .await
            .err()
            .unwrap();
        assert!(
            error.is_connect(),
            "test must exercise incidental nested Connect"
        );
        assert_eq!(classify_send_error(&error), SendFailure::Ambiguous);
        server.await.unwrap();
    }

    // Public synthetic localhost fixtures generated offline for these tests only.
    // This key is NOT a production credential. CA/leaf validity ends in 2046;
    // only this test client's additional root trusts the CA, never production.
    const TEST_CA: &str = "MIIBezCCASGgAwIBAgIUGLhYg9q/Al/VHS8Zw/knkCtNSYwwCgYIKoZIzj0EAwIwKzEpMCcGA1UEAwwgUHVibGljIHN5bnRoZXRpYyByZWZyZXNoIHRlc3QgQ0EwHhcNMjYxMDIxMTA1NzM5WhcNNDYxMDE2MTA1NzM5WjArMSkwJwYDVQQDDCBQdWJsaWMgc3ludGhldGljIHJlZnJlc2ggdGVzdCBDQTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABKPFbXyxtoHecqg9QFPmnUaRwqMlr51ZGT7lWJrjU6wdiAm9QgN6bhod1c6l3jgBhd9f7tSV5aGPCrAF5fTIyH2jIzAhMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEGMAoGCCqGSM49BAMCA0gAMEUCIQCNSOmNAj8vypFDV9QCxq0MKvqkp0vQ9DuGerTmk3ltwwIgTVWj2Zw4fWKWVsttU3GMe2L6s8+DtYcDgsfQmKlU8MI=";
    const TEST_CERT: &str = "MIIBkjCCATigAwIBAgIUbFZU4Ue3etDnol7vW2OIhfdiZxowCgYIKoZIzj0EAwIwKzEpMCcGA1UEAwwgUHVibGljIHN5bnRoZXRpYyByZWZyZXNoIHRlc3QgQ0EwHhcNMjYxMDIxMTA1NzM5WhcNNDYxMDE2MTA1NzM5WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQPJVIZ/jqVeUIxkgg7aKln+59Edhs2wSlDuR8VfCgrD3Vw30c6XzGO6ztD29559ZoSCF6RQIYqPLQfNcY/pIOUo1EwTzAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwDAYDVR0TAQH/BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwCgYIKoZIzj0EAwIDSAAwRQIhAK+Bm7mGxkegBKkFnOCn3fsQP405jmiH5rQ7kkmvamiQAiBRIjQG9siBjZLJk8eidWK49EkXb8fYXJtzZn5vnAUdGg==";
    const TEST_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgrHg+Obl2CHfYNy0VWcSYpRdGNktCYRTDWsnD/6tegyChRANCAAQPJVIZ/jqVeUIxkgg7aKln+59Edhs2wSlDuR8VfCgrD3Vw30c6XzGO6ztD29559ZoSCF6RQIYqPLQfNcY/pIOU";
    fn der(value: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .unwrap()
    }
    fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(der(TEST_CERT))],
                rustls::pki_types::PrivatePkcs8KeyDer::from(der(TEST_KEY)).into(),
            )
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    #[tokio::test]
    async fn refresh_connect_proxy_auth_tls_certificate_and_tunneled_post_drop() {
        // 407, bad certificate, then trusted tunnel which consumes the OAuth body.
        for mode in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy = format!("http://{}", listener.local_addr().unwrap());
            let bodies = Arc::new(AtomicUsize::new(0));
            let seen = bodies.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let connect = read_request(&mut stream).await;
                assert!(connect.starts_with(b"CONNECT localhost:443 HTTP/1.1\r\n"));
                assert!(
                    !connect
                        .windows(b"synthetic-refresh".len())
                        .any(|p| p == b"synthetic-refresh")
                );
                if mode == 0 {
                    stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                    return;
                }
                stream
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                match tls_acceptor().accept(stream).await {
                    Ok(mut tls) => {
                        assert_eq!(mode, 2, "untrusted certificate unexpectedly accepted");
                        let request = read_request(&mut tls).await;
                        assert!(request.starts_with(b"POST /token HTTP/1.1\r\n"));
                        seen.fetch_add(1, Ordering::SeqCst);
                        // Full OAuth body received; drop without a response.
                    }
                    Err(_) => {
                        assert_eq!(mode, 1);
                    }
                }
            });
            let mut builder = Config::default()
                .client_builder()
                .no_proxy()
                .proxy(reqwest::Proxy::https(proxy).unwrap());
            if mode == 2 {
                builder = builder
                    .add_root_certificate(reqwest::Certificate::from_der(&der(TEST_CA)).unwrap());
            }
            let (_directory, manager) = fixture(
                "https://localhost/token".parse().unwrap(),
                Some(builder.build().unwrap()),
                None,
                Duration::from_secs(30),
            )
            .await;
            let result = failure(&manager).await;
            server.await.unwrap();
            assert_eq!(attempts(&manager), 1);
            if mode == 2 {
                assert_eq!(result, AccessError::Network);
                assert_eq!(bodies.load(Ordering::SeqCst), 1);
                assert_eq!(failure(&manager).await, AccessError::UncertainRotation);
            } else {
                assert_eq!(result, AccessError::ConnectFailure);
                assert_eq!(bodies.load(Ordering::SeqCst), 0);
                assert_eq!(failure(&manager).await, AccessError::ConnectCooldown);
                assert!(manager.state.lock().unwrap().uncertain.is_none());
                manager.shutdown().await.unwrap();
            }
        }
    }

    #[derive(Debug)]
    struct Cause(Option<Box<dyn Error + Send + Sync>>);
    impl std::fmt::Display for Cause {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("synthetic-private-error")
        }
    }
    impl Error for Cause {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.as_deref().map(|e| e as _)
        }
    }
    #[derive(Debug)]
    struct Cycle;
    impl std::fmt::Display for Cycle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("synthetic-cycle")
        }
    }
    impl Error for Cycle {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self)
        }
    }

    async fn hyper_connect_error(error: io::Error) -> hyper_util::client::legacy::Error {
        let error = Arc::new(Mutex::new(Some(error)));
        let connector = tower::service_fn(move |_: http::Uri| {
            let error = error.lock().unwrap().take().unwrap();
            std::future::ready(Err::<hyper_util::rt::TokioIo<tokio::net::TcpStream>, _>(
                error,
            ))
        });
        let client: hyper_util::client::legacy::Client<_, http_body_util::Empty<bytes::Bytes>> =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(connector);
        client
            .get("http://localhost/".parse().unwrap())
            .await
            .err()
            .unwrap()
    }

    #[test]
    fn refresh_connect_retry_budget_reserves_full_connect_window_after_backoff() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert!(!retry_budget(Duration::from_millis(16_199), true));
        assert!(retry_budget(Duration::from_millis(16_200), true));
        assert!(!retry_budget(Duration::from_millis(15_999), false));
        assert!(retry_budget(Duration::from_secs(16), false));
        for error in [
            AccessError::ConnectFailure,
            AccessError::ConnectCooldown,
            AccessError::StoppedBeforeSend,
        ] {
            assert!(!format!("{error} {error:?}").contains("synthetic-private"));
        }
    }

    #[tokio::test]
    async fn refresh_connect_classifier_complete_graph_tls_deny_and_outer_io() {
        for kind in [io::ErrorKind::ConnectionRefused, io::ErrorKind::TimedOut] {
            let error = hyper_connect_error(io::Error::from(kind)).await;
            assert_eq!(
                classify_connect_graph(&error),
                SendFailure::PreSendTransientRetryable
            );
        }
        let error =
            hyper_connect_error(io::Error::other(io::Error::from(io::ErrorKind::TimedOut))).await;
        assert_eq!(
            classify_connect_graph(&error),
            SendFailure::PreSendNonRetryableSafe
        );
        let error = hyper_connect_error(io::Error::new(
            io::ErrorKind::TimedOut,
            rustls::Error::General("synthetic-private-TLS-error".into()),
        ))
        .await;
        assert_eq!(
            classify_connect_graph(&error),
            SendFailure::PreSendNonRetryableSafe
        );
        let error = io::Error::other(
            hyper_connect_error(io::Error::from(io::ErrorKind::ConnectionRefused)).await,
        );
        assert_eq!(classify_connect_graph(&error), SendFailure::Ambiguous);
        assert_eq!(classify_connect_graph(&Cycle), SendFailure::Ambiguous);
        let mut error: Box<dyn Error + Send + Sync> =
            Box::new(hyper_connect_error(io::Error::from(io::ErrorKind::ConnectionRefused)).await);
        // Hyper + IO consumes two of the 16 distinct-node allowance.
        for _ in 0..14 {
            error = Box::new(Cause(Some(error)));
        }
        assert_eq!(
            classify_connect_graph(&*error),
            SendFailure::PreSendTransientRetryable
        );
        error = Box::new(Cause(Some(error)));
        assert_eq!(classify_connect_graph(&*error), SendFailure::Ambiguous);
        // IO.source and IO.get_ref reconverge on the same inner source. It must
        // not be mistaken for a cycle or consume another distinct-node slot.
        let inner = Cause(Some(Box::new(io::Error::from(io::ErrorKind::TimedOut))));
        let error = hyper_connect_error(io::Error::new(io::ErrorKind::TimedOut, inner)).await;
        assert_eq!(
            classify_connect_graph(&error),
            SendFailure::PreSendTransientRetryable
        );
    }
}
