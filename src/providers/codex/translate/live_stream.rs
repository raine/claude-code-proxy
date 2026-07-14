use std::collections::HashMap;

use crate::anthropic::sse::encode_sse_event;
use crate::traffic::TrafficCapture;

use super::read_rewrite::sanitize_read_args;
use super::reasoning_signature::{PendingReasoning, encode_reasoning_signature};
use super::reducer::{
    CodexUsage, STOP_END_TURN, STOP_MAX_TOKENS, STOP_TOOL_USE, map_codex_usage_to_anthropic,
};

const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES: usize = 1_024;
const BUFFERED_TOOL_MAX_ARGS_BYTES: usize = 5_000_000;

enum LiveBlock {
    Text {
        index: usize,
        text: String,
        deferred: bool,
    },
    Tool {
        index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        had_delta: bool,
        buffer_until_done: bool,
        emitted_args: bool,
    },
}

struct LiveWebSearch {
    index: usize,
    result_index: usize,
    id: String,
    query: String,
}

#[derive(Clone)]
struct LiveWebSearchResult {
    title: String,
    url: String,
}

#[derive(Clone, Copy)]
struct LiveThinking {
    output_index: usize,
    anthropic_index: usize,
}

pub struct LiveStreamTranslator {
    message_id: String,
    model: String,
    message_started: bool,
    blocks_by_output_index: HashMap<usize, LiveBlock>,
    item_id_to_output_index: HashMap<String, usize>,
    anthropic_index: usize,
    thinking: Option<LiveThinking>,
    reasoning_by_output_index: HashMap<usize, PendingReasoning>,
    saw_tool_use: bool,
    web_search_requests: usize,
    web_searches: Vec<LiveWebSearch>,
    web_search_results: Vec<LiveWebSearchResult>,
    deferred_text: Vec<(usize, String)>,
    finished: bool,
}

