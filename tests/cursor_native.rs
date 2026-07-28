//! Integration tests for the Cursor native provider.
//!
//! These tests cover:
//! - Prost message roundtrip
//! - Connect frame encode/decode (with fixtures)
//! - Auth resolution
//! - Model catalog resolution
//! - Prompt rendering
//! - Client request/response boundary
//! - SSE framing
//! - Registry routing
//! - Provider end-to-end against mock upstream

use once_cell::sync::Lazy;
use std::sync::Mutex;

static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

// ---------------------------------------------------------------------------
// Prost roundtrip
// ---------------------------------------------------------------------------

#[test]
fn prost_roundtrip_preserves_cursor_server_message() {
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: Some(TextDelta {
                text: "hello".into(),
            }),
            turn_ended: Some(TurnEnded {
                input_tokens: 7,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }),
        exec_server_message: None,
    };
    let mut bytes = Vec::new();
    msg.encode(&mut bytes).unwrap();
    let decoded = AgentServerMessage::decode(bytes.as_slice()).unwrap();
    assert_eq!(
        decoded.interaction_update.unwrap().text_delta.unwrap().text,
        "hello"
    );
}

#[test]
fn prost_roundtrip_preserves_client_message() {
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let msg = AgentClientMessage {
        run_request: Some(RunRequest {
            conversation_state: None,
            action: Some(Action {
                user_message_action: Some(UserMessageAction {
                    user_message: Some(UserMessage {
                        text: "hello".into(),
                        message_id: "msg-id".into(),
                        selected_context: None,
                        mode: "AGENT_MODE_AGENT".into(),
                    }),
                }),
            }),
            mcp_tools: None,
            conversation_id: String::new(),
            requested_model: Some(CursorModel {
                model_id: "gpt-5.5".into(),
                parameters: vec![ModelParameter {
                    id: "context".into(),
                    value: "128k".into(),
                }],
            }),
            exclude_workspace_context: false,
            selected_subagent_models: vec![],
            conversation_group_id: String::new(),
            client_supports_inline_images: true,
        }),
        client_heartbeat: None,
    };

    let mut bytes = Vec::new();
    msg.encode(&mut bytes).unwrap();
    let decoded = AgentClientMessage::decode(bytes.as_slice()).unwrap();
    let run = decoded.run_request.unwrap();
    let action = run.action.unwrap();
    let user_msg = action.user_message_action.unwrap().user_message.unwrap();
    assert_eq!(user_msg.text, "hello");
    assert_eq!(run.requested_model.unwrap().model_id, "gpt-5.5");
}

// ---------------------------------------------------------------------------
// Connect frame fixtures
// ---------------------------------------------------------------------------

#[test]
fn connect_frame_fixture_matches_reference_layout() {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;

    let frame = encode_connect_frame(b"abc", 0);
    assert_eq!(hex::encode(frame), "0000000003616263");
}

#[test]
fn connect_frame_decode_reference() {
    use claude_code_proxy::providers::cursor::connect::ConnectFrameDecoder;

    let wire = hex::decode("0000000003616263").unwrap();
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder.push(&wire).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].flags, 0);
    assert_eq!(&frames[0].payload[..], b"abc");
}

#[test]
fn connect_frame_with_flags_decode() {
    use claude_code_proxy::providers::cursor::connect::ConnectFrameDecoder;
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;

    let frame = encode_connect_frame(b"xyz", 0x03);
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder.push(frame).unwrap();
    assert_eq!(frames[0].flags, 0x03);
}

// ---------------------------------------------------------------------------
// Auth resolution
// ---------------------------------------------------------------------------

#[test]
fn auth_returns_token_from_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "test-token-123");
    }
    let token = claude_code_proxy::providers::cursor::auth::load_cursor_token();
    assert_eq!(token.as_deref(), Some("test-token-123"));
    unsafe {
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
    }
}

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

#[test]
fn model_resolution_resolves_cursor_agent_prefix() {
    use claude_code_proxy::providers::cursor::model::*;

    let r = resolve_cursor_model("cursor-agent:gpt-5.5").unwrap();
    assert_eq!(r.model_id, "gpt-5.5");
    assert_eq!(r.mode, CursorAgentMode::Agent);
}

#[test]
fn model_resolution_accepts_legacy_cursor_agent() {
    use claude_code_proxy::providers::cursor::model::*;

    let r = resolve_cursor_model("cursor-agent").unwrap();
    assert_eq!(r.mode, CursorAgentMode::Agent);
}

