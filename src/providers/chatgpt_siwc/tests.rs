use super::*;
use super::{
    auth::{Attempt, Endpoints, ISSUER, RESOURCE, now},
    store::Store,
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::any,
};
use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

#[derive(Clone)]
struct Recorded {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}
struct MockData {
    base: String,
    requests: Vec<Recorded>,
    tokens: VecDeque<(StatusCode, Value)>,
    responses: VecDeque<(StatusCode, String)>,
    models: Value,
    jwks: Value,
    revoke: StatusCode,
    redirect: Option<String>,
    slow_stream: bool,
}
struct Mock {
    data: Arc<Mutex<MockData>>,
    endpoints: Endpoints,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let endpoints = endpoints(&base);
        let data = Arc::new(Mutex::new(MockData {
            base,
            requests: vec![],
            tokens: VecDeque::new(),
            responses: VecDeque::new(),
            models: json!({"models":[{"slug":"gpt-test","display_name":"Test GPT","visibility":"list"},{"slug":"hidden","visibility":"hide"}]}),
            jwks: serde_json::from_str(include_str!("../../../tests/fixtures/siwc/jwks.json"))
                .unwrap(),
            revoke: StatusCode::OK,
            redirect: None,
            slow_stream: false,
        }));
        let app = Router::new()
            .fallback(any(mock_handler))
            .with_state(data.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            data,
            endpoints,
            task,
        }
    }
    fn token(&self, status: StatusCode, body: Value) {
        self.data.lock().unwrap().tokens.push_back((status, body));
    }
    fn reply(&self, status: StatusCode, body: String) {
        self.data
            .lock()
            .unwrap()
            .responses
            .push_back((status, body));
    }
    fn count(&self, path: &str) -> usize {
        self.data
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.path == path)
            .count()
    }
}
fn endpoints(base: &str) -> Endpoints {
    Endpoints {
        authorize: format!("{base}/authorize"),
        token: format!("{base}/token"),
        discovery: format!("{base}/discovery"),
        jwks: format!("{base}/jwks"),
        revoke: format!("{base}/revoke"),
        api: format!("{base}/v1"),
    }
}
async fn mock_handler(
    State(data): State<Arc<Mutex<MockData>>>,
    req: Request<Body>,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    let mut data = data.lock().unwrap();
    data.requests.push(Recorded {
        path: path.clone(),
        headers,
        body,
    });
    match path.as_str() {
        "/discovery" => Json(json!({"issuer":ISSUER,"jwks_uri":format!("{}/jwks",data.base),"token_endpoint":format!("{}/token",data.base),"revocation_endpoint":format!("{}/revoke",data.base)})).into_response(),
        "/jwks" => Json(data.jwks.clone()).into_response(),
        "/token" => { let (status, body) = data.tokens.pop_front().expect("unexpected token request"); (status, Json(body)).into_response() },
        "/revoke" => data.revoke.into_response(),
        "/v1/models" => Json(data.models.clone()).into_response(),
        "/v1/responses" => {
            if let Some(url) = &data.redirect { return (StatusCode::TEMPORARY_REDIRECT, [("location",url.clone())]).into_response(); }
            let (status, body) = data.responses.pop_front().expect("unexpected inference request");
            if data.slow_stream && status.is_success() {
                let cut = body.rfind("data:").unwrap_or(0);
                let chunks = [body[..cut].to_string(), body[cut..].to_string()];
                let stream = futures_util::stream::unfold((0, chunks), |(index, chunks)| async move {
                    if index == 2 { return None; }
                    if index == 1 { tokio::time::sleep(std::time::Duration::from_millis(800)).await; }
                    Some((Ok::<_,std::io::Error>(bytes::Bytes::from(chunks[index].clone())), (index+1,chunks)))
                });
                return (status, [("content-type","text/event-stream"),("x-request-id","req_siwc_mock")], Body::from_stream(stream)).into_response();
            }
            (status, [("content-type",if status.is_success() {"text/event-stream"} else {"application/json"}), ("x-request-id","req_siwc_mock")], body).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
fn claims(nonce: &str) -> Value {
    json!({"iss":ISSUER,"sub":"test-user","aud":"oaiapp_test","exp":now()+3600,"iat":now(),"nonce":nonce,"email":"test@example.invalid"})
}
fn sign(claims: Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("siwc-test-key".into());
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_der(include_bytes!("../../../tests/fixtures/siwc/test-rsa.der")),
    )
    .unwrap()
}
fn token(nonce: &str) -> Value {
    json!({"access_token":"mock-access","refresh_token":"mock-refresh","id_token":sign(claims(nonce)),"token_type":"Bearer","expires_in":3600,"scope":"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"})
}
async fn pending(auth: &Auth) -> Attempt {
    let locked = auth.store.lock().await.unwrap();
    Attempt::new(
        "http://127.0.0.1:1455/auth/callback".into(),
        locked.state.ext_agent_host_id.clone(),
        locked.state.profiles.get(&auth.profile).cloned(),
    )
}
fn callback(attempt: &Attempt) -> HashMap<String, String> {
    HashMap::from([
        ("state".into(), attempt.state.clone()),
        ("code".into(), "mock-code".into()),
        ("client_id".into(), "oaiapp_test".into()),
    ])
}
async fn signed_in(mock: &Mock, store: Store) -> Auth {
    let auth = Auth::for_test(store, mock.endpoints.clone());
    let mut attempt = pending(&auth).await;
    let code = attempt.callback(&callback(&attempt)).unwrap();
    mock.token(StatusCode::OK, token(&attempt.nonce));
    assert!(auth.exchange(&attempt, &code).await.unwrap());
    auth
}
async fn expire(auth: &Auth) {
    let mut locked = auth.store.lock().await.unwrap();
    locked
        .state
        .profiles
        .get_mut(&auth.profile)
        .unwrap()
        .tokens
        .as_mut()
        .unwrap()
        .expires_at = 0;
    locked.save().unwrap();
}
fn request(stream: bool) -> MessagesRequest {
    serde_json::from_value(json!({"model":"gpt-test","stream":stream,"max_tokens":100,"messages":[{"role":"user","content":"hello"}]})).unwrap()
}
fn event(v: Value) -> String {
    format!("data: {v}\n\n")
}
fn text_events() -> String {
    [json!({"type":"response.created","response":{"id":"resp_test"}}), json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"item_text","role":"assistant","content":[]}}), json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"你好"}), json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"item_text","content":[{"type":"output_text","text":"你好"}]}}), json!({"type":"response.completed","response":{"id":"resp_test","status":"completed","output":[],"usage":{"input_tokens":12,"output_tokens":3}}})].into_iter().map(event).collect()
}
fn tool_events() -> String {
    [json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"Bash","arguments":""}}), json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"command\":\"pwd\"}"}), json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","arguments":"{\"command\":\"pwd\"}"}), json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"Bash","arguments":"{\"command\":\"pwd\"}"}}), json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":5}}})].into_iter().map(event).collect()
}
async fn body(response: axum::response::Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
fn context() -> RequestContext {
    RequestContext {
        req_id: "test".into(),
        session_id: None,
        session_seq: None,
        provider: "siwc".into(),
        traffic: None,
        monitor: None,
    }
}
fn provider(auth: Auth) -> SiwcProvider {
    SiwcProvider {
        client: Client::new(auth),
        default_model: Some("gpt-test".into()),
    }
}

#[tokio::test]
async fn dynamic_registration_pkce_and_persistent_host() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = Auth::for_test(Store::new(dir.path().join("siwc")), mock.endpoints.clone());
    let attempt = pending(&auth).await;
    let other = pending(&auth).await;
    assert_ne!(attempt.state, other.state);
    assert_ne!(attempt.nonce, other.nonce);
    assert_ne!(attempt.verifier, other.verifier);
    assert_eq!(attempt.host_id, other.host_id);
    let url = attempt.authorize_url(&mock.endpoints, false, true).unwrap();
    let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["resource"], RESOURCE);
    assert_eq!(q["agent_name_hint"], "claude-code-proxy");
    assert_eq!(q["redirect_uri"], attempt.redirect_uri);
    use sha2::Digest;
    assert_eq!(
        q["code_challenge"],
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(attempt.verifier.as_bytes()))
    );
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("chatgpt.tokens.use.direct"));
}

