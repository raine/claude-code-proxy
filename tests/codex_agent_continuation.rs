use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use claude_code_proxy::providers::codex::websocket::{
    WEBSOCKET_PROTOCOL_HEADER, invalidate_codex_websocket_pool_owner,
};
use claude_code_proxy::request_identity::ConversationIdentity;
use claude_code_proxy::server;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_HEADER: &str = "x-claude-code-session-id";
const AGENT_HEADER: &str = "x-claude-code-agent-id";
const PARENT_AGENT_HEADER: &str = "x-claude-code-parent-agent-id";

type ProbeLog = Arc<Mutex<Vec<(usize, usize, Vec<u8>)>>>;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
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

fn configure_environment(config_dir: &Path, upstream_url: &str) -> Vec<EnvGuard> {
    let mut guards = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "REQUEST_METHOD",
        "CCP_AUTO_REVIEW_MODEL",
        "CCP_CODEX_MODEL",
        "CCP_CODEX_SERVICE_TIER",
        "CCP_CODEX_EFFORT",
        "CCP_CODEX_REASONING_SUMMARY",
        "CCP_CODEX_ORIGINATOR",
        "CCP_CODEX_USER_AGENT",
        "CCP_USER_AGENT",
    ]
    .into_iter()
    .map(EnvGuard::unset)
    .collect::<Vec<_>>();
    guards.extend([
        EnvGuard::set("NO_PROXY", "127.0.0.1,localhost"),
        EnvGuard::set("no_proxy", "127.0.0.1,localhost"),
        EnvGuard::set("CCP_CONFIG_DIR", config_dir),
        EnvGuard::set("CCP_ALIAS_PROVIDER", "codex"),
        EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket"),
        EnvGuard::set("CCP_CODEX_BASE_URL", upstream_url),
        EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1"),
        EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "0"),
    ]);
    guards
}

fn write_codex_auth(config_dir: &Path) {
    let auth_dir = config_dir.join("codex");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::write(
        auth_dir.join("auth.json"),
        serde_json::to_vec(&json!({
            "access": "test-access",
            "refresh": "test-refresh",
            "expires": 4_102_444_800_000_i64,
            "account_id": "acct_test"
        }))
        .unwrap(),
    )
    .unwrap();
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    socket_ordinal: usize,
    headers: HeaderMap,
    body: Value,
}

impl CapturedRequest {
    fn marker(&self) -> &str {
        self.body["input"]
            .as_array()
            .and_then(|input| input.last())
            .and_then(|item| item.get("content"))
            .and_then(Value::as_array)
            .and_then(|content| content.last())
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("request has no final text marker: {}", self.body))
    }

    fn previous_response_id(&self) -> Option<&str> {
        self.body
            .get("previous_response_id")
            .and_then(Value::as_str)
    }

    fn assert_protocol_headers(&self, expected_session: Option<&str>) {
        assert_eq!(
            self.headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-access"),
            "socket {} authorization header",
            self.socket_ordinal
        );
        assert_eq!(
            self.headers
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("acct_test"),
            "socket {} account header",
            self.socket_ordinal
        );
        assert_eq!(
            self.headers
                .get("openai-beta")
                .and_then(|value| value.to_str().ok()),
            Some(WEBSOCKET_PROTOCOL_HEADER),
            "socket {} websocket protocol header",
            self.socket_ordinal
        );
        assert_eq!(
            self.headers
                .get("session_id")
                .and_then(|value| value.to_str().ok()),
            expected_session,
            "socket {} session_id header",
            self.socket_ordinal
        );
        assert_eq!(
            self.headers
                .get("x-client-request-id")
                .and_then(|value| value.to_str().ok()),
            expected_session,
            "socket {} x-client-request-id header",
            self.socket_ordinal
        );
    }
}

enum MockOutcome {
    Completion {
        response_id: String,
        text: String,
        close_after: bool,
        acknowledged: oneshot::Sender<()>,
    },
    RawEvent {
        event: Value,
        acknowledged: oneshot::Sender<()>,
    },
}

struct PendingRequest {
    captured: CapturedRequest,
    outcome: oneshot::Sender<MockOutcome>,
}

impl PendingRequest {
    async fn respond(self, response_id: &str, text: &str, close_after: bool) -> CapturedRequest {
        let (acknowledged, acknowledgement) = oneshot::channel();
        self.outcome
            .send(MockOutcome::Completion {
                response_id: response_id.to_string(),
                text: text.to_string(),
                close_after,
                acknowledged,
            })
            .unwrap_or_else(|_| {
                panic!(
                    "upstream socket closed before responding to {}",
                    self.captured.marker()
                )
            });
        tokio::time::timeout(REQUEST_TIMEOUT, acknowledgement)
            .await
            .expect("mock response acknowledgement timed out")
            .expect("mock response acknowledgement sender dropped");
        self.captured
    }

    async fn respond_rate_limited(self) -> CapturedRequest {
        let (acknowledged, acknowledgement) = oneshot::channel();
        self.outcome
            .send(MockOutcome::RawEvent {
                event: json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {
                            "status": 429,
                            "code": "rate_limit_exceeded",
                            "message": "rate limit exceeded",
                            "retry_after_seconds": 0
                        }
                    }
                }),
                acknowledged,
            })
            .unwrap_or_else(|_| {
                panic!(
                    "upstream socket closed before rate-limiting {}",
                    self.captured.marker()
                )
            });
        tokio::time::timeout(REQUEST_TIMEOUT, acknowledgement)
            .await
            .expect("mock rate-limit acknowledgement timed out")
            .expect("mock rate-limit acknowledgement sender dropped");
        self.captured
    }
}