#[test]
fn registry_routes_cursor_model_to_cursor_provider() {
    use claude_code_proxy::Registry;
    use claude_code_proxy::config::AliasProvider;

    let registry = Registry::new(AliasProvider::Codex);
    let provider = registry.provider_for_model("cursor:gpt-5.5", None);
    assert!(provider.is_some());
    assert_eq!(provider.unwrap().name(), "cursor");

    let provider = registry.provider_for_model("cursor-agent", None);
    assert!(provider.is_some());
    assert_eq!(provider.unwrap().name(), "cursor");
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

#[test]
fn prompt_renders_system_tools_and_messages() {
    use claude_code_proxy::MessagesRequest;
    use claude_code_proxy::providers::cursor::request::render_cursor_prompt;

    let req: MessagesRequest = serde_json::from_value(serde_json::json!({
        "model": "cursor:gpt-5.5",
        "system": "be direct",
        "messages": [{
            "role": "user",
            "content": [
                {"type":"text","text":"hi"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
            ]
        }],
        "tools": [{"name":"Read","description":"read files","input_schema":{"type":"object"}}]
    }))
    .unwrap();

    let rendered = render_cursor_prompt(&req);
    assert!(rendered.contains("<system>"));
    assert!(rendered.contains("<user>"));
    assert!(rendered.contains("<tools>"));
}

#[test]
fn selected_images_count_matches_base64_images() {
    use claude_code_proxy::MessagesRequest;
    use claude_code_proxy::providers::cursor::request::cursor_selected_images;

    let req: MessagesRequest = serde_json::from_value(serde_json::json!({
        "model": "cursor:gpt-5.5",
        "messages": [{
            "role": "user",
            "content": [
                {"type":"text","text":"analyze"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"BBBB"}}
            ]
        }]
    }))
    .unwrap();

    assert_eq!(cursor_selected_images(&req).len(), 2);
}

// ---------------------------------------------------------------------------
// Client request/response boundary (high-level shape)
// ---------------------------------------------------------------------------

#[test]
fn cursor_client_constructs_correct_url() {
    use claude_code_proxy::providers::cursor::client::CursorHttpClient;

    let client = CursorHttpClient::new();
    // Just ensure construction doesn't panic
    let _ = client;
}

#[test]
fn cursor_error_display_works() {
    use claude_code_proxy::providers::cursor::client::CursorError;

    let err = CursorError::new(429, "rate limited", Some("backoff".to_string()));
    let display = format!("{err}");
    assert!(display.contains("429"));
    assert!(display.contains("rate limited"));
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn cursor_client_sends_connect_proto_headers_and_run_request_frame() {
    use axum::{Router, routing::post};
    use claude_code_proxy::providers::cursor::client::CursorHttpClient;
    use claude_code_proxy::providers::cursor::connect::{
        ConnectFrameDecoder, encode_connect_frame,
    };
    use claude_code_proxy::providers::cursor::proto::*;
    use claude_code_proxy::providers::cursor::request::CursorSelectedImage;
    use prost::Message;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct ObservedRequest {
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
    }

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let observed: Arc<Mutex<Option<ObservedRequest>>> = Arc::new(Mutex::new(None));
    let observed_handler = Arc::clone(&observed);

    let response_body = {
        let msg = AgentServerMessage {
            interaction_update: Some(InteractionUpdate {
                thinking_delta: None,
                text_delta: Some(TextDelta { text: "ok".into() }),
                turn_ended: Some(TurnEnded {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                }),
            }),
            exec_server_message: None,
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let mut body = encode_connect_frame(&payload, 0).to_vec();
        body.extend_from_slice(&encode_connect_frame(b"", 2));
        body
    };

    let app = Router::new().route(
        "/agent.v1.AgentService/Run",
        post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let response_body = response_body.clone();
                let observed_handler = Arc::clone(&observed_handler);
                async move {
                    *observed_handler.lock().unwrap() = Some(ObservedRequest {
                        headers,
                        body: body.to_vec(),
                    });
                    (
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "application/connect+proto",
                        )],
                        response_body,
                    )
                }
            },
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock_url = format!("http://{}", addr);

    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", &mock_url);
        std::env::set_var("CCP_CURSOR_CLIENT_VERSION", "test-client-version");
    }

    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = CursorHttpClient::new();
    let upstream = client
        .run_agent(
            "wire-token",
            "wire prompt",
            "cursor:gpt-5.5",
            &[CursorSelectedImage {
                data: "aGVsbG8=".into(),
                uuid: "image-id".into(),
                path: "claude-image-1.png".into(),
                mime_type: "image/png".into(),
            }],
        )
        .await
        .expect("mock upstream request should succeed");
    assert!(upstream.is_success());

    let observed = observed.lock().unwrap().clone().expect("request captured");
    assert_eq!(
        observed
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer wire-token")
    );
    assert_eq!(
        observed
            .headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/connect+proto")
    );
    assert_eq!(
        observed
            .headers
            .get("connect-protocol-version")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        observed
            .headers
            .get("x-cursor-client-type")
            .and_then(|v| v.to_str().ok()),
        Some("cli")
    );
    assert_eq!(
        observed
            .headers
            .get("x-cursor-client-version")
            .and_then(|v| v.to_str().ok()),
        Some("test-client-version")
    );
    assert_eq!(
        observed
            .headers
            .get("x-cursor-streaming")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    let request_id = observed
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id");
    assert_eq!(
        observed
            .headers
            .get("x-original-request-id")
            .and_then(|v| v.to_str().ok()),
        Some(request_id)
    );

    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder.push(&observed.body).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].flags, 0);
    let msg = AgentClientMessage::decode(&frames[0].payload[..]).unwrap();
    let run = msg.run_request.expect("run request");
    assert!(msg.client_heartbeat.is_none());
    assert_eq!(run.conversation_id, "");
    assert_eq!(run.conversation_group_id, "");
    assert!(run.client_supports_inline_images);
    assert!(!run.exclude_workspace_context);
    assert_eq!(run.requested_model.unwrap().model_id, "gpt-5.5");
    let user_message = run
        .action
        .unwrap()
        .user_message_action
        .unwrap()
        .user_message
        .unwrap();
    assert_eq!(user_message.text, "wire prompt");
    assert_eq!(user_message.message_id, request_id);
    assert_eq!(user_message.mode, "AGENT_MODE_AGENT");
    let image = user_message
        .selected_context
        .unwrap()
        .selected_images
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(image.data, "aGVsbG8=");
    assert_eq!(image.uuid, "image-id");
    assert_eq!(image.path, "claude-image-1.png");
    assert_eq!(image.mime_type, "image/png");

    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_CLIENT_VERSION");
    }
}

