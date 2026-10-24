//! Synthetic loopback HTTP tests. No real OAuth, Google, CLI import, or inference.
use super::antigravity::{
    AntigravityProvider,
    auth::{Auth, Credential, Store},
    config::Config,
    replay,
};
use crate::{
    config::AliasProvider,
    monitor::{MonitorHandle, RequestStatus},
    provider::Provider,
    registry::Registry,
    server::{AppFeatures, app_with_features},
};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct Server {
    pub(super) url: url::Url,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub(super) async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, task }
    }
}

type Captured = Arc<Mutex<Vec<(http::HeaderMap, Value)>>>;
struct Backend {
    captured: Captured,
    replies: Vec<(u16, String)>,
    headers: http::HeaderMap,
}
async fn backend(State(state): State<Arc<Backend>>, request: Request) -> Response {
    let headers = request.headers().clone();
    let bytes = axum::body::to_bytes(request.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap();
    let index = {
        let mut captured = state.captured.lock().unwrap();
        let index = captured.len();
        captured.push((headers, value));
        index
    };
    let (status, body) = state
        .replies
        .get(index)
        .unwrap_or_else(|| state.replies.last().unwrap());
    let mut response = Response::builder()
        .status(*status)
        .body(Body::from(body.clone()))
        .unwrap();
    *response.headers_mut() = state.headers.clone();
    response
}
async fn native(replies: Vec<(u16, String)>) -> (Server, Captured) {
    native_with_headers(
        replies,
        &[("content-type", "text/event-stream"), ("retry-after", "7")],
    )
    .await
}
async fn native_with_headers(
    replies: Vec<(u16, String)>,
    headers: &[(&str, &str)],
) -> (Server, Captured) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut map = http::HeaderMap::new();
    for (name, value) in headers {
        map.append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::HeaderValue::from_str(value).unwrap(),
        );
    }
    let state = Arc::new(Backend {
        captured: captured.clone(),
        replies,
        headers: map,
    });
    let server = Server::start(
        Router::new()
            .route("/v1internal:streamGenerateContent", post(backend))
            .with_state(state),
    )
    .await;
    (server, captured)
}
fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn successful(parts: Value) -> String {
    frame(
        json!({"response":{"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":1,"totalTokenCount":13}}}),
    )
}
pub(super) fn expected_native_user_agent() -> String {
    format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"))
}

async fn provider(directory: &Path, url: &url::Url) -> AntigravityProvider {
    provider_with_user_agent(directory, url, None).await
}

async fn provider_with_user_agent(
    directory: &Path,
    url: &url::Url,
    user_agent: Option<&str>,
) -> AntigravityProvider {
    let store = Store::new(directory.join("antigravity/auth.json"));
    let lock = store.lock(Duration::from_secs(1)).await.unwrap();
    store
        .save(
            &lock,
            &Credential {
                profile_id: "profile".into(),
                project_id: Some("project".into()),
                access_token: "proxy-owned-access".into(),
                refresh_token: Some("proxy-owned-refresh".into()),
                expires_at: super::antigravity::auth::now() + 3600,
            },
        )
        .unwrap();
    drop(lock);
    let config = Config {
        base_url: url.clone(),
        client_id: Some("synthetic-client".into()),
        ..Config::read(&directory.join("config.json"), |key| {
            if key == "CCP_ANTIGRAVITY_USER_AGENT" {
                user_agent.map(Into::into)
            } else {
                None
            }
        })
        .unwrap()
    };
    AntigravityProvider::with_auth(Auth::new(config, store).unwrap())
}
async fn proxy(provider: AntigravityProvider, monitor: Option<MonitorHandle>) -> Server {
    let registry = Registry::from_providers(
        AliasProvider::Codex,
        [Arc::new(provider) as Arc<dyn Provider>],
    );
    Server::start(app_with_features(
        Arc::new(registry),
        monitor,
        AppFeatures {
            responses_api: true,
            ..AppFeatures::default()
        },
    ))
    .await
}
fn request(stream: bool) -> Value {
    json!({"model":"antigravity/gemini-4-argon-low","max_tokens":64,"stream":stream,"messages":[{"role":"user","content":"synthetic prompt"}],"thinking":{"type":"adaptive","display":"omitted"},"output_config":{"effort":"high"}})
}
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn antigravity_consumer_selected_origin_serves_discovery_and_generation() {
    for stream in [false, true] {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let token_operations = operations.clone();
        let oauth = Server::start(Router::new().route("/token", post(move |axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>| {
            let operations = token_operations.clone();
            async move {
                assert_eq!(form["grant_type"], "authorization_code");
                operations.lock().unwrap().push("token");
                axum::Json(json!({"access_token":"synthetic-access","refresh_token":"synthetic-refresh","expires_in":3600}))
            }
        }))).await;
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Backend {
            captured: captured.clone(),
            replies: vec![(200, successful(json!([{"text":"selected origin"}])))],
            headers: http::HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/event-stream"),
            )]),
        });
        let discovery_operations = operations.clone();
        let generation_operations = operations.clone();
        let chosen = Server::start(
            Router::new()
                .route(
                    "/v1internal:loadCodeAssist",
                    post(move |headers: http::HeaderMap| {
                        let operations = discovery_operations.clone();
                        async move {
                            assert_eq!(headers["authorization"], "Bearer synthetic-access");
                            operations.lock().unwrap().push("discovery");
                            axum::Json(json!({"cloudaicompanionProject":"synthetic-project"}))
                        }
                    }),
                )
                .route(
                    "/v1internal:streamGenerateContent",
                    post(move |request: Request| {
                        let state = state.clone();
                        let operations = generation_operations.clone();
                        async move {
                            assert_eq!(
                                request.uri().path_and_query().unwrap().as_str(),
                                "/v1internal:streamGenerateContent?alt=sse"
                            );
                            operations.lock().unwrap().push("generation");
                            backend(State(state), request).await
                        }
                    }),
                ),
        )
        .await;
        let directory = tempfile::tempdir().unwrap();
        // Loopback injection tests shared-origin plumbing, not live Google access.
        let auth = Auth::new(
            Config {
                base_url: chosen.url.clone(),
                token_url: oauth.url.join("/token").unwrap(),
                client_id: Some("synthetic-client".into()),
                ..Config::default()
            },
            Store::new(directory.path().join("auth.json")),
        )
        .unwrap();
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
                client().get(redirect).send().await.unwrap()
            }));
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(callback.unwrap().await.unwrap().status(), 200);
        let proxy = proxy(AntigravityProvider::with_auth(auth), None).await;
        let response = client()
            .post(proxy.url.join("/v1/messages").unwrap())
            .json(&request(stream))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.text().await.unwrap().contains("selected origin"));
        assert_eq!(
            *operations.lock().unwrap(),
            ["token", "discovery", "generation"]
        );
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0["authorization"], "Bearer synthetic-access");
        assert_eq!(captured[0].1["project"], "synthetic-project");
    }
}

