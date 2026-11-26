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

use http_body_util::BodyExt;
use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};

static ENV_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
        exec_client_message: None,
        kv_client_message: None,
        exec_client_control_message: None,
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
    let _guard = ENV_LOCK.blocking_lock();
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

    let _guard = ENV_LOCK.lock().await;
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

            conversation_checkpoint_update: None,
            kv_server_message: None,
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
            move |headers: axum::http::HeaderMap, mut body: axum::body::Body| {
                let response_body = response_body.clone();
                let observed_handler = Arc::clone(&observed_handler);
                async move {
                    let mut bytes = Vec::new();
                    while let Some(Ok(frame)) = body.frame().await {
                        if let Ok(data) = frame.into_data() {
                            bytes.extend_from_slice(&data);
                        }
                        let mut decoder = ConnectFrameDecoder::new();
                        if decoder.push(&bytes).is_ok_and(|frames| frames.len() >= 2) {
                            break;
                        }
                    }
                    *observed_handler.lock().unwrap() = Some(ObservedRequest {
                        headers,
                        body: bytes,
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
    assert!(frames.len() >= 2);
    assert_eq!(frames[0].flags, 0);
    assert_eq!(frames[0].payload.first().copied(), Some(0x0a));
    assert!(
        frames[0]
            .payload
            .windows(b"wire prompt".len())
            .any(|window| window == b"wire prompt")
    );
    assert!(
        frames[0]
            .payload
            .windows(b"gpt-5.5".len())
            .any(|window| window == b"gpt-5.5")
    );
    assert!(
        frames[0]
            .payload
            .windows(b"hello".len())
            .any(|window| window == b"hello")
    );
    assert_eq!(frames[1].payload.first().copied(), Some(0x12));

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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
async fn cursor_provider_streams_text_and_usage_from_mock_upstream() {
    use axum::{Router, routing::post};
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use claude_code_proxy::providers::cursor::response::decode_cursor_upstream;
    use claude_code_proxy::providers::cursor::sse::frame_cursor_stream;
    use prost::Message;

    let _guard = ENV_LOCK.lock().await;

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
        conversation_checkpoint_update: None,
        kv_server_message: None,
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
        conversation_checkpoint_update: None,
        kv_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));

    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let response_body = body.clone();

    let app = Router::new().route(
        "/agent.v1.AgentService/Run",
        post(move |_body: axum::body::Body| async move {
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
async fn cursor_provider_handle_messages_returns_anthropic_json() {
    use axum::{Router, routing::post};
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let _guard = ENV_LOCK.lock().await;

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
        conversation_checkpoint_update: None,
        kv_server_message: None,
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    body.extend_from_slice(&encode_connect_frame(&payload, 0));
    body.extend_from_slice(&encode_connect_frame(b"", 2));

    let response_body = body;

    let app = Router::new().route(
        "/agent.v1.AgentService/Run",
        post(move |_body: axum::body::Body| async move {
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

    let _guard = ENV_LOCK.lock().await;
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

            conversation_checkpoint_update: None,
            kv_server_message: None,
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

            conversation_checkpoint_update: None,
            kv_server_message: None,
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
            move |headers: axum::http::HeaderMap, mut body: axum::body::Body| {
                let response_body = response_body.clone();
                let observed_handler = Arc::clone(&observed_handler);
                async move {
                    let mut bytes = Vec::new();
                    while let Some(Ok(frame)) = body.frame().await {
                        if let Ok(data) = frame.into_data() {
                            bytes.extend_from_slice(&data);
                        }
                        let mut decoder = ConnectFrameDecoder::new();
                        if decoder.push(&bytes).is_ok_and(|frames| frames.len() >= 2) {
                            break;
                        }
                    }
                    *observed_handler.lock().unwrap() = Some(ObservedRequest {
                        authorization: headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                        body: bytes,
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
    assert!(frames.len() >= 2);
    assert!(
        frames[0]
            .payload
            .windows(b"hello over proxy".len())
            .any(|window| window == b"hello over proxy")
    );

    let _ = shutdown_tx.send(());
    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
        std::env::remove_var("CCP_CURSOR_CLIENT_VERSION");
    }
}

// ---------------------------------------------------------------------------
// Live run: one stream across a tool round trip
// ---------------------------------------------------------------------------

/// The regression this bridge exists for: after Claude Code runs the tool, the
/// result has to travel back on the run that asked for it. A second `Run` makes
/// the agent re-plan from scratch and re-issue the same call.
#[tokio::test]
async fn live_run_answers_the_tool_result_on_the_same_stream() {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::post;
    use claude_code_proxy::providers::cursor::connect::{
        ConnectFrameDecoder, encode_connect_frame,
    };
    use claude_code_proxy::providers::cursor::proto::*;
    use futures_util::StreamExt;
    use prost::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _guard = ENV_LOCK.lock().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let replies: Arc<Mutex<Vec<ExecClientMessage>>> = Arc::new(Mutex::new(Vec::new()));

    let app = {
        let runs = Arc::clone(&runs);
        let replies = Arc::clone(&replies);
        Router::new().route(
            "/agent.v1.AgentService/Run",
            post(move |body: Body| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let replies = Arc::clone(&replies);

                tokio::spawn(async move {
                    let mut decoder = ConnectFrameDecoder::new();
                    let mut stream = body.into_data_stream();
                    let mut asked = false;
                    let mut answered = false;

                    while let Some(Ok(chunk)) = stream.next().await {
                        let frames = match decoder.push(&chunk) {
                            Ok(frames) => frames,
                            Err(_) => break,
                        };
                        if !asked && !frames.is_empty() {
                            // The run request has landed: ask for a tool.
                            asked = true;
                            let _ = tx.send(Ok(exec_frame()));
                        }
                        for frame in frames {
                            let message = match AgentClientMessage::decode(frame.payload.clone()) {
                                Ok(message) => message,
                                Err(_) => continue,
                            };
                            let Some(exec) = message.exec_client_message else {
                                continue;
                            };
                            replies.lock().unwrap().push(exec);
                            if !answered {
                                answered = true;
                                // The agent carries on with the result it got.
                                let _ = tx.send(Ok(text_frame("the tool returned grok-ok")));
                                let _ = tx.send(Ok(usage_frame(21, 7)));
                                let _ = tx.send(Ok(encode_connect_frame(b"", 2)));
                            }
                        }
                    }
                });

                let body =
                    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|item| {
                            (
                                item.map_err(|error: std::io::Error| {
                                    std::io::Error::other(error.to_string())
                                }),
                                rx,
                            )
                        })
                    }));
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/connect+proto",
                    )],
                    body,
                )
            }),
        )
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", format!("http://{addr}"));
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "proxy-token");
    }

    let provider = claude_code_proxy::providers::cursor::CursorProvider::new();
    let tools = serde_json::json!([
        {"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
        {"name": "Read", "description": "read", "input_schema": {"type": "object"}},
        {"name": "Write", "description": "write", "input_schema": {"type": "object"}}
    ]);

    // Turn 1: the agent asks for a read.
    let first = serde_json::json!({
        "model": "cursor:gpt-5.5",
        "stream": true,
        "tools": tools,
        "messages": [{"role": "user", "content": "read /etc/hostname"}]
    });
    let first_sse = collect_sse(&provider, first, "probe-loop-1").await;
    let first_events = parse_sse_events(&first_sse);
    let tool_use_id = first_events
        .iter()
        .find_map(|(name, data)| {
            (name == "content_block_start" && data["content_block"]["type"] == "tool_use").then(
                || {
                    data["content_block"]["id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                },
            )
        })
        .expect("turn 1 asks for a tool");
    assert_eq!(
        first_events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .map(|(_, data)| data["delta"]["stop_reason"].clone()),
        Some(serde_json::json!("tool_use"))
    );

    // Turn 2: Claude Code answers with the tool result.
    let second = serde_json::json!({
        "model": "cursor:gpt-5.5",
        "stream": true,
        "tools": tools,
        "messages": [
            {"role": "user", "content": "read /etc/hostname"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": tool_use_id, "name": "Read",
                 "input": {"file_path": "/etc/hostname"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_use_id, "content": "grok-ok"}
            ]}
        ]
    });
    let second_sse = collect_sse(&provider, second, "probe-loop-1").await;
    let second_events = parse_sse_events(&second_sse);

    let text: String = second_events
        .iter()
        .filter(|(name, _)| name == "content_block_delta")
        .filter_map(|(_, data)| data["delta"]["text"].as_str())
        .collect();
    assert!(
        text.contains("the tool returned grok-ok"),
        "turn 2 must carry the final answer, got {second_sse}"
    );
    assert!(
        !second_events
            .iter()
            .any(|(name, data)| name == "content_block_start"
                && data["content_block"]["type"] == "tool_use"),
        "turn 2 re-issued the tool call instead of using the result: {second_sse}"
    );

    // The whole round trip happened on ONE run, and the reply carried the exec
    // ids the agent gave us.
    assert_eq!(runs.load(Ordering::SeqCst), 1, "a second Run was started");
    let replies = replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 1, "one exec reply");
    assert_eq!(replies[0].id, 1);
    assert_eq!(replies[0].exec_id, "exec-1");
    assert!(matches!(
        replies[0].message,
        Some(exec_client_message::Message::ReadResult(_))
    ));

    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
    }
}

/// A text-only turn still ends on the upstream's own end frame.
#[tokio::test]
async fn live_run_still_answers_plain_turns() {
    use axum::Router;
    use axum::routing::post;
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _guard = ENV_LOCK.lock().await;
    let runs = Arc::new(AtomicUsize::new(0));

    let app = {
        let runs = Arc::clone(&runs);
        Router::new().route(
            "/agent.v1.AgentService/Run",
            post(move |_body: axum::body::Body| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                let mut body = text_frame("plain answer").to_vec();
                body.extend_from_slice(&usage_frame(9, 2));
                body.extend_from_slice(&encode_connect_frame(b"", 2));
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/connect+proto",
                    )],
                    body,
                )
            }),
        )
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", format!("http://{addr}"));
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "proxy-token");
    }

    let provider = claude_code_proxy::providers::cursor::CursorProvider::new();
    let sse = collect_sse(
        &provider,
        serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}],
            "messages": [{"role": "user", "content": "hi"}]
        }),
        "probe-plain-1",
    )
    .await;

    let events = parse_sse_events(&sse);
    let text: String = events
        .iter()
        .filter(|(name, _)| name == "content_block_delta")
        .filter_map(|(_, data)| data["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "plain answer");
    assert!(events.iter().any(|(name, _)| name == "message_stop"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
    }
}

// ---------------------------------------------------------------------------
// Conversation continuation
// ---------------------------------------------------------------------------

/// A new user turn continues the Cursor conversation: the run carries the last
/// checkpoint and conversation id, the prompt holds only the new message, and
/// the server reads the earlier turn back from the agent store.
#[tokio::test]
async fn next_turn_continues_the_cursor_conversation() {
    let _guard = ENV_LOCK.lock().await;
    let (addr, records) = spawn_conversation_mock("first answer").await;
    set_mock_env(addr);

    let provider = claude_code_proxy::providers::cursor::CursorProvider::new();
    let first = collect_sse(
        &provider,
        conversation_request(serde_json::json!([
            {"role": "user", "content": "first question"}
        ])),
        "continue-next-turn",
    )
    .await;
    let second = collect_sse(
        &provider,
        conversation_request(serde_json::json!([
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "second question"}
        ])),
        "continue-next-turn",
    )
    .await;

    assert!(first.contains("first answer"), "{first}");
    assert!(second.contains("second answer"), "{second}");
    let records = records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert!(records[0].conversation_state.is_empty());
    assert_eq!(records[1].conversation_state, checkpoint_bytes());
    assert_eq!(records[1].conversation_id, records[0].conversation_id);
    assert!(records[1].prompt.contains("second question"));
    assert!(!records[1].prompt.contains("first question"));
    assert_eq!(records[1].blob_reply.as_deref(), Some(&b"turn-1"[..]));
    clear_mock_env();
}

/// A tool call recovered from the text channel ends the response early; the
/// run still finishes its turn in the background, and the request carrying the
/// tool result continues the conversation with that result alone.
#[tokio::test]
async fn tool_result_after_a_text_tool_call_continues_the_conversation() {
    let _guard = ENV_LOCK.lock().await;
    let (addr, records) =
        spawn_conversation_mock(r#"<tool_use name="Glob">{"pattern":"*.rs"}</tool_use>"#).await;
    set_mock_env(addr);

    let provider = claude_code_proxy::providers::cursor::CursorProvider::new();
    let first = collect_sse(
        &provider,
        conversation_request(serde_json::json!([
            {"role": "user", "content": "list the rust files"}
        ])),
        "continue-text-tool",
    )
    .await;
    let tool_use_id = parse_sse_events(&first)
        .iter()
        .find_map(|(name, data)| {
            (name == "content_block_start" && data["content_block"]["name"] == "Glob")
                .then(|| data["content_block"]["id"].as_str().unwrap().to_string())
        })
        .expect("turn 1 asks for Glob");
    let second = collect_sse(
        &provider,
        conversation_request(serde_json::json!([
            {"role": "user", "content": "list the rust files"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": tool_use_id, "name": "Glob",
                 "input": {"pattern": "*.rs"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_use_id, "content": "main.rs"}
            ]}
        ])),
        "continue-text-tool",
    )
    .await;

    assert!(second.contains("second answer"), "{second}");
    let records = records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].conversation_state, checkpoint_bytes());
    assert_eq!(records[1].conversation_id, records[0].conversation_id);
    assert!(records[1].prompt.contains("main.rs"));
    assert!(!records[1].prompt.contains("list the rust files"));
    clear_mock_env();
}

