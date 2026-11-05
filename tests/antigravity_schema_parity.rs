//! Public, synthetic production-library contract tests. The library dependency is
//! built without cfg(test); TLS verification and configuration use its normal APIs.
//! The bundled private key is a public test asset, never an account credential.
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use claude_code_proxy::{
    anthropic::schema::MessagesRequest,
    provider::{Provider, RequestContext},
    providers::antigravity::AntigravityProvider,
};
use futures_util::FutureExt;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io::Read,
    net::SocketAddr,
    panic::AssertUnwindSafe,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

const MODEL: &str = "antigravity/gemini-4-argon-high";
const PROFILE: &str = "synthetic-schema-profile";
const PROJECT: &str = "synthetic-schema-project";
const TOKEN: &str = "public-synthetic-token-not-a-credential";
const BODY_LIMIT: usize = 1024 * 1024;
const STEP_LIMIT: Duration = Duration::from_secs(8);
const CA: &[u8] = include_bytes!("fixtures/antigravity-schema-parity/ca.pem");
const CERT: &[u8] = include_bytes!("fixtures/antigravity-schema-parity/server.der");
const KEY: &[u8] = include_bytes!("fixtures/antigravity-schema-parity/server-key.der");

// Child environment changes never mutate the test runner's process environment.
// Only this very executable and this exact ignored entrypoint can be launched.
fn isolated(case: &str) {
    let directory = tempfile::tempdir().expect("synthetic child directory");
    for name in ["home", "config", "state", "tmp", "empty-certs"] {
        std::fs::create_dir(directory.path().join(name)).expect("synthetic directory");
    }
    let root = directory.path().join("root.pem");
    std::fs::write(&root, if case == "untrusted" { &[][..] } else { CA })
        .expect("synthetic root file");
    let mut command = Command::new(std::env::current_exe().expect("integration executable"));
    command
        .args([
            "--ignored",
            "--exact",
            "schema_parity_child_entry",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", directory.path().join("home"))
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("XDG_STATE_HOME", directory.path().join("state"))
        .env("CCP_CONFIG_DIR", directory.path().join("config"))
        .env("TMPDIR", directory.path().join("tmp"))
        .env("SSL_CERT_FILE", &root)
        .env("SSL_CERT_DIR", directory.path().join("empty-certs"))
        .env("SCHEMA_PARITY_CASE", case)
        .env("RUST_TEST_THREADS", "1")
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("owned integration child");
    fn reader(pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut output = Vec::new();
            // Closing on overflow also prevents an unbounded writer/reader pair.
            pipe.take(16 * 1024 + 1)
                .read_to_end(&mut output)
                .expect("child pipe");
            output
        })
    }
    let stdout = reader(child.stdout.take().expect("owned stdout"));
    let stderr = reader(child.stderr.take().expect("owned stderr"));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut expired = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("owned child status") {
            break status;
        }
        if Instant::now() >= deadline {
            expired = true;
            child
                .kill()
                .expect("terminate only the owned fixture child");
            break child.wait().expect("reap owned fixture child");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().expect("join stdout reader");
    let stderr = stderr.join().expect("join stderr reader");
    assert!(!expired, "synthetic child exceeded its finite deadline");
    assert!(
        stdout.len() <= 16 * 1024 && stderr.len() <= 16 * 1024,
        "child output bound"
    );
    assert!(
        status.success(),
        "synthetic case {case} failed ({status})\n{}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        String::from_utf8_lossy(&stdout).contains("1 passed"),
        "child test did not run"
    );
}

macro_rules! cases {
    ($($name:ident => $case:literal),+ $(,)?) => {$(
        #[test]
        fn $name() { isolated($case); }
    )+};
}
cases! {
    schema_parity_verified_tls_control => "tls",
    schema_parity_tls_rejects_untrusted_root => "untrusted",
    schema_parity_baseline_validation_control => "validation",
    schema_parity_v1_continuation_control => "continuation",
    schema_parity_quoted_int64_limits => "limits",
    schema_parity_presence_sensitive_defaults => "defaults",
    schema_parity_zero_constraint_omission => "zeros",
    schema_parity_closed_empty_parameters => "closed-empty",
    schema_parity_explicit_auto_validated => "auto",
    schema_parity_empty_pool_choice_controls => "choice-controls",
    schema_parity_whole_tool_legacy_fallback => "fallback",
    schema_parity_eligibility_selection => "eligibility",
    schema_parity_numeric_variants_and_budgets => "numeric-budgets",
    schema_parity_original_admission_controls => "admission",
    schema_parity_project_admission_control => "project-admission",
}

#[test]
#[ignore = "fixed subprocess entrypoint; invoked only with isolated synthetic environment"]
fn schema_parity_child_entry() {
    let case = std::env::var("SCHEMA_PARITY_CASE").expect("fixed synthetic case selector");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("fixture runtime");
    runtime.block_on(async {
        let mut fixture = Fixture::start()
            .await
            .expect("start verified HTTPS fixture");
        // Even a failed assertion/panic must first close and join fixture work.
        let result = AssertUnwindSafe(tokio::time::timeout(
            Duration::from_secs(100),
            exercise(&case, &mut fixture),
        ))
        .catch_unwind()
        .await;
        let closed = fixture.close().await;
        closed.expect("fixture closure and synthetic store preservation");
        match result {
            Ok(Ok(result)) => result.expect("synthetic contract"),
            Ok(Err(_)) => panic!("synthetic case deadline"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}

// axum's public Listener API avoids either a new Hyper dependency or a
// production-only test transport hook. Handshakes are verified by the client.
struct HttpsListener {
    tcp: TcpListener,
    tls: TlsAcceptor,
}
impl axum::serve::Listener for HttpsListener {
    type Io = TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.tcp.accept().await {
                Ok((stream, address)) => {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(Duration::from_secs(3), self.tls.accept(stream)).await
                    {
                        return (stream, address);
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

#[derive(Default)]
struct Script {
    seen: Vec<Value>,
    replies: VecDeque<Value>,
    failures: Vec<&'static str>,
}
type Shared = Arc<Mutex<Script>>;

async fn generation(State(state): State<Shared>, request: Request) -> Response {
    let valid_headers = request.method() == "POST"
        && request.uri().path() == "/v1internal:streamGenerateContent"
        && request.uri().query() == Some("alt=sse")
        && request
            .headers()
            .get("authorization")
            .is_some_and(|v| v.as_bytes() == format!("Bearer {TOKEN}").as_bytes())
        && request
            .headers()
            .get("accept")
            .is_some_and(|v| v == "text/event-stream");
    if !valid_headers {
        state
            .lock()
            .unwrap()
            .failures
            .push("unexpected generation headers or URL");
        return StatusCode::BAD_REQUEST.into_response();
    }
    let raw = match to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(raw) => raw,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let wire: Value = match serde_json::from_slice(&raw) {
        Ok(wire) => wire,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let mut script = state.lock().unwrap();
    if script.seen.len() >= 256 {
        script.failures.push("generation count bound");
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    // Keep the complete request. Do not delete unknown fields or normalize it.
    script.seen.push(wire);
    let Some(parts) = script.replies.pop_front() else {
        script.failures.push("unscripted generation");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let frame = json!({"response":{
        "candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}
    }});
    (
        [("content-type", "text/event-stream")],
        format!("data: {frame}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}

struct Fixture {
    provider: AntigravityProvider,
    state: Shared,
    origin: String,
    stop: Option<oneshot::Sender<()>>,
    server: JoinHandle<std::io::Result<()>>,
    auth_path: PathBuf,
    auth_bytes: Vec<u8>,
}
struct Exchange {
    status: StatusCode,
    stream: bool,
    content_type: String,
    bytes: Vec<u8>,
    wire: Option<Value>,
}
impl Fixture {
    async fn start() -> Result<Self> {
        let config_dir =
            PathBuf::from(std::env::var_os("CCP_CONFIG_DIR").context("isolated config")?);
        let auth_dir = config_dir.join("antigravity");
        std::fs::create_dir(&auth_dir)?;
        let auth_path = auth_dir.join("auth.json");
        let expires = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 7 * 24 * 60 * 60;
        let auth_bytes = serde_json::to_vec(&json!({
            "profile_id":PROFILE,"project_id":PROJECT,"access_token":TOKEN,
            "refresh_token":null,"expires_at":expires
        }))?;
        std::fs::write(&auth_path, &auth_bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))?;
        }
        let tcp = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let origin = format!("https://{}", tcp.local_addr()?);
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&json!({"antigravity":{
                "enabled":true,"baseUrl":origin,"projectId":PROJECT
            }}))?,
        )?;
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
        )?;
        let listener = HttpsListener {
            tcp,
            tls: TlsAcceptor::from(Arc::new(tls)),
        };
        let state: Shared = Arc::default();
        let app = Router::new()
            .route("/control", get(|| async { "synthetic verified TLS" }))
            .route("/v1internal:streamGenerateContent", post(generation))
            .fallback({
                let state = state.clone();
                move || {
                    let state = state.clone();
                    async move {
                        state.lock().unwrap().failures.push("unexpected endpoint");
                        StatusCode::NOT_FOUND
                    }
                }
            })
            .with_state(state.clone());
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Ok(Self {
            provider: AntigravityProvider::new(),
            state,
            origin,
            stop: Some(stop),
            server,
            auth_path,
            auth_bytes,
        })
    }

    async fn close(mut self) -> Result<()> {
        let maintenance =
            tokio::time::timeout(STEP_LIMIT, self.provider.finish_maintenance()).await;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let joined = tokio::time::timeout(Duration::from_secs(3), &mut self.server).await;
        match joined {
            Ok(result) => result??,
            Err(_) => {
                self.server.abort();
                let _ = self.server.await;
                bail!("HTTPS server required cancellation after graceful deadline");
            }
        }
        maintenance.context("maintenance deadline")??;
        ensure!(
            self.state.lock().unwrap().failures.is_empty(),
            "unexpected fixture endpoint or request"
        );
        ensure!(
            std::fs::read(&self.auth_path)? == self.auth_bytes,
            "synthetic auth store changed"
        );
        Ok(())
    }

    async fn exchange(&mut self, body: Value, parts: Value) -> Result<Exchange> {
        let before = {
            let mut script = self.state.lock().unwrap();
            ensure!(script.replies.is_empty(), "unconsumed fixture reply");
            script.replies.push_back(parts);
            script.seen.len()
        };
        let body: MessagesRequest = serde_json::from_value(body)?;
        let response =
            tokio::time::timeout(STEP_LIMIT, self.provider.handle_messages(body, context()))
                .await
                .context("provider response deadline")?;
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = tokio::time::timeout(STEP_LIMIT, to_bytes(response.into_body(), BODY_LIMIT))
            .await
            .context("downstream body deadline")??
            .to_vec();
        let mut script = self.state.lock().unwrap();
        ensure!(
            script.seen.len() <= before + 1,
            "unexpected generation retry"
        );
        let wire = script.seen.get(before).cloned();
        if wire.is_none() {
            script.replies.pop_front();
        }
        ensure!(script.replies.is_empty(), "reply ownership");
        let stream = content_type.starts_with("text/event-stream");
        Ok(Exchange {
            status,
            stream,
            content_type,
            bytes,
            wire,
        })
    }

    async fn project(
        &mut self,
        stream: bool,
        tools: Value,
        choice: Option<Value>,
    ) -> Result<Exchange> {
        let mut body = request(stream);
        body["tools"] = tools;
        if let Some(choice) = choice {
            body["tool_choice"] = choice;
        }
        let exchange = self
            .exchange(body, json!([{"text":"synthetic complete"}]))
            .await?;
        exchange.success(stream)?;
        Ok(exchange)
    }
}

fn context() -> RequestContext {
    RequestContext {
        req_id: "synthetic-schema-request".into(),
        session_id: None,
        session_seq: None,
        provider: "antigravity".into(),
        traffic: None,
        monitor: None,
    }
}
fn request(stream: bool) -> Value {
    json!({"model":MODEL,"max_tokens":137,"stream":stream,
        "messages":[{"role":"user","content":"Synthetic schema request."}]})
}
fn tool(name: &str, schema: Value) -> Value {
    json!({"name":name,"description":"Synthetic function.","input_schema":schema})
}
fn root(node: Value) -> Value {
    json!({"type":"object","properties":{"value":node},"required":["value"],"additionalProperties":false})
}
fn selected_root(node: Value) -> Value {
    json!({"type":"OBJECT","properties":{"value":node},"required":["value"]})
}
fn legacy_hint(constraints: Value) -> String {
    format!(
        "\nOriginal schema constraints are validated locally after JSON decoding; invalid calls are rejected, never repaired. Constraints: {constraints}"
    )
}
fn legacy_root(node: Value) -> Value {
    let mut result = selected_root(node);
    result["description"] = json!(legacy_hint(json!({"additionalProperties":false})));
    result
}
fn json_transport(schema: &Value) -> Value {
    json!({"type":"STRING","description":format!(
        "JSON-string transport: encode the original JSON value exactly as a JSON string. The proxy decodes and validates this original schema before returning a tool call. Original JSON Schema: {schema}"
    )})
}

// Preserved legacy/original values use serde_json's exact integer-aware equality.
// In particular, never round an unsafe integer through f64 in these assertions.
fn exact(actual: &Value, expected: &Value, label: &str) -> Result<()> {
    ensure!(
        actual == expected,
        "{label}: complete structural value differs"
    );
    Ok(())
}

// Only the selected projection's NumberValue domains permit integer/float
// spelling differences. Unsafe integer variants are not in that selected domain.
fn number_value_equivalent(actual: &Value, expected: &Value) -> bool {
    fn number(value: &serde_json::Number) -> Option<f64> {
        const SAFE: i64 = 9_007_199_254_740_991;
        if let Some(value) = value.as_i64() {
            return if (-SAFE..=SAFE).contains(&value) {
                Some(value as f64)
            } else {
                None
            };
        }
        if let Some(value) = value.as_u64() {
            return if value <= SAFE as u64 {
                Some(value as f64)
            } else {
                None
            };
        }
        value.as_f64().filter(|value| value.is_finite())
    }
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            matches!((number(a), number(b)), (Some(a), Some(b)) if a == b)
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| number_value_equivalent(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| number_value_equivalent(a, b)))
        }
        _ => actual == expected,
    }
}
fn selected_equivalent(actual: &Value, expected: &Value) -> bool {
    let (Some(actual), Some(expected)) = (actual.as_object(), expected.as_object()) else {
        return false;
    };
    actual.len() == expected.len()
        && actual.iter().all(|(key, actual)| {
            expected
                .get(key)
                .is_some_and(|expected| match key.as_str() {
                    "default" | "minimum" | "maximum" => number_value_equivalent(actual, expected),
                    "items" => selected_equivalent(actual, expected),
                    "properties" => {
                        let (Some(actual), Some(expected)) =
                            (actual.as_object(), expected.as_object())
                        else {
                            return false;
                        };
                        actual.len() == expected.len()
                            && actual.iter().all(|(name, actual)| {
                                expected
                                    .get(name)
                                    .is_some_and(|expected| selected_equivalent(actual, expected))
                            })
                    }
                    // Includes enum/required and quoted int64 limits: no numeric coercion.
                    _ => actual == expected,
                })
        })
}
fn selected_exact(actual: &Value, expected: &Value, label: &str) -> Result<()> {
    ensure!(
        selected_equivalent(actual, expected),
        "{label}: selected structural value differs"
    );
    Ok(())
}
fn comparison_oracle_control() -> Result<()> {
    for (left, right) in [
        (
            json!(9_007_199_254_740_992_u64),
            json!(9_007_199_254_740_993_u64),
        ),
        (json!(u64::MAX), json!(u64::MAX - 1)),
        (json!(i64::MIN), json!(i64::MIN + 1)),
        (
            json!(9_007_199_254_740_992_u64),
            json!(9_007_199_254_740_992.0),
        ),
    ] {
        let left = legacy_root(json!({"type":"NUMBER","maximum":left}));
        let right = legacy_root(json!({"type":"NUMBER","maximum":right}));
        ensure!(
            exact(&left, &right, "oracle unequal legacy integers").is_err(),
            "legacy oracle hid integer corruption"
        );
        ensure!(
            exact(&right, &left, "oracle unequal legacy integers reversed").is_err(),
            "legacy oracle comparison is asymmetric"
        );
        exact(&left, &left, "oracle unchanged legacy value")?;
    }
    selected_exact(
        &selected_root(json!({"type":"NUMBER","minimum":1.0,"default":{"nested":[0.0,-0.0,1e20]}})),
        &selected_root(json!({"type":"NUMBER","minimum":1,"default":{"nested":[0,0,1e20]}})),
        "selected NumberValue spelling equivalence",
    )?;
    ensure!(
        !selected_equivalent(
            &selected_root(json!({"type":"STRING","minLength":1})),
            &selected_root(json!({"type":"STRING","minLength":"1"})),
        ),
        "selected oracle coerced quoted int64 domain"
    );
    ensure!(
        !selected_equivalent(
            &selected_root(json!({"type":"NUMBER","enum":[1]})),
            &selected_root(json!({"type":"NUMBER","enum":[1.0]})),
        ),
        "NumberValue semantics escaped into enum domain"
    );
    ensure!(
        !number_value_equivalent(&json!(u64::MAX), &json!(u64::MAX - 1)),
        "unsafe integers entered selected NumberValue comparison"
    );
    Ok(())
}
impl Exchange {
    fn wire(&self) -> Result<&Value> {
        self.wire
            .as_ref()
            .context("expected one production HTTPS generation")
    }
    fn parameters(&self, index: usize) -> Result<&Value> {
        self.wire()?
            .pointer(&format!(
                "/request/tools/{index}/functionDeclarations/0/parameters"
            ))
            .context("present function parameters")
    }
    fn success(&self, stream: bool) -> Result<()> {
        ensure!(
            self.status == StatusCode::OK,
            "expected successful downstream HTTP status"
        );
        ensure!(self.stream == stream, "downstream mode changed");
        let blocks = self.blocks()?;
        ensure!(
            blocks.as_array().is_some_and(|b| !b.is_empty()),
            "missing output blocks"
        );
        Ok(())
    }
    fn blocks(&self) -> Result<Value> {
        if !self.stream {
            return Ok(serde_json::from_slice::<Value>(&self.bytes)?["content"].clone());
        }
        let mut blocks: Vec<Value> = Vec::new();
        let mut stopped = false;
        for frame in std::str::from_utf8(&self.bytes)?
            .split("\n\n")
            .filter(|s| !s.is_empty())
        {
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .context("SSE data")?;
            let event: Value = serde_json::from_str(data)?;
            match event["type"].as_str() {
                Some("content_block_start") => {
                    ensure!(
                        event["index"].as_u64() == Some(blocks.len() as u64),
                        "SSE block order"
                    );
                    blocks.push(event["content_block"].clone());
                }
                Some("content_block_delta") => {
                    let index = event["index"].as_u64().context("SSE index")? as usize;
                    let block = blocks.get_mut(index).context("SSE delta block")?;
                    let delta = &event["delta"];
                    match delta["type"].as_str() {
                        Some("thinking_delta") => append(block, "thinking", &delta["thinking"])?,
                        Some("signature_delta") => append(block, "signature", &delta["signature"])?,
                        Some("text_delta") => append(block, "text", &delta["text"])?,
                        Some("input_json_delta") => {
                            block["input"] = serde_json::from_str(
                                delta["partial_json"].as_str().context("SSE input JSON")?,
                            )?;
                        }
                        _ => bail!("unexpected SSE delta"),
                    }
                }
                Some("message_start" | "content_block_stop" | "message_delta") => {}
                Some("message_stop") => stopped = true,
                _ => bail!("unexpected downstream SSE event or error"),
            }
        }
        ensure!(stopped, "missing SSE terminal event");
        Ok(json!(blocks))
    }
    fn rejected_arguments(&self, stream: bool) -> Result<()> {
        ensure!(
            self.wire.is_some(),
            "argument validation must occur after actual HTTPS generation"
        );
        if stream {
            ensure!(
                self.status == StatusCode::OK && self.stream,
                "streaming validation response mode"
            );
            let text = std::str::from_utf8(&self.bytes)?;
            ensure!(
                text.contains("event: error\n")
                    && text.contains("upstream tool arguments violate original schema"),
                "missing streaming original-schema validation error"
            );
            ensure!(
                !text.contains("event: message_stop\n"),
                "invalid generated call was emitted"
            );
        } else {
            ensure!(
                self.status == StatusCode::BAD_GATEWAY,
                "invalid generated call must fail"
            );
            let error: Value = serde_json::from_slice(&self.bytes)?;
            ensure!(
                error["error"]["message"]
                    .as_str()
                    .is_some_and(|s| s.contains("upstream tool arguments violate original schema")),
                "wrong local validation error"
            );
        }
        Ok(())
    }
}
fn append(block: &mut Value, key: &str, value: &Value) -> Result<()> {
    let suffix = value.as_str().context("SSE string delta")?;
    let current = block[key].as_str().context("SSE initial string")?;
    block[key] = json!(format!("{current}{suffix}"));
    Ok(())
}

async fn exercise(case: &str, f: &mut Fixture) -> Result<()> {
    match case {
        "tls" => tls_control(f).await,
        "untrusted" => untrusted_control(f).await,
        "validation" => validation_control(f).await,
        "continuation" => continuation_control(f).await,
        "limits" => quoted_limits(f).await,
        "defaults" => defaults(f).await,
        "zeros" => zero_constraints(f).await,
        "closed-empty" => closed_empty(f).await,
        "auto" => auto_choice(f).await,
        "choice-controls" => choice_controls(f).await,
        "fallback" => fallback(f).await,
        "eligibility" => eligibility(f).await,
        "numeric-budgets" => numeric_budgets(f).await,
        "admission" => admission(f).await,
        "project-admission" => project_admission(f).await,
        _ => bail!("unknown fixed fixture case"),
    }
}

async fn tls_control(f: &mut Fixture) -> Result<()> {
    comparison_oracle_control()?;
    let response = reqwest::Client::builder()
        .build()?
        .get(format!("{}/control", f.origin))
        .timeout(STEP_LIMIT)
        .send()
        .await?;
    ensure!(
        response.status() == StatusCode::OK,
        "plain verified HTTPS control"
    );
    ensure!(
        response.text().await? == "synthetic verified TLS",
        "TLS control response"
    );
    for stream in [false, true] {
        for (model, level) in [
            ("antigravity/gemini-4-argon-low", "LOW"),
            ("antigravity/gemini-4-argon-medium", "MEDIUM"),
            (MODEL, "HIGH"),
        ] {
            let mut body = request(stream);
            body["model"] = json!(model);
            let exchange = f
                .exchange(body, json!([{"text":"synthetic complete"}]))
                .await?;
            exchange.success(stream)?;
            let wire = exchange.wire()?;
            let session = wire["request"]["sessionId"]
                .as_str()
                .context("session ID")?;
            ensure!(
                session
                    .parse::<i64>()
                    .is_ok_and(|n| n > 0 && n.to_string() == session),
                "baseline session format"
            );
            ensure!(
                wire["requestId"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("ccp-") && s.len() == 40),
                "request identity format"
            );
            exact(
                wire,
                &json!({
                    "project":PROJECT,"model":model.strip_prefix("antigravity/").unwrap(),
                    "requestId":wire["requestId"],"userAgent":"antigravity","requestType":"agent",
                    "request":{"sessionId":session,"contents":[{"role":"user","parts":[{"text":"Synthetic schema request."}]}],
                        "generationConfig":{"maxOutputTokens":137,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":level}}}
                }),
                "baseline complete wire",
            )?;
        }
    }
    Ok(())
}
async fn untrusted_control(f: &mut Fixture) -> Result<()> {
    let plain = reqwest::Client::builder()
        .build()?
        .get(format!("{}/control", f.origin))
        .timeout(STEP_LIMIT)
        .send()
        .await;
    ensure!(
        plain.is_err(),
        "synthetic root must be necessary, never bypass TLS verification"
    );
    for stream in [false, true] {
        let exchange = f
            .exchange(request(stream), json!([{"text":"unreachable"}]))
            .await?;
        ensure!(
            exchange.status == StatusCode::BAD_GATEWAY && exchange.wire.is_none(),
            "production client accepted untrusted TLS"
        );
        ensure!(
            !exchange.stream && exchange.content_type.starts_with("application/json"),
            "TLS header error mode"
        );
    }
    Ok(())
}
async fn validation_control(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        let mut body = request(stream);
        body["tools"] = json!([tool(
            "generic_checked",
            root(json!({"type":"integer","minimum":2}))
        )]);
        let valid = f.exchange(body.clone(), json!([{"functionCall":{"name":"generic_checked","args":{"value":2},"id":"synthetic-native-id"}}])).await?;
        valid.success(stream)?;
        ensure!(
            valid
                .blocks()?
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use" && b["input"] == json!({"value":2})),
            "valid original-schema tool input"
        );
        f.exchange(
            body,
            json!([{"functionCall":{"name":"generic_checked","args":{"value":1}}}]),
        )
        .await?
        .rejected_arguments(stream)?;
        // Draft 2020-12 admits an empty enum, but it validates no values.
        // Preserve that original admission/validation rather than inventing a rejection.
        let mut body = request(stream);
        body["tools"] = json!([tool(
            "generic_enum",
            root(json!({"type":"string","enum":[]}))
        )]);
        f.exchange(
            body,
            json!([{"functionCall":{"name":"generic_enum","args":{"value":""}}}]),
        )
        .await?
        .rejected_arguments(stream)?;
        // A default is guidance only: missing required arguments are never repaired.
        let mut body = request(stream);
        body["tools"] = json!([tool(
            "generic_default",
            root(json!({"type":"integer","default":3}))
        )]);
        f.exchange(
            body,
            json!([{"functionCall":{"name":"generic_default","args":{}}}]),
        )
        .await?
        .rejected_arguments(stream)?;
    }
    Ok(())
}

async fn quoted_limits(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        let schema = root(json!({"type":"array","minItems":1,"maxItems":4,
            "items":{"type":"string","minLength":2,"maxLength":9}}));
        let exchange = f
            .project(stream, json!([tool("generic_limits", schema)]), None)
            .await?;
        exact(
            &exchange.parameters(0)?["properties"]["value"]["minItems"],
            &json!("1"),
            "minItems must be a quoted int64",
        )?;
        exact(
            exchange.parameters(0)?,
            &selected_root(json!({"type":"ARRAY","minItems":"1","maxItems":"4",
            "items":{"type":"STRING","minLength":"2","maxLength":"9"}})),
            "quoted int64 limits",
        )?;
    }
    Ok(())
}
async fn defaults(f: &mut Fixture) -> Result<()> {
    let values = [
        json!(false),
        json!(0),
        json!(0.0),
        json!(-0.0),
        json!(""),
        Value::Null,
        json!({}),
        json!([]),
        json!({"items":{"allOf":[null,false,0,""]},"type":"ordinary data","default":{"required":[]}}),
    ];
    for stream in [false, true] {
        for value in &values {
            let schema = root(json!({"type":"string","default":value}));
            let exchange = f
                .project(stream, json!([tool("generic_defaults", schema)]), None)
                .await?;
            ensure!(
                exchange.parameters(0)?["properties"]["value"]
                    .get("default")
                    .is_some(),
                "explicit default must retain presence"
            );
            selected_exact(
                exchange.parameters(0)?,
                &selected_root(json!({"type":"STRING","default":value})),
                "explicit default presence",
            )?;
        }
        let exchange = f
            .project(
                stream,
                json!([tool("generic_defaults", root(json!({"type":"string"})))]),
                None,
            )
            .await?;
        exact(
            exchange.parameters(0)?,
            &selected_root(json!({"type":"STRING"})),
            "absent default stays absent",
        )?;
    }
    Ok(())
}
async fn zero_constraints(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        let schema = json!({"type":"object","properties":{
            "text":{"type":"string","minLength":0,"maxLength":0},
            "list":{"type":"array","items":{"type":"boolean"},"minItems":0,"maxItems":0},
            "number":{"type":"number","minimum":-0.0,"maximum":0.0,"default":0}
        },"required":["text","list","number"],"additionalProperties":false});
        let mut body = request(stream);
        body["tools"] = json!([tool("generic_zero", schema)]);
        let valid_args = json!({"text":"","list":[],"number":0});
        let valid = f
            .exchange(
                body.clone(),
                json!([{"functionCall":{"name":"generic_zero","args":valid_args}}]),
            )
            .await?;
        valid.success(stream)?;
        ensure!(
            valid.parameters(0)?["properties"]["text"]
                .get("maxLength")
                .is_none(),
            "zero maxLength must omit on wire"
        );
        selected_exact(
            valid.parameters(0)?,
            &json!({"type":"OBJECT","properties":{
            "text":{"type":"STRING"},"list":{"type":"ARRAY","items":{"type":"BOOLEAN"}},
            "number":{"type":"NUMBER","default":0}
        },"required":["text","list","number"]}),
            "zero constraints omit but default remains",
        )?;
        for invalid in [
            json!({"text":"x","list":[],"number":0}),
            json!({"text":"","list":[true],"number":0}),
            json!({"text":"","list":[],"number":1}),
            json!({"text":"","list":[],"number":-1}),
        ] {
            f.exchange(
                body.clone(),
                json!([{"functionCall":{"name":"generic_zero","args":invalid}}]),
            )
            .await?
            .rejected_arguments(stream)?;
        }
    }
    Ok(())
}
async fn closed_empty(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        let schema =
            json!({"type":"object","properties":{},"required":[],"additionalProperties":false});
        let mut body = request(stream);
        body["tools"] = json!([tool("generic_empty", schema)]);
        let exchange = f
            .exchange(
                body.clone(),
                json!([{"functionCall":{"name":"generic_empty"}}]),
            )
            .await?;
        exchange.success(stream)?;
        exact(
            exchange.parameters(0)?,
            &json!({"type":"OBJECT"}),
            "closed-empty parameters must be present",
        )?;
        ensure!(
            exchange
                .blocks()?
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use" && b["input"] == json!({})),
            "omitted args must remain empty"
        );
        let envelope = envelope(&exchange.blocks()?)?;
        ensure!(
            envelope["contents"][0]["parts"][0]["functionCall"]
                .get("args")
                .is_none(),
            "saved omitted args were repaired"
        );
        exact(
            &envelope["tool_mapping"][0]["codec_paths"],
            &json!([]),
            "closed-empty codec paths",
        )?;
        ensure!(
            envelope["tool_mapping"][0]["root_wrapper"].is_null(),
            "closed-empty root recipe changed"
        );
        f.exchange(
            body,
            json!([{"functionCall":{"name":"generic_empty","args":{"extra":true}}}]),
        )
        .await?
        .rejected_arguments(stream)?;
    }
    Ok(())
}
async fn auto_choice(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        let eligible = tool("generic_auto", root(json!({"type":"string"})));
        let legacy = tool("generic_legacy", root(json!({"type":"string","const":"x"})));
        let empty = tool(
            "generic_empty",
            json!({"type":"object","additionalProperties":false}),
        );
        for tools in [
            json!([eligible.clone()]),
            json!([legacy.clone()]),
            json!([eligible, legacy]),
            json!([empty]),
        ] {
            let exchange = f
                .project(stream, tools, Some(json!({"type":"auto"})))
                .await?;
            exact(
                &exchange.wire()?["request"]["toolConfig"],
                &json!({"functionCallingConfig":{"mode":"VALIDATED"}}),
                "explicit auto mode",
            )?;
        }
    }
    Ok(())
}
async fn choice_controls(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        for tools in [None, Some(json!([]))] {
            for choice in [
                None,
                Some(json!({"type":"auto"})),
                Some(json!({"type":"none"})),
            ] {
                let mut body = request(stream);
                if let Some(tools) = &tools {
                    body["tools"] = tools.clone();
                }
                if let Some(choice) = choice {
                    body["tool_choice"] = choice;
                }
                let exchange = f
                    .exchange(body, json!([{"text":"synthetic complete"}]))
                    .await?;
                exchange.success(stream)?;
                let wire = &exchange.wire()?["request"];
                ensure!(
                    wire.get("tools").is_none() && wire.get("toolConfig").is_none(),
                    "empty tool pool must omit declarations and config"
                );
            }
        }
        let tools = json!([tool("generic_choice", root(json!({"type":"string"})))]);
        for choice in [None, Some(json!({"type":"none"}))] {
            let exchange = f.project(stream, tools.clone(), choice.clone()).await?;
            let wire = &exchange.wire()?["request"];
            ensure!(
                wire.get("toolConfig").is_none(),
                "absent/none tool config changed"
            );
            ensure!(
                wire.get("tools").is_some() == choice.is_none(),
                "none tool declarations changed"
            );
        }
        for choice in [
            json!({"type":"any"}),
            json!({"type":"required"}),
            json!({"type":"tool","name":"generic_choice"}),
            json!({"type":"auto","disable_parallel_tool_use":true}),
        ] {
            let mut body = request(stream);
            body["tools"] = tools.clone();
            body["tool_choice"] = choice;
            let exchange = f.exchange(body, json!([{"text":"unreachable"}])).await?;
            ensure!(
                exchange.status == StatusCode::BAD_REQUEST && exchange.wire.is_none(),
                "unsupported choice admission changed"
            );
        }
    }
    Ok(())
}

async fn fallback(f: &mut Fixture) -> Result<()> {
    let constrained = |kind: &str, key: &str, value: Value| {
        let mut source = json!({"type":kind});
        source[key] = value.clone();
        let mut expected = json!({"type":kind.to_ascii_uppercase()});
        expected["description"] = json!(legacy_hint(json!({(key):value})));
        (source, expected)
    };
    let mut cases = vec![
        constrained("string", "const", json!("x")),
        constrained("number", "multipleOf", json!(2)),
        constrained("number", "exclusiveMinimum", json!(1.5)),
        constrained("integer", "format", json!("date")),
        constrained("string", "enum", json!(["x", 7, ""])),
        constrained("number", "enum", json!([1, 2])),
        (
            json!({"type":"string","minimum":1}),
            json!({"type":"STRING","minimum":1}),
        ),
        (
            json!({"type":"array","items":{"type":"string"},"uniqueItems":true}),
            json!({"type":"ARRAY","items":{"type":"STRING"},"description":legacy_hint(json!({"uniqueItems":true}))}),
        ),
        (
            json!({"type":"object","properties":{"x":{"type":"string"}},"minProperties":1}),
            json!({"type":"OBJECT","properties":{"x":{"type":"STRING"}},"description":legacy_hint(json!({"minProperties":1}))}),
        ),
        (
            json!({"type":"array","items":{"type":"string"},"properties":{"x":{"type":"string"}}}),
            json!({"type":"ARRAY","items":{"type":"STRING"}}),
        ),
        (
            json!({"type":"string","default":9007199254740992_u64}),
            json!({"type":"STRING"}),
        ),
    ];
    let union = json!({"oneOf":[{"type":"string"},{"type":"integer"}]});
    cases.push((union.clone(), json_transport(&union)));
    let object_extra = json!({"type":"object","properties":{"x":{"type":"string"}},"additionalProperties":{"type":"boolean"}});
    cases.push((object_extra.clone(), json_transport(&object_extra)));
    let boolean = json!(true);
    cases.push((boolean.clone(), json_transport(&boolean)));
    for stream in [false, true] {
        for (source, expected) in &cases {
            let mut schema = root(source.clone());
            schema["title"] = json!("Synthetic legacy marker");
            let mut expected = legacy_root(expected.clone());
            expected["title"] = json!("Synthetic legacy marker");
            let exchange = f
                .project(stream, json!([tool("generic_fallback", schema)]), None)
                .await?;
            exact(
                exchange.parameters(0)?,
                &expected,
                "whole-tool fallback, including unsupported child",
            )?;
        }
        // Aliased names must keep their full legacy declaration and codec plan.
        let name = "generic spaced name";
        let schema = root(json!({"type":"string","default":"ignored by legacy"}));
        let exchange = f.project(stream, json!([tool(name, schema)]), None).await?;
        let native = format!("ccp_{:x}", Sha256::digest(name.as_bytes()))[..64].to_owned();
        exact(
            &exchange.wire()?["request"]["tools"][0],
            &json!({"functionDeclarations":[{
                "name":native,"description":"Synthetic function.","parameters":legacy_root(json!({"type":"STRING"}))
            }]}),
            "aliased legacy declaration",
        )?;
        for schema in [
            json!({"type":"object","additionalProperties":false,"allOf":[{}]}),
            json!({"type":"object","additionalProperties":false,"default":9007199254740992_u64}),
        ] {
            let exchange = f
                .project(
                    stream,
                    json!([tool("generic_ineligible_empty", schema)]),
                    None,
                )
                .await?;
            exact(
                &exchange.wire()?["request"]["tools"][0],
                &json!({"functionDeclarations":[{"name":"generic_ineligible_empty","description":"Synthetic function."}]}),
                "ineligible closed-empty omission",
            )?;
        }
    }
    Ok(())
}

async fn eligibility(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        // A schema-looking property name is data, not an unsupported keyword.
        let schema = json!({"type":"object","properties":{"allOf":{"type":"string"},"items":{"type":"boolean"}},"additionalProperties":false});
        let exchange = f
            .project(stream, json!([tool("generic_keys", schema)]), None)
            .await?;
        exact(
            exchange.parameters(0)?,
            &json!({"type":"OBJECT","properties":{"allOf":{"type":"STRING"},"items":{"type":"BOOLEAN"}}}),
            "property names are not keywords",
        )?;
        let mut schema = root(
            json!({"type":"string","description":"Synthetic caller description.",
            "title":"Synthetic annotation","enum":["","x"],"format":"email","pattern":".*"}),
        );
        schema["title"] = json!("Synthetic root annotation");
        schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        let exchange = f
            .project(stream, json!([tool("generic_annotations", schema)]), None)
            .await?;
        exact(
            exchange.parameters(0)?,
            &selected_root(
                json!({"type":"STRING","description":"Synthetic caller description.",
            "enum":["","x"],"format":"email","pattern":".*"}),
            ),
            "selected annotations and real empty-string enum choice",
        )?;
        let exchange = f
            .project(
                stream,
                json!([tool(
                    "generic_empty_scalars",
                    root(json!({"type":"string","description":"","pattern":""}))
                )]),
                None,
            )
            .await?;
        exact(
            exchange.parameters(0)?,
            &selected_root(json!({"type":"STRING"})),
            "empty scalar strings omit",
        )?;
        let mut body = request(stream);
        body["tools"] = json!([tool(
            "generic_exclusive",
            root(json!({"type":"integer","minimum":-5,"maximum":5,
            "exclusiveMinimum":0,"exclusiveMaximum":3}))
        )]);
        let valid = f
            .exchange(
                body.clone(),
                json!([{"functionCall":{"name":"generic_exclusive","args":{"value":1}}}]),
            )
            .await?;
        valid.success(stream)?;
        selected_exact(
            valid.parameters(0)?,
            &selected_root(json!({"type":"INTEGER","minimum":-5,"maximum":5})),
            "exclusive bounds omitted without conversion",
        )?;
        for value in [0, 3] {
            f.exchange(
                body.clone(),
                json!([{"functionCall":{"name":"generic_exclusive","args":{"value":value}}}]),
            )
            .await?
            .rejected_arguments(stream)?;
        }
        let exchange = f
            .project(
                stream,
                json!([
                    tool(
                        "generic_selected",
                        root(json!({"type":"string","default":false}))
                    ),
                    tool("generic_old", root(json!({"type":"string","const":"x"})))
                ]),
                None,
            )
            .await?;
        exact(
            exchange.parameters(0)?,
            &selected_root(json!({"type":"STRING","default":false})),
            "mixed selected tool",
        )?;
        exact(
            exchange.parameters(1)?,
            &legacy_root(json!({"type":"STRING","description":legacy_hint(json!({"const":"x"}))})),
            "mixed legacy tool",
        )?;
    }
    Ok(())
}

async fn numeric_budgets(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        for value in [
            json!(9007199254740991_i64),
            json!(-9007199254740991_i64),
            json!(1e20),
            json!(1.0),
            json!(-0.0),
        ] {
            let source = root(json!({"type":"number","default":value,"minimum":value}));
            let mut expected = json!({"type":"NUMBER","default":value});
            if value.as_f64().unwrap() != 0.0 {
                expected["minimum"] = value;
            }
            let exchange = f
                .project(stream, json!([tool("generic_number", source)]), None)
                .await?;
            selected_exact(
                exchange.parameters(0)?,
                &selected_root(expected),
                "safe integer or finite float variant",
            )?;
        }
        for value in [
            json!(9007199254740992_u64),
            json!(-9007199254740992_i64),
            json!(i64::MIN),
            json!(i64::MAX),
            json!(u64::MAX),
        ] {
            let source = root(json!({"type":"number","default":value}));
            let exchange = f
                .project(stream, json!([tool("generic_unsafe", source)]), None)
                .await?;
            exact(
                exchange.parameters(0)?,
                &legacy_root(json!({"type":"NUMBER"})),
                "unsafe integer default whole-tool fallback",
            )?;
            let source = root(json!({"type":"number","maximum":value}));
            let exchange = f
                .project(stream, json!([tool("generic_unsafe", source)]), None)
                .await?;
            exact(
                exchange.parameters(0)?,
                &legacy_root(json!({"type":"NUMBER","maximum":value})),
                "unsafe integer bound whole-tool fallback",
            )?;
        }
        for (value, expected) in [
            (json!(1.0), json!({"type":"STRING","minLength":"1"})),
            (json!(-0.0), json!({"type":"STRING"})),
            (
                json!(9007199254740991_u64),
                json!({"type":"STRING","minLength":"9007199254740991"}),
            ),
        ] {
            let exchange = f
                .project(
                    stream,
                    json!([tool(
                        "generic_length",
                        root(json!({"type":"string","minLength":value}))
                    )]),
                    None,
                )
                .await?;
            exact(
                exchange.parameters(0)?,
                &selected_root(expected),
                "integral length numeric variant",
            )?;
        }
        for value in [json!(9007199254740992_u64), json!(u64::MAX), json!(1e20)] {
            let exchange = f
                .project(
                    stream,
                    json!([tool(
                        "generic_length",
                        root(json!({"type":"string","minLength":value}))
                    )]),
                    None,
                )
                .await?;
            exact(
                exchange.parameters(0)?,
                &legacy_root(json!({"type":"STRING","minLength":value})),
                "unsafe length falls back",
            )?;
        }
        let mut deep = json!(null);
        for _ in 0..34 {
            deep = json!([deep]);
        }
        for value in [deep, json!(vec![Value::Null; 4100])] {
            let exchange = f
                .project(
                    stream,
                    json!([tool(
                        "generic_budget",
                        root(json!({"type":"string","default":value}))
                    )]),
                    None,
                )
                .await?;
            exact(
                exchange.parameters(0)?,
                &legacy_root(json!({"type":"STRING"})),
                "default projection depth/node budget fallback",
            )?;
        }
        // Each default is below the node limit; the whole tool is not.
        let schema = json!({"type":"object","properties":{
            "a":{"type":"string","default":vec![Value::Null;2300]},
            "b":{"type":"string","default":vec![Value::Null;2300]}
        },"additionalProperties":false});
        let exchange = f
            .project(stream, json!([tool("generic_shared_budget", schema)]), None)
            .await?;
        exact(
            exchange.parameters(0)?,
            &json!({"type":"OBJECT","properties":{"a":{"type":"STRING"},"b":{"type":"STRING"}},"description":legacy_hint(json!({"additionalProperties":false}))}),
            "shared whole-tool default budget",
        )?;
        let value =
            json!({"items":vec![Value::Null;32],"allOf":"ordinary data","default":{"type":false}});
        let exchange = f
            .project(
                stream,
                json!([tool(
                    "generic_small_budget",
                    root(json!({"type":"string","default":value}))
                )]),
                None,
            )
            .await?;
        exact(
            exchange.parameters(0)?,
            &selected_root(json!({"type":"STRING","default":value})),
            "bounded default data remains selected",
        )?;
    }
    Ok(())
}

async fn admission(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        for schema in [
            root(json!({"type":"string","minLength":-1})),
            root(json!({"type":"string","minLength":1.5})),
            root(json!({"type":"string","format":"unsupported-synthetic-format"})),
            root(json!({"$ref":"https://example.invalid/never-retrieve"})),
            json!({"type":"object","properties":{},"required":["missing"],"additionalProperties":false}),
            root(json!({"type":"string","unknownSyntheticKeyword":true})),
            root(json!({"type":"string","enum":7})),
            root(json!({"type":"string","default":"x".repeat(256*1024)})),
        ] {
            let mut body = request(stream);
            body["tools"] = json!([tool("generic_invalid", schema)]);
            let exchange = f.exchange(body, json!([{"text":"unreachable"}])).await?;
            ensure!(
                exchange.status == StatusCode::BAD_REQUEST && exchange.wire.is_none(),
                "original schema admission weakened or deferred"
            );
        }
        // none must not bypass original tool admission before clearing the pool.
        let mut body = request(stream);
        body["tools"] = json!([tool("generic_invalid", root(json!({"unsupported":true})))]);
        body["tool_choice"] = json!({"type":"none"});
        let exchange = f.exchange(body, json!([{"text":"unreachable"}])).await?;
        ensure!(
            exchange.status == StatusCode::BAD_REQUEST && exchange.wire.is_none(),
            "original admission-first ordering changed"
        );
    }
    Ok(())
}