impl LiveStreamTranslator {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            message_started: false,
            blocks_by_output_index: HashMap::new(),
            item_id_to_output_index: HashMap::new(),
            anthropic_index: 0,
            thinking: None,
            reasoning_by_output_index: HashMap::new(),
            saw_tool_use: false,
            web_search_requests: 0,
            web_searches: Vec::new(),
            web_search_results: Vec::new(),
            deferred_text: Vec::new(),
            finished: false,
        }
    }

    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }

        let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = Vec::new();

        match kind {
            "codex.rate_limits" => {
                if payload
                    .get("rate_limits")
                    .and_then(|r| r.get("limit_reached"))
                    .and_then(|v| v.as_bool())
                    == Some(true)
                {
                    return Err("rate limit reached".to_string());
                }
            }
            "keepalive" => {}
            "response.failed" | "response.error" | "error" => {
                return Err(error_message(payload));
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {}
            "response.output_item.added" => {
                self.output_item_added(payload, traffic, &mut out);
            }
            "response.reasoning_summary_part.added" => {
                let output_index = output_index(payload);
                if let Some(thinking) = self
                    .thinking
                    .filter(|thinking| thinking.output_index == output_index)
                {
                    self.emit(
                        traffic,
                        &mut out,
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": thinking.anthropic_index,
                            "delta": {"type": "thinking_delta", "thinking": "\n\n"}
                        }),
                    );
                }
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(payload, traffic, &mut out);
            }
            "response.output_text.delta" => {
                self.text_delta(payload, traffic, &mut out);
            }
            "response.output_text.annotation.added" => {
                self.web_search_annotation(payload);
            }
            "response.function_call_arguments.delta" => {
                self.tool_delta(payload, traffic, &mut out)?;
            }
            "response.function_call_arguments.done" => {
                self.tool_arguments_done(payload);
            }
            "response.output_item.done" => {
                self.output_item_done(payload, traffic, &mut out);
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finish(payload, traffic, &mut out);
            }
            _ => {}
        }

        Ok(out)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn finish_after_closed_completed_tool_call(
        &mut self,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished || !self.saw_tool_use || !self.blocks_by_output_index.is_empty() {
            return out;
        }
        self.close_thinking(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        self.emit_finish(STOP_TOOL_USE, None, traffic, &mut out);
        out
    }

    pub fn error_chunk(
        &mut self,
        message: &str,
        error_type: &str,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.close_open_blocks(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        self.emit(
            traffic,
            &mut out,
            "error",
            &serde_json::json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message,
                }
            }),
        );
        self.finished = true;
        out
    }

    fn ensure_message_start(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        self.emit(
            traffic,
            out,
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0
                    }
                }
            }),
        );
    }

    fn emit(
        &self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
        event: &str,
        data: &serde_json::Value,
    ) {
        if let Some(traffic) = traffic {
            traffic.write_json_event(
                "050-downstream-event",
                &serde_json::json!({
                    "event": event,
                    "data": data,
                }),
            );
        }
        out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
    }

    fn output_item_added(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(item) = payload.get("item") else {
            return;
        };
        let output_index = output_index(payload);
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match item_type {
            "reasoning" => {
                self.reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
            }
            "message" => {
                self.close_thinking(traffic, out);
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    self.item_id_to_output_index
                        .insert(id.to_string(), output_index);
                }
                let deferred = !self.web_searches.is_empty();
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Text {
                        index,
                        text: String::new(),
                        deferred,
                    },
                );
                if !deferred {
                    self.ensure_message_start(traffic, out);
                    self.emit(
                        traffic,
                        out,
                        "content_block_start",
                        &serde_json::json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    );
                }
            }
            "function_call" => {
                self.close_thinking(traffic, out);
                self.saw_tool_use = true;
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Tool {
                        index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        had_delta: false,
                        buffer_until_done: name == "Read",
                        emitted_args: false,
                    },
                );
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": {}
                        }
                    }),
                );
            }
            "web_search_call" => {
                self.web_search_requests += 1;
            }
            _ => {}
        }
    }

    fn reasoning_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }
        if self.thinking.map(|thinking| thinking.output_index) != Some(output_index) {
            self.close_thinking(traffic, out);
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            self.thinking = Some(LiveThinking {
                output_index,
                anthropic_index: index,
            });
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                }),
            );
        }
        let index = self
            .thinking
            .expect("thinking block was started")
            .anthropic_index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        );
    }

    fn text_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }

        let output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or_else(|| {
                payload
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| self.item_id_to_output_index.get(id).copied())
            })
            .unwrap_or(0);

        if !self.blocks_by_output_index.contains_key(&output_index) {
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            let deferred = !self.web_searches.is_empty();
            self.blocks_by_output_index.insert(
                output_index,
                LiveBlock::Text {
                    index,
                    text: String::new(),
                    deferred,
                },
            );
            if !deferred {
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                );
            }
        }

        let Some(LiveBlock::Text {
            index,
            text,
            deferred,
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return;
        };
        text.push_str(delta);
        if *deferred {
            return;
        }
        let index = *index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        );
    }

    fn tool_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return Ok(());
        };
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return Ok(());
        }
        let mut repaired_read: Option<(usize, String)> = None;
        let Some(LiveBlock::Tool {
            index,
            call_id,
            name,
            args_accum,
            had_delta,
            buffer_until_done,
            emitted_args,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Ok(());
        };
        args_accum.push_str(delta);
        *had_delta = true;
        if *buffer_until_done {
            if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                return Err(format!(
                    "Buffered {name} tool arguments exceeded safe limits"
                ));
            }
            if let Some(repaired) =
                repair_whitespace_stalled_read_args(name, args_accum, Some(call_id.as_str()))
            {
                *args_accum = repaired.clone();
                *emitted_args = true;
                repaired_read = Some((*index, repaired));
            }
        } else {
            *emitted_args = true;
            let index = *index;
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": delta
                    }
                }),
            );
            return Ok(());
        }
        if let Some((index, repaired)) = repaired_read {
            self.blocks_by_output_index.remove(&output_index);
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": repaired
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            self.ensure_message_start(traffic, out);
            self.emit_finish(STOP_TOOL_USE, None, traffic, out);
        }
        Ok(())
    }

    fn tool_arguments_done(&mut self, payload: &serde_json::Value) {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return;
        };
        let Some(args) = payload.get("arguments").and_then(|v| v.as_str()) else {
            return;
        };
        let Some(LiveBlock::Tool { args_accum, .. }) =
            self.blocks_by_output_index.get_mut(&output_index)
        else {
            return;
        };
        if args_accum.is_empty() {
            *args_accum = args.to_string();
        }
    }

    fn output_item_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        if let Some(item) = payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            .filter(|item_type| *item_type == "reasoning")
            .and_then(|_| payload.get("item"))
        {
            self.reasoning_by_output_index
                .entry(output_index)
                .or_default()
                .capture(item);
            let had_active_summary = self
                .thinking
                .is_some_and(|thinking| thinking.output_index == output_index);
            self.close_thinking(traffic, out);
            if !had_active_summary {
                self.emit_signature_only_reasoning(output_index, traffic, out);
            }
            return;
        }

        if payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            == Some("web_search_call")
        {
            self.close_thinking(traffic, out);
            let item = &payload["item"];
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            let result_index = self.anthropic_index;
            self.anthropic_index += 1;
            let raw_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            self.web_searches.push(LiveWebSearch {
                index,
                result_index,
                id: super::web_search_compat::server_tool_use_id_from_codex_web_search_id(raw_id),
                query: web_search_query(item),
            });
            return;
        }

        let Some(mut state) = self.blocks_by_output_index.remove(&output_index) else {
            return;
        };

        match &mut state {
            LiveBlock::Text {
                index,
                text,
                deferred,
            } => {
                if *deferred {
                    self.deferred_text.push((*index, std::mem::take(text)));
                } else {
                    self.emit(
                        traffic,
                        out,
                        "content_block_stop",
                        &serde_json::json!({
                            "type": "content_block_stop",
                            "index": index,
                        }),
                    );
                }
            }
            LiveBlock::Tool {
                index,
                name,
                call_id,
                args_accum,
                had_delta,
                buffer_until_done,
                emitted_args,
                ..
            } => {
                if let Some(final_args) = payload
                    .get("item")
                    .and_then(|item| item.get("arguments"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    && (args_accum.is_empty() || (!*had_delta && !*emitted_args))
                {
                    *args_accum = final_args.to_string();
                }
                if !args_accum.is_empty() {
                    *args_accum = sanitize_read_args(name, args_accum, Some(call_id.as_str()));
                    if *buffer_until_done || !*emitted_args {
                        *emitted_args = true;
                        self.emit(
                            traffic,
                            out,
                            "content_block_delta",
                            &serde_json::json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": args_accum
                                }
                            }),
                        );
                    }
                }
                self.emit(
                    traffic,
                    out,
                    "content_block_stop",
                    &serde_json::json!({
                        "type": "content_block_stop",
                        "index": index,
                    }),
                );
            }
        }
    }

    fn web_search_annotation(&mut self, payload: &serde_json::Value) {
        let Some(annotation) = payload.get("annotation") else {
            return;
        };
        if annotation.get("type").and_then(|v| v.as_str()) != Some("url_citation") {
            return;
        }
        let Some(url) = annotation.get("url").and_then(|v| v.as_str()) else {
            return;
        };
        if self
            .web_search_results
            .iter()
            .any(|result| result.url == url)
        {
            return;
        }
        let title = annotation
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        self.web_search_results.push(LiveWebSearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }

    fn emit_web_searches(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let searches = std::mem::take(&mut self.web_searches);
        for search in searches {
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": search.index,
                    "content_block": {
                        "type": "server_tool_use",
                        "id": search.id,
                        "name": "web_search",
                        "input": {}
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": search.index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": serde_json::to_string(&serde_json::json!({"query": search.query})).unwrap_or_default()
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": search.index}),
            );
            let results: Vec<_> = self
                .web_search_results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "type": "web_search_result",
                        "title": result.title,
                        "url": result.url,
                    })
                })
                .collect();
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": search.result_index,
                    "content_block": {
                        "type": "web_search_tool_result",
                        "tool_use_id": search.id,
                        "content": results
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": search.result_index}),
            );
        }

        for (index, text) in std::mem::take(&mut self.deferred_text) {
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
            if !text.is_empty() {
                self.emit(
                    traffic,
                    out,
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                );
            }
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
        }
    }

    fn finish(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        self.close_open_blocks(traffic, out);
        self.emit_web_searches(traffic, out);
        self.ensure_message_start(traffic, out);
        let usage = payload.get("response").map(parse_codex_usage);
        let incomplete = response_is_incomplete(payload);
        let stop_reason = if incomplete {
            STOP_MAX_TOKENS
        } else if self.saw_tool_use {
            STOP_TOOL_USE
        } else {
            STOP_END_TURN
        };
        self.emit_finish(stop_reason, usage, traffic, out);
    }

    fn emit_finish(
        &mut self,
        stop_reason: &str,
        usage: Option<CodexUsage>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let mapped = map_codex_usage_to_anthropic(&usage, Some(self.web_search_requests));
        self.emit(
            traffic,
            out,
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": mapped,
            }),
        );
        self.emit(
            traffic,
            out,
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
        self.finished = true;
    }

    fn close_open_blocks(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.close_thinking(traffic, out);
        let open: Vec<usize> = self.blocks_by_output_index.keys().copied().collect();
        for output_index in open {
            let Some(state) = self.blocks_by_output_index.remove(&output_index) else {
                continue;
            };
            let index = match state {
                LiveBlock::Text {
                    index,
                    text,
                    deferred: true,
                } => {
                    self.deferred_text.push((index, text));
                    continue;
                }
                LiveBlock::Text { index, .. } => index,
                LiveBlock::Tool { index, .. } => index,
            };
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
        }
    }

    fn emit_signature_only_reasoning(
        &mut self,
        output_index: usize,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(replay) = self
            .reasoning_by_output_index
            .remove(&output_index)
            .and_then(|pending| pending.replay())
        else {
            return;
        };
        let Some(signature) = encode_reasoning_signature(&replay) else {
            return;
        };
        let index = self.anthropic_index;
        self.anthropic_index += 1;
        self.ensure_message_start(traffic, out);
        self.emit(
            traffic,
            out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "signature_delta", "signature": signature}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": index,
            }),
        );
    }

    fn close_thinking(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let Some(thinking) = self.thinking.take() else {
            return;
        };
        if let Some(signature) = self
            .reasoning_by_output_index
            .remove(&thinking.output_index)
            .and_then(|pending| pending.replay())
            .and_then(|replay| encode_reasoning_signature(&replay))
        {
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": thinking.anthropic_index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            );
        }
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": thinking.anthropic_index,
            }),
        );
    }
}