/// A transcript that no longer matches what the conversation saw (an edit, a
/// compaction) starts a new conversation with the whole transcript.
#[tokio::test]
async fn changed_history_starts_a_new_conversation() {
    let _guard = ENV_LOCK.lock().await;
    let (addr, records) = spawn_conversation_mock("first answer").await;
    set_mock_env(addr);

    let provider = claude_code_proxy::providers::cursor::CursorProvider::new();
    collect_sse(
        &provider,
        conversation_request(serde_json::json!([
            {"role": "user", "content": "first question"}
        ])),
        "continue-changed",
    )
    .await;
    collect_sse(
        &provider,
        conversation_request(serde_json::json!([
            {"role": "user", "content": "summary of the earlier work"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "second question"}
        ])),
        "continue-changed",
    )
    .await;

    let records = records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert!(records[1].conversation_state.is_empty());
    assert_ne!(records[1].conversation_id, records[0].conversation_id);
    assert!(records[1].prompt.contains("summary of the earlier work"));
    clear_mock_env();
}

/// What one mock `Run` received.
#[derive(Default, Clone, Debug)]
struct RunRecord {
    conversation_state: Vec<u8>,
    conversation_id: String,
    prompt: String,
    blob_reply: Option<Vec<u8>>,
}

fn conversation_request(messages: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "model": "cursor:gpt-5.5",
        "stream": true,
        "system": "be brief",
        "tools": [
            {"name": "Read", "description": "read", "input_schema": {"type": "object"}},
            {"name": "Glob", "description": "glob", "input_schema": {"type": "object"}}
        ],
        "messages": messages
    })
}