#[tokio::test]
async fn state_client_id_and_cancel_validation() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = Auth::for_test(Store::new(dir.path().join("siwc")), mock.endpoints.clone());
    let mut a = pending(&auth).await;
    let mut q = callback(&a);
    q.insert("state".into(), "wrong".into());
    assert!(a.callback(&q).is_err());
    let mut q = callback(&a);
    q.insert("error".into(), "access_denied".into());
    assert!(a.callback(&q).is_err());
    let mut q = callback(&a);
    q.remove("client_id");
    assert!(a.callback(&q).is_err());
    let mut q = callback(&a);
    q.insert("client_id".into(), "dynamic_agent_client".into());
    assert!(a.callback(&q).is_err());
    assert_eq!(mock.count("/token"), 0);
}

#[tokio::test]
async fn callback_http_rejects_unrelated_requests_and_wrong_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(auth::wait_callback(listener, "expected".into()));
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("http://{addr}/other?state=expected"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .get(format!("http://{addr}/auth/callback?state=wrong&code=bad"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let result = client
        .get(format!(
            "http://{addr}/auth/callback?state=expected&code=good&client_id=oaiapp_test"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    assert_eq!(result.headers()["referrer-policy"], "no-referrer");
    assert_eq!(task.await.unwrap().unwrap()["code"], "good");
}

#[tokio::test]
async fn validates_signature_nonce_issuer_audience_expiry_and_subject() {
    for variant in [
        "nonce",
        "issuer",
        "audience",
        "expired",
        "future_iat",
        "empty_sub",
        "signature",
        "unknown_key",
        "azp",
    ] {
        let mock = Mock::new().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::for_test(Store::new(dir.path().join("siwc")), mock.endpoints.clone());
        let mut a = pending(&auth).await;
        let code = a.callback(&callback(&a)).unwrap();
        let mut c = claims(&a.nonce);
        match variant {
            "nonce" => c["nonce"] = json!("wrong"),
            "issuer" => c["iss"] = json!("https://evil.invalid"),
            "audience" => c["aud"] = json!("another-client"),
            "expired" => {
                c["exp"] = json!(now() - 100);
                c["iat"] = json!(now() - 200)
            }
            "future_iat" => c["iat"] = json!(now() + 1000),
            "empty_sub" => c["sub"] = json!(""),
            "azp" => c["azp"] = json!("other"),
            _ => {}
        }
        let mut t = token(&a.nonce);
        t["id_token"] = json!(sign(c));
        if variant == "signature" {
            let s = t["id_token"].as_str().unwrap();
            let mut bytes = s.as_bytes().to_vec();
            let start = s.rfind('.').unwrap() + 1;
            bytes[start] = if bytes[start] == b'A' { b'B' } else { b'A' };
            t["id_token"] = json!(String::from_utf8(bytes).unwrap());
        }
        if variant == "unknown_key" {
            mock.data.lock().unwrap().jwks["keys"][0]["kid"] = json!("other-key");
        }
        mock.token(StatusCode::OK, t);
        assert!(auth.exchange(&a, &code).await.is_err(), "{variant}");
        assert!(auth.store.lock().await.unwrap().state.profiles.is_empty());
    }
}

#[tokio::test]
async fn successful_registration_and_returning_identity_isolation() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let mut a = pending(&auth).await;
    let q: HashMap<_, _> = a
        .authorize_url(&mock.endpoints, false, true)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(q["client_id"], "oaiapp_test");
    assert!(!q.contains_key("agent_name_hint"));
    assert!(q.contains_key("id_token_hint"));
    let mut wrong = callback(&a);
    wrong.insert("client_id".into(), "oaiapp_other".into());
    assert!(a.callback(&wrong).is_err());
    let mut valid = callback(&a);
    valid.remove("client_id");
    let code = a.callback(&valid).unwrap();
    let mut t = token(&a.nonce);
    let mut c = claims(&a.nonce);
    c["sub"] = json!("different-user");
    t["id_token"] = json!(sign(c));
    mock.token(StatusCode::OK, t);
    assert!(auth.exchange(&a, &code).await.is_err());
    assert_eq!(auth.access_token().await.unwrap(), "mock-access");
    let status = auth.status().await.unwrap().to_string();
    assert!(!status.contains("mock-access"));
    assert!(!status.contains("mock-refresh"));
    let requests = &mock.data.lock().unwrap().requests;
    let form: HashMap<_, _> =
        url::form_urlencoded::parse(&requests.iter().find(|r| r.path == "/token").unwrap().body)
            .into_owned()
            .collect();
    assert_eq!(form["client_id"], "oaiapp_test");
    assert_eq!(form["resource"], RESOURCE);
    assert!(!form.contains_key("client_secret"));
}

#[tokio::test]
async fn missing_scope_saves_identity_but_blocks_inference() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = Auth::for_test(Store::new(dir.path().join("siwc")), mock.endpoints.clone());
    let mut a = pending(&auth).await;
    let code = a.callback(&callback(&a)).unwrap();
    let mut t = token(&a.nonce);
    t["scope"] = json!("openid profile email");
    mock.token(StatusCode::OK, t);
    assert!(!auth.exchange(&a, &code).await.unwrap());
    assert!(auth.access_token().await.is_err());
    assert_eq!(mock.count("/v1/responses"), 0);
    assert_eq!(
        auth.status().await.unwrap()["profiles"][0]["plan_permission"],
        false
    );
}

#[tokio::test]
async fn concurrent_refresh_rotates_once_and_restart_reads_new_tokens() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("siwc"));
    let auth = signed_in(&mock, store.clone()).await;
    expire(&auth).await;
    let mut t = token("unused");
    t.as_object_mut().unwrap().remove("id_token");
    t.as_object_mut().unwrap().remove("scope");
    t["access_token"] = json!("rotated-access");
    t["refresh_token"] = json!("rotated-refresh");
    mock.token(StatusCode::OK, t);
    let other = Auth::for_test(store.clone(), mock.endpoints.clone());
    let (a, b) = tokio::join!(auth.access_token(), other.access_token());
    assert_eq!(a.unwrap(), "rotated-access");
    assert_eq!(b.unwrap(), "rotated-access");
    assert_eq!(mock.count("/token"), 2);
    let restarted = Auth::for_test(store, mock.endpoints.clone());
    assert_eq!(restarted.access_token().await.unwrap(), "rotated-access");
    let locked = restarted.store.lock().await.unwrap();
    assert_eq!(
        locked.state.profiles["default"]
            .tokens
            .as_ref()
            .unwrap()
            .refresh_token,
        "rotated-refresh"
    );
}

