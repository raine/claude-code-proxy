use super::reducer::{Reducer, ReducerEvent};
use super::search_text::search_line;
use crate::anthropic::sse::{SseEvent, encode_sse_event};
use crate::config::GrokSearchBlocks;

pub const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct SseDecoder {
    frame: Vec<u8>,
    line_start: usize,
    skip_lf: bool,
}

impl SseDecoder {
    pub fn push(&mut self, input: &[u8]) -> anyhow::Result<Vec<SseEvent>> {
        let mut events = Vec::new();
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                _ => self.push_byte(byte)?,
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        if self.frame.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("Grok SSE stream ended with an incomplete frame")
        }
    }

    fn push_byte(&mut self, byte: u8) -> anyhow::Result<()> {
        if self.frame.len() >= MAX_SSE_FRAME_BYTES {
            anyhow::bail!("Grok SSE frame exceeds the size limit");
        }
        self.frame.push(byte);
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> anyhow::Result<()> {
        if self.frame.len() == self.line_start {
            if !self.frame.is_empty()
                && let Some(event) = parse_frame(&self.frame)?
            {
                events.push(event);
            }
            self.frame.clear();
            self.line_start = 0;
            return Ok(());
        }
        self.push_byte(b'\n')?;
        self.line_start = self.frame.len();
        Ok(())
    }
}

fn parse_frame(frame: &[u8]) -> anyhow::Result<Option<SseEvent>> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| anyhow::anyhow!("Grok SSE frame contains invalid UTF-8"))?;
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseEvent {
        event,
        data: data.join("\n"),
    }))
}

pub struct StreamTranslator {
    message_id: String,
    model: String,
    started: bool,
    finished: bool,
    search_blocks: GrokSearchBlocks,
    // Seeds Claude Code's live usage counter until the provider returns
    // authoritative usage in the terminal message_delta.
    estimated_input_tokens: u64,
}

pub struct LiveStreamTranslator {
    decoder: SseDecoder,
    reducer: Reducer,
    renderer: StreamTranslator,
}

impl LiveStreamTranslator {
    pub fn new(message_id: String, model: String) -> Self {
        Self::with_estimated_input_tokens(message_id, model, 0)
    }

    pub fn with_estimated_input_tokens(
        message_id: String,
        model: String,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            decoder: SseDecoder::default(),
            reducer: Reducer::default(),
            renderer: StreamTranslator::new(message_id, model)
                .with_estimated_input_tokens(estimated_input_tokens),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        for event in self.decoder.push(chunk)? {
            let value = serde_json::from_str(&event.data)
                .map_err(|_| anyhow::anyhow!("malformed Grok SSE event"))?;
            out.extend(self.renderer.render(self.reducer.push(value)?)?);
        }
        Ok(out)
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        self.decoder.finish()?;
        if !self.reducer.finished() {
            anyhow::bail!("Grok stream ended without completion");
        }
        Ok(())
    }
}

impl StreamTranslator {
    pub fn new(message_id: String, model: String) -> Self {
        Self::with_search_blocks(message_id, model, crate::config::grok_search_blocks())
    }

    /// Tests choose the shape directly rather than through the environment.
    pub fn with_search_blocks(
        message_id: String,
        model: String,
        search_blocks: GrokSearchBlocks,
    ) -> Self {
        Self {
            message_id,
            model,
            started: false,
            finished: false,
            search_blocks,
            estimated_input_tokens: 0,
        }
    }

    pub fn with_estimated_input_tokens(mut self, estimated_input_tokens: u64) -> Self {
        self.estimated_input_tokens = estimated_input_tokens;
        self
    }