fn set_mock_env(addr: std::net::SocketAddr) {
    unsafe {
        std::env::set_var("CCP_CURSOR_BASE_URL", format!("http://{addr}"));
        std::env::set_var("CCP_CURSOR_AUTH_TOKEN", "proxy-token");
    }
}

fn clear_mock_env() {
    unsafe {
        std::env::remove_var("CCP_CURSOR_BASE_URL");
        std::env::remove_var("CCP_CURSOR_AUTH_TOKEN");
    }
}

/// A Cursor upstream that keeps one conversation.
///
/// The first run stores a blob, replies with `first_reply` and sends a
/// checkpoint that refers to the blob. Every later run asks for the blob back
/// before it answers "second answer".
async fn spawn_conversation_mock(
    first_reply: &'static str,
) -> (std::net::SocketAddr, Arc<Mutex<Vec<RunRecord>>>) {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::post;
    use claude_code_proxy::providers::cursor::connect::{
        ConnectFrameDecoder, encode_connect_frame,
    };
    use claude_code_proxy::providers::cursor::proto::*;
    use futures_util::StreamExt;
    use prost::Message;

    let records: Arc<Mutex<Vec<RunRecord>>> = Arc::default();
    let app = {
        let records = Arc::clone(&records);
        Router::new().route(
            "/agent.v1.AgentService/Run",
            post(move |body: Body| async move {
                let run = {
                    let mut records = records.lock().unwrap();
                    records.push(RunRecord::default());
                    records.len() - 1
                };
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let records = Arc::clone(&records);

                tokio::spawn(async move {
                    let mut decoder = ConnectFrameDecoder::new();
                    let mut stream = body.into_data_stream();
                    let mut started = false;
                    while let Some(Ok(chunk)) = stream.next().await {
                        let Ok(frames) = decoder.push(&chunk) else {
                            break;
                        };
                        for frame in frames {
                            if !started {
                                started = true;
                                let request = raw_field(&frame.payload, 1).unwrap_or_default();
                                let prompt = [2, 1, 1, 1]
                                    .iter()
                                    .try_fold(request.clone(), |buf, tag| raw_field(&buf, *tag))
                                    .unwrap_or_default();
                                {
                                    let mut records = records.lock().unwrap();
                                    records[run].conversation_state =
                                        raw_field(&request, 1).unwrap_or_default();
                                    records[run].conversation_id = String::from_utf8(
                                        raw_field(&request, 5).unwrap_or_default(),
                                    )
                                    .unwrap();
                                    records[run].prompt = String::from_utf8(prompt).unwrap();
                                }
                                if run == 0 {
                                    let _ = tx.send(Ok(kv_frame(
                                        1,
                                        kv_server_message::Message::SetBlobArgs(SetBlobArgs {
                                            blob_id: b"blob-1".to_vec(),
                                            blob_data: b"turn-1".to_vec(),
                                        }),
                                    )));
                                    let _ = tx.send(Ok(text_frame(first_reply)));
                                    let _ = tx.send(Ok(encode_connect_frame(
                                        length_delimited(3, &checkpoint_bytes()),
                                        0,
                                    )));
                                    let _ = tx.send(Ok(usage_frame(30, 5)));
                                    let _ = tx.send(Ok(encode_connect_frame(b"", 2)));
                                } else {
                                    let _ = tx.send(Ok(kv_frame(
                                        2,
                                        kv_server_message::Message::GetBlobArgs(GetBlobArgs {
                                            blob_id: b"blob-1".to_vec(),
                                        }),
                                    )));
                                }
                                continue;
                            }
                            let Ok(message) = AgentClientMessage::decode(frame.payload.clone())
                            else {
                                continue;
                            };
                            if let Some(KvClientMessage {
                                message: Some(kv_client_message::Message::GetBlobResult(result)),
                                ..
                            }) = message.kv_client_message
                            {
                                records.lock().unwrap()[run].blob_reply = result.blob_data;
                                let _ = tx.send(Ok(text_frame("second answer")));
                                let _ = tx.send(Ok(usage_frame(40, 5)));
                                let _ = tx.send(Ok(encode_connect_frame(b"", 2)));
                            }
                        }
                    }
                });

                let body =
                    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                        rx.recv()
                            .await
                            .map(|item: Result<bytes::Bytes, std::io::Error>| (item, rx))
                    }));
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/connect+proto",
                    )],
                    body,
                )
            }),
        )
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, records)
}