#[tokio::test]
async fn refresh_terminal_errors_clear_tokens_and_keep_registration() {
    for code in [
        "invalid_grant",
        "invalid_refresh_token",
        "token_expired",
        "refresh_token_expired",
        "refresh_token_invalidated",
        "refresh_token_reused",
    ] {
        let mock = Mock::new().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
        expire(&auth).await;
        mock.token(StatusCode::BAD_REQUEST, json!({"error":code}));
        assert!(auth.access_token().await.is_err());
        let locked = auth.store.lock().await.unwrap();
        let p = &locked.state.profiles["default"];
        assert!(p.tokens.is_none());
        assert_eq!(p.client_id, "oaiapp_test");
    }
}

#[tokio::test]
async fn uncertain_refresh_is_not_reused_and_temporary_error_retains_credentials() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    expire(&auth).await;
    mock.token(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":"temporarily_unavailable"}),
    );
    assert!(auth.access_token().await.is_err());
    {
        let mut locked = auth.store.lock().await.unwrap();
        let p = locked.state.profiles.get_mut("default").unwrap();
        assert!(p.tokens.is_some());
        assert!(!p.refresh_pending);
        p.refresh_pending = true;
        locked.save().unwrap();
    }
    assert!(
        auth.access_token()
            .await
            .unwrap_err()
            .to_string()
            .contains("uncertain")
    );
    assert_eq!(mock.count("/token"), 2);
}

