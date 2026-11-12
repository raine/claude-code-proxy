//! Production-library transport fixtures: synthetic credentials and verified loopback TLS only.
//! Every environment-sensitive provider runs in its own child; the library is cfg(test)=false.
use axum::body::to_bytes;
use bytes::Bytes;
use claude_code_proxy::{
    MessagesRequest, Provider, RequestContext, providers::antigravity::AntigravityProvider,
};
use http::{HeaderMap, Response, Version};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::task::{JoinHandle, JoinSet};

// Intentionally public, inert test credentials; never use this CA/key outside fixtures.
const CA: &[u8] = include_bytes!("fixtures/antigravity-transport/ca.pem");
const CERT: &[u8] = include_bytes!("fixtures/antigravity-transport/server.pem");
const KEY: &[u8] = include_bytes!("fixtures/antigravity-transport/server-key.pem");
const CHILD_CASE: &str = "CCP_TRANSPORT_FIXTURE_CASE";
const CHILD_STREAM: &str = "CCP_TRANSPORT_FIXTURE_STREAM";

struct Observed {
    headers: HeaderMap,
    version: Version,
    wire: Value,
}

struct TlsServer {
    url: String,
    observed: Arc<Mutex<Vec<Observed>>>,
    task: JoinHandle<()>,
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn success() -> Vec<u8> {
    format!(
        "data: {}\n\n",
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"TRANSPORT_OK"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12}}})
    )
    .into_bytes()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

impl TlsServer {
    async fn start(protocol: Version, status: u16, encoding: Option<&str>, body: Vec<u8>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            http::HeaderValue::from_static(if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            }),
        );
        headers.insert("retry-after", http::HeaderValue::from_static("7"));
        if let Some(encoding) = encoding {
            headers.insert(
                "content-encoding",
                http::HeaderValue::from_str(encoding).unwrap(),
            );
        }
        Self::start_with_headers(protocol, status, headers, body).await
    }

    async fn start_with_headers(
        protocol: Version,
        status: u16,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Self {
        let certificates = CertificateDer::pem_slice_iter(CERT)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .unwrap();
        config.alpn_protocols = vec![if protocol == Version::HTTP_2 {
            b"h2".to_vec()
        } else {
            b"http/1.1".to_vec()
        }];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captures = observed.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let acceptor = acceptor.clone();
                        let captures = captures.clone();
                        let headers = headers.clone();
                        let body = body.clone();
                        connections.spawn(async move {
                            let Ok(tls) = acceptor.accept(socket).await else { return };
                            let service = service_fn(move |request: http::Request<Incoming>| {
                                let captures = captures.clone();
                                let headers = headers.clone();
                                let body = body.clone();
                                async move {
                                    assert_eq!(request.method(), http::Method::POST);
                                    assert_eq!(request.uri().path_and_query().unwrap().as_str(), "/v1internal:streamGenerateContent?alt=sse");
                                    let (parts, mut incoming) = request.into_parts();
                                    let mut bytes = Vec::new();
                                    while let Some(frame) = incoming.frame().await {
                                        if let Ok(data) = frame.unwrap().into_data() {
                                            assert!(bytes.len() + data.len() <= 65536);
                                            bytes.extend_from_slice(&data);
                                        }
                                    }
                                    captures.lock().unwrap().push(Observed {
                                        headers: parts.headers,
                                        version: parts.version,
                                        wire: serde_json::from_slice(&bytes).unwrap(),
                                    });
                                    let mut response = Response::builder().status(status)
                                        .body(Full::new(Bytes::from(body))).unwrap();
                                    *response.headers_mut() = headers;
                                    Ok::<_, Infallible>(response)
                                }
                            });
                            if protocol == Version::HTTP_2 {
                                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                    .serve_connection(TokioIo::new(tls), service).await;
                            } else {
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(TokioIo::new(tls), service).await;
                            }
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    }
                }
            }
        });
        Self {
            url,
            observed,
            task,
        }
    }
}

fn write_synthetic_store(root: &Path) {
    use std::fs;
    let provider = root.join("config/antigravity");
    fs::create_dir_all(&provider).unwrap();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let path = provider.join("auth.json");
    fs::write(&path, json!({"profile_id":"public-test-profile","project_id":"public-test-project","access_token":"public-test-access","refresh_token":null,"expires_at":expires_at}).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(root.join("ca.pem"), CA).unwrap();
    for directory in [
        "empty-certs",
        "home",
        "tmp",
        "state",
        "cache",
        "data",
        "run",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
}

async fn child(server: &TlsServer, case: &'static str, stream: bool) {
    child_configured(server, case, stream, &server.url, &[]).await;
}

async fn child_configured(
    server: &TlsServer,
    case: &'static str,
    stream: bool,
    base_url: &str,
    proxy_env: &[(&str, &str)],
) {
    let _ = server;
    let directory = tempfile::tempdir().unwrap();
    write_synthetic_store(directory.path());
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .args(["--exact", "antigravity_transport_child", "--nocapture"])
        .env(CHILD_CASE, case)
        .env(CHILD_STREAM, if stream { "1" } else { "0" })
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("RUST_BACKTRACE", "0")
        .env("HOME", directory.path().join("home"))
        .env("TMPDIR", directory.path().join("tmp"))
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("XDG_CACHE_HOME", directory.path().join("cache"))
        .env("XDG_DATA_HOME", directory.path().join("data"))
        .env("XDG_STATE_HOME", directory.path().join("state"))
        .env("XDG_RUNTIME_DIR", directory.path().join("run"))
        .env("CCP_CONFIG_DIR", directory.path().join("config"))
        .env("CCP_ANTIGRAVITY_ENABLED", "1")
        .env("CCP_ANTIGRAVITY_BASE_URL", base_url)
        .env("CCP_ANTIGRAVITY_PROJECT_ID", "public-test-project")
        .env("SSL_CERT_FILE", directory.path().join("ca.pem"))
        .env("SSL_CERT_DIR", directory.path().join("empty-certs"))
        .env("NO_PROXY", "*")
        .env("no_proxy", "*");
    if case == "native-ua-override" {
        command.env("CCP_ANTIGRAVITY_USER_AGENT", "public-native-override");
    }
    if case == "untrusted-ca" {
        let empty_ca = directory.path().join("empty-ca.pem");
        std::fs::write(&empty_ca, b"").unwrap();
        command.env("SSL_CERT_FILE", empty_ca);
    }
    for (key, value) in proxy_env {
        assert!(matches!(
            *key,
            "HTTPS_PROXY"
                | "https_proxy"
                | "HTTP_PROXY"
                | "http_proxy"
                | "ALL_PROXY"
                | "all_proxy"
                | "NO_PROXY"
                | "no_proxy"
                | "REQUEST_METHOD"
        ));
        command.env(key, value);
    }
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.stdout.len() + output.stderr.len() < 32768,
        "bounded synthetic child output"
    );
    assert!(
        output.status.success(),
        "production child assertion failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

fn synthetic_client_setup_class() -> &'static str {
    let native = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let (valid, invalid) = roots.add_parsable_certificates(native.certs);
    let root_class = match (valid > 0, invalid > 0) {
        (true, false) => "valid",
        (true, true) => "partial_invalid",
        (false, true) => "only_invalid",
        (false, false) => "empty",
    };
    eprintln!("FIXTURE_ROOT_CLASS {root_class}");
    // Exact public builder operations from baseline Config::client_builder().
    // Construction only: no endpoint, request, send, credentials or network.
    let result = reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(180))
        .user_agent(format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION")))
        .build();
    let Err(error) = result else {
        return "replica_client_ok";
    };
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    for _ in 0..8 {
        let Some(current) = cause else { break };
        // Inspect in memory; never print arbitrary error objects/strings or paths.
        match current.to_string().as_str() {
            "zero valid certificates found in native root store" => return "native_roots_invalid",
            "empty supported tls versions" => return "tls_versions_empty",
            "invalid TLS versions" => return "tls_versions_invalid",
            "invalid TLS verification settings" => return "tls_verifier_invalid",
            "invalid certificate encoding" => return "certificate_encoding_invalid",
            "incompatible TLS identity type" => return "tls_identity_invalid",
            "Unknown TLS backend passed to `use_preconfigured_tls`" => return "tls_backend_invalid",
            "invalid HTTP header value" => return "client_header_invalid",
            _ => {}
        }
        if current.downcast_ref::<rustls::Error>().is_some() {
            return "tls_configuration_error";
        }
        if let Some(error) = current.downcast_ref::<std::io::Error>() {
            return match error.kind() {
                std::io::ErrorKind::NotFound => "client_setup_file_missing",
                std::io::ErrorKind::PermissionDenied => "client_setup_permission_denied",
                _ => "client_setup_io_error",
            };
        }
        cause = current.source();
    }
    "client_setup_unclassified"
}

fn synthetic_error_class(status: http::StatusCode, text: &str) -> &'static str {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let message = value["error"]["message"].as_str().unwrap_or("");
    match message {
        "Antigravity configuration is invalid" => {
            // Public local status does not construct Auth's HTTP client. Comparing
            // its finite validity label distinguishes settings from client setup.
            let status = claude_code_proxy::providers::antigravity::status_json();
            if status["status"]["configuration"] == "valid" {
                eprintln!(
                    "FIXTURE_CLIENT_SETUP_CLASS {}",
                    synthetic_client_setup_class()
                );
                "auth_http_client_init_invalid"
            } else {
                "config_invalid"
            }
        }
        "Antigravity credentials are invalid or unreadable" => "synthetic_store_invalid",
        "unsupported Antigravity model or replay scope" | "missing model" => {
            "request_model_invalid"
        }
        "unsupported Antigravity semantic request option" | "unsupported or missing fields" => {
            "request_option_invalid"
        }
        "Antigravity profile/project changed during request; start a fresh session" => {
            "profile_binding_changed"
        }
        "Antigravity project missing; run auth login or configure your authorized project" => {
            "project_missing"
        }
        "request needs a final user turn with complete tool results" => "request_turn_invalid",
        "Antigravity: Antigravity native headers timed out or transport failed" => {
            "native_transport_failed"
        }
        _ if status == http::StatusCode::BAD_REQUEST => "request_invalid_unclassified",
        _ if status == http::StatusCode::UNAUTHORIZED => "auth_invalid_unclassified",
        _ => "unclassified",
    }
}

#[tokio::test]
async fn antigravity_transport_child() {
    let Ok(case) = std::env::var(CHILD_CASE) else {
        return;
    };
    let stream = std::env::var(CHILD_STREAM).unwrap() == "1";
    let provider = AntigravityProvider::new();
    let request: MessagesRequest = serde_json::from_value(json!({"model":"antigravity/gemini-4-argon-low","max_tokens":512,"stream":stream,"messages":[{"role":"user","content":"public synthetic transport fixture"}],"thinking":{"type":"adaptive","display":"omitted"}})).unwrap();
    let response = provider
        .handle_messages(
            request.clone(),
            RequestContext {
                req_id: "public-test-request".into(),
                session_id: None,
                session_seq: None,
                provider: "antigravity".into(),
                traffic: None,
                monitor: None,
            },
        )
        .await;
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    for prohibited in [
        "public-test-access",
        "DO_NOT_REFLECT",
        "user:pass",
        "dXNlcjpwYXNz",
    ] {
        assert!(
            !text.contains(prohibited),
            "native response must redact synthetic origin/proxy credentials"
        );
    }
    let expected_status = match case.as_str() {
        "gzip-error" | "rejected-error" => 429,
        "redirect" => 302,
        "wrong-content-type" | "preheader-failure" | "untrusted-ca" => 502,
        "rejected-success" if !stream => 502,
        _ => 200,
    };
    if status.as_u16() != expected_status {
        eprintln!(
            "FIXTURE_ERROR_CLASS {}",
            synthetic_error_class(status, text)
        );
    }
    if case == "rejected-success" {
        assert_eq!(status.as_u16(), expected_status);
        assert!(text.contains(if stream { "event: error" } else { "api_error" }));
        for prohibited in [
            "message_start",
            "message_stop",
            "TRANSPORT_OK",
            "DO_NOT_REFLECT",
        ] {
            assert!(
                !text.contains(prohibited),
                "invalid native transport must not publish success or raw content"
            );
        }
    } else if case == "redirect" {
        assert_eq!(
            status, 302,
            "native redirect is an upstream error response, never followed"
        );
        assert!(text.contains("api_error"));
        assert!(!text.contains("message_start") && !text.contains("message_stop"));
    } else if case == "rejected-error" {
        assert_eq!(status, 429);
        assert_eq!(retry_after.as_deref(), Some("7"));
        assert!(
            text.contains("google_status=unclassified; reason=unclassified; domain=unclassified")
        );
        assert!(!text.contains("DO_NOT_REFLECT"));
    } else if case == "preheader-failure" || case == "untrusted-ca" {
        assert_eq!(status, 502);
        assert!(text.contains("api_error"));
        for prohibited in [
            "message_start",
            "message_stop",
            "TRANSPORT_OK",
            "public-test-access",
            "DO_NOT_REFLECT",
        ] {
            assert!(!text.contains(prohibited));
        }
    } else if case == "wrong-content-type" {
        assert_eq!(status, 502);
        assert!(
            text.contains("unexpected content type"),
            "content type check must precede body decoding"
        );
    } else if case == "gzip-error" {
        assert_eq!(
            status, 429,
            "original upstream status must survive compression"
        );
        assert!(text.contains("google_status=RESOURCE_EXHAUSTED; reason=RATE_LIMIT_EXCEEDED; domain=googleapis.com"), "gzip error diagnostics must decode");
        assert!(
            !text.contains("DO_NOT_REFLECT"),
            "error details must remain redacted"
        );
    } else {
        assert_eq!(status, 200, "valid native transport must succeed");
        assert!(
            text.contains("TRANSPORT_OK"),
            "validated native text must survive transport"
        );
        if stream {
            assert!(text.contains("event: message_stop"));
            assert!(!text.contains("event: error"));
        }
    }
    if case == "pooled-failure" {
        let response = provider
            .handle_messages(
                request,
                RequestContext {
                    req_id: "public-second-request".into(),
                    session_id: None,
                    session_seq: None,
                    provider: "antigravity".into(),
                    traffic: None,
                    monitor: None,
                },
            )
            .await;
        assert_eq!(
            response.status(),
            502,
            "second pooled generation failure must not retry"
        );
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("api_error"));
        for prohibited in [
            "message_start",
            "message_stop",
            "TRANSPORT_OK",
            "public-test-access",
            "user:pass",
            "dXNlcjpwYXNz",
        ] {
            assert!(!text.contains(prohibited));
        }
    }
}