#[tokio::test]
async fn antigravity_consumer_three_stage_sse_and_nonstream_tool_replay() {
    // Entirely synthetic: no captured provider signatures, prompts or credentials.
    let thought = json!({"role":"model","parts":[{"thought":true,"text":"synthetic hidden thought","thoughtSignature":"synthetic-thought-signature"}]});
    let call = json!({"role":"model","parts":[{"functionCall":{"id":"synthetic-native-call","name":"endpoint_canary_echo","args":{"value":"endpoint-canary-public-nonce"}},"thoughtSignature":"synthetic-call-signature"}]});
    let tool_reply = format!(
        "{}{}",
        frame(json!({"response":{"candidates":[{"content":thought.clone()}]}})),
        successful(call["parts"].clone())
    );
    let (upstream, captured) = native(vec![
        (200, successful(json!([{"text":"SSE_"},{"text":"TEXT_OK"}]))),
        (200, tool_reply),
        (
            200,
            successful(json!([{"text":"TOOL_"},{"text":"ROUNDTRIP_OK"}])),
        ),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let proxy = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let endpoint = proxy.url.join("/v1/messages").unwrap();
    let mut text_request = request(true);
    text_request["max_tokens"] = json!(4096);
    let response = client()
        .post(endpoint.clone())
        .json(&text_request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let events: Vec<Value> = response
        .text()
        .await
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["type"], "message_start");
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "error" && event["content_block"]["type"] != "tool_use")
    );
    let text: String = events
        .iter()
        .filter(|event| event["delta"]["type"] == "text_delta")
        .map(|event| event["delta"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "SSE_TEXT_OK");
    let terminal = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(terminal["delta"]["stop_reason"], "end_turn");
    assert_eq!(terminal["usage"]["input_tokens"], 10);
    assert_eq!(terminal["usage"]["output_tokens"], 3);

    let mut tool_request = request(false);
    tool_request["max_tokens"] = json!(4096);
    tool_request["messages"] = json!([{"role":"user","content":"Call endpoint_canary_echo with endpoint-canary-public-nonce, then wait for its result."}]);
    tool_request["tools"] = json!([{"name":"endpoint_canary_echo","input_schema":{"type":"object","properties":{"value":{"type":"string","enum":["endpoint-canary-public-nonce"]}},"required":["value"],"additionalProperties":false}}]);
    tool_request["tool_choice"] = json!({"type":"auto"});
    let response = client()
        .post(endpoint.clone())
        .json(&tool_request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let message: Value = response.json().await.unwrap();
    assert_eq!(message["stop_reason"], "tool_use");
    let content = message["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "");
    assert!(
        content[0]["signature"]
            .as_str()
            .unwrap()
            .starts_with(replay::PREFIX)
    );
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["name"], "endpoint_canary_echo");
    assert_eq!(
        content[1]["input"],
        json!({"value":"endpoint-canary-public-nonce"})
    );
    let tool_id = content[1]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .unwrap();
    // Carry the full nonstream aggregate unchanged, including its leading carrier.
    tool_request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":message["content"].clone()}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":tool_id,"content":"endpoint-canary-public-nonce"}]}),
    ]);
    let response = client()
        .post(endpoint)
        .json(&tool_request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let final_message: Value = response.json().await.unwrap();
    assert_eq!(final_message["stop_reason"], "end_turn");
    let final_content = final_message["content"].as_array().unwrap();
    assert!(
        final_content
            .iter()
            .all(|block| block["type"] != "tool_use")
    );
    let text: String = final_content
        .iter()
        .filter(|block| block["type"] == "text")
        .map(|block| block["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "TOOL_ROUNDTRIP_OK");
    assert_eq!(final_message["usage"]["input_tokens"], 10);
    assert_eq!(final_message["usage"]["output_tokens"], 3);
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 3);
    for (_, wire) in captured.iter() {
        assert_eq!(wire["model"], "gemini-4-argon-low");
        assert_eq!(wire["project"], "project");
        assert_eq!(wire["request"]["generationConfig"]["maxOutputTokens"], 4096);
    }
    let replayed = &captured[2].1["request"];
    assert_eq!(replayed["sessionId"], captured[1].1["request"]["sessionId"]);
    assert_eq!(replayed["contents"][1], thought);
    assert_eq!(replayed["contents"][2], call);
    assert_eq!(
        replayed["contents"][3],
        json!({"role":"model","parts":[{"functionResponse":{"id":"synthetic-native-call","name":"endpoint_canary_echo","response":{"output":"endpoint-canary-public-nonce"}}}]})
    );
}

#[tokio::test]
async fn antigravity_user_agent_generation_wire_matrix() {
    for override_value in [
        None,
        Some("sentinel-native/9  (interior spaces preserved)"),
        Some(
            "antigravity/cli/1.3.5 (aidev_client; os_type=darwin; arch=arm64; cl=998863838; auth_method=consumer)",
        ),
    ] {
        for stream in [false, true] {
            let (upstream, captured) =
                native(vec![(200, successful(json!([{"text":"ok"}])))]).await;
            let directory = tempfile::tempdir().unwrap();
            let monitor = MonitorHandle::new(10);
            let proxy = proxy(
                provider_with_user_agent(directory.path(), &upstream.url, override_value).await,
                Some(monitor.clone()),
            )
            .await;
            let response = client()
                .post(proxy.url.join("/v1/messages").unwrap())
                .header("user-agent", "hostile-frontend-UA")
                .header("authorization", "Bearer frontend-credential")
                .header("cookie", "private-frontend-cookie")
                .header("x-goog-api-client", "frontend-metadata")
                .json(&request(stream))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let text = response.text().await.unwrap();
            assert!(!text.contains("sentinel-native"));
            let captured = captured.lock().unwrap();
            assert_eq!(captured.len(), 1);
            let (headers, body) = &captured[0];
            assert_eq!(headers.get_all("user-agent").iter().count(), 1);
            assert_eq!(
                headers["user-agent"],
                override_value
                    .map(str::to_owned)
                    .unwrap_or_else(expected_native_user_agent)
            );
            assert_eq!(headers["authorization"], "Bearer proxy-owned-access");
            assert!(!headers.contains_key("cookie"));
            assert!(!headers.contains_key("x-goog-api-client"));
            assert_eq!(body["userAgent"], "antigravity");
            assert_eq!(body["requestType"], "agent");
            assert!(!format!("{:?}", monitor.snapshot()).contains("sentinel-native"));
        }
    }
}

#[tokio::test]
async fn antigravity_http2_refused_stream_is_not_automatically_retried() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut preface = [0_u8; 24];
        socket.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        socket
            .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut attempts = 0;
        loop {
            let mut header = [0_u8; 9];
            if !matches!(
                tokio::time::timeout(Duration::from_millis(500), socket.read_exact(&mut header))
                    .await,
                Ok(Ok(_))
            ) {
                break;
            }
            let length = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
            assert!(length <= 65536);
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).await.unwrap();
            if header[3] == 4 && header[4] & 1 == 0 {
                socket
                    .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                    .await
                    .unwrap();
            }
            if header[3] == 1 {
                attempts += 1;
                let reset = [
                    0, 0, 4, 3, 0, header[5], header[6], header[7], header[8], 0, 0, 0, 7,
                ];
                socket.write_all(&reset).await.unwrap();
            }
        }
        attempts
    });
    let directory = tempfile::tempdir().unwrap();
    let config = Config::read(&directory.path().join("config.json"), |key| {
        (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| "configured-no-retry-UA".into())
    })
    .unwrap();
    let client = config
        .client_builder()
        .no_proxy()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    assert!(
        client
            .post(format!("http://{address}/v1internal:streamGenerateContent"))
            .headers(config.native_headers())
            .json(&json!({"synthetic":"generation"}))
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        upstream.await.unwrap(),
        1,
        "even protocol nacks must not silently buy a second native attempt"
    );
}