#[tokio::test]
async fn logout_revokes_and_preserves_host_and_registration() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let before = pending(&auth).await.host_id;
    assert!(auth.logout().await.unwrap());
    assert!(auth.access_token().await.is_err());
    let after = pending(&auth).await;
    assert_eq!(before, after.host_id);
    assert_eq!(after.selected.unwrap().client_id, "oaiapp_test");
    let data = mock.data.lock().unwrap();
    let r = data.requests.iter().find(|r| r.path == "/revoke").unwrap();
    let form: HashMap<_, _> = url::form_urlencoded::parse(&r.body).into_owned().collect();
    assert_eq!(form["token_type_hint"], "refresh_token");
    assert_eq!(form["client_id"], "oaiapp_test");
}

#[tokio::test]
async fn failed_revocation_still_clears_local_tokens() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    mock.data.lock().unwrap().revoke = StatusCode::SERVICE_UNAVAILABLE;
    assert!(!auth.logout().await.unwrap());
    assert_eq!(mock.count("/revoke"), 3);
    assert!(auth.access_token().await.is_err());
}

#[tokio::test]
async fn discovery_and_outbound_requests_use_only_allowed_headers_and_paths() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let client = Client::new(auth);
    let models = client.models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["display_name"], "Test GPT");
    mock.reply(StatusCode::OK, text_events());
    let p = provider(client.auth.clone());
    let result = p.handle_messages(request(false), context()).await;
    assert_eq!(result.status(), 200);
    assert!(body(result).await.contains("你好"));
    for r in &mock.data.lock().unwrap().requests {
        assert!(!r.path.contains("backend-api"));
        if r.path.starts_with("/v1/") {
            assert_eq!(r.headers["authorization"], "Bearer mock-access");
            assert!(!r.headers.contains_key("originator"));
            assert!(!r.headers.contains_key("chatgpt-account-id"));
            assert!(!r.headers.contains_key("openai-beta"));
            assert!(
                r.headers["user-agent"]
                    .to_str()
                    .unwrap()
                    .starts_with("claude-code-proxy/")
            );
        }
    }
}

