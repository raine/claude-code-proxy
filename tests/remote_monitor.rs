use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
};
use claude_code_proxy::{
    config::AliasProvider,
    monitor::{
        EndpointKind, MonitorHandle,
        remote::RemoteMonitor,
        snapshot::{MonitorResponse, PROTOCOL_VERSION},
    },
    registry::Registry,
    server::{AppFeatures, app_with_features},
};
use http_body_util::BodyExt;
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;

struct Server {
    url: reqwest::Url,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(app: Router) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Server { url, task }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

fn app(monitor: Option<MonitorHandle>) -> Router {
    app_with_features(
        Arc::new(Registry::from_providers(AliasProvider::Codex, [])),
        monitor,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    )
}

#[tokio::test]
async fn snapshot_access_requires_a_real_loopback_peer() {
    let monitor = MonitorHandle::default();
    monitor.request_started(
        "r1",
        Some("session".into()),
        Some(1),
        EndpointKind::Messages,
    );
    for (peer, expected) in [
        (None, StatusCode::FORBIDDEN),
        (Some("192.0.2.1:1111"), StatusCode::FORBIDDEN),
        (Some("127.0.0.1:1111"), StatusCode::OK),
        (Some("[::1]:1111"), StatusCode::OK),
    ] {
        let request = Request::get("/monitor").header("x-forwarded-for", "127.0.0.1");
        let request = match peer {
            Some(peer) => request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap())),
            None => request,
        }
        .body(Body::empty())
        .unwrap();
        let response = app(Some(monitor.clone())).oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            assert_eq!(response.headers()["cache-control"], "no-store");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let response: MonitorResponse = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(response.version, PROTOCOL_VERSION);
            assert_eq!(response.snapshot.active[0].request_id, "r1");
        }
    }
}

#[tokio::test]
async fn independent_dashboards_observe_existing_history_and_detach_without_stopping_proxy() {
    let monitor = MonitorHandle::default();
    monitor.request_started(
        "before-attach",
        Some("session".into()),
        Some(1),
        EndpointKind::Messages,
    );
    monitor.request_completed("before-attach", 200, Some(13), Some(7));
    let server = serve(app(Some(monitor.clone()))).await;
    let first = RemoteMonitor::connect(client(), server.url.clone())
        .await
        .unwrap();
    let second = RemoteMonitor::connect(client(), server.url.clone())
        .await
        .unwrap();
    assert_eq!(
        first.snapshot().snapshot().recent[0].request_id,
        "before-attach"
    );
    assert_eq!(
        second.snapshot().snapshot().recent[0].output_tokens,
        Some(7)
    );
    drop(first);
    monitor.request_started(
        "after-detach",
        Some("session".into()),
        Some(2),
        EndpointKind::Messages,
    );
    wait_until(|| {
        second
            .snapshot()
            .snapshot()
            .active
            .iter()
            .any(|request| request.request_id == "after-detach")
    })
    .await;
    drop(second);
    assert_eq!(
        client()
            .get(server.url.join("healthz").unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let reattached = RemoteMonitor::connect(client(), server.url.clone())
        .await
        .unwrap();
    assert_eq!(
        reattached.snapshot().snapshot().active[0].request_id,
        "after-detach"
    );
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn polling_keeps_last_snapshot_on_failure_and_recovers() {
    let failing = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let monitor = MonitorHandle::default();
    monitor.request_started("r1", None, None, EndpointKind::Messages);
    let server = serve(Router::new().route(
        "/monitor",
        get({
            let failing = failing.clone();
            let requests = requests.clone();
            let monitor = monitor.clone();
            move || {
                let failed = failing.load(Ordering::SeqCst);
                requests.fetch_add(1, Ordering::SeqCst);
                let snapshot = MonitorResponse::from(monitor.snapshot());
                async move {
                    if failed {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    } else {
                        Json(snapshot).into_response()
                    }
                }
            }
        }),
    ))
    .await;
    let remote = RemoteMonitor::connect(client(), server.url.clone())
        .await
        .unwrap();
    failing.store(true, Ordering::SeqCst);
    wait_until(|| remote.snapshot().connection_error().is_some()).await;
    assert_eq!(remote.snapshot().snapshot().active[0].request_id, "r1");
    monitor.request_completed("r1", 200, Some(10), Some(5));
    failing.store(false, Ordering::SeqCst);
    wait_until(|| {
        remote.snapshot().connection_error().is_none()
            && remote.snapshot().snapshot().recent.len() == 1
    })
    .await;
    assert_eq!(
        remote.snapshot().snapshot().recent[0].output_tokens,
        Some(5)
    );
    drop(remote);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_drop = requests.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(requests.load(Ordering::SeqCst), after_drop);
}

#[tokio::test]
async fn incompatible_protocol_and_disabled_collection_report_attach_errors() {
    let incompatible = serve(Router::new().route(
        "/monitor",
        get(|| async {
            let snapshot = MonitorResponse {
                version: PROTOCOL_VERSION + 1,
                snapshot: MonitorHandle::default().snapshot().into(),
            };
            Json(snapshot)
        }),
    ))
    .await;
    let error = RemoteMonitor::connect(client(), incompatible.url.clone())
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("unsupported monitor protocol"));
    let disabled = serve(app(None)).await;
    let error = RemoteMonitor::connect(client(), disabled.url.clone())
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("404"));
}
