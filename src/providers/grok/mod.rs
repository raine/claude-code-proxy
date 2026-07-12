pub mod auth;
pub mod client;
pub mod count_tokens;
pub mod translate;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::monitor::MonitorHandle;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::{registry::GROK_MODELS, traffic::StreamTrafficCapture};

use self::auth::token_store::file_store;
use self::translate::{
    accumulate::accumulate_response_with_traffic,
    model_allowlist::{assert_allowed_model, resolve_model},
    request::translate_request,
    stream::{SseDecoder, StreamTranslator, stream_error},
};

pub struct GrokProvider {
    client: Arc<client::GrokClient>,
}
impl GrokProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(
                client::GrokClient::new(
                    crate::config::grok_base_url(),
                    crate::config::grok_client_version(),
                )
                .expect("Grok transport is unavailable"),
            ),
        }
    }

    pub fn with_client(client: client::GrokClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}
impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GrokProvider {
    fn name(&self) -> &'static str {
        "grok"
    }
    fn supported_models(&self) -> Vec<String> {
        GROK_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &GROK_CLI
    }
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.clone().unwrap_or_else(|| "grok-4.5".into());
        let resolved = resolve_model(&requested);
        if let Err(error) = assert_allowed_model(&resolved) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let translated = match translate_request(&body, resolved.clone()) {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match self.client.post(&translated, ctx.traffic.clone()).await {
            Ok(response) => response,
            Err(error) => return map_error(error),
        };
        if body.stream {
            stream_response(
                upstream,
                format!("msg_{}", uuid::Uuid::new_v4().simple()),
                requested,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
                ctx.traffic.clone(),
            )
        } else {
            let upstream_bytes = match upstream.into_bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    write_error(ctx.traffic.as_deref(), "body_read", "transport");
                    return map_error(error);
                }
            };
            match accumulate_response_with_traffic(
                &upstream_bytes,
                &format!("msg_{}", uuid::Uuid::new_v4().simple()),
                &requested,
                ctx.traffic.as_deref(),
            ) {
                Ok(value) => {
                    if let Some(traffic) = ctx.traffic.as_ref() {
                        traffic.write_json("051-downstream-response", &value);
                    }
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            value
                                .pointer("/usage/input_tokens")
                                .and_then(|v| v.as_u64()),
                            value
                                .pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    (StatusCode::OK, Json(value)).into_response()
                }
                Err(_) => {
                    write_error(ctx.traffic.as_deref(), "accumulate", "invalid_response");
                    json_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "Grok response is invalid",
                    )
                }
            }
        }
    }
    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.clone().unwrap_or_else(|| "grok-4.5".into());
        let resolved = resolve_model(&requested);
        if let Err(error) = assert_allowed_model(&resolved) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let translated = match translate_request(&body, resolved) {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
        let tokens = count_tokens::count_tokens(&translated);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

fn stream_response(
    response: client::GrokResponse,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
) -> Response {
    stream_body(
        response.into_stream(),
        message_id,
        model,
        monitor,
        req_id,
        traffic,
    )
}

fn stream_body<S>(
    upstream: S,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
) -> Response
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin + Send + 'static,
{
    let state = GrokStreamState {
        upstream,
        decoder: SseDecoder::default(),
        reducer: translate::reducer::Reducer::default(),
        translator: StreamTranslator::new(message_id, model),
        terminal: false,
        error_sent: false,
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
        stream_capture: traffic.as_ref().map(|traffic| traffic.stream_capture()),
        traffic,
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        state
            .next_output()
            .await
            .map(|bytes| (Ok::<Bytes, Infallible>(Bytes::from(bytes)), state))
    });
    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

struct GrokStreamState<S> {
    upstream: S,
    decoder: SseDecoder,
    reducer: translate::reducer::Reducer,
    translator: StreamTranslator,
    terminal: bool,
    error_sent: bool,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    chunks: u64,
    stream_capture: Option<StreamTrafficCapture>,
    traffic: Option<Arc<crate::traffic::TrafficCapture>>,
}

impl<S> GrokStreamState<S>
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin,
{
    async fn next_output(&mut self) -> Option<Vec<u8>> {
        if self.terminal {
            return None;
        }
        if self.error_sent {
            self.terminal = true;
            return None;
        }
        loop {
            let chunk = match self.upstream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(_)) => return Some(self.fail_at("transport", "upstream_stream")),
                None => {
                    if self.decoder.finish().is_err() || !self.reducer.finished() {
                        return Some(self.fail_at("decoder", "incomplete_stream"));
                    }
                    self.terminal = true;
                    self.finish_capture(true);
                    return None;
                }
            };
            self.bytes = self.bytes.saturating_add(chunk.len() as u64);
            self.chunks = self.chunks.saturating_add(1);
            if let Some(monitor) = self.monitor.as_ref() {
                monitor.stream_progress(&self.req_id, self.bytes, self.chunks, None, None);
            }
            let events = match self.decoder.push(&chunk) {
                Ok(events) => events,
                Err(_) => return Some(self.fail_at("decoder", "malformed_sse")),
            };
            let mut out = Vec::new();
            for event in events {
                let value: serde_json::Value = match serde_json::from_str(&event.data) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(capture) = self.stream_capture.as_mut() {
                            capture.malformed("json", "malformed_event");
                        }
                        return Some(self.fail_at("json", "malformed_event"));
                    }
                };
                if let Some(capture) = self.stream_capture.as_mut() {
                    capture.upstream_event(event.event.as_deref(), &value);
                }
                let reduced = match self.reducer.push(value) {
                    Ok(events) => events,
                    Err(_) => return Some(self.fail_at("reducer", "invalid_event")),
                };
                let usage = reduced.iter().find_map(|event| match event {
                    translate::reducer::ReducerEvent::Finish {
                        input_tokens,
                        output_tokens,
                        ..
                    } => Some((*input_tokens, *output_tokens)),
                    _ => None,
                });
                match self.translator.render(reduced) {
                    Ok(bytes) => out.extend(bytes),
                    Err(_) => return Some(self.fail_at("render", "invalid_event")),
                }
                if let Some((input_tokens, output_tokens)) = usage
                    && let Some(monitor) = self.monitor.as_ref()
                {
                    monitor.usage_updated(&self.req_id, Some(input_tokens), Some(output_tokens));
                }
                if self.reducer.finished() {
                    self.terminal = true;
                    self.capture_downstream(&out);
                    self.finish_capture(true);
                    return if out.is_empty() { None } else { Some(out) };
                }
            }
            if !out.is_empty() {
                self.capture_downstream(&out);
                return Some(out);
            }
        }
    }

    fn fail_at(&mut self, stage: &str, kind: &str) -> Vec<u8> {
        self.error_sent = true;
        if let Some(capture) = self.stream_capture.as_mut() {
            capture.malformed(stage, kind);
            capture.downstream_event("error", serde_json::json!({"type":"error","error":{"type":"api_error","message":"Grok stream is invalid"}}));
        }
        if let Some(traffic) = self.traffic.as_ref() {
            traffic.write_json("060-grok-stream-error", &serde_json::json!({"stage":stage,"kind":kind,"bytes":self.bytes,"chunks":self.chunks}));
        }
        self.finish_capture(false);
        stream_error()
    }

    fn capture_downstream(&mut self, bytes: &[u8]) {
        let Some(capture) = self.stream_capture.as_mut() else {
            return;
        };
        let mut decoder = SseDecoder::default();
        if let Ok(events) = decoder.push(bytes) {
            for event in events {
                if let Ok(data) = serde_json::from_str(&event.data) {
                    capture.downstream_event(event.event.as_deref().unwrap_or("message"), data);
                }
            }
        }
    }

    fn finish_capture(&mut self, completed: bool) {
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish(
                traffic,
                serde_json::json!({
                    "kind": if completed { "stream_completion" } else { "stream_error" },
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
    }
}

impl<S> Drop for GrokStreamState<S> {
    fn drop(&mut self) {
        if self.terminal || self.stream_capture.is_none() {
            return;
        }
        if let Some(traffic) = self.traffic.as_ref() {
            traffic.write_json(
                "060-grok-stream-abandoned",
                &serde_json::json!({
                    "stage": "downstream",
                    "kind": "client_disconnect",
                    "reason": "downstream_body_dropped",
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish(
                traffic,
                serde_json::json!({
                    "kind": "stream_abandoned",
                    "reason": "downstream_body_dropped",
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
    }
}

fn write_error(traffic: Option<&crate::traffic::TrafficCapture>, stage: &str, kind: &str) {
    if let Some(traffic) = traffic {
        traffic.write_json(
            "060-grok-stream-error",
            &serde_json::json!({"stage":stage,"kind":kind}),
        );
    }
}

fn map_error(error: client::GrokError) -> Response {
    match error.status {
        StatusCode::UNAUTHORIZED => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            error.message,
        ),
        StatusCode::TOO_MANY_REQUESTS => {
            let response = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                error.message,
            );
            if let Some(retry_after) = error.retry_after {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN => {
            json_error(error.status, "permission_error", error.message)
        }
        _ => json_error(StatusCode::BAD_GATEWAY, "api_error", error.message),
    }
}

pub struct GrokCli;
pub static GROK_CLI: GrokCli = GrokCli;
impl CliHandlers for GrokCli {
    fn login(&self) -> anyhow::Result<()> {
        let store = file_store();
        auth::login::login(&store)?;
        println!("Grok authentication saved in {}", store.auth_path());
        Ok(())
    }
    fn device(&self) -> anyhow::Result<()> {
        let store = file_store();
        auth::device::device_login(&store)?;
        println!("Grok authentication saved in {}", store.auth_path());
        Ok(())
    }
    fn status(&self) -> anyhow::Result<()> {
        let store = file_store();
        match store.load_auth()? {
            Some(auth) => {
                println!("Auth path: {}", store.auth_path());
                println!("Authenticated: true");
                println!(
                    "Expires in {}s",
                    auth.expires_at_ms.saturating_sub(now_ms()) / 1000
                );
                Ok(())
            }
            None => anyhow::bail!("Not authenticated"),
        }
    }
    fn logout(&self) -> anyhow::Result<()> {
        let store = file_store();
        store.clear_auth()?;
        println!("Grok proxy credentials removed");
        Ok(())
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use crate::monitor::{EndpointKind, MonitorHandle};
    use crate::traffic::test_capture;
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;

    struct ChannelStream(mpsc::Receiver<Result<Bytes, client::GrokError>>);

    impl Stream for ChannelStream {
        type Item = Result<Bytes, client::GrokError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    #[tokio::test]
    async fn streaming_usage_updates_completed_monitor_request() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "req_1",
            Some("session_1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.provider_selected("req_1", "grok", "grok-4.5", None);
        monitor.request_completed("req_1", 200, None, None);

        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n",
        ))]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            Some(monitor.clone()),
            "req_1".into(),
            None,
        );
        let _ = response.into_body().collect().await.unwrap();

        let snapshot = monitor.snapshot();
        let request = snapshot
            .recent
            .iter()
            .find(|request| request.request_id == "req_1")
            .unwrap();
        assert_eq!(request.input_tokens, Some(12));
        assert_eq!(request.output_tokens, Some(3));
        assert!(request.streamed_bytes > 0);
        assert!(request.stream_chunks > 0);
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id.as_deref() == Some("session_1"))
            .unwrap();
        assert_eq!(session.input_tokens, 12);
        assert_eq!(session.output_tokens, 3);
    }

    #[tokio::test]
    async fn downstream_event_arrives_before_upstream_completion() {
        let (tx, rx) = mpsc::channel(2);
        let response = stream_body(
            ChannelStream(rx),
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            None,
        );
        let mut body = response.into_body();

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n",
        )))
        .await
        .unwrap();

        let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("downstream body waited for upstream completion")
            .expect("downstream body ended before its first event")
            .expect("downstream body frame failed")
            .into_data()
            .expect("first downstream frame was not data");
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.contains("event: message_start"));
        assert!(first.contains("first"));

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        )))
        .await
        .unwrap();
        let terminal = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("downstream completion timed out")
            .expect("downstream completion was missing")
            .expect("downstream completion frame failed")
            .into_data()
            .expect("downstream completion frame was not data");
        assert!(
            String::from_utf8(terminal.to_vec())
                .unwrap()
                .contains("event: message_stop")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), body.frame())
                .await
                .expect("downstream EOF waited for upstream EOF")
                .is_none()
        );
    }

    #[tokio::test]
    async fn dropped_downstream_body_finalizes_partial_capture() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let (tx, rx) = mpsc::channel(2);
        let response = stream_body(
            ChannelStream(rx),
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            Some(traffic),
        );
        let mut body = response.into_body();

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        )))
        .await
        .unwrap();
        let _ = body.frame().await.unwrap().unwrap();
        drop(body);

        let entries: Vec<_> = std::fs::read_dir(temp.path().join("traffic"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let abandoned = entries
            .iter()
            .find(|path| path.to_string_lossy().contains("060-grok-stream-abandoned"))
            .unwrap();
        let summary = entries
            .iter()
            .find(|path| path.to_string_lossy().contains("061-grok-stream-summary"))
            .unwrap();
        let abandoned: serde_json::Value =
            serde_json::from_slice(&std::fs::read(abandoned).unwrap()).unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&std::fs::read(summary).unwrap()).unwrap();
        assert_eq!(abandoned["kind"], "client_disconnect");
        assert_eq!(summary["completion"]["kind"], "stream_abandoned");
        assert_eq!(summary["completion"]["reason"], "downstream_body_dropped");
        assert_eq!(summary["completion"]["chunks"], 1);
        assert!(summary["upstream_events"]["captured"].as_u64().unwrap() > 0);
        assert!(summary["downstream_events"]["captured"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn streaming_capture_writes_redacted_complete_artifacts() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        ))]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            Some(traffic),
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("tool_use"));
        let names: Vec<_> = std::fs::read_dir(temp.path().join("traffic"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|name| name.contains("032-upstream-response-body.sse"))
        );
        assert!(
            names
                .iter()
                .any(|name| name.contains("061-grok-stream-summary"))
        );
    }

    #[tokio::test]
    async fn streaming_capture_records_fragmented_search_and_tool_events() {
        let temp = TempDir::new().unwrap();
        let traffic = Arc::new(test_capture(temp.path().join("traffic")));
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fir",
            )),
            Ok(Bytes::from_static(
                b"st\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
            )),
        ]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
            Some(traffic),
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("tool_use"));
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("x_search"));
        assert!(captured.contains("function_call"));
        assert!(captured.contains("stream_completion"));
    }

    #[tokio::test]
    async fn streaming_capture_records_malformed_and_failed_streams() {
        for (payload, stage) in [
            (b"data: {bad json}\n\n".as_slice(), "json"),
            (
                b"data: {\"type\":\"response.failed\",\"response\":{}}\n\n".as_slice(),
                "reducer",
            ),
        ] {
            let temp = TempDir::new().unwrap();
            let traffic = Arc::new(test_capture(temp.path().join("traffic")));
            let response = stream_body(
                futures_util::stream::iter(vec![Ok(Bytes::copy_from_slice(payload))]),
                "msg_1".into(),
                "grok-4.5".into(),
                None,
                "req_1".into(),
                Some(traffic),
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("event: error"));
            let captured = capture_contents(temp.path().join("traffic"));
            assert!(captured.contains(&format!("\"stage\": \"{stage}\"")));
            assert!(captured.contains("stream_error"));
        }
    }

    #[test]
    fn non_streaming_malformed_capture_keeps_diagnostics() {
        let temp = TempDir::new().unwrap();
        let traffic = test_capture(temp.path().join("traffic"));
        assert!(
            accumulate_response_with_traffic(
                b"data: {bad json}\n\n",
                "msg_1",
                "grok-4.5",
                Some(&traffic),
            )
            .is_err()
        );
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("malformed_event"));
        assert!(captured.contains("\"outcome\": \"error\""));
    }

    #[test]
    fn transport_failure_capture_contains_no_credentials() {
        let temp = TempDir::new().unwrap();
        let traffic = test_capture(temp.path().join("traffic"));
        client::capture_failure(Some(&traffic), "transport", "transport", 1);
        let captured = capture_contents(temp.path().join("traffic"));
        assert!(captured.contains("transport"));
        for secret in [
            "Bearer token",
            "refresh-secret",
            "oauth-code",
            "person@example.com",
        ] {
            assert!(!captured.contains(secret));
        }
    }

    fn capture_contents(root: std::path::PathBuf) -> String {
        let mut captured = String::new();
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    captured.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }
        captured
    }

    #[test]
    fn non_streaming_capture_writes_response_and_redacts_secrets() {
        let temp = TempDir::new().unwrap();
        let traffic = test_capture(temp.path().join("traffic"));
        let upstream = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\",\"access_token\":\"secret\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let value = accumulate_response_with_traffic(upstream, "msg_1", "grok-4.5", Some(&traffic))
            .unwrap();
        traffic.write_json("051-downstream-response", &value);
        let mut captured = String::new();
        for entry in std::fs::read_dir(temp.path().join("traffic")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                captured.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        assert!(captured.contains("[redacted len=6]"));
        assert!(!captured.contains("secret"));
    }
}