#[tokio::test]
async fn antigravity_serving_capability_uses_its_own_auth_and_configuration() {
    let (upstream, captured) = native(vec![(200, successful(json!([{"text":"unused"}])))]).await;
    let directory = tempfile::tempdir().unwrap();
    let proxy = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let response = client()
        .get(
            proxy
                .url
                .join("/v1/models?include_capabilities=true")
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let value: Value = response.json().await.unwrap();
    let descriptor = &value["capabilities"]["providers"]["antigravity"];
    assert_eq!(descriptor["enabled"], true);
    assert_eq!(descriptor["status"]["auth"], "ready");
    assert_eq!(descriptor["status"]["project"], "configured");
    assert_eq!(descriptor["status"]["configuration_ready"], true);
    assert_eq!(descriptor["status"]["live_verified"], false);
    assert!(captured.lock().unwrap().is_empty());
    assert!(!descriptor.to_string().contains("proxy-owned"));
}

#[tokio::test]
async fn antigravity_opaque_native_signature_corruption_is_upstream_not_local_authentication() {
    use base64::Engine;
    let binding = replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "gemini-4-argon-low".into(),
    };
    let native_contents = json!([{"role":"model","parts":[{"text":"same projection","thoughtSignature":"original-native-signature"}]}]);
    let mut blocks = replay::encode(&native_contents, &binding, "17", "full", &json!([])).unwrap();
    let original = replay::decode(&blocks, &binding).unwrap();
    let mut mutated = original.clone();
    mutated["contents"][0]["parts"][0]["thoughtSignature"] = json!("corrupt-native-signature");
    blocks[0]["signature"] = json!(format!(
        "{}{}",
        replay::PREFIX,
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&mutated).unwrap())
    ));
    let decoded = replay::decode(&blocks, &binding).unwrap();
    assert_eq!(decoded["projection_digest"], original["projection_digest"]);
    let (upstream, captured) = native(vec![(
        400,
        "upstream signature validation rejected corrupt-native-signature".into(),
    )])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let monitor = MonitorHandle::new(10);
    let proxy = proxy(
        provider(directory.path(), &upstream.url).await,
        Some(monitor.clone()),
    )
    .await;
    let mut body = request(true);
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":blocks}),
        json!({"role":"user","content":"next"}),
    ]);
    let response = client()
        .post(proxy.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let error = response.text().await.unwrap();
    assert!(!error.contains("corrupt-native-signature"));
    assert!(!error.contains("tool_use"));
    assert!(!error.contains("message_stop"));
    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "opaque corruption requires exactly one native validation attempt, never repair/retry"
    );
    assert_eq!(
        captured[0].1["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        "corrupt-native-signature"
    );
    let diagnostic = format!("{:?}", monitor.snapshot());
    assert!(!diagnostic.contains("corrupt-native-signature"));
}