/// A checkpoint as the server sends it: the turns (tag 8, a blob id the crate
/// does not model) and the token counter (tag 5).
fn checkpoint_bytes() -> Vec<u8> {
    let mut checkpoint = length_delimited(8, b"blob-1");
    checkpoint.extend(length_delimited(5, &[0x08, 0xb0, 0x09]));
    checkpoint
}

fn kv_frame(
    id: u32,
    message: claude_code_proxy::providers::cursor::proto::kv_server_message::Message,
) -> bytes::Bytes {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let message = AgentServerMessage {
        kv_server_message: Some(KvServerMessage {
            id,
            message: Some(message),
        }),
        ..Default::default()
    };
    encode_connect_frame(message.encode_to_vec(), 0)
}

/// A length-delimited protobuf field; the payloads here stay under 128 bytes.
fn length_delimited(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![(tag << 3) | 2, value.len() as u8];
    out.extend_from_slice(value);
    out
}

/// The last top-level length-delimited field `tag` of a protobuf message.
fn raw_field(buf: &[u8], tag: u64) -> Option<Vec<u8>> {
    fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *buf.get(*pos)?;
            *pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    let mut pos = 0;
    let mut found = None;
    while pos < buf.len() {
        let key = varint(buf, &mut pos)?;
        match key & 7 {
            0 => {
                varint(buf, &mut pos)?;
            }
            1 => pos += 8,
            2 => {
                let len = varint(buf, &mut pos)? as usize;
                let end = pos + len;
                if key >> 3 == tag {
                    found = Some(buf.get(pos..end)?.to_vec());
                }
                pos = end;
            }
            5 => pos += 4,
            _ => return None,
        }
    }
    found
}

