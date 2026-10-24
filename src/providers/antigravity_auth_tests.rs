use super::{
    antigravity::{
        auth::{self, Auth, Credential, Store},
        config::Config,
    },
    antigravity_http_tests::Server,
};
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn expired() -> Credential {
    Credential {
        profile_id: "profile-stable".into(),
        project_id: Some("project".into()),
        access_token: "expired-access".into(),
        refresh_token: Some("original-refresh".into()),
        expires_at: 0,
    }
}
async fn saved(store: &Store, credential: &Credential) {
    let lock = store.lock(Duration::from_secs(1)).await.unwrap();
    store.save(&lock, credential).unwrap();
}

struct Tokens {
    count: AtomicUsize,
    started: tokio::sync::Notify,
    rotate: bool,
}
async fn token(State(state): State<Arc<Tokens>>) -> Json<Value> {
    state.count.fetch_add(1, Ordering::SeqCst);
    state.started.notify_one();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut value = json!({"access_token":"new-access","expires_in":3600,"token_type":"Bearer"});
    if state.rotate {
        value["refresh_token"] = json!("rotated-refresh");
    }
    Json(value)
}
async fn auth_fixture(rotate: bool) -> (tempfile::TempDir, Server, Arc<Auth>, Arc<Tokens>) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    saved(&store, &expired()).await;
    let state = Arc::new(Tokens {
        count: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        rotate,
    });
    let server = Server::start(
        Router::new()
            .route("/token", post(token))
            .with_state(state.clone()),
    )
    .await;
    let config = Config {
        token_url: server.url.join("/token").unwrap(),
        client_id: Some("synthetic-client".into()),
        ..Config::default()
    };
    let auth = Arc::new(Auth::new(config, store).unwrap());
    (directory, server, auth, state)
}

struct PausedRotation {
    requests: AtomicUsize,
    issued: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

async fn rotating_token(
    State(state): State<Arc<PausedRotation>>,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let index = state.requests.fetch_add(1, Ordering::SeqCst);
    if index > 0
        && form
            .get("refresh_token")
            .is_some_and(|t| t == "original-refresh")
    {
        return axum::response::IntoResponse::into_response((
            http::StatusCode::BAD_REQUEST,
            "obsolete refresh token",
        ));
    }
    state.issued.notify_one();
    let body = axum::body::Body::from_stream(futures_util::stream::once(async move {
        state.release.notified().await;
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"{\"access_token\":\"rotated-access\",\"refresh_token\":\"rotated-refresh\",\"expires_in\":3600,\"token_type\":\"Bearer\"}"))
    }));
    axum::response::Response::builder()
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

async fn paused_rotation() -> (
    tempfile::TempDir,
    Server,
    Auth,
    Arc<PausedRotation>,
    Arc<AtomicUsize>,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    saved(&store, &expired()).await;
    let state = Arc::new(PausedRotation {
        requests: AtomicUsize::new(0),
        issued: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let generations = Arc::new(AtomicUsize::new(0));
    let counted = generations.clone();
    let server = Server::start(Router::new().route("/token", post(rotating_token)).route("/v1internal:streamGenerateContent", post(move |headers: http::HeaderMap| {
        let counted = counted.clone(); async move {
            assert_eq!(headers["authorization"], "Bearer rotated-access");
            counted.fetch_add(1, Ordering::SeqCst);
            ([ ("content-type", "text/event-stream") ], format!("data: {}\n\n", json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}})))
        }
    })).with_state(state.clone())).await;
    let config = Config {
        base_url: server.url.clone(),
        token_url: server.url.join("/token").unwrap(),
        client_id: Some("synthetic-client".into()),
        ..Config::default()
    };
    let auth = Auth::new(config, store).unwrap();
    (directory, server, auth, state, generations)
}