#[tokio::test]
async fn antigravity_transport_control_plain_verified_tls() {
    for protocol in [Version::HTTP_11, Version::HTTP_2] {
        let server = TlsServer::start(protocol, 200, None, success()).await;
        for stream in [false, true] {
            child(&server, "plain", stream).await;
        }
        let observed = server.observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        for request in observed.iter() {
            assert_eq!(request.version, protocol);
            assert_eq!(
                request.headers["authorization"],
                "Bearer public-test-access"
            );
            assert_eq!(
                request.headers["user-agent"],
                format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"))
            );
            assert_eq!(
                request.wire["request"]["generationConfig"]["maxOutputTokens"],
                512
            );
        }
    }
}

#[test]
fn antigravity_transport_control_cached_multigzip_contract() {
    fn decode(input: &[u8], chunk_size: usize) -> std::io::Result<Vec<u8>> {
        let mut decoder = flate2::write::MultiGzDecoder::new(Vec::new());
        for chunk in input.chunks(chunk_size) {
            decoder.write_all(chunk)?;
        }
        decoder.finish()
    }
    let first = gzip(b"first");
    let mut concatenated = first.clone();
    concatenated.extend(gzip(b"second"));
    for chunk_size in [1, 3, 16, 4096] {
        assert_eq!(decode(&concatenated, chunk_size).unwrap(), b"firstsecond");
        let mut corrupt = first.clone();
        let crc = corrupt.len() - 8;
        corrupt[crc] ^= 1;
        let mut truncated = first.clone();
        truncated.pop();
        let mut junk = first.clone();
        junk.extend_from_slice(b"not gzip");
        let mut partial_member = first.clone();
        partial_member.extend_from_slice(&[0x1f, 0x8b]);
        for invalid in [Vec::new(), corrupt, truncated, junk, partial_member] {
            assert!(
                decode(&invalid, chunk_size).is_err(),
                "gzip must consume and validate every member through EOF"
            );
        }
    }
}

#[tokio::test]
async fn antigravity_transport_red_accept_is_absent_at_runtime() {
    let server = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
    child(&server, "plain", false).await;
    let observed = server.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert!(
        !observed[0].headers.contains_key("accept"),
        "generation Accept must be truly absent"
    );
}

#[tokio::test]
async fn antigravity_transport_red_gzip_is_advertised_at_runtime() {
    let server = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
    child(&server, "plain", false).await;
    let observed = server.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    let values: Vec<_> = observed[0]
        .headers
        .get_all("accept-encoding")
        .iter()
        .collect();
    assert_eq!(
        values.len(),
        1,
        "generation must advertise exactly one gzip value"
    );
    assert_eq!(values[0], "gzip");
}

#[tokio::test]
async fn antigravity_transport_red_unknown_length_http1() {
    let server = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
    child(&server, "plain", false).await;
    let observed = server.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].version, Version::HTTP_11);
    assert!(
        !observed[0].headers.contains_key("content-length"),
        "unknown-length HTTP/1.1 must not emit Content-Length"
    );
    assert_eq!(observed[0].headers["transfer-encoding"], "chunked");
}

#[tokio::test]
async fn antigravity_transport_red_unknown_length_http2() {
    let server = TlsServer::start(Version::HTTP_2, 200, None, success()).await;
    child(&server, "plain", false).await;
    let observed = server.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].version, Version::HTTP_2);
    assert!(
        !observed[0].headers.contains_key("content-length"),
        "unknown-length HTTP/2 must not emit Content-Length"
    );
    assert!(!observed[0].headers.contains_key("transfer-encoding"));
}

#[tokio::test]
async fn antigravity_transport_red_gzip_sse_decodes() {
    let server = TlsServer::start(Version::HTTP_11, 200, Some("gzip"), gzip(&success())).await;
    for stream in [false, true] {
        child(&server, "gzip-success", stream).await;
    }
}