#[tokio::test]
async fn antigravity_http_preserves_preheader_errors_without_retry_or_secret_echo() {
    for status in [401, 403, 429, 503] {
        let (upstream, captured) = native(vec![(
            status,
            "private OAuth token and native signature".into(),
        )])
        .await;
        let directory = tempfile::tempdir().unwrap();
        let proxy = proxy(
            provider_with_user_agent(
                directory.path(),
                &upstream.url,
                Some("sentinel-no-retry-UA"),
            )
            .await,
            None,
        )
        .await;
        let response = client()
            .post(proxy.url.join("/v1/messages").unwrap())
            .header("authorization", "Bearer frontend-secret")
            .header("anthropic-beta", "unrelated-beta")
            .json(&request(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["retry-after"], "7");
        let text = response.text().await.unwrap();
        assert!(!text.contains("private OAuth"));
        assert!(!text.contains("sentinel-no-retry-UA"));
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0["authorization"], "Bearer proxy-owned-access");
        assert!(!captured[0].0.contains_key("anthropic-beta"));
        assert_eq!(captured[0].0.get_all("user-agent").iter().count(), 1);
        assert_eq!(captured[0].0["user-agent"], "sentinel-no-retry-UA");
    }
}

#[tokio::test]
async fn antigravity_http_replay_restores_native_calls_after_fresh_provider() {
    let original = json!([{"thought":true,"text":"hidden native thought","thoughtSignature":"raw-thought"},
        {"functionCall":{"name":"Read","args":{"limit":1}},"thoughtSignature":"raw-call"}]);
    let (upstream, captured) = native(vec![
        (200, successful(original.clone())),
        (
            200,
            successful(json!([{"text":"done","thoughtSignature":"raw-final"}])),
        ),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let first = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let mut body = request(false);
    body["tools"] = json!([{"name":"Read","input_schema":{"type":"object","properties":{"limit":{"type":"integer","exclusiveMinimum":0}},"required":["limit"],"additionalProperties":false}}]);
    let response = client()
        .post(first.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let message: Value = response.json().await.unwrap();
    assert_eq!(message["content"][0]["thinking"], "");
    let local = message["content"][1]["id"].as_str().unwrap();
    drop(first);
    let second = proxy(provider(directory.path(), &upstream.url).await, None).await;
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":message["content"]}),
        json!({"role":"system","content":"ordered system after resume"}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":local,"content":"{\"untouched\":true}"},{"type":"text","text":"continue"}]})
    ]);
    let response = client()
        .post(second.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    {
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let wire = &captured[1].1["request"];
        assert_eq!(
            wire["contents"][1],
            json!({"role":"model","parts":original})
        );
        assert_eq!(
            wire["contents"][2],
            json!({"role":"model","parts":[{"functionResponse":{"name":"Read","response":{"output":"{\"untouched\":true}"}}}]})
        );
        assert_eq!(
            wire["contents"][3],
            json!({"role":"user","parts":[{"text":"continue"}]})
        );
        assert_eq!(wire["sessionId"], captured[0].1["request"]["sessionId"]);
        assert_eq!(
            wire["systemInstruction"]["parts"],
            json!([{"text":"ordered system after resume"}])
        );
    }
    body["model"] = json!("antigravity/gemini-4-argon-high");
    let response = client()
        .post(second.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        captured.lock().unwrap().len(),
        2,
        "model switch must not purchase inference"
    );
}