    pub fn render(&mut self, events: Vec<ReducerEvent>) -> anyhow::Result<Vec<u8>> {
        if self.finished && !events.is_empty() {
            anyhow::bail!("event after terminal completion");
        }
        let mut out = Vec::new();
        for event in events {
            if !self.started
                && matches!(
                    event,
                    ReducerEvent::ThinkingStart(_)
                        | ReducerEvent::TextStart(_)
                        | ReducerEvent::ToolStart(_, _, _)
                        | ReducerEvent::HostedSearch { .. }
                        | ReducerEvent::Finish { .. }
                )
            {
                self.started = true;
                emit(
                    &mut out,
                    "message_start",
                    serde_json::json!({"type":"message_start","message":{"id":self.message_id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":self.estimated_input_tokens,"output_tokens":0}}}),
                );
            }
            if matches!(event, ReducerEvent::Finish { .. }) {
                self.finished = true;
            }
            render(&mut out, event, self.search_blocks);
        }
        Ok(out)
    }
}

pub fn translate_stream_bytes(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> anyhow::Result<Vec<u8>> {
    translate_stream_bytes_with_blocks(
        upstream,
        message_id,
        model,
        crate::config::grok_search_blocks(),
    )
}

/// Tests choose the hosted-search block shape directly rather than through the
/// environment.
pub fn translate_stream_bytes_with_blocks(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    search_blocks: GrokSearchBlocks,
) -> anyhow::Result<Vec<u8>> {
    let mut decoder = SseDecoder::default();
    let mut reducer = Reducer::default();
    let mut translator =
        StreamTranslator::with_search_blocks(message_id.into(), model.into(), search_blocks);
    let mut out = Vec::new();
    for event in decoder.push(upstream)? {
        let value = serde_json::from_str(&event.data)
            .map_err(|_| anyhow::anyhow!("malformed Grok SSE event"))?;
        out.extend(translator.render(reducer.push(value)?)?);
    }
    decoder.finish()?;
    if !reducer.finished() {
        anyhow::bail!("Grok stream ended without completion");
    }
    Ok(out)
}

pub fn stream_error() -> Vec<u8> {
    let data = serde_json::json!({"type":"error","error":{"type":"api_error","message":"Grok stream is invalid"}});
    encode_sse_event(Some("error"), &data.to_string())
}

fn emit(out: &mut Vec<u8>, event: &str, data: serde_json::Value) {
    out.extend(encode_sse_event(Some(event), &data.to_string()));
}

fn render(out: &mut Vec<u8>, event: ReducerEvent, search_blocks: GrokSearchBlocks) {
    match event {
        ReducerEvent::ThinkingStart(i) => emit(
            out,
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":i,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ),
        ReducerEvent::ThinkingDelta(i, t) => emit(
            out,
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"thinking_delta","thinking":t}}),
        ),
        ReducerEvent::ThinkingStop(i) | ReducerEvent::TextStop(i) | ReducerEvent::ToolStop(i) => {
            emit(
                out,
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":i}),
            )
        }
        ReducerEvent::TextStart(i) => emit(
            out,
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":i,"content_block":{"type":"text","text":""}}),
        ),
        ReducerEvent::TextDelta(i, t) => emit(
            out,
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":t}}),
        ),
        ReducerEvent::ToolStart(i, id, name) => emit(
            out,
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
        ),
        ReducerEvent::ToolDelta(i, t) => emit(
            out,
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":t}}),
        ),
        ReducerEvent::HostedSearch {
            index,
            result_index,
            id,
            name,
            query,
        } if search_blocks == GrokSearchBlocks::Text => {
            // Two text blocks, because the reducer already allocated two
            // indices for this search and a gap in the sequence would leave a
            // hole in the client's block array. The line goes in the first;
            // the second stays empty and draws as nothing.
            emit(
                out,
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
            );
            emit(
                out,
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":search_line(&name,&query)}}),
            );
            emit(
                out,
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":index}),
            );
            emit(
                out,
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":result_index,"content_block":{"type":"text","text":""}}),
            );
            emit(
                out,
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":result_index}),
            );
            let _ = id;
        }
        ReducerEvent::HostedSearch {
            index,
            result_index,
            id,
            name,
            query,
        } => {
            let result_type = format!("{name}_tool_result");
            emit(
                out,
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":index,"content_block":{"type":"server_tool_use","id":id,"name":name,"input":{}}}),
            );
            emit(
                out,
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":serde_json::json!({"query":query}).to_string()}}),
            );
            emit(
                out,
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":index}),
            );
            emit(
                out,
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":result_index,"content_block":{"type":result_type,"tool_use_id":id,"content":[]}}),
            );
            emit(
                out,
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":result_index}),
            );
        }
        ReducerEvent::Citation(i, annotation) => {
            let citation = serde_json::json!({
                "type":"web_search_result_location",
                "url":annotation.get("url").and_then(serde_json::Value::as_str).unwrap_or_default(),
                "title":annotation.get("title").and_then(serde_json::Value::as_str).unwrap_or_default(),
                "cited_text":annotation.get("text").and_then(serde_json::Value::as_str).unwrap_or_default()
            });
            emit(
                out,
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"citations_delta","citation":citation}}),
            );
        }
        ReducerEvent::Finish {
            stop_reason,
            input_tokens,
            output_tokens,
            web_search_requests,
            x_search_requests,
        } => {
            let hosted_search_requests = web_search_requests + x_search_requests;
            // Emit input usage only when the provider actually reported it.
            // Claude Code keeps the seeded message_start value when the field
            // is absent, whereas a synthesized zero would erase the estimate.
            let mut usage = serde_json::json!({"output_tokens":output_tokens,"server_tool_use":{"web_search_requests":hosted_search_requests,"x_search_requests":x_search_requests}});
            if let Some(input_tokens) = input_tokens {
                usage["input_tokens"] = serde_json::json!(input_tokens);
            }
            emit(
                out,
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":usage}),
            );
            emit(
                out,
                "message_stop",
                serde_json::json!({"type":"message_stop"}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_accepts_every_boundary_and_line_ending() {
        let input = b": note\r\nevent: ignored\r\ndata: first\r\ndata: second\r\n\r\n";
        let expected = vec![SseEvent {
            event: Some("ignored".into()),
            data: "first\nsecond".into(),
        }];
        for split in 0..=input.len() {
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&input[..split]).unwrap();
            events.extend(decoder.push(&input[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(events, expected);
        }
    }

    #[test]
    fn decoder_ignores_data_less_frames_at_every_boundary() {
        let input = b": keepalive\n\nid: 42\n\nevent: ignored\n\nretry: 5000\n\ndata: complete\n\n";
        let expected = vec![SseEvent {
            event: None,
            data: "complete".into(),
        }];
        for split in 0..=input.len() {
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&input[..split]).unwrap();
            events.extend(decoder.push(&input[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(events, expected);
        }
    }

    #[test]
    fn decoder_requires_terminated_valid_frames_and_bounds_them() {
        assert!(SseDecoder::default().push(b"data: \xff\n\n").is_err());
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: incomplete").unwrap();
        assert!(decoder.finish().is_err());
        let mut decoder = SseDecoder::default();
        let exact = vec![b'x'; MAX_SSE_FRAME_BYTES - b"data: \n".len()];
        assert!(decoder.push(b"data: ").is_ok());
        assert!(decoder.push(&exact).is_ok());
        let events = decoder.push(b"\n\n").unwrap();
        assert_eq!(events[0].data.len(), exact.len());
        decoder.finish().unwrap();
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: ").is_ok());
        assert!(decoder.push(&vec![b'x'; exact.len() + 1]).is_ok());
        assert!(decoder.push(b"\n").is_err());
    }

    #[test]
    fn stream_translates_hosted_web_search_and_citations() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\ndata: {\"type\":\"response.web_search_call.in_progress\",\"item_id\":\"ws_1\"}\n\ndata: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws_1\"}\n\ndata: {\"type\":\"response.web_search_call.completed\",\"item_id\":\"ws_1\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"action\":{\"query\":\"rust news\"}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Result\"}\n\ndata: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://example.com\",\"title\":\"Example\"}}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";
        let output = String::from_utf8(
            translate_stream_bytes_with_blocks(
                input,
                "msg_1",
                "grok-4.5",
                GrokSearchBlocks::Native,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(output.contains("server_tool_use"));
        assert!(output.contains("web_search_tool_result"));
        assert!(output.contains("citations_delta"));
        assert!(output.contains("https://example.com"));
        assert!(output.contains("\"web_search_requests\":1"));

        let text = String::from_utf8(
            translate_stream_bytes_with_blocks(input, "msg_1", "grok-4.5", GrokSearchBlocks::Text)
                .unwrap(),
        )
        .unwrap();
        // The usage object legitimately carries a `server_tool_use` counter, so
        // match the block type rather than the bare word.
        assert!(!text.contains("\"type\":\"server_tool_use\""));
        assert!(!text.contains("web_search_tool_result"));
        assert!(text.contains("[web search: rust news]"));
        // Citations and usage are untouched by the block shape.
        assert!(text.contains("citations_delta"));
        assert!(text.contains("https://example.com"));
        assert!(text.contains("\"web_search_requests\":1"));
    }

    #[test]
    fn text_search_blocks_keep_the_block_index_sequence_contiguous() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"action\":{\"query\":\"rust news\"}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Result\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";
        let output = String::from_utf8(
            translate_stream_bytes_with_blocks(input, "msg_1", "grok-4.5", GrokSearchBlocks::Text)
                .unwrap(),
        )
        .unwrap();
        // The reducer allocates two indices per search. A client that stores
        // blocks by index would be left with a hole if only one were emitted.
        let started: Vec<u64> = output
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .filter(|event| event["type"] == "content_block_start")
            .filter_map(|event| event["index"].as_u64())
            .collect();
        assert_eq!(started, vec![0, 1, 2], "block indices are not contiguous");
    }

    #[test]
    fn stream_translates_hosted_x_search_usage_and_citations() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"xs_1\"}}\n\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"xs_1\",\"delta\":\"{\\\"query\\\":\\\"claude-code-proxy\\\"}\"}\n\ndata: {\"type\":\"response.custom_tool_call_input.done\",\"item_id\":\"xs_1\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"xs_1\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Recent post\"}\n\ndata: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://x.com/example/status/1\",\"title\":\"Example post\"}}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":3}}}\n\n";
        let output = String::from_utf8(
            translate_stream_bytes_with_blocks(
                input,
                "msg_1",
                "grok-4.5",
                GrokSearchBlocks::Native,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(output.contains("\"name\":\"x_search\""));
        assert!(output.contains("x_search_tool_result"));
        assert!(output.contains("https://x.com/example/status/1"));
        assert!(output.contains("\"web_search_requests\":1"));
        assert!(output.contains("\"x_search_requests\":1"));
        assert!(!output.contains("\"name\":\"Bash\""));

        let text = String::from_utf8(
            translate_stream_bytes_with_blocks(input, "msg_1", "grok-4.5", GrokSearchBlocks::Text)
                .unwrap(),
        )
        .unwrap();
        assert!(!text.contains("x_search_tool_result"));
        // The usage object legitimately carries a `server_tool_use` counter, so
        // match the block type rather than the bare word.
        assert!(!text.contains("\"type\":\"server_tool_use\""));
        assert!(text.contains("[X search: claude-code-proxy]"));
        assert!(text.contains("https://x.com/example/status/1"));
        assert!(text.contains("\"x_search_requests\":1"));
    }

    #[test]
    fn live_translator_emits_first_event_before_upstream_completion() {
        let mut translator = LiveStreamTranslator::new("msg_1".into(), "grok-4.5".into());
        let output = translator
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n")
            .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("first"));
    }

    #[test]
    fn estimated_input_is_visible_at_start_and_provider_usage_is_exact_at_finish() {
        let mut translator = LiveStreamTranslator::with_estimated_input_tokens(
            "msg_1".into(),
            "grok-4.5".into(),
            321,
        );
        let started = translator
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n")
            .unwrap();
        let started = crate::anthropic::sse::parse_sse_events(&started)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_start")
            })
            .unwrap();
        assert_eq!(
            started.pointer("/message/usage/input_tokens"),
            Some(&serde_json::json!(321))
        );

        let finished = translator
            .push(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n")
            .unwrap();
        let finished = crate::anthropic::sse::parse_sse_events(&finished)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_delta")
            })
            .unwrap();
        assert_eq!(
            finished.pointer("/usage/input_tokens"),
            Some(&serde_json::json!(12))
        );
        assert_eq!(
            finished.pointer("/usage/output_tokens"),
            Some(&serde_json::json!(3))
        );
    }

    #[test]
    fn provider_input_tokens_are_visible_on_message_delta() {
        let mut translator = LiveStreamTranslator::new("msg_1".into(), "grok-4.5".into());
        let _ = translator
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n")
            .unwrap();
        let finished = translator
            .push(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n")
            .unwrap();
        let finished = crate::anthropic::sse::parse_sse_events(&finished)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_delta")
            })
            .unwrap();
        assert_eq!(
            finished.pointer("/usage/input_tokens"),
            Some(&serde_json::json!(12))
        );
        assert_eq!(
            finished.pointer("/usage/output_tokens"),
            Some(&serde_json::json!(3))
        );
    }

    /// Renders one estimated stream and returns the input usage seeded at
    /// `message_start`, plus the input and output usage published at
    /// `message_delta`. `None` means the field was omitted downstream.
    fn streamed_usage(completed_response: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
        let mut translator = LiveStreamTranslator::with_estimated_input_tokens(
            "msg_1".into(),
            "grok-4.5".into(),
            321,
        );
        let started = translator
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n")
            .unwrap();
        let started = crate::anthropic::sse::parse_sse_events(&started)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_start")
            })
            .unwrap();
        let payload = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{completed_response}}}\n\n"
        );
        let finished = translator.push(payload.as_bytes()).unwrap();
        let finished = crate::anthropic::sse::parse_sse_events(&finished)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_delta")
            })
            .unwrap();
        let value = |pointer: &str| {
            finished
                .pointer(pointer)
                .and_then(serde_json::Value::as_u64)
        };
        (
            started
                .pointer("/message/usage/input_tokens")
                .and_then(serde_json::Value::as_u64),
            value("/usage/input_tokens"),
            value("/usage/output_tokens"),
        )
    }

    #[test]
    fn missing_provider_input_usage_keeps_the_seeded_estimate() {
        // Exact provider usage replaces the estimate.
        assert_eq!(
            streamed_usage("{\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}"),
            (Some(321), Some(12), Some(3))
        );
        // A provider-reported zero is a real value and replaces the estimate.
        assert_eq!(
            streamed_usage("{\"usage\":{\"input_tokens\":0,\"output_tokens\":3}}"),
            (Some(321), Some(0), Some(3))
        );
        // Absent, empty, output-only, and non-numeric input usage must not
        // publish a zero. The terminal delta omits the field so the client
        // keeps the seeded estimate, while output usage still flows.
        assert_eq!(streamed_usage("{\"usage\":{}}"), (Some(321), None, Some(0)));
        assert_eq!(
            streamed_usage("{\"usage\":{\"output_tokens\":3}}"),
            (Some(321), None, Some(3))
        );
        assert_eq!(streamed_usage("{}"), (Some(321), None, Some(0)));
        assert_eq!(
            streamed_usage("{\"usage\":{\"input_tokens\":\"12\"}}"),
            (Some(321), None, Some(0))
        );
    }
}
