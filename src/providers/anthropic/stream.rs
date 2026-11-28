use std::{convert::Infallible, pin::Pin};

use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::anthropic::sse::encode_sse_event;
use crate::provider::RequestContext;
use crate::providers::grok::translate::stream::SseDecoder;
use crate::traffic::StreamTrafficCapture;

pub fn stream_body(response: reqwest::Response, ctx: RequestContext) -> Body {
    let capture = ctx.traffic.as_ref().map(|traffic| traffic.stream_capture());
    let state = StreamState {
        upstream: Box::pin(response.bytes_stream()),
        decoder: SseDecoder::default(),
        ctx,
        capture,
        done: false,
        bytes: 0,
        chunks: 0,
    };
    Body::from_stream(futures_util::stream::unfold(state, |mut state| async move {
        state.next().await.map(|bytes| (Ok::<Bytes, Infallible>(bytes), state))
    }))
}

struct StreamState {
    upstream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    decoder: SseDecoder,
    ctx: RequestContext,
    capture: Option<StreamTrafficCapture>,
    done: bool,
    bytes: u64,
    chunks: u64,
}

impl StreamState {
    async fn next(&mut self) -> Option<Bytes> {
        if self.done {
            return None;
        }
        let chunk = match self.upstream.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(_)) => return Some(self.fail("Anthropic upstream stream failed")),
            None => return Some(self.fail("Anthropic upstream stream ended before message_stop")),
        };
        if self.bytes == 0 && let Some(monitor) = &self.ctx.monitor {
            monitor.generation_started(&self.ctx.req_id);
        }
        self.bytes = self.bytes.saturating_add(chunk.len() as u64);
        self.chunks = self.chunks.saturating_add(1);
        let events = match self.decoder.push(&chunk) {
            Ok(events) => events,
            Err(_) => return Some(self.fail("Invalid Anthropic upstream SSE")),
        };
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut completed = false;
        for event in events {
            let value: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => return Some(self.fail("Invalid Anthropic upstream SSE JSON")),
            };
            if let Some(capture) = &mut self.capture {
                capture.upstream_event(event.event.as_deref(), &value);
                capture.downstream_event(event.event.as_deref().unwrap_or("message"), value.clone());
            }
            input_tokens = value.pointer("/message/usage/input_tokens")
                .or_else(|| value.pointer("/usage/input_tokens"))
                .and_then(serde_json::Value::as_u64).or(input_tokens);
            output_tokens = value.pointer("/message/usage/output_tokens")
                .or_else(|| value.pointer("/usage/output_tokens"))
                .and_then(serde_json::Value::as_u64).or(output_tokens);
            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("message_stop") => { self.done = true; completed = true; }
                // Forward native error events unchanged, including their error type.
                Some("error") => self.done = true,
                _ => {}
            }
        }
        if let Some(monitor) = &self.ctx.monitor {
            monitor.stream_progress(&self.ctx.req_id, chunk.len() as u64, 1, input_tokens, output_tokens);
        }
        if self.done {
            self.finish(if completed { "stream_completion" } else { "stream_error" });
        }
        // Observe SSE for accounting, but forward original bytes/signatures unchanged.
        Some(chunk)
    }

    fn fail(&mut self, message: &str) -> Bytes {
        self.done = true;
        let value = serde_json::json!({"type": "error", "error": {"type": "api_error", "message": message}});
        if let Some(capture) = &mut self.capture {
            capture.downstream_event("error", value.clone());
        }
        self.finish("stream_error");
        Bytes::from(encode_sse_event(Some("error"), &value.to_string()))
    }

    fn finish(&mut self, kind: &str) {
        if let (Some(capture), Some(traffic)) = (self.capture.take(), self.ctx.traffic.as_ref()) {
            capture.finish_named(traffic, serde_json::json!({
                "kind": kind, "bytes": self.bytes, "chunks": self.chunks
            }), "061-anthropic-stream-summary");
        }
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        self.finish("stream_abandoned");
    }
}