struct InstrumentedUpstream {
    base_url: String,
    requests: mpsc::UnboundedReceiver<PendingRequest>,
    captures: Arc<Mutex<Vec<CapturedRequest>>>,
    probes: ProbeLog,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl InstrumentedUpstream {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let captures = Arc::new(Mutex::new(Vec::new()));
        let probes = Arc::new(Mutex::new(Vec::new()));
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task_captures = captures.clone();
        let task_probes = probes.clone();
        let task = tokio::spawn(async move {
            run_upstream(
                listener,
                requests_tx,
                task_captures,
                task_probes,
                shutdown_rx,
            )
            .await;
        });
        Self {
            base_url: format!("http://{address}/backend-api/codex/responses"),
            requests,
            captures,
            probes,
            shutdown: Some(shutdown),
            task,
        }
    }

    async fn next_request(
        &mut self,
        expected_marker: &str,
        expected_session: Option<&str>,
    ) -> PendingRequest {
        let pending = self.next_any_request(expected_session).await;
        assert_eq!(pending.captured.marker(), expected_marker);
        pending
    }

    async fn next_any_request(&mut self, expected_session: Option<&str>) -> PendingRequest {
        let pending = tokio::time::timeout(REQUEST_TIMEOUT, self.requests.recv())
            .await
            .expect("timed out waiting for response.create")
            .expect("mock upstream stopped before response.create");
        pending.captured.assert_protocol_headers(expected_session);
        pending
    }

    fn snapshot(&self) -> Vec<CapturedRequest> {
        self.captures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        tokio::time::timeout(REQUEST_TIMEOUT, self.task)
            .await
            .expect("mock upstream shutdown timed out")
            .expect("mock upstream task failed");

        let captures = self
            .captures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut requests_per_socket = HashMap::new();
        let mut expected_probe_sites = captures
            .iter()
            .filter_map(|capture| {
                let prior_requests = requests_per_socket
                    .entry(capture.socket_ordinal)
                    .or_insert(0usize);
                let site =
                    (*prior_requests > 0).then_some((capture.socket_ordinal, *prior_requests));
                *prior_requests += 1;
                site
            })
            .collect::<Vec<_>>();
        drop(captures);

        let probes = self
            .probes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut actual_probe_sites = probes
            .iter()
            .map(|(socket_ordinal, prior_requests, _)| (*socket_ordinal, *prior_requests))
            .collect::<Vec<_>>();
        expected_probe_sites.sort_unstable();
        actual_probe_sites.sort_unstable();
        assert_eq!(
            actual_probe_sites, expected_probe_sites,
            "each successful pooled reuse must have one probe at its exact socket-local request boundary: {probes:?}"
        );
        let unique = probes
            .iter()
            .map(|(_, _, payload)| payload.clone())
            .collect::<HashSet<_>>();
        assert_eq!(
            unique.len(),
            probes.len(),
            "pooled validation probes must use unique payloads: {probes:?}"
        );
        assert!(
            probes.iter().all(|(_, _, payload)| payload.len() == 8),
            "pooled validation probes must preserve their eight-byte nonce: {probes:?}"
        );
    }
}

async fn run_upstream(
    listener: TcpListener,
    requests: mpsc::UnboundedSender<PendingRequest>,
    captures: Arc<Mutex<Vec<CapturedRequest>>>,
    probes: ProbeLog,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut sockets = JoinSet::new();
    let mut next_socket_ordinal = 1usize;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let socket_ordinal = next_socket_ordinal;
                next_socket_ordinal += 1;
                sockets.spawn(handle_socket(
                    stream,
                    socket_ordinal,
                    requests.clone(),
                    captures.clone(),
                    probes.clone(),
                ));
            }
            completed = sockets.join_next(), if !sockets.is_empty() => {
                if let Some(Err(error)) = completed {
                    panic!("mock upstream socket task failed: {error}");
                }
            }
        }
    }

    sockets.abort_all();
    while sockets.join_next().await.is_some() {}
}

#[allow(clippy::result_large_err)]
async fn handle_socket(
    stream: TcpStream,
    socket_ordinal: usize,
    requests: mpsc::UnboundedSender<PendingRequest>,
    captures: Arc<Mutex<Vec<CapturedRequest>>>,
    probes: ProbeLog,
) {
    let handshake_headers = Arc::new(Mutex::new(None));
    let callback_headers = handshake_headers.clone();
    let Ok(mut websocket) =
        accept_hdr_async(stream, move |request: &http::Request<()>, response| {
            *callback_headers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(request.headers().clone());
            Ok(response)
        })
        .await
    else {
        return;
    };
    let headers = handshake_headers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("websocket handshake headers");
    let mut requests_seen = 0usize;

    loop {
        let Some(frame) = websocket.next().await else {
            return;
        };
        match frame {
            Ok(Message::Ping(payload)) => {
                probes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((socket_ordinal, requests_seen, payload.clone()));
                if websocket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Ok(Message::Pong(_)) | Ok(Message::Binary(_)) | Ok(Message::Frame(_)) => {}
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(Message::Text(text)) => {
                let body: Value = serde_json::from_str(&text).unwrap_or_else(|error| {
                    panic!("invalid response.create JSON: {error}: {text}")
                });
                assert_eq!(body["type"], "response.create");
                assert!(body.get("stream").is_none(), "websocket payload: {body}");
                let captured = CapturedRequest {
                    socket_ordinal,
                    headers: headers.clone(),
                    body,
                };
                captures
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(captured.clone());
                requests_seen += 1;
                let (outcome, response) = oneshot::channel();
                if requests.send(PendingRequest { captured, outcome }).is_err() {
                    return;
                }
                let outcome = match tokio::time::timeout(REQUEST_TIMEOUT, response).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(_)) | Err(_) => return,
                };
                match outcome {
                    MockOutcome::Completion {
                        response_id,
                        text,
                        close_after,
                        acknowledged,
                    } => {
                        if emit_completion(&mut websocket, &response_id, &text)
                            .await
                            .is_err()
                        {
                            let _ = acknowledged.send(());
                            return;
                        }
                        if close_after {
                            let _ = websocket.send(Message::Close(None)).await;
                            drop(websocket);
                            let _ = acknowledged.send(());
                            return;
                        }
                        let _ = acknowledged.send(());
                    }
                    MockOutcome::RawEvent {
                        event,
                        acknowledged,
                    } => {
                        if websocket
                            .send(Message::Text(event.to_string()))
                            .await
                            .is_err()
                        {
                            let _ = acknowledged.send(());
                            return;
                        }
                        let _ = acknowledged.send(());
                    }
                }
            }
        }
    }
}

