//! Process-level regression with isolated catalog, credentials and loopback upstream.
use axum::{Json, Router, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;

struct Proxy(Child);
impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(dir: &TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
    // Avoid inherited credentials, routing overrides, proxy settings and logs.
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("USERPROFILE", dir.path())
        .env("HOME", dir.path())
        .env("CODEX_HOME", dir.path())
        .env("CCP_CONFIG_DIR", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env("CCP_CODEX_TRANSPORT", "http")
        .env("CCP_CODEX_RESPONSES_API", "1")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*");
    cmd
}

fn write_catalog(dir: &TempDir, extra: &str) {
    let entry = |slug, supported, visibility, lite| {
        json!({
            "slug":slug, "supported_in_api":supported,"visibility":visibility,"use_responses_lite":lite,
            "supported_reasoning_levels":[{"effort":"medium","description":"Balanced"}]
        })
    };
    std::fs::write(
        dir.path().join("models_cache.json"),
        serde_json::to_vec(&json!({
            "fetched_at":"2020-01-01T00:00:00Z", "client_version":"0.1.0", "etag":"fixture",
            "models":[entry(extra,true,"list",true), entry("gpt-test-hidden",true,"hide",false),
                entry("gpt-test-denied",false,"list",true),
                entry("gpt-5.4",true,"list",true), entry("gpt-6.1-terra",false,"hide",false),
                entry("opus",true,"list",false), entry("gpt-test-forged-fast",true,"list",true),
                {"slug":"gpt-test-incomplete"}]
        }))
        .unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn catalog_drives_discovery_routing_tiers_and_lanes_in_actual_process() {
    let dir = TempDir::new().unwrap();
    write_catalog(&dir, "gpt-test-future");
    let output = command(&dir).args(["models", "--full"]).output().unwrap();
    assert!(output.status.success());
    let listing = String::from_utf8(output.stdout).unwrap();
    assert!(listing.contains("gpt-test-future-fast"));
    for absent in [
        "gpt-test-hidden",
        "gpt-test-denied",
        "gpt-test-incomplete",
        "gpt-test-forged-fast",
    ] {
        assert!(!listing.contains(absent), "{absent}");
    }
    assert!(listing.contains("gpt-6.1-terra"));
    assert!(listing.contains("gpt-6-astra-ultrafast"));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let app = Router::new().route("/responses", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let tx = tx.clone();
        async move {
            tx.send((headers, body.clone())).unwrap();
            let event = json!({"type":"response.completed","response":{
                "id":"resp-fixture","object":"response","status":"completed", "model":body["model"],
                "output":[{"type":"message","id":"msg_fixture","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"synthetic ok","annotations":[]}]}],
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}});
            let item = &event["response"]["output"][0];
            let added = json!({"type":"response.output_item.added","output_index":0,"item":item});
            let delta = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"synthetic ok"});
            let done = json!({"type":"response.output_item.done","output_index":0,"item":item});
            ([("content-type", "text/event-stream")], format!("data: {added}\n\ndata: {delta}\n\ndata: {done}\n\ndata: {event}\n\n"))
        }
    }));
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream, app).await.unwrap();
    });
    std::fs::create_dir(dir.path().join("codex")).unwrap();
    std::fs::write(
        dir.path().join("codex/auth.json"),
        r#"{"access":"synthetic-access","refresh":"synthetic-refresh","expires":4102444800000}"#,
    )
    .unwrap();
    // Reserve a loopback port, then release it immediately before spawning.
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut proxy = Proxy(
        command(&dir)
            .args(["serve", "--no-monitor", "--port", &port.to_string()])
            .env(
                "CCP_CODEX_BASE_URL",
                format!("http://{upstream_addr}/responses"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..150 {
        assert!(proxy.0.try_wait().unwrap().is_none(), "proxy exited");
        if client.get(format!("{url}/healthz")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "proxy did not start");
    let listing: Value = client
        .get(format!("{url}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listing.to_string().contains("gpt-test-future-fast"));
    assert!(!listing.to_string().contains("gpt-test-hidden"));

    for (requested, expected, lite, tier) in [
        (
            "gpt-test-future-fast",
            "gpt-test-future",
            true,
            Some("priority"),
        ),
        (
            "gpt-test-hidden-fast",
            "gpt-test-hidden",
            false,
            Some("priority"),
        ),
        ("gpt-5.4", "gpt-5.4", false, None),
        ("gpt-6.1-terra", "gpt-6.1-terra", true, None),
        ("opus", "gpt-6-sol", true, None),
        (
            "gpt-6-astra-ultrafast",
            "gpt-6-astra",
            true,
            Some("ultrafast"),
        ),
    ] {
        let response = client
            .post(format!("{url}/v1/responses"))
            .json(&json!({"model":requested,"input":"synthetic","stream":false}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "{requested}: {status} {text}");
        let (headers, body) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body["model"], expected);
        assert_eq!(body.get("service_tier").and_then(Value::as_str), tier);
        assert_eq!(
            headers.contains_key("x-openai-internal-codex-responses-lite"),
            lite,
            "{requested}"
        );
        assert_eq!(headers["authorization"], "Bearer synthetic-access");
    }
    for route in ["/v1/messages", "/v1/chat/completions"] {
        let mut request = json!({"model":"gpt-test-future-fast","stream":false,
                "messages":[{"role":"user","content":"synthetic"}]});
        if route == "/v1/messages" {
            request["max_tokens"] = json!(64);
        }
        let response = client
            .post(format!("{url}{route}"))
            .json(&request)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "{route}: {status} {text}");
        assert!(text.contains("synthetic ok"), "{route}: {text}");
        let (headers, body) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body["model"], "gpt-test-future");
        assert_eq!(body["service_tier"], "priority");
        assert!(headers.contains_key("x-openai-internal-codex-responses-lite"));
        assert!(body.get("client_metadata").is_some());
    }
    for rejected in [
        "gpt-test-denied",
        "gpt-test-incomplete",
        "gpt-test-forged-fast",
        "gpt-test-future-ultrafast",
        "gpt-test-future-fast-fast",
    ] {
        let response = client
            .post(format!("{url}/v1/responses"))
            .json(&json!({"model":rejected,"input":"synthetic"}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error(), "{rejected}");
    }
    assert!(rx.try_recv().is_err());
    write_catalog(&dir, "gpt-test-added");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let listing = client
        .get(format!("{url}/v1/models"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(listing.contains("gpt-test-added"));
    assert!(!listing.contains("gpt-test-future"));
    let response = client
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gpt-test-added-fast","input":"synthetic"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let (_, body) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body["model"], "gpt-test-added");
    assert_eq!(body["service_tier"], "priority");
    std::fs::remove_file(dir.path().join("models_cache.json")).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let response = client
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gpt-test-added","input":"synthetic"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    let listing = client
        .get(format!("{url}/v1/models"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!listing.contains("gpt-test-added"));
    assert!(listing.contains("gpt-6.1-terra"));
    upstream_task.abort();
}
