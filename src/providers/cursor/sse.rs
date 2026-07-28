use crate::providers::cursor::client::CursorUpstreamResponse;
use crate::providers::cursor::response::{CursorStreamEvent, decode_upstream_response};

/// SSE event name constants.
pub const EVENT_MESSAGE_START: &str = "message_start";
pub const EVENT_CONTENT_BLOCK_START: &str = "content_block_start";
pub const EVENT_CONTENT_BLOCK_DELTA: &str = "content_block_delta";
pub const EVENT_CONTENT_BLOCK_STOP: &str = "content_block_stop";
pub const EVENT_MESSAGE_DELTA: &str = "message_delta";
pub const EVENT_MESSAGE_STOP: &str = "message_stop";
pub const EVENT_PING: &str = "ping";
pub const EVENT_ERROR: &str = "error";

/// Frame upstream Cursor response bytes into Anthropic SSE event bytes.
///
/// Produces the standard message lifecycle:
/// 1. message_start (with initial usage)
/// 2. content_block_start (text)
/// 3. content_block_delta (text deltas) / content_block_delta (thinking deltas)
/// 4. content_block_stop
/// 5. message_delta (with final usage and stop_reason)
/// 6. message_stop
pub fn frame_cursor_stream(
    upstream: &CursorUpstreamResponse,
    message_id: &str,
    model: &str,
) -> Vec<u8> {
    let events = match decode_upstream_response(&upstream.body) {
        Ok(e) => e,
        Err(e) => {
            return format_sse_error(&e.to_string());
        }
    };

    let mut sse = Vec::new();
    let mut framer = CursorSseFramer::new(&mut sse, message_id, model);

    for event in &events {
        match event {
            CursorStreamEvent::ThinkingDelta { text } => {
                framer.emit_thinking_delta(text);
            }
            CursorStreamEvent::TextDelta { text } => {
                framer.emit_text_delta(text);
            }
            CursorStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => {
                framer.record_usage(
                    *input_tokens,
                    *output_tokens,
                    *cache_read_tokens,
                    *cache_write_tokens,
                );
            }
            CursorStreamEvent::End => {
                framer.emit_final_message("end_turn");
            }
            CursorStreamEvent::Session { .. } => {
                // Session events are informational, not mapped to SSE
            }
        }
    }

    framer.finalize();
    sse
}

/// Format an SSE error event.
fn format_sse_error(error: &str) -> Vec<u8> {
    let data = serde_json::json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": error
        }
    });
    format_sse_event_bytes("error", &data)
}

/// Format a single SSE event into bytes.
pub(crate) fn format_sse_event_bytes(event: &str, data: &serde_json::Value) -> Vec<u8> {
    let json_str = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    format!("event: {event}\ndata: {json_str}\n\n").into_bytes()
}

// ---------------------------------------------------------------------------
// SSE Framer
// ---------------------------------------------------------------------------

/// SSE framer that tracks state to produce well-formed Anthropic SSE events.
pub struct CursorSseFramer<'a> {
    output: &'a mut Vec<u8>,
    message_id: &'a str,
    model: &'a str,
    started: bool,
    thinking_open: bool,
    text_open: bool,
    next_index: i32,
    thinking_index: i32,
    text_index: i32,
    usage_input_tokens: u64,
    usage_output_tokens: u64,
    finalized: bool,
}

impl<'a> CursorSseFramer<'a> {
    pub fn new(output: &'a mut Vec<u8>, message_id: &'a str, model: &'a str) -> Self {
        Self {
            output,
            message_id,
            model,
            started: false,
            thinking_open: false,
            text_open: false,
            next_index: 0,
            thinking_index: -1,
            text_index: -1,
            usage_input_tokens: 0,
            usage_output_tokens: 0,
            finalized: false,
        }
    }

    pub fn ensure_start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;

