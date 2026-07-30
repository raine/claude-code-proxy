#![cfg(unix)]

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DRIFT_WARNING: &str = "\"msg\":\"provider_config_generation_stale\"";
const SECRET_SENTINEL: &str = "CONFIG_SECRET_MUST_NOT_BE_LOGGED";

fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    listener.local_addr().expect("reserved address").port()
}

fn write_config(config_dir: &std::path::Path, base_url: &str, revision: u8) {
    let mut config = serde_json::json!({
        "codex": {
            "baseUrl": base_url,
            "transport": "http",
            "originator": SECRET_SENTINEL,
        }
    });
    match revision {
        1 => {}
        2 => {
            config["log"] = serde_json::json!({"verbose": false});
        }
        3 => {
            config["log"] = serde_json::json!({"verbose": false});
            config["grok"] = serde_json::json!({"streamHeartbeatMs": 1234});
        }
        other => panic!("unsupported config revision {other}"),
    }
    std::fs::write(
        config_dir.join("config.json"),
        serde_json::to_vec(&config).expect("serialize isolated config"),
    )
    .expect("write isolated config");
}

fn write_codex_auth(config_dir: &std::path::Path) {
    let auth_dir = config_dir.join("codex");
    std::fs::create_dir_all(&auth_dir).expect("create isolated Codex auth directory");
    std::fs::write(
        auth_dir.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "access": "test-access",
            "refresh": "test-refresh",
            "expires": 4_102_444_800_000_u64,
            "account_id": "acct_test",
        }))
        .expect("serialize isolated Codex auth"),
    )
    .expect("write isolated Codex auth");
}

struct MockCodexUpstream {
    label: &'static str,
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MockCodexUpstream {
    fn spawn(label: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Codex upstream");
        listener
            .set_nonblocking(true)
            .expect("make mock listener nonblocking");
        let address = listener.local_addr().expect("mock upstream address");
        let requests = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_requests = requests.clone();
        let worker_shutdown = shutdown.clone();
        let worker = thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("set mock read timeout");
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .expect("set mock write timeout");
                        if read_http_request(&mut stream).is_err() {
                            continue;
                        }
                        worker_requests.fetch_add(1, Ordering::AcqRel);
                        let body = mock_codex_sse(label);
                        let response = format!(
                            concat!(
                                "HTTP/1.1 200 OK\r\n",
                                "content-type: text/event-stream\r\n",
                                "content-length: {}\r\n",
                                "connection: close\r\n",
                                "\r\n",
                                "{}"
                            ),
                            body.len(),
                            body
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("write mock Codex response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock Codex accept failed: {error}"),
                }
            }
        });
        Self {
            label,
            address,
            requests,
            shutdown,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/responses", self.address)
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }
}

impl Drop for MockCodexUpstream {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .unwrap_or_else(|_| panic!("mock Codex {} worker panicked", self.label));
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<()> {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut expected_len = None;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_REQUEST_BYTES {
            return Err(std::io::Error::other("mock request exceeded limit"));
        }
        if expected_len.is_none()
            && let Some(header_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
        {
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
            expected_len = Some(header_end.saturating_add(content_length));
        }
        if expected_len.is_some_and(|expected| request.len() >= expected) {
            return Ok(());
        }
    }
    expected_len
        .is_some_and(|expected| request.len() >= expected)
        .then_some(())
        .ok_or_else(|| std::io::Error::other("mock request ended early"))
}

fn mock_codex_sse(label: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,",
            "\"item\":{{\"type\":\"message\",\"id\":\"msg_up\"}}}}\n\n",
            "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,",
            "\"item_id\":\"msg_up\",\"delta\":\"{0}\"}}\n\n",
            "data: {{\"type\":\"response.output_text.done\",\"output_index\":0,",
            "\"item_id\":\"msg_up\",\"text\":\"{0}\"}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{{\"type\":\"message\",\"id\":\"msg_up\"}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",",
            "\"usage\":{{\"input_tokens\":5,\"output_tokens\":2}}}}}}\n\n"
        ),
        label
    )
}

struct ServerChild(Option<Child>);

impl ServerChild {
    fn stop(mut self) -> Output {
        let mut child = self.0.take().expect("server child is present");
        child.kill().expect("stop isolated proxy");
        child.wait_with_output().expect("collect proxy output")
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_server(config_dir: &std::path::Path, port: u16) -> ServerChild {
    let child = Command::new(assert_cmd::cargo::cargo_bin!("claude-code-proxy"))
        .args(["serve", "--no-monitor"])
        .env_clear()
        .env("HOME", config_dir)
        .env("CCP_CONFIG_DIR", config_dir)
        .env("CCP_BIND_ADDRESS", "127.0.0.1")
        .env("PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn isolated proxy");
    ServerChild(Some(child))
}

fn get_version(port: u16) -> std::io::Result<Value> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(200),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(
        format!("GET /version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| std::io::Error::other("HTTP response has no header terminator"))?;
    serde_json::from_slice(&response[split..]).map_err(std::io::Error::other)
}

fn post_messages(port: u16) -> Value {
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build loopback test client")
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "config-generation-route-probe")
        .json(&serde_json::json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "identify the mock endpoint"}],
        }))
        .send()
        .expect("send message through isolated proxy");
    let status = response.status();
    let body = response.bytes().expect("read isolated proxy response");
    assert!(
        status.is_success(),
        "message request failed with {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).expect("parse isolated proxy response")
}

fn response_text(response: &Value) -> &str {
    response["content"][0]["text"]
        .as_str()
        .expect("Anthropic text response")
}