async fn project_admission(f: &mut Fixture) -> Result<()> {
    // Change only this fixture's synthetic files, never a global environment.
    // Restore them before propagating any assertion error to the common closer.
    let config_path = f
        .auth_path
        .parent()
        .and_then(|p| p.parent())
        .context("synthetic config parent")?
        .join("config.json");
    let config_bytes = std::fs::read(&config_path)?;
    let mut credential: Value = serde_json::from_slice(&f.auth_bytes)?;
    credential["project_id"] = Value::Null;
    std::fs::write(&f.auth_path, serde_json::to_vec(&credential)?)?;
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"antigravity":{
            "enabled":true,"baseUrl":f.origin
        }}))?,
    )?;
    tokio::time::timeout(STEP_LIMIT, f.provider.finish_maintenance()).await??;
    f.provider = AntigravityProvider::new();
    let result = async {
        for stream in [false,true] {
            let mut body = request(stream);
            body["tools"] = json!([tool("generic_invalid",root(json!({"unknownSyntheticKeyword":true})))]);
            let invalid = f.exchange(body,json!([{"text":"unreachable"}])).await?;
            ensure!(invalid.status == StatusCode::BAD_REQUEST && invalid.wire.is_none(),"admission before project resolution");
            let error: Value = serde_json::from_slice(&invalid.bytes)?;
            ensure!(error["error"]["message"] == "unsupported schema keyword or reference","original admission error precedence");
            let missing = f.exchange(request(stream),json!([{"text":"unreachable"}])).await?;
            ensure!(missing.status == StatusCode::BAD_REQUEST && missing.wire.is_none(),"missing project must not send generation");
            let error: Value = serde_json::from_slice(&missing.bytes)?;
            ensure!(error["error"]["message"] == "Antigravity project missing; run auth login or configure your authorized project","legacy missing-project error changed");
        }
        Ok::<(),anyhow::Error>(())
    }.await;
    std::fs::write(&f.auth_path, &f.auth_bytes)?;
    std::fs::write(config_path, config_bytes)?;
    result
}