#[test]
fn request_whitelist_and_additional_tools_preserve_full_history() {
    let mut req = request(true);
    req.extra.insert("temperature".into(), json!(0.9));
    req.extra
        .insert("metadata".into(), json!({"user_id":"secret"}));
    req.extra.insert("top_p".into(), json!(0.4));
    req.extra.insert("system".into(), json!("developer rules"));
    req.extra.insert("tools".into(),json!([{"name":"Bash","description":"run local command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}]));
    req.messages.extend(serde_json::from_value::<Vec<_>>(json!([{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"pwd"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"/test"}]},{"role":"developer","content":"more rules"}])).unwrap());
    let prepared = translate::prepare(&req, "gpt-test".into()).unwrap();
    let wire = prepared.body;
    assert_eq!(wire["store"], false);
    assert_eq!(wire["stream"], true);
    assert_eq!(wire["input"][0]["type"], "additional_tools");
    assert_eq!(wire["input"][0]["role"], "developer");
    for field in [
        "max_tokens",
        "max_output_tokens",
        "temperature",
        "top_p",
        "metadata",
        "client_metadata",
        "previous_response_id",
        "service_tier",
        "tools",
    ] {
        assert!(wire.get(field).is_none(), "{field}");
    }
    assert_eq!(wire["instructions"], "developer rules");
    assert_eq!(wire["input"][2]["call_id"], wire["input"][3]["call_id"]);
    assert_eq!(wire["input"][4]["role"], "developer");
    assert_eq!(
        wire,
        translate::prepare(&req, "gpt-test".into()).unwrap().body
    );
}

#[test]
fn rejects_forbidden_parameters_hosted_tools_and_broken_tool_history() {
    for field in [
        "max_output_tokens",
        "background",
        "conversation",
        "previous_response_id",
        "client_metadata",
        "tool_search",
        "service_tier",
    ] {
        let mut req = request(true);
        if field == "tool_search" {
            req.extra.insert(
                "tools".into(),
                json!([{"type":"tool_search","name":"tool_search"}]),
            );
        } else {
            req.extra.insert(field.into(), json!("x"));
        }
        assert!(
            translate::prepare(&req, "gpt-test".into()).is_err(),
            "{field}"
        );
    }
    for kind in [
        "mcp",
        "image_generation",
        "file_search",
        "code_interpreter",
        "computer_use",
        "programmatic_tool_calling",
    ] {
        let mut req = request(true);
        req.extra.insert(
            "tools".into(),
            json!([{"type":kind,"name":"hosted","input_schema":{}}]),
        );
        assert!(translate::prepare(&req, "gpt-test".into()).is_err());
    }
    for messages in [
        json!([{"role":"user","content":[{"type":"tool_result","tool_use_id":"missing","content":"x"}]}]),
        json!([{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Bash","input":{}}]}]),
        json!([{"role":"user","content":[{"type":"document","source":{}}]}]),
    ] {
        let mut req = request(true);
        req.messages = serde_json::from_value(messages).unwrap();
        assert!(translate::prepare(&req, "gpt-test".into()).is_err());
    }
}

#[tokio::test]
async fn streaming_text_and_tool_calls_produce_valid_anthropic_events() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth);
    mock.reply(StatusCode::OK, text_events());
    let text = body(p.handle_messages(request(true), context()).await).await;
    assert!(text.contains("你好"));
    assert!(text.contains("message_stop"));
    mock.reply(StatusCode::OK, tool_events());
    let mut req = request(true);
    req.extra.insert(
        "tools".into(),
        json!([{"name":"Bash","input_schema":{"type":"object"}}]),
    );
    let text = body(p.handle_messages(req, context()).await).await;
    assert!(text.contains("call_1"));
    assert!(text.contains("tool_use"));
    assert!(text.contains("message_stop"));
    assert!(!text.contains("siwc_stream_error"), "{text}");
}

#[tokio::test]
async fn http_errors_preserve_status_shape_codes_and_request_id_without_retrying_limits() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let client = Client::new(auth);
    for (status, error) in [
        (401, json!({"detail":"identity refused"})),
        (
            403,
            json!({"error":{"code":"subscription_sharing_user_not_eligible","message":"not eligible"}}),
        ),
        (
            404,
            json!({"error":{"code":"model_not_found","message":"model unavailable","param":"model"}}),
        ),
        (
            429,
            json!({"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"app limit"}}),
        ),
    ] {
        mock.reply(StatusCode::from_u16(status).unwrap(), error.to_string());
        let e = client.responses(&json!({})).await.unwrap_err();
        assert_eq!(e.status.as_u16(), status);
        assert_eq!(e.body, error);
        assert_eq!(e.request_id.as_deref(), Some("req_siwc_mock"));
    }
    assert_eq!(mock.count("/v1/responses"), 4);
    assert_eq!(mock.count("/token"), 1);
}

#[tokio::test]
async fn unavailable_route_retries_at_most_three_times_and_redirects_are_blocked() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let client = Client::new(auth);
    for _ in 0..3 {
        mock.reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"detail":"route unavailable"}).to_string(),
        );
    }
    assert_eq!(client.responses(&json!({})).await.unwrap_err().status, 503);
    assert_eq!(mock.count("/v1/responses"), 3);
    let other = Mock::new().await;
    mock.data.lock().unwrap().redirect =
        Some(format!("{}/v1/responses", other.data.lock().unwrap().base));
    assert_eq!(client.responses(&json!({})).await.unwrap_err().status, 307);
    assert_eq!(other.count("/v1/responses"), 0);
}