async fn emit_completion(
    websocket: &mut WebSocketStream<TcpStream>,
    response_id: &str,
    text: &str,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let events = [
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": format!("msg-{response_id}")}
        }),
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": text
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message", "id": format!("msg-{response_id}")}
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": {"input_tokens": 5, "output_tokens": 2}
            }
        }),
    ];
    for event in events {
        websocket.send(Message::Text(event.to_string())).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct IdentityHeaders {
    values: Vec<(&'static str, String)>,
    upstream_session: Option<String>,
}

impl IdentityHeaders {
    fn main(session: &str) -> Self {
        Self {
            values: vec![(SESSION_HEADER, session.to_string())],
            upstream_session: Some(session.to_string()),
        }
    }

    fn agent(session: &str, agent: &str, parent: Option<&str>) -> Self {
        let mut values = vec![
            (SESSION_HEADER, session.to_string()),
            (AGENT_HEADER, agent.to_string()),
        ];
        if let Some(parent) = parent {
            values.push((PARENT_AGENT_HEADER, parent.to_string()));
        }
        Self {
            values,
            upstream_session: Some(session.to_string()),
        }
    }

    fn malformed_agent(session: &str, raw_agent: &str) -> Self {
        Self {
            values: vec![
                (SESSION_HEADER, session.to_string()),
                (AGENT_HEADER, raw_agent.to_string()),
            ],
            upstream_session: Some(session.to_string()),
        }
    }
}

struct DrainedResponse {
    status: StatusCode,
    body: String,
}

impl DrainedResponse {
    fn assert_success(&self, expected_text: &str) {
        assert_eq!(
            self.status,
            StatusCode::OK,
            "downstream body: {}",
            self.body
        );
        assert!(
            self.body.contains(expected_text),
            "downstream response did not contain {expected_text:?}: {}",
            self.body
        );
    }
}

struct TestHarness {
    client: reqwest::Client,
    proxy_url: String,
    upstream: InstrumentedUpstream,
    server_shutdown: Option<oneshot::Sender<()>>,
    server_task: JoinHandle<anyhow::Result<()>>,
    _config_dir: TempDir,
    _environment: Vec<EnvGuard>,
}

impl TestHarness {
    async fn start() -> Self {
        let upstream = InstrumentedUpstream::spawn().await;
        let config_dir = TempDir::new().unwrap();
        write_codex_auth(config_dir.path());
        let environment = configure_environment(config_dir.path(), &upstream.base_url);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let (server_shutdown, shutdown) = oneshot::channel();
        let server_task = tokio::spawn(server::serve_listener(listener, None, async move {
            let _ = shutdown.await;
        }));
        let client = reqwest::Client::builder()
            .http1_only()
            .no_proxy()
            .build()
            .unwrap();

        Self {
            client,
            proxy_url: format!("http://{proxy_address}/v1/messages"),
            upstream,
            server_shutdown: Some(server_shutdown),
            server_task,
            _config_dir: config_dir,
            _environment: environment,
        }
    }

    fn start_request(&self, body: Value, identity: IdentityHeaders) -> JoinHandle<DrainedResponse> {
        let client = self.client.clone();
        let url = self.proxy_url.clone();
        tokio::spawn(async move {
            let mut request = client.post(url).json(&body);
            for (name, value) in identity.values {
                request = request.header(name, value);
            }
            let response = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
                .await
                .expect("proxy response headers timed out")
                .expect("proxy request failed");
            let status = response.status();
            let body = tokio::time::timeout(REQUEST_TIMEOUT, response.bytes())
                .await
                .expect("proxy response body timed out")
                .expect("proxy response body failed");
            DrainedResponse {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            }
        })
    }

    async fn pending(&mut self, marker: &str, identity: &IdentityHeaders) -> PendingRequest {
        self.upstream
            .next_request(marker, identity.upstream_session.as_deref())
            .await
    }

    async fn round_trip(
        &mut self,
        body: Value,
        identity: IdentityHeaders,
        marker: &str,
        response_id: &str,
        reply: &str,
    ) -> CapturedRequest {
        let request = self.start_request(body, identity.clone());
        let pending = self.pending(marker, &identity).await;
        resolve_request(pending, request, response_id, reply, false).await
    }

    async fn shutdown(mut self) {
        drop(self.client);
        if let Some(shutdown) = self.server_shutdown.take() {
            let _ = shutdown.send(());
        }
        tokio::time::timeout(REQUEST_TIMEOUT, self.server_task)
            .await
            .expect("proxy shutdown timed out")
            .expect("proxy server task failed")
            .expect("proxy server returned an error");
        self.upstream.shutdown().await;
    }
}

async fn resolve_request(
    pending: PendingRequest,
    request: JoinHandle<DrainedResponse>,
    response_id: &str,
    reply: &str,
    close_after: bool,
) -> CapturedRequest {
    let captured = pending.respond(response_id, reply, close_after).await;
    let response = tokio::time::timeout(REQUEST_TIMEOUT, request)
        .await
        .expect("downstream request task timed out")
        .expect("downstream request task failed");
    response.assert_success(reply);
    captured
}

fn unique(label: &str) -> String {
    format!("{label}-{}", Uuid::new_v4())
}

fn tagged(case: &str, label: &str) -> String {
    format!("{case}-{label}")
}

fn message(role: &str, text: &str) -> Value {
    json!({"role": role, "content": text})
}

fn messages_body(stream: bool, messages: Vec<Value>) -> Value {
    json!({
        "model": "gpt-5.6-sol",
        "max_tokens": 64,
        "stream": stream,
        "messages": messages
    })
}

fn upstream_item(role: &str, text: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{"type": content_type, "text": text}]
    })
}