/// Call the provider the way the proxy does and collect the whole SSE body.
async fn collect_sse(
    provider: &claude_code_proxy::providers::cursor::CursorProvider,
    body: serde_json::Value,
    session_id: &str,
) -> String {
    use claude_code_proxy::provider::{Provider, RequestContext};

    let request: claude_code_proxy::MessagesRequest = serde_json::from_value(body).unwrap();
    let response = provider
        .handle_messages(
            request,
            RequestContext {
                req_id: format!("test-{session_id}"),
                session_id: Some(session_id.to_string()),
                session_seq: None,
                provider: "cursor".to_string(),
                traffic: None,
                monitor: None,
            },
        )
        .await;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect sse")
        .to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// The frame the agent asks a tool in: AgentServerMessage tag 2 ->
/// ExecServerMessage id 1, read_args tag 7, exec_id tag 15.
fn exec_frame() -> bytes::Bytes {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let message = AgentServerMessage {
        interaction_update: None,
        exec_server_message: Some(ExecServerMessage {
            id: Some(1),
            exec_id: Some("exec-1".to_string()),
            read_args: Some(ReadArgs {
                path: "/etc/hostname".to_string(),
                tool_call_id: Some("tool_abc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),

        conversation_checkpoint_update: None,
        kv_server_message: None,
    };
    encode_connect_frame(message.encode_to_vec(), 0)
}

fn text_frame(text: &str) -> bytes::Bytes {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let message = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            text_delta: Some(TextDelta {
                text: text.to_string(),
            }),
            ..Default::default()
        }),
        exec_server_message: None,
        conversation_checkpoint_update: None,
        kv_server_message: None,
    };
    encode_connect_frame(message.encode_to_vec(), 0)
}

fn usage_frame(input: u64, output: u64) -> bytes::Bytes {
    use claude_code_proxy::providers::cursor::connect::encode_connect_frame;
    use claude_code_proxy::providers::cursor::proto::*;
    use prost::Message;

    let message = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            turn_ended: Some(TurnEnded {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            ..Default::default()
        }),
        exec_server_message: None,
        conversation_checkpoint_update: None,
        kv_server_message: None,
    };
    encode_connect_frame(message.encode_to_vec(), 0)
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