#[tokio::test]
async fn failed_incomplete_and_disconnected_streams_never_emit_success() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth);
    for ending in ["", "data: [DONE]\n\n", "data: broken\n\n"] {
        mock.reply(
            StatusCode::OK,
            format!(
                "{}{}",
                event(
                    json!({"type":"response.output_text.delta","output_index":0,"delta":"partial"})
                ),
                ending
            ),
        );
        let text = body(p.handle_messages(request(true), context()).await).await;
        assert!(text.contains("event: error"));
        assert!(!text.contains("message_stop"));
    }
    for kind in ["response.failed", "response.incomplete"] {
        *p.client.usage_pause.lock().unwrap() = None;
        mock.reply(StatusCode::OK,event(json!({"type":kind,"response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit reached"}}})));
        let text = body(p.handle_messages(request(true), context()).await).await;
        assert!(text.contains("subscription_sharing_usage_limit_exceeded"));
        assert!(!text.contains("message_stop"));
    }
    *p.client.usage_pause.lock().unwrap() = None;
    mock.reply(StatusCode::OK, String::new());
    assert_eq!(
        p.handle_messages(request(false), context()).await.status(),
        502
    );
}

#[tokio::test]
async fn output_tool_identity_mismatch_is_an_error() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth);
    let events = tool_events().replacen("\"item_id\":\"fc_1\"", "\"item_id\":\"wrong\"", 1);
    mock.reply(StatusCode::OK, events);
    let mut req = request(true);
    req.extra.insert(
        "tools".into(),
        json!([{"name":"Bash","input_schema":{"type":"object"}}]),
    );
    let result = body(p.handle_messages(req, context()).await).await;
    assert!(result.contains("siwc_stream_error"));
    assert!(!result.contains("message_stop"));
}

#[test]
fn sse_decoder_handles_utf8_fragmentation_crlf_and_multiline_data() {
    let bytes="event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"你好\"}\r\n\r\n".as_bytes();
    let mut decoder = stream::Decoder::default();
    for b in bytes {
        decoder.push(&[*b]).unwrap();
    }
    assert_eq!(decoder.events.pop_front().unwrap()["delta"], "你好");
}