#[tokio::test]
async fn antigravity_refresh_rotation_persists_after_inference_disconnect_without_generation() {
    let (_directory, _upstream, auth, state, generations) = paused_rotation().await;
    let store = auth.store.clone();
    let monitor = crate::monitor::MonitorHandle::new(10);
    let registry = crate::registry::Registry::from_providers(
        crate::config::AliasProvider::Codex,
        [
            Arc::new(super::antigravity::AntigravityProvider::with_auth(auth))
                as Arc<dyn crate::provider::Provider>,
        ],
    );
    let proxy = Server::start(crate::server::app_with_features(
        Arc::new(registry),
        Some(monitor.clone()),
        crate::server::AppFeatures::default(),
    ))
    .await;
    let mut downstream = tokio::net::TcpStream::connect(("127.0.0.1", proxy.url.port().unwrap()))
        .await
        .unwrap();
    let body = json!({"model":"antigravity/gemini-4-argon-low","max_tokens":32,"messages":[{"role":"user","content":"hello"}]}).to_string();
    downstream.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    state.issued.notified().await;
    drop(downstream);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !monitor.snapshot().active.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.release.notify_one();
    let persisted = tokio::time::timeout(Duration::from_secs(2), async {
        while store.load().unwrap().unwrap().refresh_token.as_deref() != Some("rotated-refresh") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        persisted.is_ok(),
        "caller cancellation discarded an issued replacement credential"
    );
    assert_eq!(
        generations.load(Ordering::SeqCst),
        0,
        "credential maintenance must never continue inference"
    );
    let response = reqwest::Client::new()
        .post(proxy.url.join("/v1/messages").unwrap())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        state.requests.load(Ordering::SeqCst),
        1,
        "obsolete refresh token was reused"
    );
    assert_eq!(generations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn antigravity_pending_rotation_survives_persistence_failure_without_obsolete_retry() {
    let (directory, _upstream, auth, state, _) = paused_rotation().await;
    let auth = Arc::new(auth);
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    state.issued.notified().await;
    let path = directory.path().join("auth.json");
    let backup = directory.path().join("auth.before-fault.json");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    state.release.notify_one();
    assert!(
        matches!(
            worker.await.unwrap(),
            Err(auth::AccessError::PendingPersistence)
        ),
        "unwritable credential destination must report typed pending persistence"
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    // An incidental metadata edit is NOT a new profile/epoch/token. It cannot
    // authorize retrying the refresh token whose replacement is pending.
    let mut same_scope = auth.store.load().unwrap().unwrap();
    same_scope.expires_at = 1;
    saved(&auth.store, &same_scope).await;
    let next = auth
        .access()
        .await
        .expect("manager must persist its pending rotated credential without reusing old token");
    assert_eq!(next.refresh_token.as_deref(), Some("rotated-refresh"));
    assert_eq!(
        auth.store.load().unwrap().unwrap().refresh_token.as_deref(),
        Some("rotated-refresh")
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn antigravity_cancelled_lock_waiter_does_not_begin_refresh_network() {
    let (_directory, _upstream, auth, state, _) = paused_rotation().await;
    let auth = Arc::new(auth);
    let lock = auth.store.lock(Duration::from_secs(1)).await.unwrap();
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    worker.abort();
    let _ = worker.await;
    drop(lock);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn antigravity_refresh_lock_deadline_precedes_network_and_timeout_never_retries_old_token() {
    let (_directory, _server, auth, state, _) = paused_rotation().await;
    let auth =
        Arc::new(auth.with_refresh_timeouts(Duration::from_millis(20), Duration::from_secs(2)));
    let lock = auth.store.lock(Duration::from_secs(1)).await.unwrap();
    assert!(matches!(
        auth.access().await,
        Err(auth::AccessError::Lock(_))
    ));
    assert_eq!(state.requests.load(Ordering::SeqCst), 0);
    drop(lock);
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    state.issued.notified().await;
    state.release.notify_one();
    assert!(worker.await.unwrap().is_ok());
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);

    let (_directory, _server, auth, state, _) = paused_rotation().await;
    let auth =
        Arc::new(auth.with_refresh_timeouts(Duration::from_secs(1), Duration::from_millis(500)));
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    state.issued.notified().await;
    assert!(worker.await.unwrap().is_err());
    assert!(matches!(
        auth.access().await,
        Err(auth::AccessError::UncertainRotation)
    ));
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    drop(auth.store.lock(Duration::from_secs(1)).await.unwrap());
}

#[tokio::test]
async fn antigravity_graceful_manager_shutdown_joins_maintenance_without_inference() {
    let (_directory, _server, auth, state, generations) = paused_rotation().await;
    let auth = Arc::new(auth);
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    state.issued.notified().await;
    worker.abort();
    let _ = worker.await;
    // Cancellation of a shutdown waiter must not discard the owned task handle.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), auth.finish_maintenance())
            .await
            .is_err()
    );
    assert!(matches!(
        auth.access().await,
        Err(auth::AccessError::Closed)
    ));
    state.release.notify_one();
    auth.finish_maintenance().await.unwrap();
    assert_eq!(
        auth.store.load().unwrap().unwrap().refresh_token.as_deref(),
        Some("rotated-refresh")
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(generations.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn antigravity_validated_rotation_persistence_outlives_waiter_and_network_timer() {
    let (_directory, _server, auth, state, generations) = paused_rotation().await;
    let saving = Arc::new(tokio::sync::Notify::new());
    let (release_io, io_wait) = std::sync::mpsc::channel();
    let io_wait = std::sync::Mutex::new(io_wait);
    let notify = saving.clone();
    let store = auth.store.clone().with_before_save(Arc::new(move || {
        notify.notify_one();
        // A controlled synchronous filesystem-phase pause, not a claim that
        // kernel IO can be interrupted by async cancellation.
        io_wait
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
    }));
    let auth = Arc::new(
        Auth::new(auth.config.clone(), store)
            .unwrap()
            .with_refresh_timeouts(Duration::from_secs(1), Duration::from_millis(200)),
    );
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    state.issued.notified().await;
    state.release.notify_one();
    saving.notified().await;
    worker.abort();
    let _ = worker.await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    release_io.send(()).unwrap();
    auth.finish_maintenance().await.unwrap();
    assert_eq!(
        auth.store.load().unwrap().unwrap().refresh_token.as_deref(),
        Some("rotated-refresh")
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(generations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn antigravity_pending_rotation_cannot_resurrect_logout_or_overwrite_new_login() {
    for logout in [true, false] {
        let (directory, _server, auth, state, _) = paused_rotation().await;
        let auth = Arc::new(auth);
        let worker = {
            let auth = auth.clone();
            tokio::spawn(async move { auth.access().await })
        };
        state.issued.notified().await;
        let path = directory.path().join("auth.json");
        let backup = directory.path().join("auth.before-fault.json");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        state.release.notify_one();
        assert!(matches!(
            worker.await.unwrap(),
            Err(auth::AccessError::PendingPersistence)
        ));
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        if logout {
            auth.logout().await.unwrap();
            assert!(matches!(
                auth.access().await,
                Err(auth::AccessError::Missing)
            ));
            assert!(!path.exists());
        } else {
            let mut newer = expired();
            newer.profile_id = "new-profile".into();
            newer.access_token = "new-login-access".into();
            newer.expires_at = auth::now() + 3600;
            let mut lock = auth.store.lock(Duration::from_secs(1)).await.unwrap();
            lock.advance().unwrap();
            auth.store.save(&lock, &newer).unwrap();
            drop(lock);
            assert_eq!(
                auth.access().await.unwrap().access_token,
                "new-login-access"
            );
            assert_eq!(
                auth.store.load().unwrap().unwrap().profile_id,
                "new-profile"
            );
        }
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn antigravity_logout_after_cancelled_refresh_waits_then_clears_without_resurrection() {
    let (_directory, _server, auth, state, generations) = paused_rotation().await;
    let auth = Arc::new(auth);
    let worker = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access().await })
    };
    state.issued.notified().await;
    worker.abort();
    let _ = worker.await;
    let logout = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.logout().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!logout.is_finished());
    state.release.notify_one();
    logout.await.unwrap().unwrap();
    assert!(auth.store.load().unwrap().is_none());
    assert!(matches!(
        auth.access().await,
        Err(auth::AccessError::Missing)
    ));
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(generations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn antigravity_refresh_connect_failure_retains_session_and_cools_down() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    saved(&store, &expired()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config = Config {
        token_url: format!("http://{address}/token").parse().unwrap(),
        client_id: Some("synthetic-client".into()),
        ..Config::default()
    };
    // A short budget cannot accommodate the second connection's 16s reserve.
    let auth = Auth::new(config, store)
        .unwrap()
        .with_refresh_timeouts(Duration::from_secs(1), Duration::from_secs(2));
    let error = auth.access().await.err().expect("closed port must fail");
    assert_eq!(
        error.to_string(),
        "Antigravity refresh connection failed before token submission; credentials retained"
    );
    assert_eq!(
        auth.access()
            .await
            .err()
            .expect("cooldown must fail")
            .to_string(),
        "Antigravity refresh connection retry temporarily deferred; credentials retained"
    );
    assert_eq!(
        auth.store.load().unwrap().unwrap().refresh_token,
        expired().refresh_token
    );
    auth.finish_maintenance().await.unwrap();
}

#[tokio::test]
async fn antigravity_refresh_connect_http_errors_are_redacted_503_before_stream_headers() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    saved(&store, &expired()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let generations = Arc::new(AtomicUsize::new(0));
    let counter = generations.clone();
    let upstream = Server::start(Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            "must not generate"
        }
    }))
    .await;
    let auth = Auth::new(
        Config {
            base_url: upstream.url.clone(),
            token_url: format!("http://{address}/private-token-url")
                .parse()
                .unwrap(),
            client_id: Some("private-client-id".into()),
            client_secret: Some("private-client-secret".into()),
            ..Config::default()
        },
        store.clone(),
    )
    .unwrap()
    .with_refresh_timeouts(Duration::from_secs(1), Duration::from_secs(2));
    let registry = crate::registry::Registry::from_providers(
        crate::config::AliasProvider::Codex,
        [
            Arc::new(super::antigravity::AntigravityProvider::with_auth(auth))
                as Arc<dyn crate::provider::Provider>,
        ],
    );
    let proxy = Server::start(crate::server::app_with_features(
        Arc::new(registry),
        None,
        crate::server::AppFeatures::default(),
    ))
    .await;
    for stream in [false, true] {
        let response = reqwest::Client::builder().no_proxy().build().unwrap()
            .post(proxy.url.join("/v1/messages").unwrap())
            .json(&json!({"model":"antigravity/gemini-4-argon-low", "max_tokens":32, "stream":stream, "messages":[{"role":"user", "content":"hello"}]}))
            .send().await.unwrap();
        assert_eq!(response.status(), 503);
        assert!(!response.headers().contains_key("retry-after"));
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["type"], "api_error");
        let rendered = error.to_string();
        for sentinel in [
            "private-token-url",
            "private-client-id",
            "private-client-secret",
            "original-refresh",
            "expired-access",
            "127.0.0.1",
        ] {
            assert!(!rendered.contains(sentinel));
        }
    }
    assert_eq!(generations.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.load().unwrap().unwrap().refresh_token,
        expired().refresh_token
    );
}

#[tokio::test]
async fn antigravity_refresh_is_single_flight_and_preserves_profile_and_rotation() {
    for rotate in [false, true] {
        let (_directory, _server, auth, state) = auth_fixture(rotate).await;
        let results = futures_util::future::join_all((0..8).map(|_| auth.access())).await;
        for result in results {
            let result = result.unwrap();
            assert_eq!(result.profile_id, "profile-stable");
            assert_eq!(result.access_token, "new-access");
            assert_eq!(
                result.refresh_token.as_deref(),
                Some(if rotate {
                    "rotated-refresh"
                } else {
                    "original-refresh"
                })
            );
        }
        assert_eq!(state.count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn antigravity_logout_and_new_login_cannot_be_overwritten_by_refresh() {
    for logout in [true, false] {
        let (_directory, _server, auth, state) = auth_fixture(true).await;
        let refreshed = {
            let auth = auth.clone();
            tokio::spawn(async move { auth.access().await })
        };
        state.started.notified().await;
        if logout {
            auth.logout().await.unwrap();
        } else {
            let mut lock = auth.store.lock(Duration::from_secs(1)).await.unwrap();
            let mut newer = expired();
            newer.profile_id = "new-login-profile".into();
            newer.access_token = "new-login-access".into();
            newer.expires_at = auth::now() + 3600;
            lock.advance().unwrap();
            auth.store.save(&lock, &newer).unwrap();
        }
        assert!(refreshed.await.unwrap().is_ok());
        if logout {
            assert!(auth.store.load().unwrap().is_none());
            assert!(auth.access().await.is_err());
        } else {
            assert_eq!(
                auth.store.load().unwrap().unwrap().profile_id,
                "new-login-profile"
            );
        }
        assert_eq!(state.count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn antigravity_user_agent_oauth_and_project_wire_matrix() {
    for override_value in [
        None,
        Some("sentinel-native/9  (exact project identity)"),
        Some(
            "antigravity/cli/1.3.5 (aidev_client; os_type=darwin; arch=arm64; cl=998863838; auth_method=consumer)",
        ),
    ] {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tokens = captured.clone();
        let projects = captured.clone();
        let server = Server::start(Router::new()
            .route("/token", post(move |headers: http::HeaderMap, axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>| {
                let captured = tokens.clone();
                async move {
                    captured.lock().unwrap().push((form["grant_type"].clone(), headers));
                    Json(json!({"access_token":"synthetic-access","refresh_token":"synthetic-refresh","expires_in":3600}))
                }
            }))
            .route("/v1internal:loadCodeAssist", post(move |headers: http::HeaderMap, Json(body): Json<Value>| {
                let captured = projects.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer synthetic-access");
                    assert!(body.get("metadata").is_some());
                    captured.lock().unwrap().push(("project".into(), headers));
                    Json(json!({"cloudaicompanionProject":"synthetic-project"}))
                }
            }))).await;
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            base_url: server.url.clone(),
            token_url: server.url.join("/token").unwrap(),
            client_id: Some("public-client".into()),
            ..Config::read(&directory.path().join("config.json"), |key| {
                if key == "CCP_ANTIGRAVITY_USER_AGENT" {
                    override_value.map(Into::into)
                } else {
                    None
                }
            })
            .unwrap()
        };
        let auth = Auth::new(config, Store::new(directory.path().join("auth.json"))).unwrap();
        let mut callback = None;
        auth.login_with_opener(|url| {
            let pairs = url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            let mut redirect = url::Url::parse(&pairs["redirect_uri"]).unwrap();
            redirect.query_pairs_mut().extend_pairs([
                ("state", pairs["state"].as_ref()),
                ("code", "synthetic-code"),
            ]);
            callback = Some(tokio::spawn(async move {
                reqwest::Client::new().get(redirect).send().await.unwrap()
            }));
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(callback.unwrap().await.unwrap().status(), 200);
        let mut credential = auth.store.load().unwrap().unwrap();
        credential.expires_at = 0;
        saved(&auth.store, &credential).await;
        auth.access().await.unwrap();
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let proxy_identity = format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"));
        for ((operation, headers), expected_operation) in
            requests
                .iter()
                .zip(["authorization_code", "project", "refresh_token"])
        {
            assert_eq!(operation, expected_operation);
            assert_eq!(headers.get_all("user-agent").iter().count(), 1);
            let expected = if operation == "project" {
                override_value
                    .map(str::to_owned)
                    .unwrap_or_else(super::antigravity_http_tests::expected_native_user_agent)
            } else {
                proxy_identity.clone()
            };
            assert_eq!(headers["user-agent"], expected);
            assert!(!headers.contains_key("cookie"));
        }
    }
}

#[tokio::test]
async fn antigravity_explicit_login_exchanges_pkce_and_saves_discovered_project() {
    for project in [
        json!("discovered-project"),
        json!({"id":"discovered-project"}),
    ] {
        let exchanges = Arc::new(std::sync::Mutex::new(Vec::<
            std::collections::HashMap<String, String>,
        >::new()));
        let capture = exchanges.clone();
        let project_response = project.clone();
        let server=Server::start(Router::new()
            .route("/token",post(move |axum::extract::Form(form):axum::extract::Form<std::collections::HashMap<String,String>>| {let capture=capture.clone();async move {capture.lock().unwrap().push(form);Json(json!({"access_token":"login-access","refresh_token":"login-refresh","expires_in":3600}))}}))
            .route("/v1internal:loadCodeAssist",post(move |headers:http::HeaderMap| {let project=project_response.clone();async move {assert_eq!(headers["authorization"],"Bearer login-access");Json(json!({"cloudaicompanionProject":project}))}}))).await;
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("auth.json"));
        let config = Config {
            base_url: server.url.clone(),
            token_url: server.url.join("/token").unwrap(),
            client_id: Some("public-client".into()),
            ..Config::default()
        };
        let auth = Auth::new(config, store).unwrap();
        let mut callback = None;
        let mut challenge = None;
        auth.login_with_opener(|url| {
            let pairs = url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            challenge = Some(pairs["code_challenge"].to_string());
            let mut redirect = url::Url::parse(&pairs["redirect_uri"]).unwrap();
            redirect.query_pairs_mut().extend_pairs([
                ("state", pairs["state"].as_ref()),
                ("code", "synthetic-code"),
            ]);
            callback = Some(tokio::spawn(async move {
                reqwest::Client::new().get(redirect).send().await.unwrap()
            }));
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(callback.unwrap().await.unwrap().status(), 200);
        let credentials = auth.store.load().unwrap().unwrap();
        assert_eq!(
            credentials.project_id.as_deref(),
            Some("discovered-project")
        );
        assert_eq!(credentials.access_token, "login-access");
        let exchange = exchanges.lock().unwrap();
        assert_eq!(exchange.len(), 1);
        assert_eq!(exchange[0]["grant_type"], "authorization_code");
        assert_eq!(exchange[0]["code"], "synthetic-code");
        use base64::Engine;
        use sha2::Digest;
        assert_eq!(
            challenge.unwrap(),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(
                exchange[0]["code_verifier"].as_bytes()
            ))
        );
    }
}

#[tokio::test]
async fn antigravity_manual_login_exchanges_once_with_fresh_pkce_and_exact_redirect() {
    use base64::Engine;
    use sha2::Digest;
    let exchanges = Arc::new(std::sync::Mutex::new(Vec::<
        std::collections::HashMap<String, String>,
    >::new()));
    let captured = exchanges.clone();
    let server = Server::start(Router::new().route("/token", post(move |axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push(form);
            Json(json!({"access_token":"manual-private-access", "refresh_token":"manual-private-refresh", "expires_in":3600}))
        }
    }))).await;
    let directory = tempfile::tempdir().unwrap();
    let auth = Auth::new(
        Config {
            token_url: server.url.join("/token").unwrap(),
            client_id: Some("synthetic-public-client".into()),
            project: Some("configured-project".into()),
            ..Config::default()
        },
        Store::new(directory.path().join("auth.json")),
    )
    .unwrap();
    let mut previous = None;
    let mut previous_profile = None;
    for full_url in [false, true] {
        let mut issued = None;
        auth.login_manual_with_completion(|url, state| {
            let pairs = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(
                pairs["redirect_uri"],
                "https://antigravity.google/oauth-callback"
            );
            assert_eq!(pairs["state"], state);
            assert_eq!(pairs["code_challenge_method"], "S256");
            issued = Some((state.clone(), pairs["code_challenge"].clone()));
            let store = auth.store.clone();
            async move {
                // Consent must not retain the cross-process auth lock.
                drop(store.lock(Duration::from_millis(50)).await.unwrap());
                if full_url {
                    let mut redirect =
                        url::Url::parse("https://antigravity.google/oauth-callback").unwrap();
                    redirect
                        .query_pairs_mut()
                        .extend_pairs([("state", state.as_str()), ("code", "4/0manual+secret=")]);
                    Ok(redirect.to_string())
                } else {
                    Ok("4/0manual+secret=".to_string())
                }
            }
        })
        .await
        .unwrap();
        let issued = issued.unwrap();
        if let Some(previous) = previous.as_ref() {
            assert_ne!(&issued, previous);
        }
        let exchange = exchanges.lock().unwrap().last().unwrap().clone();
        assert_eq!(exchange["code"], "4/0manual+secret=");
        assert_eq!(
            exchange["redirect_uri"],
            "https://antigravity.google/oauth-callback"
        );
        assert_eq!(exchange["grant_type"], "authorization_code");
        assert_eq!(
            issued.1,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(exchange["code_verifier"].as_bytes()))
        );
        let credential = auth.store.load().unwrap().unwrap();
        if let Some(previous) = previous_profile.as_ref() {
            assert_ne!(&credential.profile_id, previous);
        }
        previous = Some(issued);
        previous_profile = Some(credential.profile_id);
    }
    assert_eq!(exchanges.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn antigravity_manual_consent_epoch_changes_prevent_exchange() {
    for logout in [false, true] {
        let (_directory, _server, auth, state) = auth_fixture(true).await;
        let visited = Arc::new(AtomicUsize::new(0));
        let entered = visited.clone();
        let result = auth
            .login_manual_with_completion(|_, _| async {
                entered.fetch_add(1, Ordering::SeqCst);
                if logout {
                    auth.logout().await.unwrap();
                } else {
                    let mut lock = auth.store.lock(Duration::from_millis(50)).await.unwrap();
                    lock.advance().unwrap();
                    let mut new = expired();
                    new.profile_id = "newer-profile".into();
                    auth.store.save(&lock, &new).unwrap();
                }
                Ok("4/0stale-code".into())
            })
            .await;
        assert_eq!(visited.load(Ordering::SeqCst), 1);
        assert!(result.is_err());
        assert_eq!(state.count.load(Ordering::SeqCst), 0);
        if logout {
            assert!(auth.store.load().unwrap().is_none());
        } else {
            assert_eq!(
                auth.store.load().unwrap().unwrap().profile_id,
                "newer-profile"
            );
        }
    }
}

#[tokio::test]
async fn antigravity_manual_invalid_completion_never_exchanges_or_mutates_credentials() {
    for input in [
        "",
        "code=secret",
        "https:/invalid",
        "https://antigravity.google/oauth-callback?state=wrong&code=secret",
    ] {
        let (_directory, _server, auth, state) = auth_fixture(true).await;
        let before = auth.store.load().unwrap().unwrap();
        let result = auth
            .login_manual_with_completion(|_, _| async { Ok(input.to_owned()) })
            .await;
        assert!(result.is_err());
        assert_eq!(state.count.load(Ordering::SeqCst), 0);
        assert!(auth.store.load().unwrap().unwrap() == before);
    }
}

#[tokio::test]
async fn antigravity_manual_foreign_or_unbound_code_is_not_retried_or_saved() {
    // This fake AS enforces the required PKCE/downgrade policy. It does NOT
    // establish that Google's live AS enforces either property for this client.
    for code in ["foreign-attempt-code", "code-issued-without-challenge"] {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let server = Server::start(Router::new().route(
            "/token",
            post(
                move |axum::extract::Form(form): axum::extract::Form<
                    std::collections::HashMap<String, String>,
                >| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(form["code"], code);
                        assert!(!form["code_verifier"].is_empty());
                        (
                            http::StatusCode::BAD_REQUEST,
                            "sentinel-private-token-error",
                        )
                    }
                },
            ),
        ))
        .await;
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("auth.json"));
        let before = expired();
        saved(&store, &before).await;
        let auth = Auth::new(
            Config {
                token_url: server.url.join("/token").unwrap(),
                client_id: Some("public-client".into()),
                project: Some("project".into()),
                ..Config::default()
            },
            store,
        )
        .unwrap();
        let error = auth
            .login_manual_with_completion(|_, _| async { Ok(code.to_owned()) })
            .await
            .unwrap_err();
        assert!(!error.contains("sentinel"));
        assert!(!error.contains(code));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(auth.store.load().unwrap().unwrap() == before);
    }
}

#[tokio::test]
async fn antigravity_manual_exchange_redirect_is_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let sink = Server::start(Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            "unexpected"
        }
    }))
    .await;
    let destination = sink.url.to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Server::start(Router::new().route(
        "/token",
        post(move || {
            let counter = counter.clone();
            let location = destination.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (
                    http::StatusCode::TEMPORARY_REDIRECT,
                    [("location", location)],
                    "private response",
                )
            }
        }),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let auth = Auth::new(
        Config {
            token_url: server.url.join("/token").unwrap(),
            client_id: Some("public-client".into()),
            ..Config::default()
        },
        Store::new(directory.path().join("auth.json")),
    )
    .unwrap();
    assert!(
        auth.login_manual_with_completion(|_, _| async { Ok("4/0private-code".into()) })
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(auth.store.load().unwrap().is_none());
}

#[tokio::test]
async fn antigravity_logout_during_browser_consent_prevents_exchange_and_resurrection() {
    let (_directory, _server, auth, state) = auth_fixture(true).await;
    let mut callback = None;
    let result = auth
        .login_with_opener(|url| {
            let pairs = url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            let mut redirect = url::Url::parse(&pairs["redirect_uri"]).unwrap();
            redirect.query_pairs_mut().extend_pairs([
                ("state", pairs["state"].as_ref()),
                ("code", "synthetic-code"),
            ]);
            let auth = auth.clone();
            callback = Some(tokio::spawn(async move {
                auth.logout().await.unwrap();
                reqwest::Client::new().get(redirect).send().await.unwrap()
            }));
            Ok(())
        })
        .await;
    callback.unwrap().await.unwrap();
    assert!(result.is_err());
    assert!(auth.store.load().unwrap().is_none());
    assert_eq!(state.count.load(Ordering::SeqCst), 0);
}

#[test]
fn antigravity_pkce_state_and_project_shapes_are_explicit() {
    let url = auth::authorization_url(
        &Config::default(),
        "public-client",
        "http://127.0.0.1:1234/oauth/callback",
        "independent-state",
        "local-secret-verifier",
    );
    let query = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query["state"], "independent-state");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!url.as_str().contains("local-secret-verifier"));
    assert_eq!(query["access_type"], "offline");
    assert_eq!(query["prompt"], "consent");
    for project in [json!("project"), json!({"id":"project"})] {
        assert_eq!(
            auth::project_id(&json!({"cloudaicompanionProject":project})).unwrap(),
            "project"
        );
    }
    for value in [json!({}), json!({"cloudaicompanionProject":{"id":""}})] {
        assert!(auth::project_id(&value).is_err());
    }
}

#[tokio::test]
async fn antigravity_oauth_callback_ignores_preconnects_paths_and_wrong_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), auth::callback(listener, "state"))
            .await
            .unwrap()
    });
    let idle = tokio::net::TcpStream::connect(address).await.unwrap();
    for path in [
        "/favicon.ico",
        "/oauth/callback?state=wrong&code=wrong",
        "/oauth/callback?state=state&code=accepted",
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
    }
    drop(idle);
    assert_eq!(task.await.unwrap().unwrap(), "accepted");
}