fn envelope(blocks: &Value) -> Result<Value> {
    let signature = blocks[0]["signature"]
        .as_str()
        .context("V1 carrier signature")?;
    let encoded = signature
        .strip_prefix("ccp:antigravity:v1:")
        .context("V1 prefix unchanged")?;
    let raw = URL_SAFE_NO_PAD.decode(encoded)?;
    ensure!(raw.len() <= BODY_LIMIT, "synthetic V1 envelope bound");
    Ok(serde_json::from_slice(&raw)?)
}
async fn continuation_control(f: &mut Fixture) -> Result<()> {
    for stream in [false, true] {
        let spaced = "generic alias name";
        let native_alias = format!("ccp_{:x}", Sha256::digest(spaced.as_bytes()))[..64].to_owned();
        let tools = json!([
            tool(
                "generic_empty",
                json!({"type":"object","additionalProperties":false})
            ),
            tool("generic_path", root(json!({"type":["string","null"]}))),
            tool(
                spaced,
                json!({"type":"object","additionalProperties":{"type":"string"}})
            ),
            tool(
                "generic_selected",
                root(json!({"type":"integer","default":3}))
            )
        ]);
        let parts = json!([
            {"text":"Synthetic thought.","thought":true,"thoughtSignature":"synthetic-opaque-signature","syntheticUnknown":{"keep":true}},
            {"functionCall":{"name":"generic_empty","id":"synthetic-empty-id"}},
            {"functionCall":{"name":"generic_path","args":{"value":"null"}}},
            {"functionCall":{"name":native_alias,"id":"synthetic-alias-id","args":{"json":"{\"x\":\"y\"}"}}},
            {"functionCall":{"name":"generic_selected","args":{"value":3}}}
        ]);
        let mut body = request(stream);
        body["tools"] = tools.clone();
        let first = f.exchange(body.clone(), parts.clone()).await?;
        first.success(stream)?;
        let blocks = first.blocks()?;
        let saved = envelope(&blocks)?;
        exact(
            &saved["contents"],
            &json!([{"role":"model","parts":parts}]),
            "V1 unknown native contents preserved",
        )?;
        ensure!(
            saved["version"] == 1
                && saved["profile_id"] == PROFILE
                && saved["project_id"] == PROJECT,
            "V1 scope/version"
        );
        let mapping = saved["tool_mapping"].as_array().context("V1 recipes")?;
        ensure!(mapping.len() == 4, "V1 multiple tool mappings");
        for (index, (caller, paths, wrapper)) in [
            ("generic_empty", json!([]), Value::Null),
            ("generic_path", json!(["/value"]), Value::Null),
            (spaced, json!([]), json!("json")),
            ("generic_selected", json!([]), Value::Null),
        ]
        .into_iter()
        .enumerate()
        {
            ensure!(
                mapping[index]["caller_name"] == caller,
                "V1 caller-name recipe"
            );
            exact(&mapping[index]["codec_paths"], &paths, "V1 path recipe")?;
            exact(&mapping[index]["root_wrapper"], &wrapper, "V1 root recipe")?;
        }
        let mut results = Vec::new();
        for block in blocks
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "tool_use")
        {
            results.push(json!({"type":"tool_result","tool_use_id":block["id"],"content":"Synthetic result."}));
        }
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":blocks}),
            json!({"role":"user","content":results}),
        ]);
        // Current declarations can change, but saved codecs and carrier must not.
        body["tools"] = json!([tool(
            "generic_selected",
            root(json!({"type":"integer","default":4}))
        )]);
        let second = f
            .exchange(body, json!([{"text":"Synthetic continuation."}]))
            .await?;
        second.success(stream)?;
        let contents = &second.wire()?["request"]["contents"];
        exact(
            &contents[1],
            &saved["contents"][0],
            "V1 native history exact reuse",
        )?;
        ensure!(
            second.wire()?["request"]["sessionId"] == saved["session_id"],
            "V1 session reuse"
        );
        ensure!(
            contents[2]["parts"]
                .as_array()
                .is_some_and(|p| p.len() == 4),
            "V1 complete result batch"
        );
        for (index, call) in saved["contents"][0]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p.get("functionCall"))
            .enumerate()
        {
            let response = &contents[2]["parts"][index]["functionResponse"];
            ensure!(
                response["name"] == call["name"] && response.get("id") == call.get("id"),
                "V1 native result identity"
            );
        }
    }
    Ok(())
}