#[tokio::test]
async fn antigravity_transport_red_gzip_error_preserves_diagnostics() {
    let error = json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"DO_NOT_REFLECT", "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED","domain":"googleapis.com","metadata":{"private":"DO_NOT_REFLECT"}}]}}).to_string();
    let server =
        TlsServer::start(Version::HTTP_11, 429, Some("gzip"), gzip(error.as_bytes())).await;
    for stream in [false, true] {
        child(&server, "gzip-error", stream).await;
    }
}

#[tokio::test]
async fn antigravity_transport_gzip_concatenated_members_and_identity_are_supported() {
    let native = success();
    let split = native.len() / 2;
    let mut concatenated = gzip(&native[..split]);
    concatenated.extend(gzip(&native[split..]));
    let mut empty_first = gzip(b"");
    empty_first.extend(gzip(&native));
    for protocol in [Version::HTTP_11, Version::HTTP_2] {
        for (encoding, body) in [
            ("identity", native.clone()),
            ("GZip", gzip(&native)),
            ("gzip", concatenated.clone()),
            ("gzip", empty_first.clone()),
        ] {
            let server = TlsServer::start(protocol, 200, Some(encoding), body).await;
            for stream in [false, true] {
                child(&server, "plain", stream).await;
            }
        }
    }
}

#[tokio::test]
async fn antigravity_transport_gzip_crc_eof_and_trailing_fail_closed() {
    let valid = gzip(&success());
    let mut crc = valid.clone();
    let offset = crc.len() - 8;
    crc[offset] ^= 1;
    let mut truncated = valid.clone();
    truncated.pop();
    let mut junk = valid.clone();
    junk.extend_from_slice(b"DO_NOT_REFLECT");
    let mut partial = valid.clone();
    partial.extend_from_slice(&[0x1f, 0x8b]);
    let mut zero = valid.clone();
    zero.push(0);
    for body in [Vec::new(), vec![0x1f], crc, truncated, junk, partial, zero] {
        let server = TlsServer::start(Version::HTTP_11, 200, Some("gzip"), body).await;
        for stream in [false, true] {
            child(&server, "rejected-success", stream).await;
        }
    }
}

#[tokio::test]
async fn antigravity_transport_all_encoding_headers_are_checked() {
    for encodings in [
        vec!["br"],
        vec!["deflate"],
        vec![""],
        vec!["gzip, identity"],
        vec!["gzip", "gzip"],
        vec!["identity", "gzip"],
    ] {
        for status in [200, 429] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                http::HeaderValue::from_static(if status == 200 {
                    "text/event-stream"
                } else {
                    "application/json"
                }),
            );
            headers.insert("retry-after", http::HeaderValue::from_static("7"));
            for encoding in &encodings {
                headers.append(
                    "content-encoding",
                    http::HeaderValue::from_str(encoding).unwrap(),
                );
            }
            let server =
                TlsServer::start_with_headers(Version::HTTP_11, status, headers, gzip(&success()))
                    .await;
            for stream in [false, true] {
                child(
                    &server,
                    if status == 200 {
                        "rejected-success"
                    } else {
                        "rejected-error"
                    },
                    stream,
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn antigravity_transport_invalid_gzip_errors_preserve_status_and_retry() {
    for body in [
        Vec::new(),
        b"DO_NOT_REFLECT".to_vec(),
        gzip(&vec![b'x'; 8193]),
    ] {
        let server = TlsServer::start(Version::HTTP_11, 429, Some("gzip"), body).await;
        for stream in [false, true] {
            child(&server, "rejected-error", stream).await;
        }
    }
}

#[tokio::test]
async fn antigravity_transport_encoded_and_decoded_bodies_are_bounded() {
    let part = format!(
        "data: {}\n\n",
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"x".repeat(128 * 1024)}]}}]}})
    );
    let mut oversized_decoded = part.repeat(65).into_bytes();
    oversized_decoded.extend(success());
    for (encoding, body) in [
        (None, vec![b'x'; 8 * 1024 * 1024 + 1]),
        (Some("gzip"), gzip(&oversized_decoded)),
        (Some("gzip"), gzip(&vec![b'x'; 1024 * 1024 + 1])),
    ] {
        let server = TlsServer::start(Version::HTTP_11, 200, encoding, body).await;
        for stream in [false, true] {
            child(&server, "rejected-success", stream).await;
        }
    }
    let server = TlsServer::start(Version::HTTP_11, 429, Some("gzip"), vec![0; 8193]).await;
    child(&server, "rejected-error", false).await;
}

#[tokio::test]
async fn antigravity_transport_content_type_precedes_success_decoding() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert("content-encoding", http::HeaderValue::from_static("br"));
    let server =
        TlsServer::start_with_headers(Version::HTTP_11, 200, headers, b"DO_NOT_REFLECT".to_vec())
            .await;
    for stream in [false, true] {
        child(&server, "wrong-content-type", stream).await;
    }
}

#[tokio::test]
async fn antigravity_transport_redirect_is_not_followed_or_forwarded_credentials() {
    let target = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        "location",
        http::HeaderValue::from_str(&target.url).unwrap(),
    );
    let origin =
        TlsServer::start_with_headers(Version::HTTP_11, 302, headers, b"DO_NOT_REFLECT".to_vec())
            .await;
    for stream in [false, true] {
        child(&origin, "redirect", stream).await;
    }
    assert_eq!(origin.observed.lock().unwrap().len(), 2);
    assert!(
        target.observed.lock().unwrap().is_empty(),
        "redirect target must receive neither a second generation nor Authorization"
    );
}

mod retry_tests {
    use super::*;
    use std::{
        io,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        sync::oneshot,
    };

    const MAX_CONNECTIONS: usize = 4;
    const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

    #[derive(Default)]
    struct RetryCounts {
        tcp: AtomicUsize,
        tls: AtomicUsize,
        requests: AtomicUsize,
        h2_prefaces: AtomicUsize,
        settings_acknowledged: AtomicUsize,
        refused_streams: AtomicUsize,
        errors: AtomicUsize,
    }

    struct RetryFixture {
        server: TlsServer,
        counts: Arc<RetryCounts>,
        stop: Option<oneshot::Sender<()>>,
        drained: oneshot::Receiver<()>,
    }