#[tokio::test]
async fn antigravity_oauth_denial_and_timeout_are_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(auth::callback(listener, "state"));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(b"GET /oauth/callback?state=state&error=access_denied HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    assert_eq!(task.await.unwrap().unwrap_err(), "OAuth consent denied");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), auth::callback(listener, "state"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn antigravity_redirects_do_not_leak_bearer_or_refresh_credentials() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let sink = Server::start(Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            "unexpected"
        }
    }))
    .await;
    let destination = sink.url.to_string();
    let redirect = Server::start(Router::new().fallback(move || {
        let location = destination.clone();
        async move {
            (
                http::StatusCode::TEMPORARY_REDIRECT,
                [("location", location)],
                "private error",
            )
        }
    }))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    saved(&store, &expired()).await;
    let config = Config {
        token_url: redirect.url.clone(),
        client_id: Some("synthetic-client".into()),
        ..Config::read(&directory.path().join("config.json"), |key| {
            (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| "configured-no-redirect-UA".into())
        })
        .unwrap()
    };
    let auth = Auth::new(config, store).unwrap();
    assert!(auth.access().await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let response = auth
        .client
        .post(redirect.url.clone())
        .bearer_auth("synthetic-access")
        .headers(auth.config.native_headers())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 307);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn antigravity_cross_process_lock_child() {
    let Ok(path) = std::env::var("CCP_ANTIGRAVITY_TEST_LOCK") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.lock().unwrap();
    std::fs::write(format!("{path}.ready"), b"ready").unwrap();
    std::thread::sleep(Duration::from_millis(500));
}