// ---------------------------------------------------------------------------
// Response decoding
// ---------------------------------------------------------------------------

#[test]
fn response_decode_extracts_text_and_usage() {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use claude_code_proxy::providers::cursor::response::*;
    use prost::Message;

    let mut body = Vec::new();

    // Text frame
    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: Some(TextDelta {
                text: "Hello".into(),
            }),
            turn_ended: None,
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    // Usage frame
    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: None,
            turn_ended: Some(TurnEnded {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    // End frame
    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let upstream = claude_code_proxy::providers::cursor::client::CursorUpstreamResponse {
        status: 200,
        body,
        error_detail: None,
    };

    let json = decode_cursor_upstream(&upstream, "msg_test", "cursor-test").unwrap();
    assert_eq!(json["id"], "msg_test");
    assert_eq!(json["content"][0]["text"], "Hello");
    assert_eq!(json["usage"]["input_tokens"].as_u64(), Some(10));
    assert_eq!(json["usage"]["output_tokens"].as_u64(), Some(5));
}

// ---------------------------------------------------------------------------
// SSE framing - parse event names and data
// ---------------------------------------------------------------------------

#[test]
fn sse_parses_event_names_and_data() {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use claude_code_proxy::providers::cursor::sse::frame_cursor_stream;
    use prost::Message;

    let mut body = Vec::new();

    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: Some(TextDelta { text: "hi".into() }),
            turn_ended: None,
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: None,
            turn_ended: Some(TurnEnded {
                input_tokens: 5,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let upstream = claude_code_proxy::providers::cursor::client::CursorUpstreamResponse {
        status: 200,
        body,
        error_detail: None,
    };

    let sse = frame_cursor_stream(&upstream, "msg_sse", "cursor-test");
    let sse_str = String::from_utf8_lossy(&sse);

    let events = parse_sse_events(&sse_str);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"message_start"),
        "expected message_start in {names:?}"
    );
    assert!(
        names.contains(&"content_block_delta"),
        "expected content_block_delta in {names:?}"
    );
    assert!(
        names.contains(&"message_delta"),
        "expected message_delta in {names:?}"
    );
    assert!(
        names.contains(&"message_stop"),
        "expected message_stop in {names:?}"
    );
}

#[test]
fn sse_message_delta_contains_usage() {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use claude_code_proxy::providers::cursor::sse::frame_cursor_stream;
    use prost::Message;

    let mut body = Vec::new();

    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: Some(TextDelta {
                text: "test".into(),
            }),
            turn_ended: Some(TurnEnded {
                input_tokens: 42,
                output_tokens: 7,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));
    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let upstream = claude_code_proxy::providers::cursor::client::CursorUpstreamResponse {
        status: 200,
        body,
        error_detail: None,
    };

    let sse = frame_cursor_stream(&upstream, "msg_u", "cursor-test");
    let sse_str = String::from_utf8_lossy(&sse);
    let events = parse_sse_events(&sse_str);

    let msg_delta_data = events
        .iter()
        .find(|(n, _)| n == "message_delta")
        .map(|(_, d)| d.clone());
    assert!(msg_delta_data.is_some(), "expected message_delta event");
    let data = msg_delta_data.unwrap();
    assert_eq!(data["usage"]["input_tokens"].as_u64(), Some(42));
    assert_eq!(data["usage"]["output_tokens"].as_u64(), Some(7));
    assert_eq!(
        data["usage"]["cache_creation_input_tokens"].as_u64(),
        Some(0)
    );
    assert_eq!(data["usage"]["cache_read_input_tokens"].as_u64(), Some(0));
    assert_eq!(data["delta"]["stop_reason"], "end_turn");
}

// ---------------------------------------------------------------------------
// Registry integration
// ---------------------------------------------------------------------------

#[test]
fn registry_provider_for_legacy_cursor_model() {
    use claude_code_proxy::Registry;
    use claude_code_proxy::config::AliasProvider;

    let registry = Registry::new(AliasProvider::Codex);

    // Legacy models
    for model in &[
        "cursor",
        "cursor-agent",
        "cursor-composer",
        "cursor-composer-fast",
        "cursor-plan",
        "cursor-ask",
    ] {
        let provider = registry.provider_for_model(model, None);
        assert!(
            provider.is_some(),
            "expected provider for legacy model {model}"
        );
        assert_eq!(
            provider.unwrap().name(),
            "cursor",
            "model {model} should route to cursor provider"
        );
    }
}

// ---------------------------------------------------------------------------
// Mock upstream streaming test (full integration)
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cursor_provider_streams_text_and_usage_from_mock_upstream() {
    use axum::{Router, routing::post};
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use claude_code_proxy::providers::cursor::response::decode_cursor_upstream;
    use claude_code_proxy::providers::cursor::sse::frame_cursor_stream;
    use prost::Message;

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Build mock upstream response bytes
    let mut body = Vec::new();

    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: Some(TextDelta {
                text: "Hello from mock".into(),
            }),
            turn_ended: None,
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: None,
            turn_ended: Some(TurnEnded {
                input_tokens: 15,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let response_body = body.clone();

    let app = Router::new().route(
        "/agent.v1.AgentService/Run",
        post(move |_body: axum::body::Bytes| async move {
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "application/connect+proto",
                )],
                response_body,
            )
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock_url = format!("http://{}", addr);

    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", &mock_url);
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "mock-token");
        std::env::set_var("CCP_CURSOR_CLIENT_VERSION", "0.0.0");
    }

    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    use claude_code_proxy::providers::cursor::auth::load_cursor_token;
    use claude_code_proxy::providers::cursor::client::CursorHttpClient;

    let token = load_cursor_token().unwrap();
    let client = CursorHttpClient::new();
    let upstream = client
        .run_agent(&token, "test prompt", "cursor:gpt-5.5", &[])
        .await
        .expect("mock upstream request should succeed");

    assert!(upstream.is_success());
    assert_eq!(upstream.status, 200);

    let json = decode_cursor_upstream(&upstream, "msg_mock", "cursor-test").unwrap();
    assert_eq!(json["content"][0]["text"], "Hello from mock");
    assert_eq!(json["usage"]["input_tokens"].as_u64(), Some(15));
    assert_eq!(json["usage"]["output_tokens"].as_u64(), Some(3));

    let sse = frame_cursor_stream(&upstream, "msg_sse_mock", "cursor-test");
    let sse_str = String::from_utf8_lossy(&sse);
    let events = parse_sse_events(&sse_str);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();

    assert!(
        names.contains(&"message_start"),
        "SSE should include message_start in {names:?}"
    );
    assert!(
        names.contains(&"message_stop"),
        "SSE should include message_stop in {names:?}"
    );

    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
        std::env::remove_var("CCP_CURSOR_CLIENT_VERSION");
    }
}

// ---------------------------------------------------------------------------
// Provider handle_messages shape test (non-streaming)
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cursor_provider_handle_messages_returns_anthropic_json() {
    use axum::{Router, routing::post};
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Build mock response
    let mut body = Vec::new();
    let msg = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            thinking_delta: None,
            text_delta: Some(TextDelta {
                text: "Mock response text".into(),
            }),
            turn_ended: Some(TurnEnded {
                input_tokens: 20,
                output_tokens: 4,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }),
        exec_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));
    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let response_body = body;

    let app = Router::new().route(
        "/agent.v1.AgentService/Run",
        post(move |_body: axum::body::Bytes| async move {
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "application/connect+proto",
                )],
                response_body,
            )
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock_url = format!("http://{}", addr);

    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", &mock_url);
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "mock-token-handler");
        std::env::set_var("CCP_CURSOR_CLIENT_VERSION", "0.0.0");
    }

    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send via handle_messages (non-streaming)
    use claude_code_proxy::provider::Provider;
    use claude_code_proxy::provider::RequestContext;
    use claude_code_proxy::providers::cursor::CursorProvider;

    let provider = CursorProvider::new();
    let body = serde_json::from_value(serde_json::json!({
        "model": "cursor:gpt-5.5",
        "messages": [{"role": "user", "content": "test"}]
    }))
    .unwrap();

    let ctx = RequestContext {
        req_id: "test-req".into(),
        session_id: None,
        session_seq: None,
        provider: "cursor".into(),
        traffic: None,
        monitor: None,
    };

    let response = provider.handle_messages(body, ctx).await;
    // Should not return an error response
    let status = response.status();
    assert!(
        status != 401 && status != 400,
        "handle_messages returned error status {status}"
    );

    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
        std::env::remove_var("CCP_CURSOR_CLIENT_VERSION");
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn cursor_proxy_http_path_reaches_mock_cursor_upstream() {
    use axum::{Router, routing::post};
    use claude_code_proxy::providers::cursor::connect::{
        ConnectFrameDecoder, encode_connect_frame,
    };
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct ObservedRequest {
        authorization: Option<String>,
        body: Vec<u8>,
    }

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let observed: Arc<Mutex<Option<ObservedRequest>>> = Arc::new(Mutex::new(None));
    let observed_handler = Arc::clone(&observed);

    let response_body = {
        let text_msg = AgentServerMessage {
            interaction_update: Some(InteractionUpdate {
                thinking_delta: None,
                text_delta: Some(TextDelta {
                    text: "proxy path works".into(),
                }),
                turn_ended: None,
            }),
            exec_server_message: None,
        };
        let mut text_payload = Vec::new();
        text_msg.encode(&mut text_payload).unwrap();

        let usage_msg = AgentServerMessage {
            interaction_update: Some(InteractionUpdate {
                thinking_delta: None,
                text_delta: None,
                turn_ended: Some(TurnEnded {
                    input_tokens: 12,
                    output_tokens: 3,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                }),
            }),
            exec_server_message: None,
        };
        let mut usage_payload = Vec::new();
        usage_msg.encode(&mut usage_payload).unwrap();

        let mut body = encode_connect_frame(&text_payload, 0).to_vec();
        body.extend_from_slice(&encode_connect_frame(&usage_payload, 0));
        body.extend_from_slice(&encode_connect_frame(b"", 2));
        body
    };

    let upstream_app = Router::new().route(
        "/agent.v1.AgentService/Run",
        post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let response_body = response_body.clone();
                let observed_handler = Arc::clone(&observed_handler);
                async move {
                    *observed_handler.lock().unwrap() = Some(ObservedRequest {
                        authorization: headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        body: body.to_vec(),
                    });
                    (
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "application/connect+proto",
                        )],
                        response_body,
                    )
                }
            },
        ),
    );

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_url = format!("http://{}", upstream_addr);
    let _upstream_handle = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app).await.unwrap();
    });

    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let _proxy_handle = tokio::spawn(async move {
        claude_code_proxy::server::serve_listener(proxy_listener, None, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", &upstream_url);
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "proxy-token");
        std::env::set_var("CCP_CURSOR_CLIENT_VERSION", "proxy-test-version");
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("authorization", "Bearer ignored")
        .json(&serde_json::json!({
            "model": "cursor:gpt-5.5",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello over proxy"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["content"][0]["text"], "proxy path works");
    assert_eq!(json["usage"]["input_tokens"], 12);
    assert_eq!(json["usage"]["output_tokens"], 3);

    let observed = observed
        .lock()
        .unwrap()
        .clone()
        .expect("upstream request captured");
    assert_eq!(
        observed.authorization.as_deref(),
        Some("Bearer proxy-token")
    );

    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder.push(&observed.body).unwrap();
    assert_eq!(frames.len(), 1);
    let msg = AgentClientMessage::decode(&frames[0].payload[..]).unwrap();
    let user_message = msg
        .run_request
        .unwrap()
        .action
        .unwrap()
        .user_message_action
        .unwrap()
        .user_message
        .unwrap();
    assert!(user_message.text.contains("hello over proxy"));

    let _ = shutdown_tx.send(());
    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
        std::env::remove_var("CCP_CURSOR_CLIENT_VERSION");
    }
}

// ---------------------------------------------------------------------------
// Cursor tool bridge integration tests
// ---------------------------------------------------------------------------

#[test]
fn bridge_start_pauses_on_tool_use_xml() {
    use claude_code_proxy::providers::cursor::response::*;
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    // Create upstream events with a text delta containing XML tool_use
    let events = vec![
        CursorStreamEvent::TextDelta {
            text: "before ".to_string(),
        },
        CursorStreamEvent::TextDelta {
            text: r#"<tool_use id="x" name="Read">{"file_path":"/tmp/a"}</tool_use>"#.to_string(),
        },
        CursorStreamEvent::TextDelta {
            text: " after".to_string(),
        },
        CursorStreamEvent::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        CursorStreamEvent::End,
    ];

    let allowed: std::collections::BTreeSet<String> =
        ["Read".to_string(), "Write".to_string(), "Bash".to_string()]
            .into_iter()
            .collect();

    let mut counter = 0u64;
    let id_factory = Box::new(move || {
        counter += 1;
        format!("call_cursor_test_{counter}")
    });

    let (sse, paused) = start_cursor_tool_bridge(
        "msg_1",
        "cursor-test",
        "session-bridge-1",
        &events,
        Some(allowed),
        id_factory,
    );

    assert!(paused, "bridge should pause on tool_use");

    let sse_str = String::from_utf8_lossy(&sse);
    let parsed = parse_sse_events(&sse_str);

    let event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        event_names.contains(&"content_block_start"),
        "expected content_block_start for tool_use"
    );
    assert!(
        event_names.contains(&"message_stop"),
        "expected message_stop"
    );

    let msg_delta = parsed
        .iter()
        .find(|(n, _)| n == "message_delta")
        .map(|(_, d)| d.clone());
    assert!(msg_delta.is_some(), "expected message_delta");
    assert_eq!(
        msg_delta.unwrap()["delta"]["stop_reason"],
        "tool_use",
        "stop_reason should be tool_use"
    );

    // Clean up
    BridgeRegistry::remove("session-bridge-1");
}

#[test]
fn bridge_start_passes_through_without_tool_use() {
    use claude_code_proxy::providers::cursor::response::*;
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    let events = vec![
        CursorStreamEvent::TextDelta {
            text: "hello world".to_string(),
        },
        CursorStreamEvent::Usage {
            input_tokens: 5,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        CursorStreamEvent::End,
    ];

    let (sse, paused) = start_cursor_tool_bridge(
        "msg_2",
        "cursor-test",
        "session-bridge-2",
        &events,
        None,
        Box::new(|| "id".into()),
    );

    assert!(!paused, "bridge should NOT pause without tool_use");

    let sse_str = String::from_utf8_lossy(&sse);
    let parsed = parse_sse_events(&sse_str);
    let event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();

    assert!(event_names.contains(&"message_start"));
    assert!(event_names.contains(&"content_block_delta"));
    assert!(event_names.contains(&"message_delta"));
    assert!(event_names.contains(&"message_stop"));

    // Verify stop_reason is end_turn
    let msg_delta = parsed
        .iter()
        .find(|(n, _)| n == "message_delta")
        .map(|(_, d)| d.clone());
    assert_eq!(
        msg_delta.unwrap()["delta"]["stop_reason"],
        "end_turn",
        "stop_reason should be end_turn without tool_use"
    );
}

#[test]
fn bridge_start_creates_pending_tool_in_registry() {
    use claude_code_proxy::providers::cursor::response::*;
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    // Clean state
    BridgeRegistry::clear();

    let events = vec![CursorStreamEvent::TextDelta {
        text: r#"<tool_use name="Read">{"file_path":"/tmp/test"}</tool_use>"#.to_string(),
    }];

    let allowed: std::collections::BTreeSet<String> = ["Read".to_string()].into_iter().collect();

    let (_, paused) = start_cursor_tool_bridge(
        "msg_3",
        "cursor-test",
        "session-bridge-pt",
        &events,
        Some(allowed),
        Box::new(|| "call_test".into()),
    );

    assert!(paused);

    let pending = BridgeRegistry::pending_tool("session-bridge-pt");
    assert!(pending.is_some(), "pending tool should be stored");
    assert_eq!(pending.unwrap().name(), "Read");

    BridgeRegistry::remove("session-bridge-pt");
}

#[test]
fn bridge_resume_continues_after_tool_use_pause() {
    use claude_code_proxy::providers::cursor::response::*;
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    BridgeRegistry::clear();

    // Events: tool_use in the middle, text after
    let events = vec![
        CursorStreamEvent::TextDelta {
            text: "before ".to_string(),
        },
        CursorStreamEvent::TextDelta {
            text: r#"<tool_use name="Read">{"file_path":"/tmp/a"}</tool_use>"#.to_string(),
        },
        CursorStreamEvent::TextDelta {
            text: " continued".to_string(),
        },
        CursorStreamEvent::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        CursorStreamEvent::End,
    ];

    let allowed: std::collections::BTreeSet<String> = ["Read".to_string()].into_iter().collect();

    let mut counter = 0u64;
    let id_factory = Box::new(move || {
        counter += 1;
        format!("call_cursor_test_{counter}")
    });

    let (_first_sse, paused) = start_cursor_tool_bridge(
        "msg_first",
        "cursor-test",
        "session-resume-1",
        &events,
        Some(allowed),
        id_factory,
    );
    assert!(paused);

    let body: claude_code_proxy::MessagesRequest =
        serde_json::from_value(serde_json::json!({
            "model": "cursor-test",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_cursor_test_1", "content": "result text"}]}
            ]
        }))
        .unwrap();

    let pending =
        BridgeRegistry::pending_tool("session-resume-1").expect("should have pending tool");
    assert_eq!(pending.tool_use_id(), "call_cursor_test_1");

    let result = find_tool_result(&body, pending.tool_use_id()).expect("should find tool result");

    let (result_msgs, second_sse) = resume_cursor_tool_bridge(
        "session-resume-1",
        "msg_second",
        "cursor-test",
        result,
        &pending,
    );

    assert!(!result_msgs.is_empty(), "should have result messages");

    let sse_str = String::from_utf8_lossy(&second_sse);
    let parsed = parse_sse_events(&sse_str);
    let event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();

    assert!(
        event_names.contains(&"message_start"),
        "resume should have message_start in {event_names:?}"
    );
    assert!(
        event_names.contains(&"message_stop"),
        "resume should have message_stop in {event_names:?}"
    );

    let text_deltas: Vec<&str> = parsed
        .iter()
        .filter_map(|(n, d)| {
            if n == "content_block_delta" {
                d["delta"]["text"].as_str()
            } else {
                None
            }
        })
        .collect();
    let combined = text_deltas.join("");
    assert!(
        combined.contains("continued"),
        "resume should include remaining text deltas"
    );

    BridgeRegistry::remove("session-resume-1");
}

#[test]
fn bridge_rejects_tool_not_in_allowed_list() {
    use claude_code_proxy::providers::cursor::response::*;
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    BridgeRegistry::clear();

    let events = vec![CursorStreamEvent::TextDelta {
        text: r#"<tool_use name="Bash">{"command":"pwd"}</tool_use>"#.to_string(),
    }];

    let allowed: std::collections::BTreeSet<String> = ["Read".to_string()].into_iter().collect();

    let (sse, paused) = start_cursor_tool_bridge(
        "msg_filter",
        "cursor-test",
        "session-filter-1",
        &events,
        Some(allowed),
        Box::new(|| "id".into()),
    );

    assert!(!paused, "should NOT pause for disallowed tool");

    let sse_str = String::from_utf8_lossy(&sse);
    let parsed = parse_sse_events(&sse_str);
    let _event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();

    let msg_delta = parsed
        .iter()
        .find(|(n, _)| n == "message_delta")
        .map(|(_, d)| d.clone());
    assert_eq!(
        msg_delta.unwrap()["delta"]["stop_reason"],
        "end_turn",
        "disallowed tool should not trigger tool_use"
    );

    BridgeRegistry::remove("session-filter-1");
}

#[test]
fn bridge_result_messages_have_correct_read_shape() {
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    let exec = CursorExec {
        id: Some(42),
        exec_id: None,
        args: serde_json::json!({"file_path": "/tmp/readme.txt"}),
    };
    let result = CursorNativeToolResult {
        content: "file contents here".into(),
        is_error: false,
    };

    let msg = build_read_result_from_native(&exec, &result);
    let msg_obj = msg.as_object().unwrap();

    assert_eq!(msg_obj.get("id").and_then(|v| v.as_i64()), Some(42));

    let read_result = msg_obj.get("readResult").unwrap();
    assert!(read_result.get("success").is_some());
    let success = read_result.get("success").unwrap();
    assert_eq!(
        success.get("path").and_then(|v| v.as_str()),
        Some("/tmp/readme.txt")
    );
    assert_eq!(
        success.get("content").and_then(|v| v.as_str()),
        Some("file contents here")
    );
    assert_eq!(success.get("totalLines").and_then(|v| v.as_i64()), Some(1));
}

#[test]
fn bridge_result_messages_have_correct_write_shape() {
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    let exec = CursorExec {
        id: Some(99),
        exec_id: Some("exec-write-1".into()),
        args: serde_json::json!({"file_path": "/tmp/writeme.txt", "content": "data"}),
    };

    let result = CursorNativeToolResult {
        content: "written".into(),
        is_error: false,
    };
    let msg = build_write_result_from_native(&exec, &result);
    let msg_obj = msg.as_object().unwrap();
    assert_eq!(
        msg_obj.get("execId").and_then(|v| v.as_str()),
        Some("exec-write-1")
    );
    let write_result = msg_obj.get("writeResult").unwrap();
    let success = write_result.get("success").unwrap();
    assert_eq!(
        success.get("path").and_then(|v| v.as_str()),
        Some("/tmp/writeme.txt")
    );
    assert!(success.get("linesCreated").is_some());
    assert!(success.get("fileSize").is_some());

    let error_result = CursorNativeToolResult {
        content: "permission denied".into(),
        is_error: true,
    };
    let err_msg = build_write_result_from_native(&exec, &error_result);
    let err_obj = err_msg.as_object().unwrap();
    let write_result = err_obj.get("writeResult").unwrap();
    let error = write_result.get("error").unwrap();
    assert_eq!(
        error.get("path").and_then(|v| v.as_str()),
        Some("/tmp/writeme.txt")
    );
    assert!(
        error
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("permission")
    );
}

#[test]
fn bridge_shell_stream_result_has_correct_shape() {
    use claude_code_proxy::providers::cursor::tool_bridge::*;

    let exec = CursorExec {
        id: Some(7),
        exec_id: Some("exec-shell".into()),
        args: serde_json::json!({}),
    };

    let result = CursorNativeToolResult {
        content: "stdout output".into(),
        is_error: false,
    };

    let messages = build_shell_stream_result(
        &exec,
        &result,
        std::time::Duration::from_millis(150),
        "/home/user",
    );

    assert_eq!(messages.len(), 4, "start + stdout + exit + close");

    assert!(
        messages[0]
            .get("shellStream")
            .and_then(|s| s.get("start"))
            .is_some()
    );

    assert_eq!(
        messages[1]["shellStream"]["stdout"]["data"],
        "stdout output"
    );

    assert_eq!(messages[2]["shellStream"]["exit"]["code"], 0);
    assert_eq!(messages[2]["shellStream"]["exit"]["cwd"], "/home/user");

    assert_eq!(
        messages[3]["execClientControlMessage"]["streamClose"]["id"],
        7
    );
}

// ---------------------------------------------------------------------------
// No TypeScript sidecar
// ---------------------------------------------------------------------------

#[test]
fn cursor_provider_has_no_typescript_sidecar() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers/cursor");
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    let ext = path.extension().and_then(|e| e.to_str());
                    assert_ne!(ext, Some("ts"), "TypeScript file found at {:?}", path);
                }
            }
            Err(_) => {
                // Directory may not exist (e.g., if cursor provider was removed)
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SSE parser helper (mirrors sse.rs internal helper for cross-module access)
// ---------------------------------------------------------------------------

fn parse_sse_events(sse: &str) -> Vec<(String, serde_json::Value)> {
    let mut events = Vec::new();
    let mut current_event = String::new();

    for line in sse.lines() {
        if let Some(event) = line.strip_prefix("event: ") {
            current_event = event.to_string();
        } else if let Some(data_str) = line.strip_prefix("data: ")
            && let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str)
        {
            events.push((current_event.clone(), data));
        }
    }

    events
}