    fn fixture_error() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, "synthetic retry fixture failed")
    }

    fn retry_acceptor(protocol: Version) -> tokio_rustls::TlsAcceptor {
        let certificates = CertificateDer::pem_slice_iter(CERT)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .unwrap();
        config.alpn_protocols = vec![if protocol == Version::HTTP_2 {
            b"h2".to_vec()
        } else {
            b"http/1.1".to_vec()
        }];
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    impl RetryFixture {
        async fn start(protocol: Version) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("https://{}", listener.local_addr().unwrap());
            let acceptor = retry_acceptor(protocol);
            let observed = Arc::new(Mutex::new(Vec::new()));
            let records = observed.clone();
            let counts = Arc::new(RetryCounts::default());
            let captures = counts.clone();
            let (stop, mut stopping) = oneshot::channel();
            let (done, drained) = oneshot::channel();
            let task = tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        // Drain already-queued TCP attempts before honoring the
                        // post-child stop barrier, rather than hiding retries.
                        biased;
                        accepted = listener.accept() => {
                            let Ok((socket, _)) = accepted else {
                                captures.errors.fetch_add(1, Ordering::SeqCst);
                                break;
                            };
                            if captures.tcp.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                                captures.errors.fetch_add(1, Ordering::SeqCst);
                                drop(socket);
                                break;
                            }
                            let acceptor = acceptor.clone();
                            let captures = captures.clone();
                            let records = records.clone();
                            connections.spawn(async move {
                                let connection = async {
                                    let tls = acceptor.accept(socket).await?;
                                    captures.tls.fetch_add(1, Ordering::SeqCst);
                                    let expected_alpn: &[u8] = if protocol == Version::HTTP_2 {
                                        b"h2"
                                    } else {
                                        b"http/1.1"
                                    };
                                    if tls.get_ref().1.alpn_protocol() != Some(expected_alpn) {
                                        return Err(fixture_error());
                                    }
                                    if protocol == Version::HTTP_2 {
                                        refuse_h2_streams(tls, &captures).await
                                    } else {
                                        pooled_http1(tls, captures.clone(), records).await
                                    }
                                };
                                if !matches!(tokio::time::timeout(CONNECTION_TIMEOUT, connection).await, Ok(Ok(()))) {
                                    captures.errors.fetch_add(1, Ordering::SeqCst);
                                }
                            });
                        }
                        Some(result) = connections.join_next(), if !connections.is_empty() => {
                            if result.is_err() {
                                captures.errors.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        _ = &mut stopping => break,
                    }
                }
                drop(listener);
                let complete = tokio::time::timeout(Duration::from_secs(2), async {
                    while let Some(result) = connections.join_next().await {
                        if result.is_err() {
                            captures.errors.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
                .await;
                if complete.is_err() {
                    captures.errors.fetch_add(1, Ordering::SeqCst);
                }
                // JoinSet aborts anything still pending on timeout or when the
                // enclosing TlsServer's Drop aborts this owner task.
                drop(connections);
                let _ = done.send(());
            });
            Self {
                server: TlsServer {
                    url,
                    observed,
                    task,
                },
                counts,
                stop: Some(stop),
                drained,
            }
        }

        async fn finish(&mut self) {
            self.stop.take().unwrap().send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(3), &mut self.drained)
                .await
                .expect("retry fixture shutdown must be bounded")
                .expect("retry fixture owner must complete");
            assert_eq!(
                self.counts.errors.load(Ordering::SeqCst),
                0,
                "local retry fixture must complete without parsing/task errors"
            );
        }
    }

    async fn h2_frame<S: AsyncWrite + Unpin>(
        stream: &mut S,
        kind: u8,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() > 65536 || stream_id & 0x8000_0000 != 0 {
            return Err(fixture_error());
        }
        let length = payload.len();
        let id = stream_id.to_be_bytes();
        stream
            .write_all(&[
                (length >> 16) as u8,
                (length >> 8) as u8,
                length as u8,
                kind,
                flags,
                id[0],
                id[1],
                id[2],
                id[3],
            ])
            .await?;
        stream.write_all(payload).await?;
        stream.flush().await
    }

    async fn refuse_h2_streams<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        counts: &RetryCounts,
    ) -> io::Result<()> {
        let mut preface = [0; 24];
        stream.read_exact(&mut preface).await?;
        if &preface != b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" {
            return Err(fixture_error());
        }
        counts.h2_prefaces.fetch_add(1, Ordering::SeqCst);
        h2_frame(&mut stream, 4, 0, 0, &[]).await?; // Initial server SETTINGS.
        let mut total_payload = 0usize;
        let mut refused = false;
        for _ in 0..128 {
            let mut header = [0; 9];
            if let Err(error) = stream.read_exact(&mut header).await {
                // The isolated child closes its connection after the terminal
                // 502. No raw TLS/error/header/body data is retained or printed.
                return if refused { Ok(()) } else { Err(error) };
            }
            let length = (usize::from(header[0]) << 16)
                | (usize::from(header[1]) << 8)
                | usize::from(header[2]);
            if length > 65536 || total_payload + length > 256 * 1024 {
                return Err(fixture_error());
            }
            total_payload += length;
            let stream_id = u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff;
            let mut payload = vec![0; length];
            stream.read_exact(&mut payload).await?;
            match header[3] {
                4 => {
                    if stream_id != 0 || (header[4] & 1 != 0 && length != 0) || length % 6 != 0 {
                        return Err(fixture_error());
                    }
                    if header[4] & 1 == 0 {
                        h2_frame(&mut stream, 4, 1, 0, &[]).await?;
                        counts.settings_acknowledged.fetch_add(1, Ordering::SeqCst);
                    }
                }
                1 => {
                    if stream_id == 0 || stream_id % 2 == 0 {
                        return Err(fixture_error());
                    }
                    if counts.requests.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        return Err(fixture_error());
                    }
                    // RFC 7540 REFUSED_STREAM (7). Keep the connection alive so
                    // a retransmission on this connection is counted as well.
                    h2_frame(&mut stream, 3, 0, stream_id, &7u32.to_be_bytes()).await?;
                    counts.refused_streams.fetch_add(1, Ordering::SeqCst);
                    refused = true;
                }
                6 => {
                    if stream_id != 0 || length != 8 {
                        return Err(fixture_error());
                    }
                    if header[4] & 1 == 0 {
                        h2_frame(&mut stream, 6, 1, 0, &payload).await?;
                    }
                }
                7 => {
                    return if refused {
                        Ok(())
                    } else {
                        Err(fixture_error())
                    };
                }
                // DATA, CONTINUATION and flow-control payloads are bounded and
                // discarded. This fixture deliberately does not decode HPACK.
                _ => {}
            }
        }
        Err(fixture_error())
    }

    async fn pooled_http1(
        tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        counts: Arc<RetryCounts>,
        observed: Arc<Mutex<Vec<Observed>>>,
    ) -> io::Result<()> {
        let captures = counts.clone();
        let service = service_fn(move |request: http::Request<Incoming>| {
            let counts = captures.clone();
            let observed = observed.clone();
            async move {
                let ordinal = counts.requests.fetch_add(1, Ordering::SeqCst);
                if ordinal >= MAX_CONNECTIONS
                    || request.method() != http::Method::POST
                    || request.version() != Version::HTTP_11
                    || request.uri().path_and_query().map(|value| value.as_str())
                        != Some("/v1internal:streamGenerateContent?alt=sse")
                {
                    counts.errors.fetch_add(1, Ordering::SeqCst);
                    return Err::<Response<Full<Bytes>>, _>(fixture_error());
                }
                let (parts, mut incoming) = request.into_parts();
                let mut bytes = Vec::new();
                while let Some(frame) = incoming.frame().await {
                    let frame = frame.map_err(|_| fixture_error())?;
                    if let Ok(data) = frame.into_data() {
                        if bytes.len() + data.len() > 65536 {
                            counts.errors.fetch_add(1, Ordering::SeqCst);
                            return Err(fixture_error());
                        }
                        bytes.extend_from_slice(&data);
                    }
                }
                let wire = serde_json::from_slice(&bytes).map_err(|_| fixture_error())?;
                observed.lock().unwrap().push(Observed {
                    headers: parts.headers,
                    version: parts.version,
                    wire,
                });
                if ordinal == 0 {
                    Ok(Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Full::new(Bytes::from(success())))
                        .unwrap())
                } else {
                    // The request reached the reused connection. Returning a
                    // service error closes it before any response headers.
                    Err(io::Error::other("synthetic pooled connection failure"))
                }
            }
        });
        let result = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(tls), service)
            .await;
        if result.is_err() && counts.requests.load(Ordering::SeqCst) < 2 {
            return Err(fixture_error());
        }
        Ok(())
    }

    #[tokio::test]
    async fn antigravity_transport_retry_h2_refused_stream_is_one_native_attempt() {
        for stream in [false, true] {
            let mut fixture = RetryFixture::start(Version::HTTP_2).await;
            child(&fixture.server, "preheader-failure", stream).await;
            fixture.finish().await;
            assert_eq!(
                fixture.counts.tcp.load(Ordering::SeqCst),
                1,
                "REFUSED_STREAM must not open a second TCP connection"
            );
            assert_eq!(fixture.counts.tls.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.counts.h2_prefaces.load(Ordering::SeqCst), 1);
            assert!(fixture.counts.settings_acknowledged.load(Ordering::SeqCst) >= 1);
            assert_eq!(
                fixture.counts.requests.load(Ordering::SeqCst),
                1,
                "REFUSED_STREAM must not retransmit generation HEADERS on any stream"
            );
            assert_eq!(fixture.counts.refused_streams.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn antigravity_transport_retry_reused_http1_service_failure_is_not_retransmitted() {
        let mut fixture = RetryFixture::start(Version::HTTP_11).await;
        // The child consumes one successful turn, then sends another using the
        // SAME public provider. Its second request must return a terminal 502.
        child(&fixture.server, "pooled-failure", false).await;
        fixture.finish().await;
        assert_eq!(
            fixture.counts.tcp.load(Ordering::SeqCst),
            1,
            "both turns must use one pooled TCP connection; no reconnect retry"
        );
        assert_eq!(fixture.counts.tls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.counts.requests.load(Ordering::SeqCst),
            2,
            "one successful turn plus one failed generation, with no retransmission"
        );
        let observed = fixture.server.observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        for request in observed.iter() {
            assert_eq!(request.version, Version::HTTP_11);
            assert_eq!(request.headers.get_all("authorization").iter().count(), 1);
            assert_eq!(
                request.headers["authorization"],
                "Bearer public-test-access"
            );
            assert!(!request.headers.contains_key("proxy-authorization"));
            assert_eq!(
                request.wire["request"]["generationConfig"]["maxOutputTokens"],
                512
            );
        }
    }
}

mod proxy_tests {
    use super::*;
    use std::{
        io,
        net::{Ipv4Addr, Ipv6Addr, SocketAddr},
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    const BASIC: &str = "Basic dXNlcjpwYXNz";
    const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_CONNECTIONS: usize = 8;

    #[derive(Clone, Copy)]
    enum ConnectReply {
        Tunnel,
        AuthenticationRequired,
        Refused,
        Truncated,
    }

    #[derive(Clone, Copy)]
    enum SocksAuth {
        None,
        Password,
        RejectPassword,
    }

    #[derive(Clone, Copy)]
    enum ProxyKind {
        Connect(ConnectReply),
        Socks4,
        Socks5 { auth: SocksAuth, refuse: bool },
        Close,
    }

    struct ConnectObserved {
        authority: String,
        proxy_authorizations: usize,
        expected_basic: bool,
        client_user_agents: usize,
        expected_client_user_agent: bool,
        origin_authorizations: usize,
        origin_bearer_seen: bool,
    }

    #[derive(Debug, PartialEq)]
    enum SocksAddress {
        Ipv4(Ipv4Addr),
        Ipv6(Ipv6Addr),
        Hostname(String),
    }

    struct SocksObserved {
        version: u8,
        address: Option<SocksAddress>,
        port: Option<u16>,
        authenticated: bool,
        expected_credentials: bool,
        origin_bearer_seen: bool,
    }

    #[derive(Default)]
    struct ProxyObserved {
        tcp_connections: AtomicUsize,
        tls_connections: AtomicUsize,
        no_proxy_alpn: AtomicUsize,
        http1_proxy_alpn: AtomicUsize,
        fixture_errors: AtomicUsize,
        connect: Mutex<Vec<ConnectObserved>>,
        socks: Mutex<Vec<SocksObserved>>,
    }

    // The fixture never connects to the requested hostname/address. Its only
    // upstream socket is this already-owned origin, including for .invalid names.
    struct FakeProxy {
        address: SocketAddr,
        observed: Arc<ProxyObserved>,
        task: JoinHandle<()>,
    }

    impl Drop for FakeProxy {
        fn drop(&mut self) {
            // Dropping the accept task also drops its JoinSet and aborts every
            // handshake/forwarder. No collector or tunnel task is detached.
            self.task.abort();
        }
    }

    trait ProxyIo: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> ProxyIo for T {}

    fn invalid_handshake() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid synthetic proxy handshake",
        )
    }

    fn origin_address(origin: &TlsServer) -> SocketAddr {
        let uri: http::Uri = origin.url.parse().unwrap();
        SocketAddr::new(
            uri.host().unwrap().parse().unwrap(),
            uri.port_u16().unwrap(),
        )
    }

    fn origin_url(origin: &TlsServer, hostname: &str) -> String {
        format!("https://{hostname}:{}", origin_address(origin).port())
    }

    fn proxy_tls_acceptor() -> tokio_rustls::TlsAcceptor {
        let certificates = CertificateDer::pem_slice_iter(CERT)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .unwrap();
        // The outer CONNECT carrier deliberately cannot negotiate h2. Origin
        // TLS, inside this channel, must still independently negotiate h1/h2.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    impl FakeProxy {
        async fn start(
            origin: &TlsServer,
            bind: &str,
            tls: bool,
            kind: ProxyKind,
        ) -> io::Result<Self> {
            let target = origin_address(origin);
            assert!(target.ip().is_loopback());
            let listener = tokio::net::TcpListener::bind(bind).await?;
            let address = listener.local_addr()?;
            assert!(address.ip().is_loopback());
            let acceptor = tls.then(proxy_tls_acceptor);
            let observed = Arc::new(ProxyObserved::default());
            let captures = observed.clone();
            let task = tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((socket, _)) = accepted else {
                                captures.fixture_errors.fetch_add(1, Ordering::SeqCst);
                                break;
                            };
                            let number = captures.tcp_connections.fetch_add(1, Ordering::SeqCst);
                            if number >= MAX_CONNECTIONS {
                                captures.fixture_errors.fetch_add(1, Ordering::SeqCst);
                                drop(socket);
                                break;
                            }
                            let captures = captures.clone();
                            let acceptor = acceptor.clone();
                            connections.spawn(async move {
                                let connection = async {
                                    if matches!(kind, ProxyKind::Close) {
                                        drop(socket);
                                        return Ok::<_, io::Error>(());
                                    }
                                    let mut stream: Box<dyn ProxyIo> = if let Some(acceptor) = acceptor {
                                        // Certificate rejection by the client is an expected
                                        // outcome in negative proxy-TLS cases.
                                        let Ok(tls) = acceptor.accept(socket).await else {
                                            return Ok(());
                                        };
                                        captures.tls_connections.fetch_add(1, Ordering::SeqCst);
                                        if tls.get_ref().1.alpn_protocol().is_none() {
                                            captures.no_proxy_alpn.fetch_add(1, Ordering::SeqCst);
                                        }
                                        if tls.get_ref().1.alpn_protocol() == Some(b"http/1.1".as_slice()) {
                                            captures.http1_proxy_alpn.fetch_add(1, Ordering::SeqCst);
                                        }
                                        Box::new(tls)
                                    } else {
                                        Box::new(socket)
                                    };
                                    let forward = match kind {
                                        ProxyKind::Connect(reply) => {
                                            connect_handshake(&mut stream, &captures, reply).await?
                                        }
                                        ProxyKind::Socks4 => {
                                            socks4_handshake(&mut stream, &captures, target.port()).await?
                                        }
                                        ProxyKind::Socks5 { auth, refuse } => {
                                            socks5_handshake(&mut stream, &captures, target.port(), auth, refuse).await?
                                        }
                                        ProxyKind::Close => unreachable!(),
                                    };
                                    if forward {
                                        let mut upstream = tokio::net::TcpStream::connect(target).await?;
                                        // Closure/reset after failed origin TLS is expected.
                                        let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                                    } else {
                                        stream.shutdown().await?;
                                    }
                                    Ok(())
                                };
                                if !matches!(tokio::time::timeout(FIXTURE_TIMEOUT, connection).await, Ok(Ok(()))) {
                                    captures.fixture_errors.fetch_add(1, Ordering::SeqCst);
                                }
                            });
                        }
                        Some(result) = connections.join_next(), if !connections.is_empty() => {
                            if result.is_err() {
                                captures.fixture_errors.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                }
            });
            Ok(Self {
                address,
                observed,
                task,
            })
        }

        fn url(&self, scheme: &str, credentials: bool) -> String {
            format!(
                "{scheme}://{}{address}",
                if credentials { "user:pass@" } else { "" },
                address = self.address
            )
        }

        fn assert_connections(&self, expected: usize) {
            assert_eq!(
                self.observed.tcp_connections.load(Ordering::SeqCst),
                expected,
                "one selected-proxy connection per generation; no handshake retry"
            );
            assert_eq!(
                self.observed.fixture_errors.load(Ordering::SeqCst),
                0,
                "bounded fixture parsing/forwarding must complete without errors"
            );
        }

        fn assert_connect(&self, authority: &str, count: usize, credentials: bool) {
            self.assert_connections(count);
            let observed = self.observed.connect.lock().unwrap();
            assert_eq!(observed.len(), count);
            for request in observed.iter() {
                assert_eq!(request.authority, authority);
                assert_eq!(request.proxy_authorizations, usize::from(credentials));
                assert_eq!(request.expected_basic, credentials);
                assert_eq!(
                    request.client_user_agents, 1,
                    "CONNECT preserves the measured legacy client UA"
                );
                assert!(
                    request.expected_client_user_agent,
                    "CONNECT must not use the native per-request override"
                );
                assert_eq!(
                    request.origin_authorizations, 0,
                    "origin Authorization must never appear on outer CONNECT"
                );
                assert!(!request.origin_bearer_seen);
            }
        }
    }

    async fn read_nul_string<S: AsyncRead + Unpin>(
        stream: &mut S,
        limit: usize,
    ) -> io::Result<Vec<u8>> {
        let mut value = Vec::new();
        for _ in 0..=limit {
            let byte = stream.read_u8().await?;
            if byte == 0 {
                return Ok(value);
            }
            if value.len() == limit {
                return Err(invalid_handshake());
            }
            value.push(byte);
        }
        Err(invalid_handshake())
    }

    async fn connect_handshake<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        captures: &ProxyObserved,
        reply: ConnectReply,
    ) -> io::Result<bool> {
        let mut bytes = Vec::new();
        // Read exactly through the terminator, never consuming inner TLS bytes.
        while bytes.len() < 8192 && !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(stream.read_u8().await?);
        }
        if !bytes.ends_with(b"\r\n\r\n") {
            return Err(invalid_handshake());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid_handshake())?;
        let mut lines = text.split("\r\n");
        let mut request_line = lines
            .next()
            .ok_or_else(invalid_handshake)?
            .split_whitespace();
        if request_line.next() != Some("CONNECT") {
            return Err(invalid_handshake());
        }
        let authority = request_line.next().ok_or_else(invalid_handshake)?;
        if authority.len() > 256
            || request_line.next() != Some("HTTP/1.1")
            || request_line.next().is_some()
        {
            return Err(invalid_handshake());
        }
        let mut request = ConnectObserved {
            authority: authority.to_owned(),
            proxy_authorizations: 0,
            expected_basic: false,
            client_user_agents: 0,
            expected_client_user_agent: false,
            origin_authorizations: 0,
            origin_bearer_seen: text.contains("public-test-access"),
        };
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').ok_or_else(invalid_handshake)?;
            if name.eq_ignore_ascii_case("proxy-authorization") {
                request.proxy_authorizations += 1;
                request.expected_basic = value.trim() == BASIC;
            }
            if name.eq_ignore_ascii_case("user-agent") {
                request.client_user_agents += 1;
                request.expected_client_user_agent =
                    value.trim() == concat!("claude-code-proxy/", env!("CARGO_PKG_VERSION"));
            }
            if name.eq_ignore_ascii_case("authorization") {
                request.origin_authorizations += 1;
            }
        }
        let legacy_ua = request.client_user_agents == 1 && request.expected_client_user_agent;
        {
            let mut observed = captures.connect.lock().unwrap();
            if observed.len() >= MAX_CONNECTIONS {
                return Err(invalid_handshake());
            }
            observed.push(request);
        }
        let response: &[u8] = match reply {
            // A strict proxy policy justified by the unchanged legacy-builder
            // runtime probe, not an invented official client fingerprint.
            ConnectReply::Tunnel if legacy_ua => b"HTTP/1.1 200 Connection Established\r\n\r\n",
            ConnectReply::Tunnel => b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            ConnectReply::AuthenticationRequired => b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"fixture\"\r\nContent-Length: 14\r\n\r\nDO_NOT_REFLECT",
            ConnectReply::Refused => b"HTTP/1.1 403 Forbidden\r\nContent-Length: 14\r\n\r\nDO_NOT_REFLECT",
            ConnectReply::Truncated => b"HTTP/1.1 200 Connection Established\r\nX-Fixture: DO_NOT_REFLECT",
        };
        stream.write_all(response).await?;
        Ok(matches!(reply, ConnectReply::Tunnel) && legacy_ua)
    }

    async fn socks4_handshake<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        captures: &ProxyObserved,
        target_port: u16,
    ) -> io::Result<bool> {
        let mut header = [0; 8];
        stream.read_exact(&mut header).await?;
        if header[0] != 4 || header[1] != 1 {
            return Err(invalid_handshake());
        }
        let port = u16::from_be_bytes([header[2], header[3]]);
        let user = read_nul_string(stream, 255).await?;
        let address = if header[4..7] == [0, 0, 0] && header[7] != 0 {
            let hostname = read_nul_string(stream, 255).await?;
            SocksAddress::Hostname(String::from_utf8(hostname).map_err(|_| invalid_handshake())?)
        } else {
            SocksAddress::Ipv4(Ipv4Addr::new(header[4], header[5], header[6], header[7]))
        };
        if port != target_port {
            return Err(invalid_handshake());
        }
        let valid_domain =
            !matches!(&address, SocksAddress::Hostname(hostname) if hostname.is_empty());
        captures.socks.lock().unwrap().push(SocksObserved {
            version: 4,
            address: Some(address),
            port: Some(port),
            authenticated: !user.is_empty(),
            expected_credentials: false,
            origin_bearer_seen: user
                .windows(b"public-test-access".len())
                .any(|window| window == b"public-test-access"),
        });
        stream
            .write_all(&[
                0,
                if valid_domain { 90 } else { 91 },
                header[2],
                header[3],
                127,
                0,
                0,
                1,
            ])
            .await?;
        Ok(valid_domain)
    }

    async fn socks5_handshake<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        captures: &ProxyObserved,
        target_port: u16,
        auth: SocksAuth,
        refuse: bool,
    ) -> io::Result<bool> {
        let version = stream.read_u8().await?;
        let count = usize::from(stream.read_u8().await?);
        if version != 5 || count == 0 || count > 16 {
            return Err(invalid_handshake());
        }
        let mut methods = vec![0; count];
        stream.read_exact(&mut methods).await?;
        let authenticated = !matches!(auth, SocksAuth::None);
        let method = if authenticated { 2 } else { 0 };
        if !methods.contains(&method) {
            return Err(invalid_handshake());
        }
        stream.write_all(&[5, method]).await?;
        let mut expected_credentials = false;
        let mut origin_bearer_seen = false;
        if authenticated {
            if stream.read_u8().await? != 1 {
                return Err(invalid_handshake());
            }
            let length = usize::from(stream.read_u8().await?);
            let mut user = vec![0; length];
            stream.read_exact(&mut user).await?;
            let length = usize::from(stream.read_u8().await?);
            let mut password = vec![0; length];
            stream.read_exact(&mut password).await?;
            expected_credentials = user == b"user" && password == b"pass";
            origin_bearer_seen = [&user, &password].iter().any(|value| {
                value
                    .windows(b"public-test-access".len())
                    .any(|window| window == b"public-test-access")
            });
            let reject = matches!(auth, SocksAuth::RejectPassword);
            stream.write_all(&[1, u8::from(reject)]).await?;
            if reject {
                captures.socks.lock().unwrap().push(SocksObserved {
                    version: 5,
                    address: None,
                    port: None,
                    authenticated,
                    expected_credentials,
                    origin_bearer_seen,
                });
                return Ok(false);
            }
        }
        let mut header = [0; 4];
        stream.read_exact(&mut header).await?;
        if header[..3] != [5, 1, 0] {
            return Err(invalid_handshake());
        }
        let address = match header[3] {
            1 => {
                let mut address = [0; 4];
                stream.read_exact(&mut address).await?;
                SocksAddress::Ipv4(Ipv4Addr::from(address))
            }
            3 => {
                let length = usize::from(stream.read_u8().await?);
                if length == 0 {
                    return Err(invalid_handshake());
                }
                let mut hostname = vec![0; length];
                stream.read_exact(&mut hostname).await?;
                SocksAddress::Hostname(
                    String::from_utf8(hostname).map_err(|_| invalid_handshake())?,
                )
            }
            4 => {
                let mut address = [0; 16];
                stream.read_exact(&mut address).await?;
                SocksAddress::Ipv6(Ipv6Addr::from(address))
            }
            _ => return Err(invalid_handshake()),
        };
        let port = stream.read_u16().await?;
        if port != target_port {
            return Err(invalid_handshake());
        }
        captures.socks.lock().unwrap().push(SocksObserved {
            version: 5,
            address: Some(address),
            port: Some(port),
            authenticated,
            expected_credentials,
            origin_bearer_seen,
        });
        stream
            .write_all(&[5, if refuse { 5 } else { 0 }, 0, 1, 127, 0, 0, 1, 0, 0])
            .await?;
        Ok(!refuse)
    }

    async fn proxied_child(
        origin: &TlsServer,
        proxy_url: &str,
        case: &'static str,
        stream: bool,
        base_url: &str,
    ) {
        child_configured(
            origin,
            case,
            stream,
            base_url,
            &[
                ("HTTPS_PROXY", proxy_url),
                ("NO_PROXY", ""),
                ("no_proxy", ""),
            ],
        )
        .await;
    }

    fn assert_origin(origin: &TlsServer, protocol: Version, expected: usize) {
        let observed = origin.observed.lock().unwrap();
        assert_eq!(
            observed.len(),
            expected,
            "no extra generation requests or direct fallback"
        );
        for request in observed.iter() {
            assert_eq!(request.version, protocol);
            assert_eq!(request.headers.get_all("authorization").iter().count(), 1);
            assert_eq!(
                request.headers["authorization"],
                "Bearer public-test-access"
            );
            assert!(
                !request.headers.contains_key("proxy-authorization"),
                "proxy credentials must stay outside origin TLS"
            );
            assert!(!request.headers.contains_key("proxy-connection"));
            assert!(!request.headers.values().any(|value| {
                let value = value.to_str().unwrap_or("");
                value.contains("dXNlcjpwYXNz") || value.contains("user:pass")
            }));
        }
    }

    async fn connect_matrix(protocol: Version, tls: bool) {
        let origin = TlsServer::start(protocol, 200, None, success()).await;
        let proxy = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            tls,
            ProxyKind::Connect(ConnectReply::Tunnel),
        )
        .await
        .unwrap();
        let base_url = origin_url(&origin, "fixture.invalid");
        let proxy_url = proxy.url(if tls { "https" } else { "http" }, true);
        for stream in [false, true] {
            proxied_child(&origin, &proxy_url, "native-ua-override", stream, &base_url).await;
        }
        proxy.assert_connect(
            &format!("fixture.invalid:{}", origin_address(&origin).port()),
            2,
            true,
        );
        if tls {
            assert_eq!(proxy.observed.tls_connections.load(Ordering::SeqCst), 2);
            assert_eq!(
                proxy.observed.no_proxy_alpn.load(Ordering::SeqCst),
                2,
                "baseline clears proxy ALPN; successful parsed CONNECT still proves HTTP/1.1"
            );
            assert_eq!(proxy.observed.http1_proxy_alpn.load(Ordering::SeqCst), 0);
        }
        assert_origin(&origin, protocol, 2);
        for request in origin.observed.lock().unwrap().iter() {
            assert_eq!(request.headers["user-agent"], "public-native-override");
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_http_connect_origin_http1() {
        connect_matrix(Version::HTTP_11, false).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_http_connect_origin_http2() {
        connect_matrix(Version::HTTP_2, false).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_https_connect_origin_http1() {
        connect_matrix(Version::HTTP_11, true).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_https_connect_origin_http2() {
        connect_matrix(Version::HTTP_2, true).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_connect_rejections_do_not_retry_or_fall_back() {
        for tls in [false, true] {
            for reply in [
                ConnectReply::AuthenticationRequired,
                ConnectReply::Refused,
                ConnectReply::Truncated,
            ] {
                let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
                let proxy =
                    FakeProxy::start(&origin, "127.0.0.1:0", tls, ProxyKind::Connect(reply))
                        .await
                        .unwrap();
                let proxy_url = proxy.url(if tls { "https" } else { "http" }, true);
                for stream in [false, true] {
                    proxied_child(
                        &origin,
                        &proxy_url,
                        "preheader-failure",
                        stream,
                        &origin.url,
                    )
                    .await;
                }
                proxy.assert_connect(&origin_address(&origin).to_string(), 2, true);
                assert_origin(&origin, Version::HTTP_11, 0);
            }
        }
    }

    #[tokio::test]
    async fn antigravity_transport_tls_untrusted_origin_ca_is_rejected() {
        let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
        for stream in [false, true] {
            child(&origin, "untrusted-ca", stream).await;
        }
        assert_origin(&origin, Version::HTTP_11, 0);
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_connect_verifies_origin_hostname_and_ca() {
        for (wrong_hostname, proxy_tls) in [(false, false), (true, false), (true, true)] {
            let origin = TlsServer::start(Version::HTTP_2, 200, None, success()).await;
            let proxy = FakeProxy::start(
                &origin,
                "127.0.0.1:0",
                proxy_tls,
                ProxyKind::Connect(ConnectReply::Tunnel),
            )
            .await
            .unwrap();
            let hostname = if wrong_hostname {
                "wrong-san.invalid"
            } else {
                "fixture.invalid"
            };
            let base_url = origin_url(&origin, hostname);
            let proxy_url = proxy.url(if proxy_tls { "https" } else { "http" }, true);
            for stream in [false, true] {
                proxied_child(
                    &origin,
                    &proxy_url,
                    if wrong_hostname {
                        "preheader-failure"
                    } else {
                        "untrusted-ca"
                    },
                    stream,
                    &base_url,
                )
                .await;
            }
            proxy.assert_connect(
                &format!("{hostname}:{}", origin_address(&origin).port()),
                2,
                true,
            );
            if proxy_tls {
                assert_eq!(proxy.observed.tls_connections.load(Ordering::SeqCst), 2);
            }
            assert_origin(&origin, Version::HTTP_2, 0);
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_https_rejects_untrusted_ca_before_connect() {
        let origin = TlsServer::start(Version::HTTP_2, 200, None, success()).await;
        let proxy = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            true,
            ProxyKind::Connect(ConnectReply::Tunnel),
        )
        .await
        .unwrap();
        let proxy_url = proxy.url("https", true);
        for stream in [false, true] {
            proxied_child(&origin, &proxy_url, "untrusted-ca", stream, &origin.url).await;
        }
        proxy.assert_connections(2);
        assert!(proxy.observed.connect.lock().unwrap().is_empty());
        assert_eq!(proxy.observed.tls_connections.load(Ordering::SeqCst), 0);
        assert_origin(&origin, Version::HTTP_2, 0);
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_https_rejects_wrong_proxy_san_before_connect() {
        let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
        let proxy = match FakeProxy::start(
            &origin,
            "127.0.0.2:0",
            true,
            ProxyKind::Connect(ConnectReply::Tunnel),
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                ) =>
            {
                eprintln!("SKIP_IPV4_LOOPBACK_ALIAS_UNAVAILABLE");
                return;
            }
            Err(_) => panic!("synthetic IPv4 loopback alias fixture failed"),
        };
        let proxy_url = proxy.url("https", true);
        for stream in [false, true] {
            proxied_child(
                &origin,
                &proxy_url,
                "preheader-failure",
                stream,
                &origin.url,
            )
            .await;
        }
        proxy.assert_connections(2);
        assert!(proxy.observed.connect.lock().unwrap().is_empty());
        assert_eq!(proxy.observed.tls_connections.load(Ordering::SeqCst), 0);
        assert_origin(&origin, Version::HTTP_11, 0);
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_https_failed_handshake_does_not_fall_back() {
        let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
        let proxy = FakeProxy::start(&origin, "127.0.0.1:0", false, ProxyKind::Close)
            .await
            .unwrap();
        let proxy_url = proxy.url("https", true);
        for stream in [false, true] {
            proxied_child(
                &origin,
                &proxy_url,
                "preheader-failure",
                stream,
                &origin.url,
            )
            .await;
        }
        proxy.assert_connections(2);
        assert!(proxy.observed.connect.lock().unwrap().is_empty());
        assert_origin(&origin, Version::HTTP_11, 0);
    }

    async fn positive_socks5_matrix(scheme: &str, remote_dns: bool, authenticated: bool) {
        for protocol in [Version::HTTP_11, Version::HTTP_2] {
            let origin = TlsServer::start(protocol, 200, None, success()).await;
            let kind = ProxyKind::Socks5 {
                auth: if authenticated {
                    SocksAuth::Password
                } else {
                    SocksAuth::None
                },
                refuse: false,
            };
            let proxy = FakeProxy::start(&origin, "127.0.0.1:0", false, kind)
                .await
                .unwrap();
            // localhost must actually resolve before the local-DNS handshake;
            // fixture.invalid cannot resolve and must remain a remote hostname.
            let base_url = origin_url(
                &origin,
                if remote_dns {
                    "fixture.invalid"
                } else {
                    "localhost"
                },
            );
            let proxy_url = proxy.url(scheme, authenticated);
            for stream in [false, true] {
                proxied_child(&origin, &proxy_url, "plain", stream, &base_url).await;
            }
            proxy.assert_connections(2);
            let observed = proxy.observed.socks.lock().unwrap();
            assert_eq!(observed.len(), 2);
            for request in observed.iter() {
                assert_eq!(request.version, 5);
                assert_eq!(request.port, Some(origin_address(&origin).port()));
                let expected = if remote_dns {
                    SocksAddress::Hostname("fixture.invalid".into())
                } else {
                    // Only a representable local IPv4 result is supported by
                    // the cached Uri-based helper; literal IPv6 is separate.
                    SocksAddress::Ipv4(Ipv4Addr::LOCALHOST)
                };
                assert_eq!(
                    request.address.as_ref(),
                    Some(&expected),
                    "assert actual local-IP versus remote-hostname address form"
                );
                assert_eq!(request.authenticated, authenticated);
                assert_eq!(request.expected_credentials, authenticated);
                assert!(!request.origin_bearer_seen);
            }
            assert_origin(&origin, protocol, 2);
        }
    }

    async fn strict_socks4_legacy_failure(scheme: &str, remote_dns: bool) {
        // NEGATIVE compatibility rows only. The unchanged legacy builder probe
        // proved the cached helper's extra NUL; never forgive/drop that byte in
        // this strict proxy or advertise working strict SOCKS4/4a support.
        for protocol in [Version::HTTP_11, Version::HTTP_2] {
            let origin = TlsServer::start(protocol, 200, None, success()).await;
            let proxy = FakeProxy::start(&origin, "127.0.0.1:0", false, ProxyKind::Socks4)
                .await
                .unwrap();
            let base_url = origin_url(
                &origin,
                if remote_dns {
                    "fixture.invalid"
                } else {
                    "localhost"
                },
            );
            let proxy_url = proxy.url(scheme, false);
            for stream in [false, true] {
                proxied_child(&origin, &proxy_url, "preheader-failure", stream, &base_url).await;
            }
            proxy.assert_connections(2);
            let observed = proxy.observed.socks.lock().unwrap();
            assert_eq!(observed.len(), 2);
            for request in observed.iter() {
                assert_eq!(request.version, 4);
                assert_eq!(request.port, Some(origin_address(&origin).port()));
                assert!(
                    !request.authenticated
                        && !request.expected_credentials
                        && !request.origin_bearer_seen
                );
                let expected = if remote_dns {
                    SocksAddress::Hostname(String::new())
                } else {
                    SocksAddress::Ipv4(Ipv4Addr::LOCALHOST)
                };
                assert_eq!(
                    request.address.as_ref(),
                    Some(&expected),
                    "preserve observed legacy framing failure, not a repaired protocol"
                );
            }
            assert_origin(&origin, protocol, 0);
            eprintln!("INHERITED_STRICT_SOCKS4_INCOMPATIBILITY_NOT_POSITIVE_SUPPORT");
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_socks4_strict_rfc_failure_preserves_legacy_framing() {
        strict_socks4_legacy_failure("socks4", false).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_socks4a_strict_rfc_failure_preserves_legacy_framing() {
        strict_socks4_legacy_failure("socks4a", true).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_socks5_local_dns_and_password_auth() {
        positive_socks5_matrix("socks5", false, true).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_socks5h_remote_dns_and_password_auth() {
        positive_socks5_matrix("socks5h", true, true).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_socks5h_no_auth_is_supported() {
        positive_socks5_matrix("socks5h", true, false).await;
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_socks5_auth_and_connect_rejections_do_not_fall_back() {
        for (auth, refuse) in [
            (SocksAuth::RejectPassword, false),
            (SocksAuth::Password, true),
        ] {
            let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
            let proxy = FakeProxy::start(
                &origin,
                "127.0.0.1:0",
                false,
                ProxyKind::Socks5 { auth, refuse },
            )
            .await
            .unwrap();
            let proxy_url = proxy.url("socks5h", true);
            let base_url = origin_url(&origin, "fixture.invalid");
            for stream in [false, true] {
                proxied_child(&origin, &proxy_url, "preheader-failure", stream, &base_url).await;
            }
            proxy.assert_connections(2);
            let observed = proxy.observed.socks.lock().unwrap();
            assert_eq!(observed.len(), 2);
            for request in observed.iter() {
                assert!(request.authenticated);
                assert!(request.expected_credentials);
                assert!(!request.origin_bearer_seen);
                if refuse {
                    assert_eq!(
                        request.address,
                        Some(SocksAddress::Hostname("fixture.invalid".into()))
                    );
                    assert_eq!(request.port, Some(origin_address(&origin).port()));
                } else {
                    assert!(
                        request.address.is_none(),
                        "rejected password must prevent CONNECT"
                    );
                    assert!(request.port.is_none());
                }
            }
            assert_origin(&origin, Version::HTTP_11, 0);
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_ipv6_tcp_loopback_is_separate_from_destination_support() {
        for (scheme, kind) in [
            ("http", ProxyKind::Connect(ConnectReply::Tunnel)),
            // Strict SOCKS4a is an explicit negative legacy-compatibility row,
            // not part of positive IPv6 proxy-TCP support.
            (
                "socks5h",
                ProxyKind::Socks5 {
                    auth: SocksAuth::Password,
                    refuse: false,
                },
            ),
        ] {
            let origin = TlsServer::start(Version::HTTP_2, 200, None, success()).await;
            let proxy = match FakeProxy::start(&origin, "[::1]:0", false, kind).await {
                Ok(proxy) => proxy,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                    ) =>
                {
                    // A finite, explicit coverage skip, never universal IPv6 PASS.
                    eprintln!("SKIP_IPV6_LOOPBACK_UNAVAILABLE");
                    return;
                }
                Err(_) => panic!("synthetic IPv6 proxy loopback fixture failed"),
            };
            let credentials = true;
            let proxy_url = proxy.url(scheme, credentials);
            assert!(proxy_url.contains("[::1]:"));
            let base_url = origin_url(&origin, "fixture.invalid");
            for stream in [false, true] {
                proxied_child(&origin, &proxy_url, "plain", stream, &base_url).await;
            }
            proxy.assert_connections(2);
            if scheme == "http" {
                proxy.assert_connect(
                    &format!("fixture.invalid:{}", origin_address(&origin).port()),
                    2,
                    true,
                );
            } else {
                let observed = proxy.observed.socks.lock().unwrap();
                assert_eq!(observed.len(), 2);
                for request in observed.iter() {
                    assert_eq!(
                        request.address,
                        Some(SocksAddress::Hostname("fixture.invalid".into()))
                    );
                    assert_eq!(request.port, Some(origin_address(&origin).port()));
                    assert_eq!(request.authenticated, credentials);
                    assert_eq!(request.expected_credentials, credentials);
                    assert!(!request.origin_bearer_seen);
                }
            }
            assert_origin(&origin, Version::HTTP_2, 2);
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_no_proxy_wildcard_ip_and_cidr_bypass() {
        // Cached baseline wildcard is a domain matcher, not an IP matcher.
        // Exercise domain wildcard separately from literal-IP and CIDR bypass.
        for (bypass, hostname) in [
            ("*", "localhost"),
            ("127.0.0.1", "127.0.0.1"),
            ("127.0.0.0/8", "127.0.0.1"),
        ] {
            let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
            let rejected = FakeProxy::start(
                &origin,
                "127.0.0.1:0",
                false,
                ProxyKind::Connect(ConnectReply::AuthenticationRequired),
            )
            .await
            .unwrap();
            let proxy_url = rejected.url("http", true);
            let base_url = origin_url(&origin, hostname);
            for stream in [false, true] {
                child_configured(
                    &origin,
                    "plain",
                    stream,
                    &base_url,
                    &[
                        ("HTTPS_PROXY", &proxy_url),
                        ("NO_PROXY", bypass),
                        ("no_proxy", bypass),
                    ],
                )
                .await;
            }
            rejected.assert_connections(0);
            assert_origin(&origin, Version::HTTP_11, 2);
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_wildcard_ip_literal_preserves_cached_baseline() {
        // Do not silently broaden the inherited matcher into a direct-route
        // privacy change: '*' currently matches domains, not IP literals.
        let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
        let proxy = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            false,
            ProxyKind::Connect(ConnectReply::Tunnel),
        )
        .await
        .unwrap();
        let proxy_url = proxy.url("http", true);
        for stream in [false, true] {
            child_configured(
                &origin,
                "plain",
                stream,
                &origin.url,
                &[
                    ("HTTPS_PROXY", &proxy_url),
                    ("NO_PROXY", "*"),
                    ("no_proxy", "*"),
                ],
            )
            .await;
        }
        proxy.assert_connect(&origin_address(&origin).to_string(), 2, true);
        assert_origin(&origin, Version::HTTP_11, 2);
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_uppercase_https_overrides_lowercase() {
        let origin = TlsServer::start(Version::HTTP_2, 200, None, success()).await;
        let selected = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            false,
            ProxyKind::Connect(ConnectReply::Tunnel),
        )
        .await
        .unwrap();
        let rejected = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            false,
            ProxyKind::Connect(ConnectReply::AuthenticationRequired),
        )
        .await
        .unwrap();
        let selected_url = selected.url("http", true);
        let rejected_url = rejected.url("http", true);
        for stream in [false, true] {
            child_configured(
                &origin,
                "plain",
                stream,
                &origin.url,
                &[
                    ("HTTPS_PROXY", &selected_url),
                    ("https_proxy", &rejected_url),
                    ("NO_PROXY", ""),
                    ("no_proxy", ""),
                ],
            )
            .await;
        }
        selected.assert_connect(&origin_address(&origin).to_string(), 2, true);
        rejected.assert_connections(0);
        assert_origin(&origin, Version::HTTP_2, 2);
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_https_specific_precedes_all_proxy() {
        let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
        let selected = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            false,
            ProxyKind::Connect(ConnectReply::Tunnel),
        )
        .await
        .unwrap();
        let rejected = FakeProxy::start(
            &origin,
            "127.0.0.1:0",
            false,
            ProxyKind::Connect(ConnectReply::AuthenticationRequired),
        )
        .await
        .unwrap();
        let selected_url = selected.url("http", true);
        let rejected_url = rejected.url("http", true);
        for stream in [false, true] {
            child_configured(
                &origin,
                "plain",
                stream,
                &origin.url,
                &[
                    ("HTTPS_PROXY", &selected_url),
                    ("ALL_PROXY", &rejected_url),
                    ("NO_PROXY", ""),
                    ("no_proxy", ""),
                ],
            )
            .await;
        }
        selected.assert_connect(&origin_address(&origin).to_string(), 2, true);
        rejected.assert_connections(0);
        assert_origin(&origin, Version::HTTP_11, 2);
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_uppercase_no_proxy_even_empty_precedes_lowercase() {
        // Cached Matcher::get_first_env returns the first present variable,
        // including an explicitly empty uppercase value.
        for (uppercase, lowercase, proxy_requests) in [("", "*", 2), ("*", "", 0)] {
            let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
            let selected = FakeProxy::start(
                &origin,
                "127.0.0.1:0",
                false,
                ProxyKind::Connect(ConnectReply::Tunnel),
            )
            .await
            .unwrap();
            let selected_url = selected.url("http", true);
            let base_url = origin_url(&origin, "localhost");
            for stream in [false, true] {
                child_configured(
                    &origin,
                    "plain",
                    stream,
                    &base_url,
                    &[
                        ("HTTPS_PROXY", &selected_url),
                        ("NO_PROXY", uppercase),
                        ("no_proxy", lowercase),
                    ],
                )
                .await;
            }
            selected.assert_connect(
                &format!("localhost:{}", origin_address(&origin).port()),
                proxy_requests,
                true,
            );
            assert_origin(&origin, Version::HTTP_11, 2);
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_uppercase_all_precedence_and_cgi_bypass() {
        for cgi in [false, true] {
            for https_specific in [false, true] {
                let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
                let selected = FakeProxy::start(
                    &origin,
                    "127.0.0.1:0",
                    false,
                    ProxyKind::Connect(ConnectReply::Tunnel),
                )
                .await
                .unwrap();
                let rejected = FakeProxy::start(
                    &origin,
                    "127.0.0.1:0",
                    false,
                    ProxyKind::Connect(ConnectReply::AuthenticationRequired),
                )
                .await
                .unwrap();
                let selected_url = selected.url("http", true);
                let rejected_url = rejected.url("http", true);
                let mut env = vec![
                    ("HTTP_PROXY", rejected_url.as_str()),
                    ("ALL_PROXY", selected_url.as_str()),
                    ("all_proxy", rejected_url.as_str()),
                    ("NO_PROXY", ""),
                    ("no_proxy", ""),
                ];
                if https_specific {
                    env.push(("HTTPS_PROXY", selected_url.as_str()));
                }
                if cgi {
                    env.push(("REQUEST_METHOD", "GET"));
                }
                for stream in [false, true] {
                    child_configured(&origin, "plain", stream, &origin.url, &env).await;
                }
                // Cached Linux Matcher::from_system disables every proxy in
                // CGI, not just HTTP_PROXY. This is source-backed compatibility,
                // not an assumption about Go or other proxy implementations.
                selected.assert_connect(
                    &origin_address(&origin).to_string(),
                    if cgi { 0 } else { 2 },
                    true,
                );
                rejected.assert_connections(0);
                assert_origin(&origin, Version::HTTP_11, 2);
            }
        }
    }

    #[tokio::test]
    async fn antigravity_transport_proxy_http_environment_is_not_an_https_fallback_even_in_cgi() {
        // The release public API accepts only HTTPS origins: this exercises CGI
        // handling without claiming coverage of cleartext HTTP CGI destinations.
        for cgi in [false, true] {
            let origin = TlsServer::start(Version::HTTP_11, 200, None, success()).await;
            let rejected = FakeProxy::start(
                &origin,
                "127.0.0.1:0",
                false,
                ProxyKind::Connect(ConnectReply::AuthenticationRequired),
            )
            .await
            .unwrap();
            let rejected_url = rejected.url("http", true);
            let mut env = vec![
                ("HTTP_PROXY", rejected_url.as_str()),
                ("http_proxy", rejected_url.as_str()),
                ("NO_PROXY", ""),
                ("no_proxy", ""),
            ];
            if cgi {
                env.push(("REQUEST_METHOD", "GET"));
            }
            for stream in [false, true] {
                child_configured(&origin, "plain", stream, &origin.url, &env).await;
            }
            rejected.assert_connections(0);
            assert_origin(&origin, Version::HTTP_11, 2);
        }
    }
}