        let data = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.usage_input_tokens.max(1),
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_MESSAGE_START, &data));
    }

    fn open_thinking(&mut self) {
        if self.thinking_open {
            return;
        }
        self.ensure_start();
        self.thinking_open = true;
        self.thinking_index = self.next_index;
        self.next_index += 1;

        let data = serde_json::json!({
            "type": "content_block_start",
            "index": self.thinking_index,
            "content_block": {
                "type": "thinking",
                "thinking": "",
                "signature": ""
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_START, &data));
    }

    fn open_text(&mut self) {
        if self.text_open {
            return;
        }
        self.ensure_start();
        self.text_open = true;
        self.text_index = self.next_index;
        self.next_index += 1;

        let data = serde_json::json!({
            "type": "content_block_start",
            "index": self.text_index,
            "content_block": {
                "type": "text",
                "text": ""
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_START, &data));
    }

    pub fn close_open_blocks(&mut self) {
        if self.thinking_open {
            let data = serde_json::json!({
                "type": "content_block_stop",
                "index": self.thinking_index
            });
            self.output
                .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_STOP, &data));
            self.thinking_open = false;
        }
        if self.text_open {
            let data = serde_json::json!({
                "type": "content_block_stop",
                "index": self.text_index
            });
            self.output
                .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_STOP, &data));
            self.text_open = false;
        }
    }

    pub fn emit_thinking_delta(&mut self, text: &str) {
        self.open_thinking();
        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": self.thinking_index,
            "delta": {
                "type": "thinking_delta",
                "thinking": text
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_DELTA, &data));
    }

    pub fn emit_text_delta(&mut self, text: &str) {
        // Close thinking block if open before starting text
        if self.thinking_open {
            let data = serde_json::json!({
                "type": "content_block_stop",
                "index": self.thinking_index
            });
            self.output
                .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_STOP, &data));
            self.thinking_open = false;
        }
        self.open_text();
        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": self.text_index,
            "delta": {
                "type": "text_delta",
                "text": text
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_DELTA, &data));
    }

    pub fn record_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        _cache_read_tokens: u64,
        _cache_write_tokens: u64,
    ) {
        self.usage_input_tokens = input_tokens;
        self.usage_output_tokens = output_tokens;
    }

    pub fn next_content_block_index(&mut self) -> i32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// Emit a tool_use pause: content block + message_delta with
    /// stop_reason="tool_use" + message_stop.
    pub fn emit_tool_pause(&mut self, tool_use_id: &str, tool_name: &str, partial_json: &str) {
        self.close_open_blocks();
        self.ensure_start();
        let index = self.next_content_block_index();

        let data = serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "tool_use",
                "id": tool_use_id,
                "name": tool_name,
                "input": {}
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_START, &data));

        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {
                "type": "input_json_delta",
                "partial_json": partial_json
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_DELTA, &data));

        let data = serde_json::json!({
            "type": "content_block_stop",
            "index": index
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_CONTENT_BLOCK_STOP, &data));

        self.emit_final_message("tool_use");
    }

    pub fn emit_final_message(&mut self, stop_reason: &str) {
        if self.finalized {
            return;
        }
        self.ensure_start();
        self.close_open_blocks();

        // message_delta
        let data = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null
            },
            "usage": {
                "input_tokens": self.usage_input_tokens.max(1),
                "output_tokens": self.usage_output_tokens,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_MESSAGE_DELTA, &data));

        // message_stop
        let data = serde_json::json!({
            "type": "message_stop"
        });
        self.output
            .extend_from_slice(&format_sse_event_bytes(EVENT_MESSAGE_STOP, &data));

        self.finalized = true;
    }

    pub fn finalize(&mut self) {
        if !self.finalized {
            self.ensure_start();
            self.close_open_blocks();
            self.emit_final_message("end_turn");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::cursor::test_frames;

    #[test]
    fn sse_produces_message_start_and_stop() {
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("hello"));
        body.extend_from_slice(&test_frames::usage_frame(10, 5));
        body.extend_from_slice(&test_frames::end_frame());

        let upstream = CursorUpstreamResponse {
            status: 200,
            body,
            error_detail: None,
        };

        let sse = frame_cursor_stream(&upstream, "msg_1", "cursor-test");
        let sse_str = String::from_utf8_lossy(&sse);

        // Verify event structure with explicit parsing
        let events = parse_sse_events(&sse_str);
        let event_names: Vec<&str> = events.iter().map(|e| e.0.as_str()).collect();

        assert_eq!(
            event_names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
    }

    #[test]
    fn sse_includes_text_delta_content() {
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("Hello world"));
        body.extend_from_slice(&test_frames::usage_frame(10, 2));
        body.extend_from_slice(&test_frames::end_frame());

        let upstream = CursorUpstreamResponse {
            status: 200,
            body,
            error_detail: None,
        };

        let sse = frame_cursor_stream(&upstream, "msg_1", "cursor-test");
        let sse_str = String::from_utf8_lossy(&sse);
        let events = parse_sse_events(&sse_str);

        // Find text_delta event
        let text_delta = events
            .iter()
            .find(|(name, _)| *name == "content_block_delta")
            .map(|(_, data)| data["delta"]["text"].as_str().unwrap_or(""));
        assert_eq!(text_delta, Some("Hello world"));
    }

    #[test]
    fn sse_includes_usage_in_message_delta() {
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("hi"));
        body.extend_from_slice(&test_frames::usage_frame(25, 7));
        body.extend_from_slice(&test_frames::end_frame());

        let upstream = CursorUpstreamResponse {
            status: 200,
            body,
            error_detail: None,
        };

        let sse = frame_cursor_stream(&upstream, "msg_1", "cursor-test");
        let sse_str = String::from_utf8_lossy(&sse);
        let events = parse_sse_events(&sse_str);

        let msg_delta = events
            .iter()
            .find(|(name, _)| *name == "message_delta")
            .map(|(_, data)| data.clone());
        assert!(msg_delta.is_some());
        let delta = msg_delta.unwrap();
        assert_eq!(delta["usage"]["input_tokens"].as_u64(), Some(25));
        assert_eq!(delta["usage"]["output_tokens"].as_u64(), Some(7));
        assert_eq!(
            delta["usage"]["cache_creation_input_tokens"].as_u64(),
            Some(0)
        );
        assert_eq!(delta["usage"]["cache_read_input_tokens"].as_u64(), Some(0));
    }

    #[test]
    fn sse_handles_empty_upstream() {
        let upstream = CursorUpstreamResponse {
            status: 200,
            body: Vec::new(),
            error_detail: None,
        };

        let sse = frame_cursor_stream(&upstream, "msg_1", "cursor-test");
        let sse_str = String::from_utf8_lossy(&sse);

        // Should still produce events even with empty body
        let events = parse_sse_events(&sse_str);
        let event_names: Vec<&str> = events.iter().map(|e| e.0.as_str()).collect();
        assert!(event_names.contains(&"message_start"));
        assert!(event_names.contains(&"message_stop"));
    }

    #[test]
    fn sse_emits_thinking_before_text() {
        let mut body = test_frames::thinking_frame("thinking...");

        body.extend_from_slice(&test_frames::text_frame("result"));
        body.extend_from_slice(&test_frames::usage_frame(10, 5));
        body.extend_from_slice(&test_frames::end_frame());

        let upstream = CursorUpstreamResponse {
            status: 200,
            body,
            error_detail: None,
        };

        let sse = frame_cursor_stream(&upstream, "msg_1", "cursor-test");
        let sse_str = String::from_utf8_lossy(&sse);
        let events = parse_sse_events(&sse_str);
        assert!(events.iter().any(|(_, data)| {
            data.get("content_block")
                .and_then(|c| c.get("type"))
                .and_then(|t| t.as_str())
                == Some("thinking")
        }));

        // Should have text content block
        assert!(events.iter().any(|(_, data)| {
            data.get("content_block")
                .and_then(|c| c.get("type"))
                .and_then(|t| t.as_str())
                == Some("text")
        }));
    }

    #[test]
    fn sse_error_response() {
        let sse = format_sse_error("something broke");
        let sse_str = String::from_utf8_lossy(&sse);
        let events = parse_sse_events(&sse_str);

        let (name, data) = &events[0];
        assert_eq!(name, "error");
        assert_eq!(data["error"]["type"], "api_error");
        assert_eq!(data["error"]["message"], "something broke");
    }

    // -----------------------------------------------------------------------
    // SSE parser helper for tests
    // -----------------------------------------------------------------------

    pub fn parse_sse_events(sse: &str) -> Vec<(String, serde_json::Value)> {
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
}