#[tokio::test]
async fn antigravity_late_error_sets_one_failed_monitor_outcome_and_no_success_events() {
    let (upstream,captured)=native(vec![(200,frame(json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"must not publish"}]}}]}})))]).await;
    let directory = tempfile::tempdir().unwrap();
    let monitor = MonitorHandle::new(10);
    let proxy = proxy(
        provider(directory.path(), &upstream.url).await,
        Some(monitor.clone()),
    )
    .await;
    let response = client()
        .post(proxy.url.join("/v1/messages").unwrap())
        .json(&request(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("event: error"));
    for prohibited in [
        "message_stop",
        "tool_use",
        "must not publish",
        "message_start",
    ] {
        assert!(!text.contains(prohibited));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let snapshot = monitor.snapshot();
    assert!(snapshot.active.is_empty());
    assert_eq!(snapshot.recent.len(), 1);
    assert_eq!(snapshot.recent[0].status, RequestStatus::Failed);
    assert_eq!(captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn antigravity_effective_tier_is_observable_and_openai_stays_unsupported() {
    let (upstream, captured) = native(vec![(200, successful(json!([{"text":"ok"}])))]).await;
    let directory = tempfile::tempdir().unwrap();
    let monitor = MonitorHandle::new(10);
    let proxy = proxy(
        provider(directory.path(), &upstream.url).await,
        Some(monitor.clone()),
    )
    .await;
    let response = client()
        .post(proxy.url.join("/v1/messages").unwrap())
        .json(&request(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-antigravity-reasoning-tier")
            .and_then(|v| v.to_str().ok()),
        Some("low")
    );
    let _ = response.text().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(monitor.snapshot().recent[0].effort.as_deref(), Some("low"));
    assert_eq!(monitor.snapshot().recent[0].input_tokens, Some(10));
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(3));
    let response=client().post(proxy.url.join("/v1/chat/completions").unwrap()).json(&json!({"model":"antigravity/gemini-4-argon-low","messages":[{"role":"user","content":"x"}]})).send().await.unwrap();
    assert_eq!(response.status(), 501);
    assert_eq!(captured.lock().unwrap().len(), 1);
}

async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        raw.extend_from_slice(&buffer[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&raw[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if raw.len() >= end + 4 + length {
                return;
            }
        }
    }
}

#[tokio::test]
async fn antigravity_real_disconnect_cancels_preheader_nonstream_and_stream_collection() {
    for (headers_sent, streaming) in [(false, false), (false, true), (true, false), (true, true)] {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}", upstream.local_addr().unwrap())).unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let native_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            read_request(&mut socket).await;
            if headers_sent {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            }
            let _ = accepted_tx.send(());
            let mut byte = [0u8; 1];
            let closed = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte)).await;
            let _ = closed_tx.send(matches!(closed, Ok(Ok(0)) | Ok(Err(_))));
        });
        let directory = tempfile::tempdir().unwrap();
        let monitor = MonitorHandle::new(10);
        let proxy = proxy(
            provider(directory.path(), &url).await,
            Some(monitor.clone()),
        )
        .await;
        let mut downstream =
            tokio::net::TcpStream::connect(("127.0.0.1", proxy.url.port().unwrap()))
                .await
                .unwrap();
        let body = request(streaming).to_string();
        downstream.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        accepted_rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(downstream);
        assert!(
            closed_rx.await.unwrap(),
            "upstream task survived disconnect: headers={headers_sent} stream={streaming}"
        );
        native_task.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(monitor.snapshot().active.is_empty());
    }
}

fn safe_suffix(
    status: &str,
    reason: &str,
    domain: &str,
    quota: &str,
    hint: &str,
    seconds: &str,
) -> String {
    format!(
        "; google_status={status}; reason={reason}; domain={domain}; quota_failure={quota}; retry_hint={hint}; retry_s={seconds}"
    )
}
fn error_info(reason: &str, domain: &str) -> Value {
    json!({"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":reason,"domain":domain,"metadata":{"project":"DO_NOT_REFLECT_native_secret","email":"DO_NOT_REFLECT_native_secret@example.test"}})
}
fn retry_info(delay: Value) -> Value {
    json!({"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":delay})
}
fn error_body(details: Value) -> String {
    json!({"error":{"code":429,"message":"DO_NOT_REFLECT_native_secret","status":"RESOURCE_EXHAUSTED","details":details}}).to_string()
}
async fn assert_safe_error(
    body: &str,
    headers: &[(&str, &str)],
    status: u16,
    expected: (&str, Option<&str>),
) {
    for streaming in [false, true] {
        let (upstream, captured) = native_with_headers(vec![(status, body.into())], headers).await;
        let directory = tempfile::tempdir().unwrap();
        let monitor = MonitorHandle::new(10);
        let proxy = proxy(
            provider(directory.path(), &upstream.url).await,
            Some(monitor.clone()),
        )
        .await;
        let response = client()
            .post(proxy.url.join("/v1/messages").unwrap())
            .json(&request(streaming))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["x-antigravity-reasoning-tier"], "low");
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            expected.1
        );
        let text = response.text().await.unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let kind = match status {
            401 => "authentication_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "api_error",
        };
        assert_eq!(
            value,
            json!({"type":"error","error":{"type":kind,"message":format!("Antigravity upstream returned HTTP {status}; no retry or fallback attempted{}", expected.0)}})
        );
        assert!(!text.contains("DO_NOT_REFLECT_native_secret"));
        assert!(!text.contains("message_start"));
        assert!(!text.contains("message_stop"));
        assert!(!text.contains("tool_use"));
        assert!(!format!("{:?}", monitor.snapshot()).contains("DO_NOT_REFLECT_native_secret"));
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "one generation attempt only"
        );
    }
}