#[tokio::test]
async fn isolated_router_discovers_models_disables_native_routes_and_replays_after_restart() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("siwc"));
    let auth = signed_in(&mock, store.clone()).await;
    let make_app = |auth| {
        let registry = crate::Registry::from_providers(
            crate::config::AliasProvider::Siwc,
            [Arc::new(provider(auth)) as Arc<dyn Provider>],
        );
        crate::server::app_with_features(
            Arc::new(registry),
            None,
            crate::server::AppFeatures {
                responses_api: true,
                images_api: true,
                transcriptions_api: true,
            },
        )
    };
    let app = make_app(auth);
    let result = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    assert!(body(result).await.contains("Test GPT"));
    for path in [
        "/v1/responses",
        "/v1/chat/completions",
        "/v1/images/generations",
        "/v1/audio/transcriptions",
    ] {
        let result = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.status(), 404);
    }
    let mut req = request(false);
    req.messages.extend(serde_json::from_value::<Vec<_>>(json!([{"role":"assistant","content":"earlier answer"},{"role":"user","content":"continue resumed conversation"}])).unwrap());
    for app in [app, make_app(Auth::for_test(store, mock.endpoints.clone()))] {
        mock.reply(StatusCode::OK, text_events());
        let result = app
            .oneshot(
                Request::builder()
                    .uri("/v1/messages")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.status(), 200);
        assert!(body(result).await.contains("你好"));
    }
    let data = mock.data.lock().unwrap();
    let bodies: Vec<Value> = data
        .requests
        .iter()
        .filter(|r| r.path == "/v1/responses")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[0]["input"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn isolated_router_rejects_browser_origin_and_dns_rebinding() {
    let registry = crate::Registry::new(crate::config::AliasProvider::Siwc);
    assert_eq!(registry.list_provider_names(), vec!["siwc"]);
    assert_eq!(
        registry
            .provider_for_model("gpt-future", None)
            .unwrap()
            .name(),
        "siwc"
    );
    let app = crate::server::app(Arc::new(registry));
    for (key, value) in [
        ("origin", "https://evil.invalid"),
        ("host", "evil.invalid:18765"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header(key, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn credentials_are_private_atomic_and_reject_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("siwc");
    let store = Store::new(root.clone());
    let locked = store.lock().await.unwrap();
    drop(locked);
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(root.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let target = dir.path().join("target");
    std::fs::write(&target, "sensitive").unwrap();
    std::fs::remove_file(root.join("state.json")).unwrap();
    symlink(&target, root.join("state.json")).unwrap();
    assert!(store.lock().await.is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "sensitive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_is_serialized_across_os_processes() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("siwc");
    let auth = signed_in(&mock, Store::new(root.clone())).await;
    expire(&auth).await;
    let mut t = token("unused");
    t.as_object_mut().unwrap().remove("id_token");
    t["access_token"] = json!("process-rotated");
    t["refresh_token"] = json!("process-refresh");
    mock.token(StatusCode::OK, t);
    let mut children = vec![];
    for _ in 0..2 {
        children.push(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "providers::chatgpt_siwc::tests::refresh_child",
                    "--nocapture",
                ])
                .env("SIWC_TEST_CHILD_ROOT", &root)
                .env("SIWC_TEST_CHILD_URL", &mock.data.lock().unwrap().base)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(mock.count("/token"), 2); // One login plus one rotation across both processes.
}

#[tokio::test]
async fn refresh_child() {
    let Ok(root) = std::env::var("SIWC_TEST_CHILD_ROOT") else {
        return;
    };
    let base = std::env::var("SIWC_TEST_CHILD_URL").unwrap();
    assert!(base.starts_with("http://127.0.0.1:"));
    let auth = Auth::for_test(Store::new(root.into()), endpoints(&base));
    assert_eq!(auth.access_token().await.unwrap(), "process-rotated");
}

#[tokio::test]
async fn profiles_with_the_same_email_keep_distinct_client_ids_and_tokens() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let first = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let mut second = first.clone();
    second.profile = "second".into();
    let mut a = pending(&second).await;
    let mut q = callback(&a);
    q.insert("client_id".into(), "oaiapp_second".into());
    let code = a.callback(&q).unwrap();
    let mut t = token(&a.nonce);
    let mut c = claims(&a.nonce);
    c["aud"] = json!("oaiapp_second");
    t["id_token"] = json!(sign(c));
    t["access_token"] = json!("second-access");
    mock.token(StatusCode::OK, t);
    second.exchange(&a, &code).await.unwrap();
    assert_eq!(first.access_token().await.unwrap(), "mock-access");
    assert_eq!(second.access_token().await.unwrap(), "second-access");
    assert!(second.logout().await.unwrap());
    assert_eq!(first.access_token().await.unwrap(), "mock-access");
}

#[tokio::test]
async fn quota_failure_pauses_new_inference_and_logout_stops_streams() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth.clone());
    mock.reply(StatusCode::OK,event(json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"app limit"}}})));
    let text = body(p.handle_messages(request(true), context()).await).await;
    assert!(text.contains("rate_limit_error"));
    assert_eq!(
        p.handle_messages(request(true), context()).await.status(),
        429
    );
    assert_eq!(mock.count("/v1/responses"), 1);
    let p = provider(auth.clone());
    mock.reply(StatusCode::OK, text_events());
    let response = p.handle_messages(request(true), context()).await;
    auth.logout().await.unwrap();
    let text = body(response).await;
    assert!(text.contains("signed out"));
    assert!(!text.contains("message_stop"));
}

#[tokio::test]
async fn refresh_scope_reduction_blocks_requests_and_earliest_refresh_is_respected() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    {
        let mut locked = auth.store.lock().await.unwrap();
        let t = locked
            .state
            .profiles
            .get_mut("default")
            .unwrap()
            .tokens
            .as_mut()
            .unwrap();
        t.expires_at = now() + 30;
        t.earliest_refresh_at = Some(now() + 20);
        locked.save().unwrap();
    }
    assert_eq!(auth.access_token().await.unwrap(), "mock-access");
    assert_eq!(mock.count("/token"), 1);
    {
        let mut locked = auth.store.lock().await.unwrap();
        let t = locked
            .state
            .profiles
            .get_mut("default")
            .unwrap()
            .tokens
            .as_mut()
            .unwrap();
        t.expires_at = 0;
        t.earliest_refresh_at = None;
        locked.save().unwrap();
    }
    let mut t = token("unused");
    t.as_object_mut().unwrap().remove("id_token");
    t["scope"] = json!("openid profile email");
    mock.token(StatusCode::OK, t);
    assert!(auth.access_token().await.is_err());
    assert!(auth.access_token().await.is_err());
    assert_eq!(mock.count("/token"), 2);
}

#[test]
fn production_endpoints_are_fixed_official_urls() {
    let e = Endpoints::default();
    assert_eq!(e.api, "https://api.openai.com/v1");
    assert_eq!(
        e.authorize,
        "https://auth.openai.com/api/accounts/authorize"
    );
    assert_eq!(e.token, "https://auth.openai.com/api/accounts/oauth/token");
    let registry = crate::Registry::new(crate::config::AliasProvider::Siwc);
    for model in [
        "gpt-test",
        "siwc/gpt-test",
        "claude-sonnet-4-6",
        "unknown-model",
    ] {
        assert_eq!(
            registry.provider_for_model(model, None).unwrap().name(),
            "siwc"
        );
    }
    assert!(registry.provider("codex").is_none());
}