fn web_search_query(item: &serde_json::Value) -> String {
    let Some(action) = item.get("action") else {
        return String::new();
    };
    action
        .get("query")
        .and_then(|v| v.as_str())
        .or_else(|| {
            action
                .get("queries")
                .and_then(|v| v.as_array())
                .and_then(|queries| queries.iter().find_map(|query| query.as_str()))
        })
        .unwrap_or("")
        .to_string()
}

fn output_index(payload: &serde_json::Value) -> usize {
    payload
        .get("output_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

fn parse_codex_usage(response: &serde_json::Value) -> CodexUsage {
    let usage = match response.get("usage") {
        Some(u) => u,
        None => return CodexUsage::default(),
    };
    CodexUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        input_tokens_details_cached: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
    }
}

fn response_is_incomplete(payload: &serde_json::Value) -> bool {
    payload.get("type").and_then(|v| v.as_str()) == Some("response.incomplete")
        || payload
            .get("response")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str())
            == Some("incomplete")
        || payload
            .get("response")
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .and_then(|v| v.as_str())
            .is_some()
}

fn repair_whitespace_stalled_read_args(
    name: &str,
    args: &str,
    call_id: Option<&str>,
) -> Option<String> {
    if name != "Read" {
        return None;
    }
    let trimmed = args.trim_end();
    let trailing_whitespace = args.len().saturating_sub(trimmed.len());
    if trailing_whitespace < BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES {
        return None;
    }
    parse_read_args_candidate(trimmed, call_id).or_else(|| {
        let with_brace = format!("{trimmed}}}");
        parse_read_args_candidate(&with_brace, call_id)
    })
}