#[tokio::test]
async fn antigravity_safe_errors_preserve_status_and_extract_only_allowed_fields() {
    let body = error_body(json!([error_info("RATE_LIMIT_EXCEEDED", "googleapis.com"),
        {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"subject":"DO_NOT_REFLECT_native_secret","description":"DO_NOT_REFLECT_native_secret"}]},
        {"@type":"DO_NOT_REFLECT_native_secret","message":"DO_NOT_REFLECT_native_secret"}]));
    let suffix = safe_suffix(
        "RESOURCE_EXHAUSTED",
        "RATE_LIMIT_EXCEEDED",
        "googleapis.com",
        "reported",
        "valid",
        "7",
    );
    for status in [400, 401, 403, 404, 429, 500, 503] {
        assert_safe_error(
            &body,
            &[("retry-after", "00007")],
            status,
            (&suffix, Some("7")),
        )
        .await;
    }
    assert_safe_error(
        &error_body(json!([])),
        &[],
        429,
        (
            &safe_suffix(
                "RESOURCE_EXHAUSTED",
                "unclassified",
                "unclassified",
                "absent",
                "absent",
                "none",
            ),
            None,
        ),
    )
    .await;
}

#[tokio::test]
async fn antigravity_safe_errors_unknown_ambiguous_and_quota_details_fail_closed() {
    for (details, reason, domain, quota) in [
        (
            json!([error_info(
                "MODEL_CAPACITY_EXHAUSTED",
                "cloudcode-pa.googleapis.com"
            )]),
            "MODEL_CAPACITY_EXHAUSTED",
            "cloudcode-pa.googleapis.com",
            "absent",
        ),
        (
            json!([error_info(
                "DO_NOT_REFLECT_native_secret_CAPACITY",
                "googleapis.com"
            )]),
            "unclassified",
            "googleapis.com",
            "absent",
        ),
        (
            json!([error_info(
                "RATE_LIMIT_EXCEEDED",
                "DO_NOT_REFLECT_native_secret"
            )]),
            "unclassified",
            "unclassified",
            "absent",
        ),
        (
            json!([
                error_info("RATE_LIMIT_EXCEEDED", "googleapis.com"),
                error_info("RATE_LIMIT_EXCEEDED", "googleapis.com")
            ]),
            "unclassified",
            "unclassified",
            "absent",
        ),
        (
            json!([{"@type":"type.googleapis.com/google.rpc.ErrorInfo","domain":"googleapis.com","reason":42}]),
            "unclassified",
            "unclassified",
            "absent",
        ),
        (
            json!([{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[]}]),
            "unclassified",
            "unclassified",
            "unclassified",
        ),
        (
            json!([{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":["DO_NOT_REFLECT_native_secret"]}]),
            "unclassified",
            "unclassified",
            "unclassified",
        ),
        (
            json!([{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{}]}, {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{}]}]),
            "unclassified",
            "unclassified",
            "unclassified",
        ),
    ] {
        assert_safe_error(
            &error_body(details),
            &[],
            429,
            (
                &safe_suffix(
                    "RESOURCE_EXHAUSTED",
                    reason,
                    domain,
                    quota,
                    "absent",
                    "none",
                ),
                None,
            ),
        )
        .await;
    }
    for status in ["OK", "DO_NOT_REFLECT_native_secret", "resource_exhausted"] {
        let body = json!({"error":{"status":status}}).to_string();
        assert_safe_error(
            &body,
            &[],
            429,
            (
                &safe_suffix(
                    "unclassified",
                    "unclassified",
                    "unclassified",
                    "absent",
                    "absent",
                    "none",
                ),
                None,
            ),
        )
        .await;
    }
}

#[tokio::test]
async fn antigravity_safe_errors_unusable_bodies_never_claim_no_retry_hint() {
    let mut bodies = vec![
        String::new(),
        "DO_NOT_REFLECT_native_secret".into(),
        "null".into(),
        "{}".into(),
        "{\"error\":{},\"error\":{}}".into(),
        "{\"error\":{\"status\":\"UNKNOWN\",\"status\":\"INTERNAL\"}}".into(),
        error_body(json!({})),
        error_body(json!([{}])),
        error_body(json!([{"@type":1}])),
        error_body(json!([1])),
        error_body(json!(vec![json!({"@type":"ignored"}); 17])),
        format!(
            "{{\"error\":{{\"unused\":{}0{}}}}}",
            "[".repeat(65),
            "]".repeat(65)
        ),
        format!("{{\"error\":{{\"message\":\"{}\"}}}}", "x".repeat(8192)),
    ];
    bodies.push(format!("{} trailing", error_body(json!([]))));
    for body in bodies {
        for (headers, hint, seconds, forwarded) in [
            (vec![], "invalid", "none", None),
            (vec![("retry-after", "7")], "valid", "7", Some("7")),
        ] {
            assert_safe_error(
                &body,
                &headers,
                503,
                (
                    &safe_suffix(
                        "unclassified",
                        "unclassified",
                        "unclassified",
                        "unclassified",
                        hint,
                        seconds,
                    ),
                    forwarded,
                ),
            )
            .await;
        }
    }
    let body = "{\"error\":{}}";
    let padded = format!("{body}{}", " ".repeat(8192 - body.len()));
    assert_safe_error(
        &padded,
        &[],
        400,
        (
            &safe_suffix(
                "unclassified",
                "unclassified",
                "unclassified",
                "absent",
                "absent",
                "none",
            ),
            None,
        ),
    )
    .await;
}

#[tokio::test]
async fn antigravity_safe_errors_retry_truth_table_and_header_only_forwarding() {
    // Header and RetryInfo are independent: never invent an HTTP header from the body.
    for (headers, details, hint, seconds, forwarded) in [
        (vec![], json!([]), "absent", "none", None),
        (vec![], json!([retry_info(json!("7s"))]), "valid", "7", None),
        (
            vec![],
            json!([retry_info(json!("0.000000001s"))]),
            "valid",
            "1",
            None,
        ),
        (
            vec![],
            json!([retry_info(json!("7.250s"))]),
            "valid",
            "8",
            None,
        ),
        (
            vec![],
            json!([retry_info(json!("86400.000000000s"))]),
            "valid",
            "86400",
            None,
        ),
        (
            vec![("retry-after", "7")],
            json!([retry_info(json!("7s"))]),
            "valid",
            "7",
            Some("7"),
        ),
        (
            vec![("retry-after", "7")],
            json!([retry_info(json!("8s"))]),
            "conflicting",
            "none",
            Some("7"),
        ),
        (
            vec![("retry-after", "7")],
            json!([retry_info(json!("bad"))]),
            "invalid",
            "none",
            Some("7"),
        ),
        (
            vec![("retry-after", "DO_NOT_REFLECT_native_secret")],
            json!([retry_info(json!("7s"))]),
            "invalid",
            "none",
            None,
        ),
        (
            vec![("retry-after", "7"), ("retry-after", "7")],
            json!([]),
            "invalid",
            "none",
            None,
        ),
        (
            vec![],
            json!([retry_info(json!("7s")), retry_info(json!("7s"))]),
            "conflicting",
            "none",
            None,
        ),
        (
            vec![("retry-after", "7")],
            json!([retry_info(json!("7s")), retry_info(json!("7s"))]),
            "conflicting",
            "none",
            Some("7"),
        ),
        (
            vec![],
            json!([retry_info(json!("7s")), retry_info(json!("bad"))]),
            "invalid",
            "none",
            None,
        ),
        (
            vec![("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT")],
            json!([retry_info(json!("7s"))]),
            "valid",
            "7",
            Some("Sun, 06 Nov 1994 08:49:37 GMT"),
        ),
    ] {
        assert_safe_error(
            &error_body(details),
            &headers,
            429,
            (
                &safe_suffix(
                    "RESOURCE_EXHAUSTED",
                    "unclassified",
                    "unclassified",
                    "absent",
                    hint,
                    seconds,
                ),
                forwarded,
            ),
        )
        .await;
    }
    for delay in [
        json!("-1s"),
        json!("+1s"),
        json!("01s"),
        json!("1e2s"),
        json!("1.s"),
        json!(".1s"),
        json!("1.0000000001s"),
        json!("86400.000000001s"),
        json!("86401s"),
        json!("100000s"),
        json!(" 1s"),
        json!(1),
        json!(null),
        json!({"seconds":1,"nanos":0}),
    ] {
        assert_safe_error(
            &error_body(json!([retry_info(delay)])),
            &[],
            429,
            (
                &safe_suffix(
                    "RESOURCE_EXHAUSTED",
                    "unclassified",
                    "unclassified",
                    "absent",
                    "invalid",
                    "none",
                ),
                None,
            ),
        )
        .await;
    }
    for header in [
        "",
        "-1",
        "+1",
        "1.0",
        "1e2",
        "86401",
        "000000",
        "7, 7",
        "Mon, 06 Nov 1994 08:49:37 GMT",
        "Sun, 31 Feb 2024 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
        "Sun, 06 Nov 1994 08:49:37 UTC",
        "Fri, 31 Dec 9999 23:59:59 GMT",
        "DO_NOT_REFLECT_native_secret",
    ] {
        assert_safe_error(
            &error_body(json!([])),
            &[("retry-after", header)],
            429,
            (
                &safe_suffix(
                    "RESOURCE_EXHAUSTED",
                    "unclassified",
                    "unclassified",
                    "absent",
                    "invalid",
                    "none",
                ),
                None,
            ),
        )
        .await;
    }
    for header in ["0", "86400"] {
        assert_safe_error(
            &error_body(json!([])),
            &[("retry-after", header)],
            429,
            (
                &safe_suffix(
                    "RESOURCE_EXHAUSTED",
                    "unclassified",
                    "unclassified",
                    "absent",
                    "valid",
                    header,
                ),
                Some(header),
            ),
        )
        .await;
    }
}

#[tokio::test]
async fn antigravity_safe_errors_chunked_overflow_trickle_and_read_failure_are_bounded() {
    for streaming in [false, true] {
        for mode in [
            "trickle",
            "overflow",
            "known_oversize",
            "disconnect",
            "utf8",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url =
                url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                if mode == "known_oversize" {
                    socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 8193\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                }
                match mode {
                    "overflow" => {
                        let body = format!("2001\r\n{}\r\n", "x".repeat(8193));
                        let _ = socket.write_all(body.as_bytes()).await;
                    }
                    "trickle" => {
                        for _ in 0..40 {
                            if socket.write_all(b"1\r\n \r\n").await.is_err() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                    "disconnect" => {
                        return;
                    }
                    "utf8" => {
                        let _ = socket.write_all(b"1\r\n\xff\r\n0\r\n\r\n").await;
                    }
                    _ => {}
                }
                let mut byte = [0u8; 1];
                assert!(
                    matches!(
                        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await,
                        Ok(Ok(0)) | Ok(Err(_))
                    ),
                    "error collector retained upstream socket: {mode}"
                );
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "unexpected second native request"
                );
            });
            let directory = tempfile::tempdir().unwrap();
            let proxy = proxy(provider(directory.path(), &url).await, None).await;
            let started = tokio::time::Instant::now();
            let response = client()
                .post(proxy.url.join("/v1/messages").unwrap())
                .json(&request(streaming))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 429);
            let value: Value = response.json().await.unwrap();
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap()
                    .ends_with(&safe_suffix(
                        "unclassified",
                        "unclassified",
                        "unclassified",
                        "unclassified",
                        "invalid",
                        "none"
                    ))
            );
            assert!(started.elapsed() < Duration::from_millis(1800));
            if mode == "trickle" {
                assert!(started.elapsed() >= Duration::from_millis(800));
            }
            task.await.unwrap();
        }
    }
}