fn wait_for_version(port: u16) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(version) = get_version(port) {
            return version;
        }
        assert!(
            Instant::now() < deadline,
            "proxy did not become ready on port {port}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_new_generation(port: u16, previous: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(version) = get_version(port)
            && version["configGeneration"]
                .as_u64()
                .is_some_and(|generation| generation > previous)
        {
            return version;
        }
        assert!(
            Instant::now() < deadline,
            "config generation did not advance beyond {previous}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn version_observes_stale_transport_until_restart_switches_from_a_to_b() {
    let config_dir = TempDir::new().expect("isolated config directory");
    let upstream_a = MockCodexUpstream::spawn("upstream-a");
    let upstream_b = MockCodexUpstream::spawn("upstream-b");
    write_codex_auth(config_dir.path());
    write_config(config_dir.path(), &upstream_a.base_url(), 1);

    let first_port = reserve_port();
    let first_child = spawn_server(config_dir.path(), first_port);
    let initial = wait_for_version(first_port);
    let provider_generation = initial["providerConstructionConfigGeneration"]
        .as_u64()
        .expect("provider construction generation");
    let initial_generation = initial["configGeneration"]
        .as_u64()
        .expect("initial config generation");
    assert_eq!(provider_generation, initial_generation);
    assert_eq!(
        initial["providerConstructionConfigGenerationEnd"],
        provider_generation
    );
    assert_eq!(initial["providerConstructionSnapshotStable"], true);
    assert_eq!(
        initial["configGenerationChangedSinceProviderConstruction"],
        false
    );

    let initial_response = post_messages(first_port);
    assert_eq!(response_text(&initial_response), "upstream-a");
    assert_eq!(upstream_a.request_count(), 1);
    assert_eq!(upstream_b.request_count(), 0);

    write_config(config_dir.path(), &upstream_b.base_url(), 2);
    let second = wait_for_new_generation(first_port, initial_generation);
    let second_generation = second["configGeneration"]
        .as_u64()
        .expect("second config generation");
    assert_eq!(
        second["providerConstructionConfigGeneration"],
        provider_generation
    );
    assert_eq!(
        second["configGenerationChangedSinceProviderConstruction"],
        true
    );
    assert_eq!(
        second["configReload"]["status"],
        "generation_changed_check_restart_required_fields"
    );
    assert_eq!(second["providerConstructionSnapshotStable"], true);

    // The file now names B, but baseUrl belongs to the transport client that
    // was constructed from A. A real message must still reach A until restart.
    let stale_response = post_messages(first_port);
    assert_eq!(response_text(&stale_response), "upstream-a");
    assert_eq!(upstream_a.request_count(), 2);
    assert_eq!(
        upstream_b.request_count(),
        0,
        "a file reload must not hot-swap the Codex transport client"
    );

    // Repeated observations of one accepted snapshot must not spam warnings.
    for _ in 0..3 {
        let repeated = get_version(first_port).expect("repeat version request");
        assert_eq!(repeated["configGeneration"], second_generation);
    }

    write_config(config_dir.path(), &upstream_b.base_url(), 3);
    let third = wait_for_new_generation(first_port, second_generation);
    let third_generation = third["configGeneration"]
        .as_u64()
        .expect("third config generation");
    assert_eq!(
        third["providerConstructionConfigGeneration"],
        provider_generation
    );
    assert_eq!(
        third["configGenerationChangedSinceProviderConstruction"],
        true
    );
    for _ in 0..3 {
        let repeated = get_version(first_port).expect("repeat newest version request");
        assert_eq!(repeated["configGeneration"], third_generation);
    }
    let newest_stale_response = post_messages(first_port);
    assert_eq!(response_text(&newest_stale_response), "upstream-a");
    assert_eq!(upstream_a.request_count(), 3);
    assert_eq!(upstream_b.request_count(), 0);

    let first_output = first_child.stop();
    let first_stderr = String::from_utf8_lossy(&first_output.stderr);
    assert_eq!(
        first_stderr.matches(DRIFT_WARNING).count(),
        2,
        "one warning is expected for each newly observed stale generation; stderr={first_stderr}"
    );
    assert!(
        !first_stderr.contains(SECRET_SENTINEL),
        "config values must not appear in the generation warning"
    );

    // A fresh process constructs providers from the latest accepted file
    // snapshot, so the running provider generation is current again.
    let restart_port = reserve_port();
    let restart_child = spawn_server(config_dir.path(), restart_port);
    let restarted = wait_for_version(restart_port);
    assert_eq!(
        restarted["providerConstructionConfigGeneration"],
        restarted["configGeneration"]
    );
    assert_eq!(
        restarted["providerConstructionConfigGenerationEnd"],
        restarted["providerConstructionConfigGeneration"]
    );
    assert_eq!(restarted["providerConstructionSnapshotStable"], true);
    assert_eq!(
        restarted["configGenerationChangedSinceProviderConstruction"],
        false
    );
    assert_eq!(
        restarted["configReload"]["status"],
        "provider_generation_current"
    );

    let restarted_response = post_messages(restart_port);
    assert_eq!(response_text(&restarted_response), "upstream-b");
    assert_eq!(
        upstream_a.request_count(),
        3,
        "restart must stop routing to the old endpoint"
    );
    assert_eq!(upstream_b.request_count(), 1);

    let restart_output = restart_child.stop();
    let restart_stderr = String::from_utf8_lossy(&restart_output.stderr);
    assert_eq!(restart_stderr.matches(DRIFT_WARNING).count(), 0);
    assert!(!restart_stderr.contains(SECRET_SENTINEL));
}
