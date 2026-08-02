use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;

use crate::anthropic::schema::MessagesRequest;
use crate::monitor::{MonitorHandle, usage_from_anthropic_sse};
use crate::providers::codex::translate::{
    live_stream::LiveStreamTranslator, request::translate_openai_compatible_request,
};
use crate::providers::grok::translate::stream::SseDecoder;
use crate::traffic::{StreamTrafficCapture, TrafficCapture};

use super::client::{OpenCodeError, OpenCodeResponse};

pub fn prepare_request(
    body: &MessagesRequest,
    model: &str,
    session_id: Option<String>,
) -> anyhow::Result<serde_json::Value> {
    let translated = translate_openai_compatible_request(body, model.to_string(), session_id)?;
    let mut value = serde_json::to_value(translated)?;
    if let Some(max_tokens) = body.max_tokens.filter(|value| *value > 0) {
        value["max_output_tokens"] = serde_json::json!(max_tokens);
    }
    Ok(value)
}

pub fn stream_body(
    upstream: OpenCodeResponse,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<TrafficCapture>>,
) -> Body {
    let state = ResponsesStreamState {
        upstream: upstream.into_stream(),
        decoder: SseDecoder::default(),
        translator: LiveStreamTranslator::new(message_id, model),
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
    Body::from_stream(stream)
}

struct ResponsesStreamState<S> {
    upstream: S,
    decoder: SseDecoder,
    translator: LiveStreamTranslator,
    terminal: bool,
    error_sent: bool,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    chunks: u64,
    stream_capture: Option<StreamTrafficCapture>,
    traffic: Option<Arc<TrafficCapture>>,
}

impl<S> ResponsesStreamState<S>
where
    S: futures_util::Stream<Item = Result<Bytes, OpenCodeError>> + Unpin,
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
                    if self.decoder.finish().is_err() || !self.translator.is_finished() {
                        return Some(self.fail_at("decoder", "incomplete_stream"));
                    }
                    self.terminal = true;
                    self.finish_capture(true);
                    return None;
                }
            };

            if self.bytes == 0
                && let Some(monitor) = self.monitor.as_ref()
            {
                monitor.generation_started(&self.req_id);
            }
            self.bytes = self.bytes.saturating_add(chunk.len() as u64);
            self.chunks = self.chunks.saturating_add(1);

            let events = match self.decoder.push(&chunk) {
                Ok(events) => events,
                Err(_) => return Some(self.fail_at("decoder", "malformed_sse")),
            };
            let mut completion_seen = false;
            let mut done_seen = false;
            for event in &events {
                let data = event.data.trim();
                if data == "[DONE]" {
                    if !completion_seen || done_seen {
                        return Some(self.fail_at("protocol", "premature_done"));
                    }
                    done_seen = true;
                    continue;
                }
                if completion_seen || done_seen {
                    return Some(self.fail_at("protocol", "event_after_completion"));
                }
                let value: serde_json::Value = match serde_json::from_str(data) {
                    Ok(value) => value,
                    Err(_) => return Some(self.fail_at("json", "malformed_event")),
                };
                completion_seen = matches!(
                    value.get("type").and_then(serde_json::Value::as_str),
                    Some("response.completed" | "response.incomplete" | "response.done")
                );
            }
            if completion_seen && self.decoder.finish().is_err() {
                return Some(self.fail_at("decoder", "trailing_incomplete_frame"));
            }

            let mut output = Vec::new();
            for event in events {
                let data = event.data.trim();
                if data == "[DONE]" {
                    if let Some(capture) = self.stream_capture.as_mut() {
                        capture
                            .upstream_event(event.event.as_deref(), &serde_json::json!("[DONE]"));
                    }
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(data) {
                    Ok(value) => value,
                    Err(_) => return Some(self.fail_at("json", "malformed_event")),
                };
                if let Some(capture) = self.stream_capture.as_mut() {
                    capture.upstream_event(event.event.as_deref(), &value);
                }
                let translated = match self.translator.accept(&value, self.traffic.as_deref()) {
                    Ok(translated) => translated,
                    Err(_) => return Some(self.fail_at("translation", "invalid_event")),
                };
                output.extend(translated);
            }
            if self.translator.is_finished() {
                self.terminal = true;
                self.record_progress(&output);
                self.capture_downstream(&output);
                self.finish_capture(true);
                return (!output.is_empty()).then_some(output);
            }
            if !output.is_empty() {
                self.record_progress(&output);
                self.capture_downstream(&output);
                return Some(output);
            }
        }
    }

    fn record_progress(&self, output: &[u8]) {
        let Some(monitor) = self.monitor.as_ref() else {
            return;
        };
        let (input_tokens, output_tokens) = usage_from_anthropic_sse(output);
        monitor.stream_progress(
            &self.req_id,
            output.len() as u64,
            count_sse_events(output),
            input_tokens,
            output_tokens,
        );
    }

    fn fail_at(&mut self, stage: &str, kind: &str) -> Vec<u8> {
        self.error_sent = true;
        if let Some(capture) = self.stream_capture.as_mut() {
            capture.malformed(stage, kind);
        }
        if let Some(traffic) = self.traffic.as_ref() {
            traffic.write_json(
                "060-opencode-responses-stream-error",
                &serde_json::json!({
                    "stage": stage,
                    "kind": kind,
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
            );
        }
        let output = self.translator.error_chunk(
            "OpenCode Go Responses stream is invalid",
            "api_error",
            self.traffic.as_deref(),
        );
        self.capture_downstream(&output);
        self.finish_capture(false);
        output
    }

    fn capture_downstream(&mut self, bytes: &[u8]) {
        let Some(capture) = self.stream_capture.as_mut() else {
            return;
        };
        let mut decoder = SseDecoder::default();
        if let Ok(events) = decoder.push(bytes) {
            for event in events {
                if let Ok(value) = serde_json::from_str(&event.data) {
                    capture.downstream_event(event.event.as_deref().unwrap_or("message"), value);
                }
            }
        }
    }

    fn finish_capture(&mut self, completed: bool) {
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish_named(
                traffic,
                serde_json::json!({
                    "kind": if completed { "stream_completion" } else { "stream_error" },
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
                "061-opencode-responses-stream-summary",
            );
        }
    }
}

impl<S> Drop for ResponsesStreamState<S> {
    fn drop(&mut self) {
        if self.terminal || self.stream_capture.is_none() {
            return;
        }
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish_named(
                traffic,
                serde_json::json!({
                    "kind": "stream_abandoned",
                    "reason": "downstream_body_dropped",
                    "bytes": self.bytes,
                    "chunks": self.chunks,
                }),
                "061-opencode-responses-stream-summary",
            );
        }
    }
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_uses_standard_responses_fields_without_codex_lane_metadata() {
        let body: MessagesRequest = serde_json::from_value(json!({
            "model": "opencode-go/gpt-5.6-luna",
            "max_tokens": 2048,
            "output_config": {"effort": "xhigh"},
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let translated = prepare_request(&body, "gpt-5.6-luna", Some("session-1".into()))
            .expect("translate Responses request");

        assert_eq!(translated["model"], "gpt-5.6-luna");
        assert_eq!(translated["stream"], true);
        assert_eq!(translated["max_output_tokens"], 2048);
        assert_eq!(translated["reasoning"]["effort"], "xhigh");
        assert_eq!(translated["reasoning"]["summary"], "auto");
        assert_eq!(translated["prompt_cache_key"], "session-1");
        assert!(translated.get("service_tier").is_none());
        assert!(translated.get("client_metadata").is_none());
    }

    #[tokio::test]
    async fn live_stream_rejects_an_incomplete_frame_after_completion() {
        let upstream = futures_util::stream::iter([Ok::<Bytes, OpenCodeError>(
            Bytes::from_static(
                b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\ndata: {",
            ),
        )]);
        let mut state = ResponsesStreamState {
            upstream,
            decoder: SseDecoder::default(),
            translator: LiveStreamTranslator::new("msg_1", "gpt-5.6-luna"),
            terminal: false,
            error_sent: false,
            monitor: None,
            req_id: "req".into(),
            bytes: 0,
            chunks: 0,
            stream_capture: None,
            traffic: None,
        };
        let output = state.next_output().await.expect("error event");
        assert!(
            String::from_utf8_lossy(&output).contains("OpenCode Go Responses stream is invalid")
        );
    }
}