#[tokio::test]
async fn antigravity_safe_errors_downstream_disconnect_cancels_body_read() {
    for streaming in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            ready_tx.send(()).unwrap();
            let mut byte = [0u8; 1];
            assert!(
                matches!(
                    tokio::time::timeout(Duration::from_millis(700), socket.read(&mut byte)).await,
                    Ok(Ok(0)) | Ok(Err(_))
                ),
                "body read must cancel before its one-second timeout"
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let proxy = proxy(provider(directory.path(), &url).await, None).await;
        let mut downstream =
            tokio::net::TcpStream::connect(("127.0.0.1", proxy.url.port().unwrap()))
                .await
                .unwrap();
        let body = request(streaming).to_string();
        downstream.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        ready_rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(downstream);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn antigravity_safe_errors_reuse_remaining_header_budget() {
    for remaining in [Duration::ZERO, Duration::from_millis(100)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            let mut byte = [0u8; 1];
            assert!(matches!(
                tokio::time::timeout(Duration::from_millis(700), socket.read(&mut byte)).await,
                Ok(Ok(0)) | Ok(Err(_))
            ));
        });
        let upstream = client()
            .post(format!("http://{address}/"))
            .body("x")
            .send()
            .await
            .unwrap();
        let start = tokio::time::Instant::now();
        let response = super::antigravity::transport::http_error(upstream, start + remaining).await;
        assert_eq!(response.status(), 503);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "must not grant a fresh second after header budget"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("retry_hint=invalid; retry_s=none")
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn antigravity_safe_errors_verbose_log_redaction() {
    const CHILD: &str = "CCP_SAFE_ERROR_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(crate::config::log_verbose());
        assert!(crate::config::log_stderr());
        assert_safe_error(
            &error_body(json!([error_info(
                "DO_NOT_REFLECT_native_secret",
                "DO_NOT_REFLECT_native_secret"
            )])),
            &[("retry-after", "DO_NOT_REFLECT_native_secret")],
            429,
            (
                &safe_suffix(
                    "RESOURCE_EXHAUSTED",
                    "unclassified",
                    "unclassified",
                    "absent",
                    "invalid",
                    "none",
                ),
                None,
            ),
        )
        .await;
        return;
    }
    // Isolate logger configuration from parallel tests. RUST_LOG does not configure this logger.
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "providers::antigravity_http_tests::antigravity_safe_errors_verbose_log_redaction",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("CCP_LOG_VERBOSE", "1")
        .env("CCP_LOG_STDERR", "1")
        .env("CCP_CONFIG_DIR", directory.path().join("config"))
        .env("XDG_STATE_HOME", directory.path().join("state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 passed"),
        "child must actually execute the test: {stdout}"
    );
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("DO_NOT_REFLECT_native_secret"));
    }
    let logs = std::fs::read_to_string(directory.path().join("state/claude-code-proxy/proxy.log"))
        .unwrap();
    assert!(!logs.is_empty());
    assert!(!logs.contains("DO_NOT_REFLECT_native_secret"));
    let captures: Vec<_> =
        std::fs::read_dir(directory.path().join("state/claude-code-proxy/errors"))
            .unwrap()
            .collect();
    assert!(!captures.is_empty());
    for entry in captures {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains("DO_NOT_REFLECT_native_secret"));
    }
}

#[tokio::test]
async fn antigravity_count_estimate_is_signature_size_invariant_and_network_free() {
    let (upstream, captured) = native(vec![(200, successful(json!([{"text":"unused"}])))]).await;
    let directory = tempfile::tempdir().unwrap();
    let proxy = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let binding = replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "gemini-4-argon-low".into(),
    };
    let mut estimates = Vec::new();
    for signature in ["x".to_string(), "x".repeat(100000)] {
        let contents =
            json!([{"role":"model","parts":[{"text":"same text","thoughtSignature":signature}]}]);
        let blocks = replay::encode(&contents, &binding, "123", "omitted", &json!([])).unwrap();
        let mut body = request(false);
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":blocks}),
            json!({"role":"user","content":"next"}),
        ]);
        let response = client()
            .post(proxy.url.join("/v1/messages/count_tokens").unwrap())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        estimates.push(response.json::<Value>().await.unwrap()["input_tokens"].clone());
    }
    assert_eq!(estimates[0], estimates[1]);
    assert!(captured.lock().unwrap().is_empty());
}