fn parse_read_args_candidate(args: &str, call_id: Option<&str>) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    if !is_valid_read_args(&parsed) {
        return None;
    }
    Some(sanitize_read_args(
        "Read",
        &serde_json::to_string(&parsed).ok()?,
        call_id,
    ))
}

fn is_valid_read_args(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "file_path" | "offset" | "limit" | "pages") {
            return false;
        }
    }
    let Some(file_path) = obj.get("file_path").and_then(|v| v.as_str()) else {
        return false;
    };
    if file_path.is_empty() {
        return false;
    }
    if let Some(offset) = obj.get("offset").and_then(|v| v.as_i64())
        && offset < 0
    {
        return false;
    }
    if let Some(limit) = obj.get("limit").and_then(|v| v.as_i64())
        && limit <= 0
    {
        return false;
    }
    if obj.get("offset").is_some_and(|v| !v.is_i64()) {
        return false;
    }
    if obj.get("limit").is_some_and(|v| !v.is_i64()) {
        return false;
    }
    if obj.get("pages").is_some_and(|v| !v.is_string()) {
        return false;
    }
    true
}

fn error_message(payload: &serde_json::Value) -> String {
    payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            payload
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("Upstream error")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(events: Vec<serde_json::Value>) -> String {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.accept(&event, None).unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn emits_text_delta_before_terminal_event() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "hello"
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("message_start"));
        assert!(out.contains("content_block_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("hello"));
        assert!(!out.contains("message_stop"));
    }

    #[test]
    fn finishes_text_stream() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "hello"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {"input_tokens": 2, "output_tokens": 1}}
            }),
        ]);
        assert!(out.contains("content_block_stop"));
        assert!(out.contains("message_delta"));
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(out.contains("message_stop"));
    }

    #[test]
    fn completed_response_with_null_incomplete_details_is_end_turn() {
        let out = render(vec![json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {}}
        })]);
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(!out.contains(r#""stop_reason":"max_tokens""#));
    }

    #[test]
    fn buffers_read_tool_args_until_done() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "Read",
                    "arguments": "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);
        assert!(out.contains("tool_use"));
        assert!(out.contains("input_json_delta"));
        assert!(out.contains("/tmp/a"));
        assert!(!out.contains("pages"));
    }

    #[test]
    fn repairs_whitespace_stalled_read_args_as_tool_use_finish() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {"type":"function_call","call_id":"call_1","name":"Read"}
                    }),
                    None,
                )
                .unwrap(),
        );
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": 0,
                        "delta": format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
                    }),
                    None,
                )
                .unwrap(),
        );
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(r#""partial_json":"{\"file_path\":\"/tmp/a\"}""#));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn finishes_after_closed_completed_tool_call() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"call_1","name":"WebSearch"}
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": "{\"query\":\"claude-code-proxy github\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type":"function_call",
                    "call_id":"call_1",
                    "name":"WebSearch",
                    "arguments":"{\"query\":\"claude-code-proxy github\"}"
                }
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }
        out.extend(translator.finish_after_closed_completed_tool_call(None));
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("content_block_start"));
        assert!(rendered.contains("input_json_delta"));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(!rendered.contains("event: error"));
    }

    #[test]
    fn emits_web_search_results_from_citations_before_deferred_text() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "ws_1"}
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "action": {"query": "grok reasoning effort"}
                }
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 1,
                "delta": "See the official docs."
            }),
            json!({
                "type": "response.output_text.annotation.added",
                "annotation": {
                    "type": "url_citation",
                    "title": "Reasoning",
                    "url": "https://docs.x.ai/docs/guides/reasoning"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"status": "completed", "usage": {}}
            }),
        ]);

        let tool = out.find("server_tool_use").unwrap();
        let result = out.find("web_search_tool_result").unwrap();
        let text = out.find("See the official docs.").unwrap();
        assert!(tool < result && result < text);
        assert!(out.contains("https://docs.x.ai/docs/guides/reasoning"));
        assert!(out.contains(r#""web_search_requests":1"#));
    }

    #[test]
    fn rate_limit_event_returns_error() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let err = translator
            .accept(
                &json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {"limit_reached": true}
                }),
                None,
            )
            .unwrap_err();
        assert_eq!(err, "rate limit reached");
    }

    #[test]
    fn live_stream_emits_signature_delta_before_thinking_stop() {
        let out = render(vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
            }),
            json!({
                "type":"response.reasoning_summary_text.delta",
                "output_index":0,
                "delta":"plan"
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1"}
            }),
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","usage":{}}
            }),
        ]);
        let thinking_delta = out.find(r#""type":"thinking_delta""#).unwrap();
        let signature_delta = out.find(r#""type":"signature_delta""#).unwrap();
        let thinking_stop = out[signature_delta..]
            .find("event: content_block_stop")
            .map(|offset| signature_delta + offset)
            .unwrap();
        assert!(thinking_delta < signature_delta);
        assert!(signature_delta < thinking_stop);
        assert!(out.contains("ccp:codex:v1:"));
    }
}