#[tokio::test]
async fn antigravity_cross_process_lock_serializes_stable_sibling() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    drop(store.lock(Duration::from_secs(1)).await.unwrap());
    let path = directory.path().join("auth.lock");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "providers::antigravity_auth_tests::antigravity_cross_process_lock_child",
            "--exact",
            "--nocapture",
        ])
        .env("CCP_ANTIGRAVITY_TEST_LOCK", &path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !directory.path().join("auth.lock.ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(store.lock(Duration::from_millis(30)).await.is_err());
    assert!(child.wait().unwrap().success());
    assert!(store.lock(Duration::from_secs(1)).await.is_ok());
}

#[test]
fn antigravity_local_status_distinguishes_missing_corrupt_and_expired_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("absent/auth.json"));
    let result = auth::descriptor(&Ok(Config::default()), &store);
    assert_eq!(result["status"]["auth"], "missing");
    assert!(!directory.path().join("absent").exists());
    let config = directory.path().join("config.json");
    std::fs::write(&config,r#"{"antigravity":{"enabled":true,"clientId":"public-client","projectId":"private-project"}}"#).unwrap();
    let parsed = Config::read(&config, |_| None).unwrap();
    assert!(parsed.enabled);
    let rendered = auth::descriptor(&Ok(parsed), &store).to_string();
    assert!(!rendered.contains("private-project"));
    assert!(!rendered.contains("public-client"));
    std::fs::write(&config, b"corrupt").unwrap();
    assert!(Config::read(&config, |_| None).is_err());
    std::fs::write(&config, b"[]").unwrap();
    assert!(
        Config::read(&config, |_| None).is_err(),
        "non-object configuration must not be silently valid"
    );
}

#[tokio::test]
async fn antigravity_status_never_refreshes_or_exposes_private_values() {
    let (_directory, _server, auth, state) = auth_fixture(false).await;
    for (expires, refresh, expected) in [
        (0, Some("private-refresh".to_string()), "refresh_required"),
        (0, None, "login_required"),
        (auth::now() + 3600, None, "ready"),
    ] {
        let mut credential = expired();
        credential.expires_at = expires;
        credential.refresh_token = refresh;
        credential.project_id = Some("private-project".into());
        saved(&auth.store, &credential).await;
        let descriptor = auth::descriptor(&Ok(auth.config.clone()), &auth.store);
        assert_eq!(descriptor["status"]["auth"], expected);
        assert_eq!(
            descriptor["status"]["configuration_ready"],
            expected != "login_required"
        );
        for secret in [
            "private-project",
            "private-refresh",
            "expired-access",
            "profile-stable",
        ] {
            assert!(!descriptor.to_string().contains(secret));
        }
    }
    assert_eq!(state.count.load(Ordering::SeqCst), 0);
}