fn assert_full_input(request: &CapturedRequest, expected: &[(&str, &str)]) {
    assert!(
        request.body.get("previous_response_id").is_none(),
        "{} must omit previous_response_id on socket {}: {}",
        request.marker(),
        request.socket_ordinal,
        request.body
    );
    let expected = expected
        .iter()
        .map(|(role, text)| upstream_item(role, text))
        .collect::<Vec<_>>();
    assert_eq!(
        request.body["input"].as_array(),
        Some(&expected),
        "{} must send complete input on socket {}",
        request.marker(),
        request.socket_ordinal
    );
}

fn assert_delta_input(
    request: &CapturedRequest,
    previous_response_id: &str,
    socket_ordinal: usize,
    delta_text: &str,
) {
    assert_eq!(
        request.previous_response_id(),
        Some(previous_response_id),
        "{} previous_response_id",
        request.marker()
    );
    assert_eq!(
        request.socket_ordinal,
        socket_ordinal,
        "{} originating socket",
        request.marker()
    );
    assert_eq!(
        request.body["input"].as_array(),
        Some(&vec![upstream_item("user", delta_text)]),
        "{} must send exactly one appended input item",
        request.marker()
    );
}

fn pending_pair(
    first: PendingRequest,
    second: PendingRequest,
    first_marker: &str,
    second_marker: &str,
) -> (PendingRequest, PendingRequest) {
    match (
        first.captured.marker() == first_marker,
        second.captured.marker() == second_marker,
    ) {
        (true, true) => (first, second),
        (false, false)
            if first.captured.marker() == second_marker
                && second.captured.marker() == first_marker =>
        {
            (second, first)
        }
        _ => panic!(
            "expected pending markers {first_marker:?}/{second_marker:?}, got {:?}/{:?}",
            first.captured.marker(),
            second.captured.marker()
        ),
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn main_child_and_nested_child_interleave_independent_continuations() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("lineage");
    let session = tagged(&case, "session");
    let agent_a = tagged(&case, "agent-a");
    let agent_n = tagged(&case, "agent-n");
    let main = IdentityHeaders::main(&session);
    let child = IdentityHeaders::agent(&session, &agent_a, None);
    let nested = IdentityHeaders::agent(&session, &agent_n, Some(&agent_a));

    let m1 = tagged(&case, "main-1");
    let a1 = tagged(&case, "a-1");
    let n1 = tagged(&case, "n-1");
    let m2 = tagged(&case, "main-2");
    let n2 = tagged(&case, "n-2");
    let a2 = tagged(&case, "a-2");
    let rm1 = tagged(&case, "reply-main-1");
    let ra1 = tagged(&case, "reply-a-1");
    let rn1 = tagged(&case, "reply-n-1");
    let resp_m1 = tagged(&case, "resp-main-1");
    let resp_a1 = tagged(&case, "resp-a-1");
    let resp_n1 = tagged(&case, "resp-n-1");

    let main_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &m1)]),
            main.clone(),
            &m1,
            &resp_m1,
            &rm1,
        )
        .await;
    let child_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &a1)]),
            child.clone(),
            &a1,
            &resp_a1,
            &ra1,
        )
        .await;
    let nested_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &n1)]),
            nested.clone(),
            &n1,
            &resp_n1,
            &rn1,
        )
        .await;

    assert_full_input(&main_first, &[("user", &m1)]);
    assert_full_input(&child_first, &[("user", &a1)]);
    assert_full_input(&nested_first, &[("user", &n1)]);
    assert_eq!(main_first.socket_ordinal, 1);
    assert_eq!(child_first.socket_ordinal, 2);
    assert_eq!(nested_first.socket_ordinal, 3);

    let main_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &m1),
                    message("assistant", &rm1),
                    message("user", &m2),
                ],
            ),
            main,
            &m2,
            &tagged(&case, "resp-main-2"),
            &tagged(&case, "reply-main-2"),
        )
        .await;
    let nested_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &n1),
                    message("assistant", &rn1),
                    message("user", &n2),
                ],
            ),
            nested,
            &n2,
            &tagged(&case, "resp-n-2"),
            &tagged(&case, "reply-n-2"),
        )
        .await;
    let child_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &a1),
                    message("assistant", &ra1),
                    message("user", &a2),
                ],
            ),
            child,
            &a2,
            &tagged(&case, "resp-a-2"),
            &tagged(&case, "reply-a-2"),
        )
        .await;

    assert_delta_input(&main_second, &resp_m1, main_first.socket_ordinal, &m2);
    assert_delta_input(&nested_second, &resp_n1, nested_first.socket_ordinal, &n2);
    assert_delta_input(&child_second, &resp_a1, child_first.socket_ordinal, &a2);
    assert_ne!(nested_second.socket_ordinal, child_first.socket_ordinal);
    assert_eq!(harness.upstream.snapshot().len(), 6);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compatible_prefix_sibling_cannot_steal_owner_continuation() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("prefix-attack");
    let session = tagged(&case, "session");
    let a_headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-a"), None);
    let b_headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-b"), None);
    let a1 = tagged(&case, "a-1");
    let a_reply = tagged(&case, "a-reply");
    let attack = tagged(&case, "b-compatible-suffix");
    let a2 = tagged(&case, "a-2");
    let resp_a1 = tagged(&case, "resp-a-1");

    let first = harness
        .round_trip(
            messages_body(false, vec![message("user", &a1)]),
            a_headers.clone(),
            &a1,
            &resp_a1,
            &a_reply,
        )
        .await;
    let sibling_attack = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &a1),
                    message("assistant", &a_reply),
                    message("user", &attack),
                ],
            ),
            b_headers,
            &attack,
            &tagged(&case, "resp-b-1"),
            &tagged(&case, "b-reply"),
        )
        .await;
    let second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &a1),
                    message("assistant", &a_reply),
                    message("user", &a2),
                ],
            ),
            a_headers,
            &a2,
            &tagged(&case, "resp-a-2"),
            &tagged(&case, "a-reply-2"),
        )
        .await;

    assert_full_input(&first, &[("user", &a1)]);
    assert_full_input(
        &sibling_attack,
        &[("user", &a1), ("assistant", &a_reply), ("user", &attack)],
    );
    assert_eq!(first.socket_ordinal, 1);
    assert_eq!(sibling_attack.socket_ordinal, 2);
    assert_delta_input(&second, &resp_a1, first.socket_ordinal, &a2);
    assert_ne!(sibling_attack.socket_ordinal, second.socket_ordinal);
    assert_eq!(harness.upstream.snapshot().len(), 3);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn divergent_siblings_retain_their_own_response_and_socket() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("divergent-siblings");
    let session = tagged(&case, "session");
    let a_headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-a"), None);
    let b_headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-b"), None);
    let a1 = tagged(&case, "a-1");
    let b1 = tagged(&case, "b-1");
    let a2 = tagged(&case, "a-2");
    let b2 = tagged(&case, "b-2");
    let ar1 = tagged(&case, "a-reply-1");
    let br1 = tagged(&case, "b-reply-1");
    let a_resp = tagged(&case, "resp-a-1");
    let b_resp = tagged(&case, "resp-b-1");

    let a_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &a1)]),
            a_headers.clone(),
            &a1,
            &a_resp,
            &ar1,
        )
        .await;
    let b_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &b1)]),
            b_headers.clone(),
            &b1,
            &b_resp,
            &br1,
        )
        .await;
    let a_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &a1),
                    message("assistant", &ar1),
                    message("user", &a2),
                ],
            ),
            a_headers,
            &a2,
            &tagged(&case, "resp-a-2"),
            &tagged(&case, "a-reply-2"),
        )
        .await;
    let b_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &b1),
                    message("assistant", &br1),
                    message("user", &b2),
                ],
            ),
            b_headers,
            &b2,
            &tagged(&case, "resp-b-2"),
            &tagged(&case, "b-reply-2"),
        )
        .await;

    assert_full_input(&a_first, &[("user", &a1)]);
    assert_full_input(&b_first, &[("user", &b1)]);
    assert_eq!(a_first.socket_ordinal, 1);
    assert_eq!(b_first.socket_ordinal, 2);
    assert_delta_input(&a_second, &a_resp, a_first.socket_ordinal, &a2);
    assert_delta_input(&b_second, &b_resp, b_first.socket_ordinal, &b2);
    assert_ne!(a_second.socket_ordinal, b_second.socket_ordinal);
    assert_eq!(harness.upstream.snapshot().len(), 4);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_agent_id_in_different_sessions_is_independent() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("same-agent-different-sessions");
    let agent = tagged(&case, "shared-agent");
    let session_one = tagged(&case, "session-one");
    let session_two = tagged(&case, "session-two");
    let one_headers = IdentityHeaders::agent(&session_one, &agent, None);
    let two_headers = IdentityHeaders::agent(&session_two, &agent, None);
    let one1 = tagged(&case, "one-1");
    let two1 = tagged(&case, "two-1");
    let one2 = tagged(&case, "one-2");
    let two2 = tagged(&case, "two-2");
    let one_reply = tagged(&case, "one-reply");
    let two_reply = tagged(&case, "two-reply");
    let one_response = tagged(&case, "resp-one-1");
    let two_response = tagged(&case, "resp-two-1");

    let one_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &one1)]),
            one_headers.clone(),
            &one1,
            &one_response,
            &one_reply,
        )
        .await;
    let two_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &two1)]),
            two_headers.clone(),
            &two1,
            &two_response,
            &two_reply,
        )
        .await;

    assert_full_input(&one_first, &[("user", &one1)]);
    assert_full_input(&two_first, &[("user", &two1)]);
    let one_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &one1),
                    message("assistant", &one_reply),
                    message("user", &one2),
                ],
            ),
            one_headers,
            &one2,
            &tagged(&case, "resp-one-2"),
            &tagged(&case, "one-reply-2"),
        )
        .await;
    let two_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &two1),
                    message("assistant", &two_reply),
                    message("user", &two2),
                ],
            ),
            two_headers,
            &two2,
            &tagged(&case, "resp-two-2"),
            &tagged(&case, "two-reply-2"),
        )
        .await;

    assert_eq!(one_first.socket_ordinal, 1);
    assert_eq!(two_first.socket_ordinal, 2);
    assert_ne!(one_first.socket_ordinal, two_first.socket_ordinal);
    assert_delta_input(&one_second, &one_response, one_first.socket_ordinal, &one2);
    assert_delta_input(&two_second, &two_response, two_first.socket_ordinal, &two2);
    assert_eq!(harness.upstream.snapshot().len(), 4);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_identity_sandwich_is_stateless_and_preserves_main() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("malformed-sandwich");
    let session = tagged(&case, "session");
    let main_headers = IdentityHeaders::main(&session);
    let malformed_headers = IdentityHeaders::malformed_agent(
        &session,
        &format!(
            "{}, {}",
            tagged(&case, "raw-agent"),
            tagged(&case, "forged-agent")
        ),
    );
    let no_identity = IdentityHeaders::default();
    let main1 = tagged(&case, "main-1");
    let main_reply = tagged(&case, "main-reply");
    let main2 = tagged(&case, "main-2");
    let main_response = tagged(&case, "resp-main-1");
    let malformed1 = tagged(&case, "malformed-1");
    let malformed_reply = tagged(&case, "malformed-reply");
    let malformed2 = tagged(&case, "malformed-2");
    let control = tagged(&case, "no-identity-control");

    let main_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &main1)]),
            main_headers.clone(),
            &main1,
            &main_response,
            &main_reply,
        )
        .await;
    let malformed_first = harness
        .round_trip(
            messages_body(false, vec![message("user", &malformed1)]),
            malformed_headers.clone(),
            &malformed1,
            &tagged(&case, "resp-malformed-1"),
            &malformed_reply,
        )
        .await;
    let malformed_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &malformed1),
                    message("assistant", &malformed_reply),
                    message("user", &malformed2),
                ],
            ),
            malformed_headers,
            &malformed2,
            &tagged(&case, "resp-malformed-2"),
            &tagged(&case, "malformed-reply-2"),
        )
        .await;
    let control_request = harness
        .round_trip(
            messages_body(false, vec![message("user", &control)]),
            no_identity,
            &control,
            &tagged(&case, "resp-control"),
            &tagged(&case, "control-reply"),
        )
        .await;
    let main_second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &main1),
                    message("assistant", &main_reply),
                    message("user", &main2),
                ],
            ),
            main_headers,
            &main2,
            &tagged(&case, "resp-main-2"),
            &tagged(&case, "main-reply-2"),
        )
        .await;

    assert_full_input(&main_first, &[("user", &main1)]);
    assert_full_input(&malformed_first, &[("user", &malformed1)]);
    assert_full_input(
        &malformed_second,
        &[
            ("user", &malformed1),
            ("assistant", &malformed_reply),
            ("user", &malformed2),
        ],
    );
    assert_full_input(&control_request, &[("user", &control)]);
    assert_eq!(main_first.socket_ordinal, 1);
    assert_eq!(malformed_first.socket_ordinal, 2);
    assert_eq!(malformed_second.socket_ordinal, 3);
    assert_eq!(control_request.socket_ordinal, 4);
    assert_ne!(
        malformed_first.socket_ordinal, malformed_second.socket_ordinal,
        "the raw malformed agent value must never become a pool owner"
    );
    assert_delta_input(
        &main_second,
        &main_response,
        main_first.socket_ordinal,
        &main2,
    );
    assert_eq!(harness.upstream.snapshot().len(), 5);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_review_with_agent_headers_is_stateless() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("auto-review");
    let session = tagged(&case, "session");
    let headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-a"), None);
    let a1 = tagged(&case, "a-1");
    let a_reply = tagged(&case, "a-reply");
    let review = tagged(&case, "review-command");
    let review_system = "You are a security monitor for autonomous AI coding agents.\n\n## Context";
    let a2 = tagged(&case, "a-2");
    let a_response = tagged(&case, "resp-a-1");

    let first = harness
        .round_trip(
            messages_body(false, vec![message("user", &a1)]),
            headers.clone(),
            &a1,
            &a_response,
            &a_reply,
        )
        .await;
    let classifier = harness
        .round_trip(
            json!({
                "model": "gpt-5.6-sol",
                "max_tokens": 64,
                "stream": false,
                "system": [{
                    "type": "text",
                    "text": review_system
                }],
                "messages": [{"role": "user", "content": review}],
                "tools": []
            }),
            headers.clone(),
            &review,
            &tagged(&case, "resp-review"),
            &tagged(&case, "review-reply"),
        )
        .await;
    let second = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &a1),
                    message("assistant", &a_reply),
                    message("user", &a2),
                ],
            ),
            headers,
            &a2,
            &tagged(&case, "resp-a-2"),
            &tagged(&case, "a-reply-2"),
        )
        .await;

    assert_full_input(&first, &[("user", &a1)]);
    assert_full_input(
        &classifier,
        &[("developer", review_system), ("user", &review)],
    );
    assert_eq!(classifier.body["model"], "gpt-5.6-luna");
    assert_eq!(first.socket_ordinal, 1);
    assert_eq!(classifier.socket_ordinal, 2);
    assert_delta_input(&second, &a_response, first.socket_ordinal, &a2);
    assert_ne!(classifier.socket_ordinal, second.socket_ordinal);
    assert_eq!(harness.upstream.snapshot().len(), 3);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_dead_origin_retry_full_context_once_and_republish_for_both_modes() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("origin-recovery");

    for (index, (origin, delivery, stream)) in [
        ("missing", "buffered", false),
        ("missing", "streaming", true),
        ("dead", "buffered", false),
        ("dead", "streaming", true),
    ]
    .into_iter()
    .enumerate()
    {
        let label = format!("{origin}-{delivery}");
        let session = tagged(&case, &format!("{label}-session"));
        let agent = tagged(&case, &format!("{label}-agent"));
        let owner = ConversationIdentity::Agent(session.clone(), agent.clone());
        let headers = IdentityHeaders::agent(&session, &agent, None);
        let one = tagged(&case, &format!("{label}-1"));
        let one_reply = tagged(&case, &format!("{label}-reply-1"));
        let two = tagged(&case, &format!("{label}-2"));
        let two_reply = tagged(&case, &format!("{label}-reply-2"));
        let three = tagged(&case, &format!("{label}-3"));
        let response_one = tagged(&case, &format!("resp-{label}-1"));
        let response_two = tagged(&case, &format!("resp-{label}-2"));

        let first_request = harness.start_request(
            messages_body(stream, vec![message("user", &one)]),
            headers.clone(),
        );
        let first_pending = harness.pending(&one, &headers).await;
        let first = resolve_request(
            first_pending,
            first_request,
            &response_one,
            &one_reply,
            origin == "dead",
        )
        .await;
        assert_full_input(&first, &[("user", &one)]);
        assert_eq!(first.socket_ordinal, index * 2 + 1);

        if origin == "missing" {
            invalidate_codex_websocket_pool_owner(&owner);
        }

        let second_request = harness.start_request(
            messages_body(
                stream,
                vec![
                    message("user", &one),
                    message("assistant", &one_reply),
                    message("user", &two),
                ],
            ),
            headers.clone(),
        );
        let second_pending = harness.pending(&two, &headers).await;
        let second = resolve_request(
            second_pending,
            second_request,
            &response_two,
            &two_reply,
            false,
        )
        .await;
        assert_eq!(second.socket_ordinal, index * 2 + 2);
        assert_full_input(
            &second,
            &[("user", &one), ("assistant", &one_reply), ("user", &two)],
        );

        let third = harness
            .round_trip(
                messages_body(
                    stream,
                    vec![
                        message("user", &one),
                        message("assistant", &one_reply),
                        message("user", &two),
                        message("assistant", &two_reply),
                        message("user", &three),
                    ],
                ),
                headers,
                &three,
                &tagged(&case, &format!("resp-{label}-3")),
                &tagged(&case, &format!("{label}-reply-3")),
            )
            .await;
        assert_delta_input(&third, &response_two, second.socket_ordinal, &three);

        let captures = harness.upstream.snapshot();
        assert_eq!(
            captures
                .iter()
                .filter(|capture| capture.marker() == two)
                .count(),
            1,
            "{label}: compatible turn 2 must send full context exactly once after origin recovery"
        );
        assert_eq!(
            captures.len(),
            (index + 1) * 3,
            "{label}: bounded fallback emitted an unexpected send"
        );
    }

    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limited_live_retry_republishes_successful_continuation() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("rate-limit-retry");
    let session = tagged(&case, "session");
    let agent = tagged(&case, "agent");
    let headers = IdentityHeaders::agent(&session, &agent, None);
    let first_message = tagged(&case, "first-message");
    let first_reply = tagged(&case, "first-reply");
    let appended_message = tagged(&case, "appended-message");

    let request = harness.start_request(
        messages_body(true, vec![message("user", &first_message)]),
        headers.clone(),
    );
    let first_attempt = harness.pending(&first_message, &headers).await;
    assert_eq!(first_attempt.captured.socket_ordinal, 1);
    assert_full_input(&first_attempt.captured, &[("user", &first_message)]);
    first_attempt.respond_rate_limited().await;

    let replacement = harness.pending(&first_message, &headers).await;
    assert_eq!(replacement.captured.socket_ordinal, 2);
    assert_full_input(&replacement.captured, &[("user", &first_message)]);
    let replacement = resolve_request(
        replacement,
        request,
        "resp-retry-success",
        &first_reply,
        false,
    )
    .await;

    let appended = harness
        .round_trip(
            messages_body(
                true,
                vec![
                    message("user", &first_message),
                    message("assistant", &first_reply),
                    message("user", &appended_message),
                ],
            ),
            headers,
            &appended_message,
            &tagged(&case, "resp-appended"),
            &tagged(&case, "appended-reply"),
        )
        .await;
    assert_delta_input(
        &appended,
        "resp-retry-success",
        replacement.socket_ordinal,
        &appended_message,
    );
    assert_eq!(replacement.socket_ordinal, 2);
    assert_eq!(harness.upstream.snapshot().len(), 3);

    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consecutive_rate_limit_handoffs_are_attempt_local() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("consecutive-rate-limit-retry");
    let session = tagged(&case, "session");
    let agent = tagged(&case, "agent");
    let headers = IdentityHeaders::agent(&session, &agent, None);
    let first_message = tagged(&case, "first-message");
    let first_reply = tagged(&case, "first-reply");
    let appended_message = tagged(&case, "appended-message");

    let request = harness.start_request(
        messages_body(true, vec![message("user", &first_message)]),
        headers.clone(),
    );
    for expected_socket in 1..=2 {
        let attempt = harness.pending(&first_message, &headers).await;
        assert_eq!(attempt.captured.socket_ordinal, expected_socket);
        assert_full_input(&attempt.captured, &[("user", &first_message)]);
        attempt.respond_rate_limited().await;
    }

    let replacement = harness.pending(&first_message, &headers).await;
    assert_eq!(replacement.captured.socket_ordinal, 3);
    assert_full_input(&replacement.captured, &[("user", &first_message)]);
    let replacement = resolve_request(
        replacement,
        request,
        "resp-consecutive-retry-success",
        &first_reply,
        false,
    )
    .await;

    let appended = harness
        .round_trip(
            messages_body(
                true,
                vec![
                    message("user", &first_message),
                    message("assistant", &first_reply),
                    message("user", &appended_message),
                ],
            ),
            headers,
            &appended_message,
            &tagged(&case, "resp-appended"),
            &tagged(&case, "appended-reply"),
        )
        .await;
    assert_delta_input(
        &appended,
        "resp-consecutive-retry-success",
        replacement.socket_ordinal,
        &appended_message,
    );
    assert_eq!(replacement.socket_ordinal, 3);
    assert_eq!(harness.upstream.snapshot().len(), 4);

    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_owner_stale_completion_cannot_overwrite_newer_turn() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("stale-completion");
    let session = tagged(&case, "session");
    let headers = IdentityHeaders::agent(&session, &tagged(&case, "agent"), None);
    let base = tagged(&case, "base");
    let base_reply = tagged(&case, "base-reply");
    let old = tagged(&case, "old-turn");
    let old_reply = tagged(&case, "old-reply");
    let newer = tagged(&case, "newer-turn");
    let newer_reply = tagged(&case, "newer-reply");
    let follow = tagged(&case, "follow");
    let base_response = tagged(&case, "resp-base");
    let newer_response = tagged(&case, "resp-newer");

    let baseline = harness
        .round_trip(
            messages_body(false, vec![message("user", &base)]),
            headers.clone(),
            &base,
            &base_response,
            &base_reply,
        )
        .await;
    assert_full_input(&baseline, &[("user", &base)]);
    assert_eq!(baseline.socket_ordinal, 1);

    let old_request = harness.start_request(
        messages_body(
            false,
            vec![
                message("user", &base),
                message("assistant", &base_reply),
                message("user", &old),
            ],
        ),
        headers.clone(),
    );
    let old_pending = harness.pending(&old, &headers).await;
    assert_delta_input(
        &old_pending.captured,
        &base_response,
        baseline.socket_ordinal,
        &old,
    );

    let newer_request = harness.start_request(
        messages_body(
            false,
            vec![
                message("user", &base),
                message("assistant", &base_reply),
                message("user", &newer),
            ],
        ),
        headers.clone(),
    );
    let newer_pending = harness.pending(&newer, &headers).await;
    assert_eq!(newer_pending.captured.socket_ordinal, 2);
    assert_full_input(
        &newer_pending.captured,
        &[
            ("user", &base),
            ("assistant", &base_reply),
            ("user", &newer),
        ],
    );
    let newer_capture = resolve_request(
        newer_pending,
        newer_request,
        &newer_response,
        &newer_reply,
        false,
    )
    .await;

    let old_capture = resolve_request(
        old_pending,
        old_request,
        &tagged(&case, "resp-old"),
        &old_reply,
        false,
    )
    .await;
    assert_eq!(old_capture.socket_ordinal, baseline.socket_ordinal);

    let following = harness
        .round_trip(
            messages_body(
                false,
                vec![
                    message("user", &base),
                    message("assistant", &base_reply),
                    message("user", &newer),
                    message("assistant", &newer_reply),
                    message("user", &follow),
                ],
            ),
            headers,
            &follow,
            &tagged(&case, "resp-follow"),
            &tagged(&case, "follow-reply"),
        )
        .await;
    assert_delta_input(
        &following,
        &newer_response,
        newer_capture.socket_ordinal,
        &follow,
    );
    assert_ne!(following.socket_ordinal, old_capture.socket_ordinal);
    assert_eq!(harness.upstream.snapshot().len(), 4);
    harness.shutdown().await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_owner_completion_order_is_independent() {
    let _environment_lock = env_lock();
    let mut harness = TestHarness::start().await;
    let case = unique("cross-owner-order");
    let session = tagged(&case, "session");
    let a_headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-a"), None);
    let b_headers = IdentityHeaders::agent(&session, &tagged(&case, "agent-b"), None);
    let a1 = tagged(&case, "a-1");
    let b1 = tagged(&case, "b-1");
    let a1_reply = tagged(&case, "a-reply-1");
    let b1_reply = tagged(&case, "b-reply-1");
    let a1_response = tagged(&case, "resp-a-1");
    let b1_response = tagged(&case, "resp-b-1");

    let a1_request = harness.start_request(
        messages_body(false, vec![message("user", &a1)]),
        a_headers.clone(),
    );
    let b1_request = harness.start_request(
        messages_body(false, vec![message("user", &b1)]),
        b_headers.clone(),
    );
    let pending_one = harness.upstream.next_any_request(Some(&session)).await;
    let pending_two = harness.upstream.next_any_request(Some(&session)).await;
    let (a1_pending, b1_pending) = pending_pair(pending_one, pending_two, &a1, &b1);
    assert_full_input(&a1_pending.captured, &[("user", &a1)]);
    assert_full_input(&b1_pending.captured, &[("user", &b1)]);
    assert_eq!(
        HashSet::from([
            a1_pending.captured.socket_ordinal,
            b1_pending.captured.socket_ordinal,
        ]),
        HashSet::from([1, 2])
    );

    let b1_capture = resolve_request(b1_pending, b1_request, &b1_response, &b1_reply, false).await;
    let a1_capture = resolve_request(a1_pending, a1_request, &a1_response, &a1_reply, false).await;

    let a2 = tagged(&case, "a-2");
    let b2 = tagged(&case, "b-2");
    let a2_reply = tagged(&case, "a-reply-2");
    let b2_reply = tagged(&case, "b-reply-2");
    let a2_response = tagged(&case, "resp-a-2");
    let b2_response = tagged(&case, "resp-b-2");
    let a2_request = harness.start_request(
        messages_body(
            false,
            vec![
                message("user", &a1),
                message("assistant", &a1_reply),
                message("user", &a2),
            ],
        ),
        a_headers.clone(),
    );
    let b2_request = harness.start_request(
        messages_body(
            false,
            vec![
                message("user", &b1),
                message("assistant", &b1_reply),
                message("user", &b2),
            ],
        ),
        b_headers.clone(),
    );
    let pending_one = harness.upstream.next_any_request(Some(&session)).await;
    let pending_two = harness.upstream.next_any_request(Some(&session)).await;
    let (a2_pending, b2_pending) = pending_pair(pending_one, pending_two, &a2, &b2);
    assert_delta_input(
        &a2_pending.captured,
        &a1_response,
        a1_capture.socket_ordinal,
        &a2,
    );
    assert_delta_input(
        &b2_pending.captured,
        &b1_response,
        b1_capture.socket_ordinal,
        &b2,
    );

    let a2_capture = resolve_request(a2_pending, a2_request, &a2_response, &a2_reply, false).await;
    let b2_capture = resolve_request(b2_pending, b2_request, &b2_response, &b2_reply, false).await;

    let a3 = tagged(&case, "a-3");
    let b3 = tagged(&case, "b-3");
    let a3_request = harness.start_request(
        messages_body(
            false,
            vec![
                message("user", &a1),
                message("assistant", &a1_reply),
                message("user", &a2),
                message("assistant", &a2_reply),
                message("user", &a3),
            ],
        ),
        a_headers,
    );
    let b3_request = harness.start_request(
        messages_body(
            false,
            vec![
                message("user", &b1),
                message("assistant", &b1_reply),
                message("user", &b2),
                message("assistant", &b2_reply),
                message("user", &b3),
            ],
        ),
        b_headers,
    );
    let pending_one = harness.upstream.next_any_request(Some(&session)).await;
    let pending_two = harness.upstream.next_any_request(Some(&session)).await;
    let (a3_pending, b3_pending) = pending_pair(pending_one, pending_two, &a3, &b3);
    assert_delta_input(
        &a3_pending.captured,
        &a2_response,
        a2_capture.socket_ordinal,
        &a3,
    );
    assert_delta_input(
        &b3_pending.captured,
        &b2_response,
        b2_capture.socket_ordinal,
        &b3,
    );

    let a3_capture = resolve_request(
        a3_pending,
        a3_request,
        &tagged(&case, "resp-a-3"),
        &tagged(&case, "a-reply-3"),
        false,
    )
    .await;
    let b3_capture = resolve_request(
        b3_pending,
        b3_request,
        &tagged(&case, "resp-b-3"),
        &tagged(&case, "b-reply-3"),
        false,
    )
    .await;
    assert_eq!(a3_capture.socket_ordinal, a1_capture.socket_ordinal);
    assert_eq!(b3_capture.socket_ordinal, b1_capture.socket_ordinal);
    assert_ne!(a3_capture.socket_ordinal, b3_capture.socket_ordinal);
    assert_eq!(harness.upstream.snapshot().len(), 6);
    harness.shutdown().await;
}
