#![allow(deprecated)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use claude_code_proxy::provider::RequestContext;
use claude_code_proxy::providers::codex::client::{
    ActualTransport, CodexError, CodexHttpClient, CodexResponse,
};
use claude_code_proxy::providers::codex::continuation::{
    ContinuationCandidate, abort_continuation, clear_continuation, continuation_candidate,
    has_continuation_for_tests, if_current_turn, is_current_turn, record_continuation,
    with_current_turn,
};
use claude_code_proxy::providers::codex::translate::request::ResponsesRequest;
use claude_code_proxy::providers::codex::websocket::{
    CodexWebSocketEventReceiver, invalidate_codex_websocket_pool_key,
    invalidate_codex_websocket_pool_turn,
};

fn baseline_continuation_forms(session_id: Option<&str>, body: &ResponsesRequest) {
    let candidate = continuation_candidate(session_id, body, true);
    record_continuation(
        session_id,
        candidate.turn_id,
        body,
        candidate.previous_response_id.as_deref(),
        &[],
    );
    abort_continuation(session_id, candidate.turn_id);
    let _: Option<usize> = if_current_turn(session_id, candidate.turn_id, || 1);
    let _: bool = with_current_turn(session_id, candidate.turn_id, || {});
    let _: bool = is_current_turn(session_id, candidate.turn_id);
    clear_continuation(session_id);
    let _: bool = has_continuation_for_tests("compat-session");
}

fn baseline_pool_invalidation_forms() {
    invalidate_codex_websocket_pool_key("compat-session");
    invalidate_codex_websocket_pool_turn("compat-session", Some(1));
}

async fn baseline_client_forms(
    client: &Arc<CodexHttpClient>,
    body: &ResponsesRequest,
    ctx: &RequestContext,
    candidate: &ContinuationCandidate,
) {
    let _: Result<CodexResponse, CodexError> = client.post_codex(body, ctx, Some(candidate)).await;
    let _ = client
        .stream_codex_websocket_events(body, ctx, Some(candidate))
        .await;
}

#[test]
fn baseline_codex_public_api_forms_compile() {
    let (_tx, receiver) = tokio::sync::mpsc::channel::<Result<serde_json::Value, CodexError>>(1);
    let mut receiver: CodexWebSocketEventReceiver = receiver;

    let _ = receiver.try_recv();
    receiver.close();
    let _: usize = receiver.len();
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    let _: Poll<Option<Result<serde_json::Value, CodexError>>> =
        Pin::new(&mut receiver).poll_recv(&mut context);

    let response = CodexResponse {
        body: vec![1, 2, 3],
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        transport: ActualTransport::WebSocket,
    };
    assert_eq!(response.body, vec![1, 2, 3]);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers,
        vec![("content-type".to_string(), "text/event-stream".to_string())]
    );
    assert_eq!(response.transport, ActualTransport::WebSocket);

    let _candidate = ContinuationCandidate {
        turn_id: Some(1),
        previous_response_id: Some("resp_compat".to_string()),
        input_delta: None,
        input_delta_count: 0,
        disabled_reason: None,
    };

    let _: fn(Option<&str>, &ResponsesRequest) = baseline_continuation_forms;
    let _: fn() = baseline_pool_invalidation_forms;
    let _ = baseline_client_forms;
}