#[test]
fn local_tools_errors_compaction_and_reasoning_history_are_handled_explicitly() {
    let mut req = request(false);
    req.extra.insert(
        "system".into(),
        json!("You are a helpful AI assistant tasked with summarizing conversations"),
    );
    req.extra.insert("tools".into(),json!([{"name":"Read","input_schema":{"type":"object"}},{"name":"Write","input_schema":{"type":"object"}},{"name":"mcp__local__lookup","input_schema":{"type":"object"},"defer_loading":true}]));
    req.messages=serde_json::from_value(json!([{"role":"assistant","content":[{"type":"thinking","thinking":"private","signature":"opaque-old-provider-signature"},{"type":"tool_use","id":"id_1","name":"Read","input":{"file_path":"test.txt"}},{"type":"tool_use","id":"id_2","name":"Write","input":{"file_path":"out.txt","content":"ok"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"id_1","is_error":true,"content":"file not found"},{"type":"tool_result","tool_use_id":"id_2","content":"written"}]}])).unwrap();
    let result = translate::prepare(&req, "gpt-test".into()).unwrap();
    assert_eq!(
        result.body["input"][0]["tools"].as_array().unwrap().len(),
        3
    );
    let serialized = result.body.to_string();
    assert!(serialized.contains("[tool execution error]"));
    assert!(!serialized.contains("opaque-old-provider-signature"));
    assert!(!serialized.contains("compaction_trigger"));
    assert!(result.estimated_tokens > 0);
}

#[tokio::test]
async fn live_output_arrives_before_upstream_terminal_event() {
    use http_body_util::BodyExt;
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth);
    mock.data.lock().unwrap().slow_stream = true;
    mock.reply(StatusCode::OK, text_events());
    let response = p.handle_messages(request(true), context()).await;
    let mut response = response.into_body();
    let partial = tokio::time::timeout(std::time::Duration::from_millis(400), async {
        let mut text = String::new();
        loop {
            let frame = response.frame().await.unwrap().unwrap();
            if let Some(bytes) = frame.data_ref() {
                text.push_str(&String::from_utf8_lossy(bytes));
            }
            if text.contains("你好") {
                break text;
            }
        }
    })
    .await
    .expect("text should not wait for response.completed");
    assert!(!partial.contains("message_stop"));
    let rest = response.collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&rest).contains("message_stop"));
}

#[tokio::test]
async fn top_level_error_event_preserves_code_param_and_request_id() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth);
    mock.reply(StatusCode::OK,event(json!({"type":"error","code":"subscription_sharing_unsupported_capability","param":"tools","message":"unsupported"})));
    let response = p.handle_messages(request(false), context()).await;
    assert_eq!(response.status(), 400);
    let error: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(error["error"]["param"], "tools");
    assert_eq!(error["upstream_request_id"], "req_siwc_mock");
}

#[tokio::test]
async fn stalled_read_never_finishes_before_a_late_usage_error() {
    for stream in [true, false] {
        let mock = Mock::new().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
        let p = provider(auth);
        let mut req = request(stream);
        req.extra.insert(
            "tools".into(),
            json!([{"name":"Read","input_schema":{"type":"object"}}]),
        );
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_read","call_id":"call_read","name":"Read","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_read","delta":format!("{{\"file_path\":\"test.txt\",\"pages\":\"\"{}", " ".repeat(1024))}),
            json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"plan limit"}}}),
        ].into_iter().map(event).collect();
        mock.reply(StatusCode::OK, events);
        let response = p.handle_messages(req.clone(), context()).await;
        assert_eq!(response.status(), if stream { 200 } else { 429 });
        let text = body(response).await;
        assert!(text.contains("subscription_sharing_usage_limit_exceeded"));
        assert!(!text.contains("message_stop"));
        assert!(!text.contains("input_json_delta"));
        assert_eq!(p.handle_messages(req, context()).await.status(), 429);
        assert_eq!(mock.count("/v1/responses"), 1);
    }
}

#[tokio::test]
async fn terminal_tool_call_cannot_change_completed_arguments() {
    let mock = Mock::new().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = signed_in(&mock, Store::new(dir.path().join("siwc"))).await;
    let p = provider(auth);
    for stream in [true, false] {
        let mut req = request(stream);
        req.extra.insert(
            "tools".into(),
            json!([{"name":"Bash","input_schema":{"type":"object"}}]),
        );
        let mut events = tool_events();
        events.truncate(events.rfind("data:").unwrap());
        events.push_str(&event(json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"Bash","arguments":"{\"command\":\"different\"}"}]}})));
        mock.reply(StatusCode::OK, events);
        let response = p.handle_messages(req, context()).await;
        assert_eq!(response.status(), if stream { 200 } else { 502 });
        let text = body(response).await;
        assert!(text.contains("changed tool call"));
        assert!(!text.contains("message_stop"));
    }
}
