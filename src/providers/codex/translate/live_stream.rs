use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::anthropic::sse::encode_sse_event;
use crate::traffic::TrafficCapture;

use super::super::events::validate_terminal_snapshot_status;
use super::read_rewrite::sanitize_read_args;
use super::reasoning_signature::{PendingReasoning, encode_reasoning_signature};
use super::reducer::{
    CodexUsage, OutputItemIdentity, OutputItemKind, STOP_END_TURN, STOP_TOOL_USE,
    completed_tool_counts, event_type, is_post_terminal_telemetry, map_codex_usage_to_anthropic,
    parse_output_item_added, reconcile_output_text_snapshot, register_output_item,
    registered_hosted_tool_count, registered_tool_count, require_active_output_kind,
    resolve_semantic_output_index, stop_reason_for_incomplete_response, unfinished_output_indices,
    validate_output_item_done, validate_tool_arguments,
};
use super::schema_bridge::SchemaBridge;
use super::tool_policy::ToolCallPolicy;
use super::web_search_compat::server_tool_use_id_from_codex_web_search_id;

const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES: usize = 1_024;
const BUFFERED_TOOL_MAX_ARGS_BYTES: usize = 5_000_000;
const MAX_LIVE_TRANSLATOR_INPUT_BYTES: usize = 8 * 1024 * 1024;

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
        done_snapshot: Option<String>,
        repair_candidate: Option<String>,
        emitted_args: bool,
    },
}

struct LiveWebSearch {
    output_index: usize,
    index: usize,
    result_index: usize,
    id: String,
    query: String,
    results: Vec<LiveWebSearchResult>,
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

struct PreparedToolArguments {
    output_index: usize,
    index: usize,
    arguments: String,
    emit: bool,
}

struct DeferredLiveContentEvent {
    index: usize,
    sequence: usize,
    event: String,
    data: serde_json::Value,
}

pub struct LiveStreamTranslator {
    message_id: String,
    model: String,
    message_started: bool,
    blocks_by_output_index: HashMap<usize, LiveBlock>,
    item_id_to_output_index: HashMap<String, usize>,
    call_id_to_output_index: HashMap<String, usize>,
    downstream_tool_ids: HashSet<String>,
    downstream_open_content_blocks: HashSet<usize>,
    output_identities: HashMap<usize, OutputItemIdentity>,
    completed_output_indices: HashSet<usize>,
    anthropic_index: usize,
    thinking: Option<LiveThinking>,
    reasoning_by_output_index: HashMap<usize, PendingReasoning>,
    saw_tool_use: bool,
    web_search_requests: usize,
    web_search_output_indices: HashSet<usize>,
    web_searches: Vec<LiveWebSearch>,
    web_search_results: Vec<LiveWebSearchResult>,
    deferred_text: Vec<(usize, String)>,
    defer_content_after_web_search: bool,
    deferred_content_events: Vec<DeferredLiveContentEvent>,
    deferred_content_sequence: usize,
    schema_bridge: Option<Arc<SchemaBridge>>,
    tool_policy: ToolCallPolicy,
    upstream_input_bytes: usize,
    finished: bool,
}

impl LiveStreamTranslator {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_schema_bridge(message_id, model, None)
    }

    pub(crate) fn with_schema_bridge(
        message_id: impl Into<String>,
        model: impl Into<String>,
        schema_bridge: Option<Arc<SchemaBridge>>,
    ) -> Self {
        Self::with_schema_bridge_and_tool_policy(
            message_id,
            model,
            schema_bridge,
            ToolCallPolicy::permissive(),
        )
    }

    pub(crate) fn with_schema_bridge_and_tool_policy(
        message_id: impl Into<String>,
        model: impl Into<String>,
        schema_bridge: Option<Arc<SchemaBridge>>,
        tool_policy: ToolCallPolicy,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            message_started: false,
            blocks_by_output_index: HashMap::new(),
            item_id_to_output_index: HashMap::new(),
            call_id_to_output_index: HashMap::new(),
            downstream_tool_ids: HashSet::new(),
            downstream_open_content_blocks: HashSet::new(),
            output_identities: HashMap::new(),
            completed_output_indices: HashSet::new(),
            anthropic_index: 0,
            thinking: None,
            reasoning_by_output_index: HashMap::new(),
            saw_tool_use: false,
            web_search_requests: 0,
            web_search_output_indices: HashSet::new(),
            web_searches: Vec::new(),
            web_search_results: Vec::new(),
            deferred_text: Vec::new(),
            defer_content_after_web_search: false,
            deferred_content_events: Vec::new(),
            deferred_content_sequence: 0,
            schema_bridge,
            tool_policy,
            upstream_input_bytes: 0,
            finished: false,
        }
    }

    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        if self.finished {
            if is_post_terminal_telemetry(payload) {
                return Ok(Vec::new());
            }
            let kind = payload
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("<missing>");
            return Err(format!(
                "unexpected Codex event {kind:?} after terminal response event"
            ));
        }
        let input_bytes = serde_json::to_vec(payload)
            .map_err(|error| format!("Codex stream event could not be encoded: {error}"))?
            .len();
        if self.upstream_input_bytes.saturating_add(input_bytes) > MAX_LIVE_TRANSLATOR_INPUT_BYTES {
            return Err("Codex live response exceeded the cumulative size limit".to_string());
        }
        self.upstream_input_bytes += input_bytes;

        let kind = event_type(payload)?;
        validate_terminal_snapshot_status(payload)?;
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
            // The Codex WebSocket gateway sends response-scoped headers in this
            // control frame. They influence Codex's own UI/runtime behavior but
            // have no Anthropic content mapping.
            "codex.response.metadata" => {}
            "keepalive" => {}
            "response.failed" | "response.error" | "response.cancelled" | "error" => {
                return Err(error_message(payload));
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {
                let output_index = self.semantic_output_index(payload, kind)?;
                require_active_output_kind(
                    &self.output_identities,
                    &self.completed_output_indices,
                    output_index,
                    OutputItemKind::WebSearchCall,
                    kind,
                )?;
            }
            "response.output_item.added" => {
                self.output_item_added(payload, traffic, &mut out)?;
            }
            "response.reasoning_summary_part.added" => {
                let output_index = self.semantic_output_index(payload, kind)?;
                require_active_output_kind(
                    &self.output_identities,
                    &self.completed_output_indices,
                    output_index,
                    OutputItemKind::Reasoning,
                    kind,
                )?;
                payload
                    .get("summary_index")
                    .and_then(|value| value.as_u64())
                    .ok_or_else(|| {
                        format!("{kind} is missing a non-negative integer summary_index")
                    })?;
                if payload
                    .get("part")
                    .filter(|part| part.is_object())
                    .and_then(|part| part.get("type"))
                    .and_then(|value| value.as_str())
                    != Some("summary_text")
                {
                    return Err(format!("{kind} is missing a summary_text part"));
                }
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
                self.reasoning_delta(payload, traffic, &mut out)?;
            }
            "response.output_text.delta" => {
                self.text_delta(payload, traffic, &mut out)?;
            }
            "response.output_text.annotation.added" => {
                self.web_search_annotation(payload)?;
            }
            "response.function_call_arguments.delta" => {
                self.tool_delta(payload, traffic, &mut out)?;
            }
            "response.function_call_arguments.done" => {
                self.tool_arguments_done(payload)?;
            }
            "response.output_item.done" => {
                self.output_item_done(payload, traffic, &mut out)?;
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finish(payload, traffic, &mut out)?;
            }
            "response.created" | "response.in_progress" | "response.queued" => {}
            "response.content_part.added" | "response.content_part.done" => {
                let output_index = self.semantic_output_index(payload, kind)?;
                require_active_output_kind(
                    &self.output_identities,
                    &self.completed_output_indices,
                    output_index,
                    OutputItemKind::Message,
                    kind,
                )?;
            }
            "response.output_text.done" => {
                self.text_done(payload, traffic, &mut out)?;
            }
            "response.reasoning_summary_part.done" | "response.reasoning_summary_text.done" => {
                let output_index = self.semantic_output_index(payload, kind)?;
                require_active_output_kind(
                    &self.output_identities,
                    &self.completed_output_indices,
                    output_index,
                    OutputItemKind::Reasoning,
                    kind,
                )?;
            }
            _ => return Err(format!("unsupported Codex semantic event {kind:?}")),
        }

        Ok(out)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn finish_after_closed_completed_tool_call(
        &mut self,
        unsafe_salvage_enabled: bool,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if !unsafe_salvage_enabled || self.finished || !self.saw_tool_use {
            return out;
        }

        let unfinished =
            unfinished_output_indices(&self.output_identities, &self.completed_output_indices);
        if unfinished.iter().any(|output_index| {
            self.output_identities
                .get(output_index)
                .is_none_or(|identity| identity.kind != OutputItemKind::FunctionCall)
                || !self.blocks_by_output_index.contains_key(output_index)
        }) {
            return out;
        }

        // Dangerous compatibility mode only: a flaky transport can close after
        // complete-looking JSON but before authoritative item/response terminal
        // events. Validate every remaining block transactionally before
        // synthesizing the legacy success terminal. Callers must leave this
        // disabled by default because valid JSON alone does not prove the model
        // committed that tool call.
        let mut prepared = Vec::with_capacity(self.blocks_by_output_index.len());
        for output_index in self.blocks_by_output_index.keys().copied() {
            let Ok(Some(arguments)) = self.prepare_tool_arguments(output_index, None) else {
                return out;
            };
            prepared.push(arguments);
        }
        prepared.sort_by_key(|arguments| arguments.index);

        self.close_thinking(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        for arguments in prepared {
            let output_index = arguments.output_index;
            if let Some((index, partial_json)) = self.commit_tool_arguments(arguments) {
                self.emit(
                    traffic,
                    &mut out,
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": partial_json
                        }
                    }),
                );
            }
            let Some(LiveBlock::Tool { index, .. }) =
                self.blocks_by_output_index.remove(&output_index)
            else {
                return Vec::new();
            };
            self.emit(
                traffic,
                &mut out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
        }
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
        &mut self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
        event: &str,
        data: &serde_json::Value,
    ) {
        if self.defer_content_after_web_search && event.starts_with("content_block_") {
            let index = data
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .expect("content block events carry a non-negative index")
                as usize;
            let sequence = self.deferred_content_sequence;
            self.deferred_content_sequence += 1;
            self.deferred_content_events.push(DeferredLiveContentEvent {
                index,
                sequence,
                event: event.to_string(),
                data: data.clone(),
            });
            return;
        }
        self.emit_now(traffic, out, event, data);
    }

    fn emit_now(
        &mut self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
        event: &str,
        data: &serde_json::Value,
    ) {
        if let Some(index) = data
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .map(|index| index as usize)
        {
            match event {
                "content_block_start" => {
                    self.downstream_open_content_blocks.insert(index);
                }
                "content_block_stop" => {
                    self.downstream_open_content_blocks.remove(&index);
                }
                _ => {}
            }
        }
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

    fn flush_deferred_content(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.defer_content_after_web_search = false;
        let mut events = std::mem::take(&mut self.deferred_content_events);
        events.sort_by_key(|event| (event.index, event.sequence));
        for event in events {
            self.emit_now(traffic, out, &event.event, &event.data);
        }
    }

    fn output_item_added(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let (output_index, item, identity) = parse_output_item_added(payload)?;
        if identity.kind == OutputItemKind::FunctionCall {
            self.tool_policy.validate_function_name(
                identity.name.as_deref().expect("validated function name"),
            )?;
        } else if identity.kind == OutputItemKind::WebSearchCall {
            self.tool_policy
                .validate_hosted_call(registered_hosted_tool_count(&self.output_identities))?;
        }
        if matches!(
            identity.kind,
            OutputItemKind::FunctionCall | OutputItemKind::WebSearchCall
        ) {
            self.tool_policy
                .validate_next_tool_call(registered_tool_count(&self.output_identities))?;
        }
        let item_type = identity.kind;
        let downstream_tool_id = match identity.kind {
            OutputItemKind::FunctionCall => identity.call_id.clone(),
            OutputItemKind::WebSearchCall => Some(server_tool_use_id_from_codex_web_search_id(
                identity
                    .item_id
                    .as_deref()
                    .expect("validated web search item id"),
            )?),
            OutputItemKind::Reasoning | OutputItemKind::Message => None,
        };
        if let Some(id) = downstream_tool_id.as_deref()
            && self.downstream_tool_ids.contains(id)
        {
            return Err(format!(
                "Codex produced duplicate downstream tool use id {id:?}"
            ));
        }
        register_output_item(
            output_index,
            identity,
            &mut self.output_identities,
            &mut self.item_id_to_output_index,
            &mut self.call_id_to_output_index,
        )?;
        if let Some(id) = downstream_tool_id.clone() {
            self.downstream_tool_ids.insert(id);
        }

        match item_type {
            OutputItemKind::Reasoning => {
                self.reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
            }
            OutputItemKind::Message => {
                self.close_thinking(traffic, out);
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                let deferred = self.schema_bridge.is_some() || !self.web_searches.is_empty();
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
                            "content_block": {"type": "text", "text": "", "citations": null}
                        }),
                    );
                }
            }
            OutputItemKind::FunctionCall => {
                self.close_thinking(traffic, out);
                let index = self.anthropic_index;
                let identity = self
                    .output_identities
                    .get(&output_index)
                    .expect("registered output item identity");
                let call_id = identity
                    .call_id
                    .clone()
                    .expect("validated function call_id");
                let name = identity.name.clone().expect("validated function name");
                // Do not mutate content indices or tool-use state until the
                // complete function identity has passed validation.
                self.saw_tool_use = true;
                self.anthropic_index += 1;
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Tool {
                        index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        done_snapshot: None,
                        repair_candidate: None,
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
                            "input": {},
                            "caller": {"type": "direct"}
                        }
                    }),
                );
            }
            OutputItemKind::WebSearchCall => {
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                let result_index = self.anthropic_index;
                self.anthropic_index += 1;
                self.web_searches.push(LiveWebSearch {
                    output_index,
                    index,
                    result_index,
                    id: downstream_tool_id.expect("validated downstream web search id"),
                    query: String::new(),
                    results: Vec::new(),
                });
                self.defer_content_after_web_search = true;
                if self.web_search_output_indices.insert(output_index) {
                    self.web_search_requests += 1;
                }
            }
        }
        Ok(())
    }

    fn reasoning_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let event = "response.reasoning_summary_text.delta";
        let output_index = self.semantic_output_index(payload, event)?;
        require_active_output_kind(
            &self.output_identities,
            &self.completed_output_indices,
            output_index,
            OutputItemKind::Reasoning,
            event,
        )?;
        payload
            .get("summary_index")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| format!("{event} is missing a non-negative integer summary_index"))?;
        let delta = payload
            .get("delta")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{event} is missing a string delta"))?;
        if delta.is_empty() {
            return Ok(());
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
        Ok(())
    }

    fn text_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let event = "response.output_text.delta";
        let output_index = self.semantic_output_index(payload, event)?;
        require_active_output_kind(
            &self.output_identities,
            &self.completed_output_indices,
            output_index,
            OutputItemKind::Message,
            event,
        )?;
        let delta = payload
            .get("delta")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{event} is missing a string delta"))?;
        if delta.is_empty() {
            return Ok(());
        }
        self.close_thinking(traffic, out);

        let Some(LiveBlock::Text {
            index,
            text,
            deferred,
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Err(format!(
                "{event} references closed or missing output_index {output_index}"
            ));
        };
        text.push_str(delta);
        if *deferred {
            return Ok(());
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
        Ok(())
    }

    fn text_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let event = "response.output_text.done";
        let output_index = self.semantic_output_index(payload, event)?;
        require_active_output_kind(
            &self.output_identities,
            &self.completed_output_indices,
            output_index,
            OutputItemKind::Message,
            event,
        )?;
        let snapshot = payload
            .get("text")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("{event} is missing a string text snapshot"))?;
        let (index, deferred, suffix) = {
            let Some(LiveBlock::Text {
                index,
                text,
                deferred,
            }) = self.blocks_by_output_index.get_mut(&output_index)
            else {
                return Err(format!(
                    "{event} references closed or missing output_index {output_index}"
                ));
            };
            let suffix = reconcile_output_text_snapshot(text, snapshot, event)?.to_string();
            text.push_str(&suffix);
            (*index, *deferred, suffix)
        };
        if !deferred && !suffix.is_empty() {
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": suffix}
                }),
            );
        }
        Ok(())
    }

    fn tool_delta(
        &mut self,
        payload: &serde_json::Value,
        _traffic: Option<&TrafficCapture>,
        _out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let event = "response.function_call_arguments.delta";
        let output_index = self.semantic_output_index(payload, event)?;
        require_active_output_kind(
            &self.output_identities,
            &self.completed_output_indices,
            output_index,
            OutputItemKind::FunctionCall,
            event,
        )?;
        let delta = payload
            .get("delta")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{event} is missing a string delta"))?;
        if delta.is_empty() {
            return Ok(());
        }
        let Some(LiveBlock::Tool {
            call_id,
            name,
            args_accum,
            done_snapshot,
            repair_candidate,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Err(format!(
                "{event} references closed or missing output_index {output_index}"
            ));
        };

        // `response.function_call_arguments.done` is an ordering boundary.
        if done_snapshot.is_some() {
            return Err(format!(
                "{event} arrived after arguments.done at output_index {output_index}"
            ));
        }
        args_accum.push_str(delta);
        if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
            return Err(format!(
                "Buffered {name} tool arguments exceeded safe limits"
            ));
        }
        // A repairable snapshot is not proof that the model committed the
        // tool call. Cache it for a later authoritative output_item.done, or
        // for the explicitly unsafe transport-close compatibility path.
        *repair_candidate =
            repair_whitespace_stalled_read_args(name, args_accum, Some(call_id.as_str()));
        Ok(())
    }

    fn tool_arguments_done(&mut self, payload: &serde_json::Value) -> Result<(), String> {
        let event = "response.function_call_arguments.done";
        let output_index = self.semantic_output_index(payload, event)?;
        require_active_output_kind(
            &self.output_identities,
            &self.completed_output_indices,
            output_index,
            OutputItemKind::FunctionCall,
            event,
        )?;
        let args = payload
            .get("arguments")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{event} is missing string arguments"))?;
        let Some(LiveBlock::Tool {
            name,
            done_snapshot,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Err(format!(
                "{event} references closed or missing output_index {output_index}"
            ));
        };
        if done_snapshot.is_some() {
            return Err(format!("duplicate {event} for output_index {output_index}"));
        }
        if args.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
            return Err(format!(
                "Buffered {name} tool arguments exceeded safe limits"
            ));
        }
        *done_snapshot = Some(args.to_string());
        Ok(())
    }

    fn output_item_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let event = "response.output_item.done";
        let output_index = self.semantic_output_index(payload, event)?;
        let identity = self
            .output_identities
            .get(&output_index)
            .cloned()
            .ok_or_else(|| {
                format!("{event} references output_index {output_index} before output_item.added")
            })?;
        if self.completed_output_indices.contains(&output_index) {
            return Err(format!("duplicate {event} for output_index {output_index}"));
        }
        let item = validate_output_item_done(payload, output_index, &identity)?;

        if identity.kind == OutputItemKind::Reasoning {
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
            self.completed_output_indices.insert(output_index);
            return Ok(());
        }

        if identity.kind == OutputItemKind::WebSearchCall {
            self.close_thinking(traffic, out);
            let search = self
                .web_searches
                .iter_mut()
                .find(|search| search.output_index == output_index)
                .ok_or_else(|| {
                    format!(
                        "{event} references missing web search state at output_index {output_index}"
                    )
                })?;
            search.query = web_search_query(item);
            search.results = web_search_sources(item);
            self.completed_output_indices.insert(output_index);
            return Ok(());
        }

        let final_args = payload
            .get("item")
            .and_then(|item| item.get("arguments"))
            .and_then(|value| value.as_str());
        let prepared = self.prepare_tool_arguments(output_index, final_args)?;
        if let Some((index, arguments)) =
            prepared.and_then(|prepared| self.commit_tool_arguments(prepared))
        {
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": arguments
                    }
                }),
            );
        }

        let Some(mut state) = self.blocks_by_output_index.remove(&output_index) else {
            return Err(format!(
                "{event} references closed or missing output_index {output_index}"
            ));
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
            LiveBlock::Tool { index, .. } => {
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
        self.completed_output_indices.insert(output_index);
        Ok(())
    }

    fn prepare_tool_arguments(
        &self,
        output_index: usize,
        final_args: Option<&str>,
    ) -> Result<Option<PreparedToolArguments>, String> {
        let Some(LiveBlock::Tool {
            index,
            name,
            call_id,
            args_accum,
            done_snapshot,
            repair_candidate,
            emitted_args,
            ..
        }) = self.blocks_by_output_index.get(&output_index)
        else {
            return Ok(None);
        };

        // Prefer authoritative snapshots. A cached repair is committed only
        // after output_item.done, or by the explicit unsafe-close caller.
        let arguments = done_snapshot
            .as_deref()
            .or(final_args)
            .or(repair_candidate.as_deref())
            .unwrap_or(args_accum)
            .to_string();
        let arguments = if name == "Read" && !arguments.is_empty() {
            sanitize_read_args(name, &arguments, Some(call_id.as_str()))
        } else {
            arguments
        };
        validate_tool_arguments(name, call_id, &arguments)?;

        Ok(Some(PreparedToolArguments {
            output_index,
            index: *index,
            arguments,
            emit: !*emitted_args,
        }))
    }

    fn semantic_output_index(
        &self,
        payload: &serde_json::Value,
        event: &str,
    ) -> Result<usize, String> {
        resolve_semantic_output_index(
            payload,
            event,
            &self.item_id_to_output_index,
            &self.call_id_to_output_index,
        )
    }

    fn commit_tool_arguments(
        &mut self,
        prepared: PreparedToolArguments,
    ) -> Option<(usize, String)> {
        let LiveBlock::Tool {
            args_accum,
            emitted_args,
            ..
        } = self
            .blocks_by_output_index
            .get_mut(&prepared.output_index)?
        else {
            return None;
        };
        *args_accum = prepared.arguments;
        if prepared.emit {
            *emitted_args = true;
            Some((prepared.index, std::mem::take(args_accum)))
        } else {
            None
        }
    }

    fn web_search_annotation(&mut self, payload: &serde_json::Value) -> Result<(), String> {
        let event = "response.output_text.annotation.added";
        let output_index = self.semantic_output_index(payload, event)?;
        require_active_output_kind(
            &self.output_identities,
            &self.completed_output_indices,
            output_index,
            OutputItemKind::Message,
            event,
        )?;
        let annotation = payload
            .get("annotation")
            .filter(|value| value.is_object())
            .ok_or_else(|| format!("{event} is missing an object annotation"))?;
        if annotation.get("type").and_then(|v| v.as_str()) != Some("url_citation") {
            return Err(format!("{event} contains an unsupported annotation type"));
        }
        let url = annotation
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|url| !url.is_empty())
            .ok_or_else(|| format!("{event} url citation is missing a non-empty url"))?;
        if self
            .web_search_results
            .iter()
            .any(|result| result.url == url)
        {
            return Ok(());
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
        Ok(())
    }

    fn emit_web_searches(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let searches = std::mem::take(&mut self.web_searches);
        let allow_global_annotation_fallback = searches.len() == 1;
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
                        "input": {},
                        "caller": {"type":"direct"}
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
            let search_results = if search.results.is_empty() && allow_global_annotation_fallback {
                &self.web_search_results
            } else {
                &search.results
            };
            let results: Vec<_> = search_results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "type": "web_search_result",
                        "title": result.title,
                        "url": result.url,
                        "encrypted_content": "",
                        "page_age": null,
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
                        "content": results,
                        "caller": {"type":"direct"}
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
                    "content_block": {"type": "text", "text": "", "citations": null}
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
    ) -> Result<(), String> {
        let event_type = payload
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !payload
            .get("response")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err(format!(
                "{event_type} is missing an object response snapshot"
            ));
        }
        let unfinished =
            unfinished_output_indices(&self.output_identities, &self.completed_output_indices);
        if !unfinished.is_empty() {
            return Err(format!(
                "{event_type} arrived with unfinished Codex output items at output_index(es) {unfinished:?}"
            ));
        }
        let terminal_stop_reason = stop_reason_for_incomplete_response(payload, event_type);
        if matches!(event_type, "response.completed" | "response.done")
            && terminal_stop_reason.is_none()
        {
            let (completed_function_calls, completed_hosted_calls) =
                completed_tool_counts(&self.output_identities, &self.completed_output_indices);
            self.tool_policy
                .validate_success_terminal(completed_function_calls, completed_hosted_calls)?;
        }
        self.close_thinking(traffic, out);
        let usage = payload.get("response").map(parse_codex_usage);
        let stop_reason = if let Some(reason) = terminal_stop_reason {
            reason
        } else if self.saw_tool_use {
            STOP_TOOL_USE
        } else {
            STOP_END_TURN
        };
        if stop_reason == STOP_END_TURN {
            self.normalize_deferred_structured_text()?;
        }
        self.emit_web_searches(traffic, out);
        self.flush_deferred_content(traffic, out);
        self.ensure_message_start(traffic, out);
        self.emit_finish(stop_reason, usage, traffic, out);
        Ok(())
    }

    fn normalize_deferred_structured_text(&mut self) -> Result<(), String> {
        let Some(bridge) = self.schema_bridge.as_ref() else {
            return Ok(());
        };
        if self.deferred_text.len() != 1 {
            return Err(format!(
                "Codex structured output expected exactly one completed text block, found {}",
                self.deferred_text.len()
            ));
        }
        let normalized_text = bridge
            .normalize_completed_text(&self.deferred_text[0].1)
            .map_err(|error| format!("Codex structured output validation failed: {error}"))?
            .text
            .into_owned();
        self.deferred_text[0].1 = normalized_text;
        Ok(())
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
        // An error must not flush content that was only queued behind a hosted search.
        // Drop those never-emitted blocks and close only indices that the downstream
        // has actually observed as open.
        self.defer_content_after_web_search = false;
        self.deferred_content_events.clear();
        self.blocks_by_output_index.clear();
        self.thinking = None;
        self.reasoning_by_output_index.clear();
        self.web_searches.clear();
        self.deferred_text.clear();

        let mut open: Vec<_> = self
            .downstream_open_content_blocks
            .iter()
            .copied()
            .collect();
        open.sort_unstable();
        for index in open {
            self.emit_now(
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
    if let Some(query) = action.get("query").and_then(|v| v.as_str()) {
        return query.to_string();
    }
    action
        .get("queries")
        .and_then(|v| v.as_array())
        .map(|queries| {
            queries
                .iter()
                .filter_map(|query| query.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn web_search_sources(item: &serde_json::Value) -> Vec<LiveWebSearchResult> {
    let Some(sources) = item
        .get("action")
        .and_then(|action| action.get("sources"))
        .and_then(|sources| sources.as_array())
    else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for source in sources {
        let Some(url) = source.get("url").and_then(|value| value.as_str()) else {
            continue;
        };
        if results
            .iter()
            .any(|result: &LiveWebSearchResult| result.url == url)
        {
            continue;
        }
        let title = source
            .get("title")
            .and_then(|value| value.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        results.push(LiveWebSearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }
    results
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
    use super::super::request::{ResponsesRequest, ResponsesToolChoice, ResponsesToolChoiceMode};
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

    fn optional_reason_schema_bridge() -> Arc<SchemaBridge> {
        Arc::new(
            SchemaBridge::build(&json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"},
                    "reason": {"type": "string"}
                },
                "required": ["ok"],
                "additionalProperties": false
            }))
            .unwrap(),
        )
    }

    fn structured_text_events(text: &str) -> [serde_json::Value; 3] {
        [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": text
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_up"}
            }),
        ]
    }

    fn translator_with_tool_policy(
        function_names: &[&str],
        hosted_web_search: bool,
        tool_choice: ResponsesToolChoice,
    ) -> LiveStreamTranslator {
        translator_with_tool_policy_and_parallel(
            function_names,
            hosted_web_search,
            tool_choice,
            true,
        )
    }

    fn translator_with_tool_policy_and_parallel(
        function_names: &[&str],
        hosted_web_search: bool,
        tool_choice: ResponsesToolChoice,
        parallel_tool_calls: bool,
    ) -> LiveStreamTranslator {
        let mut tools: Vec<serde_json::Value> = function_names
            .iter()
            .map(|name| {
                json!({
                    "type": "function",
                    "name": name,
                    "parameters": {"type": "object"},
                    "strict": false
                })
            })
            .collect();
        if hosted_web_search {
            tools.push(json!({
                "type": "web_search",
                "external_web_access": true,
                "search_content_types": ["text"]
            }));
        }
        let mut request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": [],
            "tools": tools,
            "store": false,
            "stream": true,
            "parallel_tool_calls": parallel_tool_calls,
            "text": {}
        }))
        .unwrap();
        request.tool_choice = Some(tool_choice);
        let policy = ToolCallPolicy::from_request(&request);
        LiveStreamTranslator::with_schema_bridge_and_tool_policy(
            "msg_policy",
            "gpt-5.6-sol",
            None,
            policy,
        )
    }

    fn function_call_added(output_index: usize, call_id: &str, name: &str) -> serde_json::Value {
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {"type": "function_call", "call_id": call_id, "name": name}
        })
    }

    #[test]
    fn tool_policy_none_rejects_upstream_function_before_emission() {
        let mut translator = translator_with_tool_policy(
            &["Bash"],
            false,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None),
        );
        let error = translator
            .accept(&function_call_added(0, "call_1", "Bash"), None)
            .unwrap_err();
        assert!(error.contains("forbids function calls"));
        assert!(translator.output_identities.is_empty());
        assert!(!translator.message_started);
    }

    #[test]
    fn tool_policy_named_function_rejects_a_different_function() {
        let mut translator = translator_with_tool_policy(
            &["Read", "Bash"],
            false,
            ResponsesToolChoice::Function {
                r#type: "function".to_string(),
                name: "Read".to_string(),
            },
        );
        let error = translator
            .accept(&function_call_added(0, "call_1", "Bash"), None)
            .unwrap_err();
        assert!(error.contains("requires \"Read\""));
        assert!(translator.output_identities.is_empty());

        let error = translator
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 1,
                    "item": {"type": "web_search_call", "id": "search_1"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("tool_choice forbids"));

        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "incomplete_details": null,
                        "usage": {}
                    }
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("required function tool \"Read\""));
    }

    #[test]
    fn tool_policy_auto_rejects_undeclared_function_and_hosted_calls() {
        let mut translator = translator_with_tool_policy(
            &["Read"],
            false,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
        );
        let error = translator
            .accept(&function_call_added(0, "call_1", "Bash"), None)
            .unwrap_err();
        assert!(error.contains("undeclared function tool \"Bash\""));

        let error = translator
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 1,
                    "item": {"type": "web_search_call", "id": "search_1"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("undeclared hosted web search"));
        assert!(translator.output_identities.is_empty());
    }

    #[test]
    fn tool_policy_required_rejects_success_terminal_without_a_call() {
        let mut translator = translator_with_tool_policy(
            &["Read"],
            false,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
        );
        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "incomplete_details": null,
                        "usage": {}
                    }
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("required by tool_choice"));
        assert!(!translator.is_finished());
    }

    #[test]
    fn tool_policy_allows_declared_parallel_function_calls() {
        let mut translator = translator_with_tool_policy(
            &["Read", "Bash"],
            false,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
        );
        let events = [
            function_call_added(3, "call_read", "Read"),
            function_call_added(7, "call_bash", "Bash"),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 3,
                "delta": "{\"file_path\":\"/tmp/a\"}"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 7,
                "delta": "{\"command\":\"pwd\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 7,
                "item": {
                    "type": "function_call",
                    "call_id": "call_bash",
                    "name": "Bash",
                    "arguments": "{\"command\":\"pwd\"}"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 3,
                "item": {
                    "type": "function_call",
                    "call_id": "call_read",
                    "name": "Read",
                    "arguments": "{\"file_path\":\"/tmp/a\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "incomplete_details": null,
                    "usage": {}
                }
            }),
        ];
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.accept(&event, None).unwrap());
        }
        let rendered = String::from_utf8(out).unwrap();
        assert_eq!(rendered.matches(r#""type":"tool_use""#).count(), 2);
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(translator.is_finished());
    }

    #[test]
    fn tool_policy_rejects_a_second_call_when_parallel_is_disabled() {
        for second in [
            function_call_added(1, "call_read_2", "Read"),
            json!({
                "type":"response.output_item.added",
                "output_index":1,
                "item":{"type":"web_search_call", "id":"search_1"}
            }),
        ] {
            let mut translator = translator_with_tool_policy_and_parallel(
                &["Read"],
                true,
                ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
                false,
            );
            translator
                .accept(&function_call_added(0, "call_read_1", "Read"), None)
                .unwrap();
            let error = translator.accept(&second, None).unwrap_err();
            assert!(error.contains("parallel_tool_calls is false"), "{error}");
            assert_eq!(translator.output_identities.len(), 1);
        }
    }

    #[test]
    fn emits_text_delta_before_terminal_event() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = translator
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message", "id": "msg_up"}
                }),
                None,
            )
            .unwrap();
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.output_text.delta",
                        "output_index": 0,
                        "delta": "hello"
                    }),
                    None,
                )
                .unwrap(),
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("message_start"));
        assert!(out.contains("content_block_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("hello"));
        assert!(!out.contains("message_stop"));
    }

    #[test]
    fn structured_text_is_buffered_until_terminal_and_optional_null_is_elided() {
        let mut translator = LiveStreamTranslator::with_schema_bridge(
            "msg_1",
            "gpt-5.6-sol",
            Some(optional_reason_schema_bridge()),
        );
        let mut before_terminal = Vec::new();
        for event in structured_text_events(r#"{"ok":true,"reason":null}"#) {
            before_terminal.extend(translator.accept(&event, None).unwrap());
        }
        assert!(before_terminal.is_empty());

        let terminal = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "usage": {}}
                }),
                None,
            )
            .unwrap();
        let values = crate::anthropic::sse::try_parse_sse_events(&terminal)
            .unwrap()
            .into_iter()
            .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
            .collect::<Vec<_>>();
        let text_deltas = values
            .iter()
            .filter(|value| value["delta"]["type"] == "text_delta")
            .filter_map(|value| value["delta"]["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(text_deltas, vec![r#"{"ok":true}"#]);
        assert_eq!(
            values
                .iter()
                .filter(|value| value["type"] == "message_stop")
                .count(),
            1
        );
    }

    #[test]
    fn invalid_structured_text_emits_error_without_success_terminal() {
        let mut translator = LiveStreamTranslator::with_schema_bridge(
            "msg_1",
            "gpt-5.6-sol",
            Some(optional_reason_schema_bridge()),
        );
        for event in structured_text_events(r#"{"ok":"wrong","reason":null}"#) {
            assert!(translator.accept(&event, None).unwrap().is_empty());
        }

        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "usage": {}}
                }),
                None,
            )
            .expect_err("invalid structured output must fail before a success terminal");
        let rendered =
            String::from_utf8(translator.error_chunk(&error, "api_error", None)).unwrap();
        assert!(rendered.contains("event: error"));
        assert!(!rendered.contains("message_stop"));
        assert!(!rendered.contains("wrong"));
    }

    #[test]
    fn codex_live_and_buffered_reconcile_output_text_done_snapshots() {
        let added = json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"message","id":"msg_up"}
        });
        let delta = json!({
            "type":"response.output_text.delta",
            "output_index":0,
            "delta":"hel"
        });
        let done = json!({
            "type":"response.output_text.done",
            "output_index":0,
            "text":"hello"
        });

        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        translator.accept(&added, None).unwrap();
        translator.accept(&delta, None).unwrap();
        let live_suffix = String::from_utf8(translator.accept(&done, None).unwrap()).unwrap();
        assert!(live_suffix.contains("\"text\":\"lo\""));

        let buffered = [
            added,
            delta,
            done,
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"message","id":"msg_up"}
            }),
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","usage":{}}
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let buffered = super::super::reducer::reduce_upstream_bytes(buffered.as_bytes()).unwrap();
        let text = buffered
            .iter()
            .filter_map(|event| match event {
                super::super::reducer::ReducerEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "hello");
    }

    #[test]
    fn codex_live_and_buffered_reject_missing_or_conflicting_text_done_snapshots() {
        let added = json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"message","id":"msg_up"}
        });
        let delta = json!({
            "type":"response.output_text.delta",
            "output_index":0,
            "delta":"hello"
        });
        for done in [
            json!({
                "type":"response.output_text.done",
                "output_index":0
            }),
            json!({
                "type":"response.output_text.done",
                "output_index":0,
                "text":"hullo"
            }),
        ] {
            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
            translator.accept(&added, None).unwrap();
            translator.accept(&delta, None).unwrap();
            let live_error = translator.accept(&done, None).unwrap_err();
            assert!(
                live_error.contains("string text snapshot")
                    || live_error.contains("disagrees with accumulated output")
            );

            let buffered = [added.clone(), delta.clone(), done]
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            let buffered_error =
                super::super::reducer::reduce_upstream_bytes(buffered.as_bytes()).unwrap_err();
            assert!(
                buffered_error.message.contains("string text snapshot")
                    || buffered_error
                        .message
                        .contains("disagrees with accumulated output")
            );
        }
    }

    #[test]
    fn codex_live_and_buffered_reject_mismatched_terminal_snapshot_statuses() {
        for (event_type, status) in [
            ("response.completed", "failed"),
            ("response.done", "incomplete"),
            ("response.incomplete", "completed"),
            ("response.failed", "completed"),
            ("response.error", "completed"),
            ("response.cancelled", "failed"),
            ("error", "completed"),
        ] {
            let terminal = json!({
                "type":event_type,
                "response":{"status":status, "usage":{}}
            });

            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
            let live_error = translator.accept(&terminal, None).unwrap_err();
            assert!(
                live_error.contains("response.status"),
                "{event_type}/{status}: {live_error}"
            );

            let buffered = format!("data: {terminal}\n\n");
            let buffered_error =
                super::super::reducer::reduce_upstream_bytes(buffered.as_bytes()).unwrap_err();
            assert!(
                buffered_error.message.contains("response.status"),
                "{event_type}/{status}: {}",
                buffered_error.message
            );
        }
    }

    #[test]
    fn codex_live_and_buffered_reject_the_same_unresolved_delta() {
        let delta = json!({
            "type":"response.output_text.delta",
            "item_id":"missing_item",
            "delta":"lost"
        });
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let live_error = translator.accept(&delta, None).unwrap_err();
        assert!(live_error.contains("unknown item_id"));

        let buffered = format!(
            "data: {}\n\ndata: {}\n\n",
            delta,
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","usage":{}}
            })
        );
        let buffered_error =
            super::super::reducer::reduce_upstream_bytes(buffered.as_bytes()).unwrap_err();
        assert!(buffered_error.message.contains("unknown item_id"));
    }

    #[test]
    fn codex_live_and_buffered_reject_terminal_with_any_unfinished_item_kind() {
        for (label, item) in [
            ("message", json!({"type":"message","id":"item_message"})),
            (
                "function_call",
                json!({
                    "type":"function_call",
                    "id":"item_tool",
                    "call_id":"call_1",
                    "name":"Read"
                }),
            ),
            (
                "reasoning",
                json!({"type":"reasoning","id":"item_reasoning"}),
            ),
            (
                "web_search_call",
                json!({"type":"web_search_call","id":"item_search"}),
            ),
        ] {
            let added = json!({
                "type":"response.output_item.added",
                "output_index":7,
                "item":item
            });
            let terminal = json!({
                "type":"response.completed",
                "response":{"id":"resp_1","usage":{}}
            });

            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
            translator.accept(&added, None).unwrap();
            let live_error = translator.accept(&terminal, None).unwrap_err();
            assert!(
                live_error.contains("unfinished Codex output items"),
                "{label}: {live_error}"
            );

            let buffered = format!("data: {added}\n\ndata: {terminal}\n\n");
            let buffered_error =
                super::super::reducer::reduce_upstream_bytes(buffered.as_bytes()).unwrap_err();
            assert!(
                buffered_error
                    .message
                    .contains("unfinished Codex output items"),
                "{label}: {}",
                buffered_error.message
            );
        }
    }

    #[test]
    fn codex_live_and_buffered_reject_mixed_known_and_unknown_item_identities() {
        let added = json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{
                "type":"function_call",
                "id":"item_1",
                "call_id":"call_1",
                "name":"Read"
            }
        });
        for (event, expected) in [
            (
                json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"item_1",
                    "call_id":"unknown_call",
                    "delta":"{}"
                }),
                "unknown call_id",
            ),
            (
                json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"unknown_item",
                    "call_id":"call_1",
                    "delta":"{}"
                }),
                "unknown item_id",
            ),
        ] {
            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
            translator.accept(&added, None).unwrap();
            let live_error = translator.accept(&event, None).unwrap_err();
            assert!(live_error.contains(expected), "{live_error}");

            let buffered = format!("data: {added}\n\ndata: {event}\n\n");
            let buffered_error =
                super::super::reducer::reduce_upstream_bytes(buffered.as_bytes()).unwrap_err();
            assert!(
                buffered_error.message.contains(expected),
                "{}",
                buffered_error.message
            );
        }
    }

    #[test]
    fn codex_duplicate_output_index_fails_before_second_start() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let first = translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"message","id":"msg_1"}
                }),
                None,
            )
            .unwrap();
        let error = translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("duplicate Codex output_index 0"));

        let mut rendered = String::from_utf8(first).unwrap();
        rendered.push_str(
            &String::from_utf8(translator.error_chunk(&error, "api_error", None)).unwrap(),
        );
        assert_eq!(rendered.matches("event: content_block_start").count(), 1);
        assert_eq!(rendered.matches("event: content_block_stop").count(), 1);
        assert!(!rendered.contains("message_stop"));
    }

    #[test]
    fn codex_missing_function_identity_fails_before_downstream_bytes() {
        for item in [
            json!({"type":"function_call","name":"Read"}),
            json!({"type":"function_call","call_id":"call_1"}),
            json!({"type":"function_call","call_id":"","name":"Read"}),
            json!({"type":"function_call","call_id":"call_1","name":""}),
        ] {
            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
            let error = translator
                .accept(
                    &json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":item
                    }),
                    None,
                )
                .unwrap_err();
            assert!(error.contains("must be a non-empty string"));
            assert!(!translator.message_started);
            assert!(translator.blocks_by_output_index.is_empty());
        }
    }

    #[test]
    fn codex_live_rejects_terminal_tail_but_allows_rate_limit_telemetry() {
        let terminal = json!({
            "type":"response.completed",
            "response":{"id":"resp_1","usage":{}}
        });
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let terminal_bytes = translator.accept(&terminal, None).unwrap();
        assert!(
            String::from_utf8(terminal_bytes)
                .unwrap()
                .contains("message_stop")
        );
        assert!(
            translator
                .accept(
                    &json!({"type":"codex.rate_limits","rate_limits":{"limit_reached":true}}),
                    None
                )
                .unwrap()
                .is_empty()
        );
        let error = translator
            .accept(
                &json!({
                    "type":"codex.response.metadata",
                    "headers":{"openai-model":"gpt-5.6-sol"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("after terminal"));
        let error = translator
            .accept(
                &json!({
                    "type":"response.output_text.delta",
                    "output_index":0,
                    "delta":"late"
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("after terminal"));
    }

    #[test]
    fn codex_live_ignores_response_metadata_control_frame() {
        let out = render(vec![
            json!({
                "type":"codex.response.metadata",
                "headers":{
                    "openai-model":"gpt-5.6-sol",
                    "x-codex-safety-buffering-enabled":"true"
                }
            }),
            json!({
                "type":"response.created",
                "response":{"id":"resp_1","status":"in_progress"}
            }),
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"msg_up"}
            }),
            json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "delta":"metadata survived"
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"message","id":"msg_up"}
            }),
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","status":"completed","usage":{}}
            }),
        ]);

        assert!(out.contains("metadata survived"));
        assert!(out.contains("message_stop"));
        assert!(!out.contains("codex.response.metadata"));
        assert!(!out.contains("x-codex-safety-buffering-enabled"));
    }

    #[test]
    fn codex_live_rejects_done_before_added_and_unknown_semantic_event() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let error = translator
            .accept(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{"type":"message","id":"msg_1"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("before output_item.added") || error.contains("unknown item_id"));

        let error = translator
            .accept(&json!({"type":"future.semantic.event","value":1}), None)
            .unwrap_err();
        assert!(error.contains("unsupported Codex semantic event"));
        let error = translator
            .accept(
                &json!({"type":"codex.response.metadata.v2","value":1}),
                None,
            )
            .unwrap_err();
        assert!(error.contains("unsupported Codex semantic event"));

        let mut translator = LiveStreamTranslator::new("msg_2", "gpt-5.6-sol");
        translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"message","id":"msg_up"}
                }),
                None,
            )
            .unwrap();
        let error = translator
            .accept(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{"type":"message"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("missing the non-empty id"));
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
                "item": {"type": "message", "id": "msg_up"}
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
    fn content_filter_incomplete_is_refusal() {
        let out = render(vec![json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_1",
                "status": "incomplete",
                "incomplete_details": {"reason": "content_filter"},
                "usage": {}
            }
        })]);
        assert!(out.contains(r#""stop_reason":"refusal""#));
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
    fn done_snapshot_overrides_deltas_and_item_snapshot() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "id":"item_1", "call_id": "call_1", "name": "Bash"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "item_1",
                "delta": "{\"command\":\"from delta\"}"
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "call_id": "call_1",
                "arguments": "{\"command\":\"from done\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "name": "Bash",
                    "arguments": "{\"command\":\"from item\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);

        assert!(out.contains(r#""partial_json":"{\"command\":\"from done\"}""#));
        assert!(!out.contains("from delta"));
        assert!(!out.contains("from item"));
        assert_eq!(out.matches(r#""type":"input_json_delta""#).count(), 1);
    }

    #[test]
    fn live_rejects_delta_after_arguments_done() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        for event in [
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"Bash"}
            }),
            json!({
                "type":"response.function_call_arguments.done",
                "output_index":0,
                "arguments":"{\"command\":\"done\"}"
            }),
        ] {
            translator.accept(&event, None).unwrap();
        }
        let error = translator
            .accept(
                &json!({
                    "type":"response.function_call_arguments.delta",
                    "output_index":0,
                    "delta":"{late garbage"
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("after arguments.done"));
    }

    #[test]
    fn malformed_done_snapshot_is_rejected_before_arguments_are_emitted() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut rendered = String::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": "{\"command\":"
            }),
        ] {
            rendered
                .push_str(&String::from_utf8(translator.accept(&event, None).unwrap()).unwrap());
        }

        let error = translator
            .accept(
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "Bash",
                        "arguments": "{\"command\":\"valid but non-authoritative\"}"
                    }
                }),
                None,
            )
            .unwrap_err();

        assert!(error.contains("invalid JSON arguments"));
        assert!(!rendered.contains("input_json_delta"));
        assert!(!translator.is_finished());
    }

    #[test]
    fn terminal_event_rejects_tool_call_without_json_arguments() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        translator
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
                }),
                None,
            )
            .unwrap();

        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "usage": {}}
                }),
                None,
            )
            .unwrap_err();

        assert!(error.contains("unfinished Codex output items"));
        assert!(!translator.is_finished());
        let terminal =
            String::from_utf8(translator.error_chunk(&error, "api_error", None)).unwrap();
        assert!(terminal.contains("event: error"));
        assert!(!terminal.contains("message_stop"));
    }

    #[test]
    fn output_item_done_rejects_malformed_tool_json() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"command\":"
            }),
        ] {
            translator.accept(&event, None).unwrap();
        }

        let error = translator
            .accept(
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "Bash",
                        "arguments": "{\"command\":"
                    }
                }),
                None,
            )
            .unwrap_err();

        assert!(error.contains("invalid JSON arguments"));
        assert!(!translator.is_finished());
    }

    #[test]
    fn terminal_rejects_buffered_tool_until_output_item_done() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": "{\"file_path\":\"/tmp/a\"}"
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "usage": {}}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("unfinished Codex output items"));
        assert!(
            !String::from_utf8(out.clone())
                .unwrap()
                .contains("input_json_delta")
        );

        for event in [
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "Read",
                    "arguments": "{\"file_path\":\"/tmp/a\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(r#""partial_json":"{\"file_path\":\"/tmp/a\"}""#));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn unfinished_terminal_rejection_is_transactional_and_recoverable() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "Read"}
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": "{\"file_path\":\"/tmp/a\"}"
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_b", "name": "Read"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "delta": "{\"file_path\":"
            }),
        ] {
            translator.accept(&event, None).unwrap();
        }

        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_bad", "usage": {}}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("unfinished Codex output items"));

        for output_index in [0, 1] {
            let Some(LiveBlock::Tool { emitted_args, .. }) =
                translator.blocks_by_output_index.get(&output_index)
            else {
                panic!("tool {output_index} should remain pending after a failed transaction");
            };
            assert!(
                !emitted_args,
                "no tool may be marked emitted when the terminal transaction fails"
            );
        }

        translator
            .accept(
                &json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": 1,
                    "delta": "\"/tmp/b\"}"
                }),
                None,
            )
            .unwrap();
        let mut recovered = translator
            .accept(
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 1,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_b",
                        "name": "Read",
                        "arguments": "{\"file_path\":\"/tmp/b\"}"
                    }
                }),
                None,
            )
            .unwrap();
        recovered.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "type": "function_call",
                            "call_id": "call_a",
                            "name": "Read",
                            "arguments": "{\"file_path\":\"/tmp/a\"}"
                        }
                    }),
                    None,
                )
                .unwrap(),
        );

        recovered.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.completed",
                        "response": {"id": "resp_ok", "usage": {}}
                    }),
                    None,
                )
                .unwrap(),
        );
        let completed = String::from_utf8(recovered).unwrap();
        assert!(completed.contains("/tmp/b"));
        assert!(
            completed.contains("/tmp/a"),
            "the valid first tool must remain available after the failed transaction"
        );
        assert!(translator.is_finished());
    }

    #[test]
    fn preserves_two_interleaved_parallel_tool_calls_with_out_of_order_completion() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 3,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Lookup"}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 7,
                "item": {"type": "function_call", "call_id": "call_2", "name": "Lookup"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 3,
                "delta": "{\"q\":\"a"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 7,
                "delta": "{\"q\":\"b\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 7,
                "item": {"type": "function_call", "call_id": "call_2", "name": "Lookup", "arguments": "{\"q\":\"b\"}"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 3,
                "delta": "\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 3,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Lookup", "arguments": "{\"q\":\"a\"}"}
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);

        let values: Vec<serde_json::Value> =
            crate::anthropic::sse::try_parse_sse_events(out.as_bytes())
                .unwrap()
                .into_iter()
                .map(|event| serde_json::from_str(&event.data).unwrap())
                .collect();
        let starts: Vec<_> = values
            .iter()
            .filter(|value| value["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(starts[0]["content_block"]["id"], "call_1");
        assert_eq!(starts[0]["content_block"]["name"], "Lookup");
        assert_eq!(starts[0]["content_block"]["caller"]["type"], "direct");
        assert_eq!(starts[1]["index"], 1);
        assert_eq!(starts[1]["content_block"]["id"], "call_2");
        assert_eq!(starts[1]["content_block"]["name"], "Lookup");

        let mut arguments = [String::new(), String::new()];
        let mut stop_order = Vec::new();
        for value in &values {
            if value["type"] == "content_block_delta"
                && value["delta"]["type"] == "input_json_delta"
            {
                let index = value["index"].as_u64().unwrap() as usize;
                arguments[index].push_str(value["delta"]["partial_json"].as_str().unwrap());
            }
            if value["type"] == "content_block_stop" {
                stop_order.push(value["index"].as_u64().unwrap());
            }
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&arguments[0]).unwrap(),
            json!({"q":"a"})
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&arguments[1]).unwrap(),
            json!({"q":"b"})
        );
        assert_eq!(stop_order, vec![1, 0]);
        assert!(out.contains(r#""stop_reason":"tool_use""#));
        assert_eq!(out.matches("event: message_stop").count(), 1);
    }

    #[test]
    fn read_stall_waits_for_authoritative_done_and_preserves_parallel_peer() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"read_1","name":"Read"}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type":"function_call","call_id":"bash_1","name":"Bash"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "delta": "{\"command\":\"echo peer\"}"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        let before_peer_done = String::from_utf8(out.clone()).unwrap();
        assert!(!before_peer_done.contains("/tmp/a"));
        assert!(!before_peer_done.contains("echo peer"));
        assert!(!before_peer_done.contains("message_stop"));
        assert!(!translator.is_finished());

        for event in [
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 1,
                "arguments": "{\"command\":\"echo peer\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type":"function_call",
                    "call_id":"bash_1",
                    "name":"Bash",
                    "arguments":"{\"command\":\"echo peer\"}"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type":"function_call",
                    "call_id":"read_1",
                    "name":"Read"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("/tmp/a"));
        assert!(rendered.contains("echo peer"));
        assert_eq!(rendered.matches("event: content_block_stop").count(), 2);
        assert_eq!(rendered.matches("event: message_stop").count(), 1);
        assert!(translator.is_finished());
    }

    #[test]
    fn preserves_three_tools_completed_in_c_a_b_order() {
        let mut events = Vec::new();
        for (output_index, call_id) in [(0, "call_a"), (1, "call_b"), (2, "call_c")] {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {"type":"function_call","call_id":call_id,"name":"Lookup"}
            }));
            events.push(json!({
                "type": "response.function_call_arguments.delta",
                "output_index": output_index,
                "delta": format!("{{\"q\":\"{call_id}\"}}")
            }));
        }
        for output_index in [2, 0, 1] {
            let call_id = ["call_a", "call_b", "call_c"][output_index];
            events.push(json!({
                "type": "response.function_call_arguments.done",
                "output_index": output_index,
                "arguments": format!("{{\"q\":\"{call_id}\"}}")
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": {
                    "type":"function_call",
                    "call_id":call_id,
                    "name":"Lookup",
                    "arguments":format!("{{\"q\":\"{call_id}\"}}")
                }
            }));
        }
        events.push(json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "usage": {}}
        }));

        let out = render(events);
        let values: Vec<serde_json::Value> =
            crate::anthropic::sse::try_parse_sse_events(out.as_bytes())
                .unwrap()
                .into_iter()
                .map(|event| serde_json::from_str(&event.data).unwrap())
                .collect();
        let stop_order: Vec<_> = values
            .iter()
            .filter(|value| value["type"] == "content_block_stop")
            .filter_map(|value| value["index"].as_u64())
            .collect();
        assert_eq!(stop_order, vec![2, 0, 1]);
        for call_id in ["call_a", "call_b", "call_c"] {
            assert!(out.contains(call_id));
        }
        assert_eq!(out.matches("event: message_stop").count(), 1);
    }

    #[test]
    fn terminal_rejects_parallel_tools_without_output_item_done() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = Vec::new();
        for event in [
            json!({
                "type":"response.output_item.added",
                "output_index":20,
                "item":{"type":"function_call","call_id":"call_a","name":"Lookup"}
            }),
            json!({
                "type":"response.output_item.added",
                "output_index":5,
                "item":{"type":"function_call","call_id":"call_b","name":"Lookup"}
            }),
            json!({
                "type":"response.output_item.added",
                "output_index":10,
                "item":{"type":"function_call","call_id":"call_c","name":"Lookup"}
            }),
            json!({
                "type":"response.function_call_arguments.done",
                "output_index":10,
                "arguments":"{\"q\":\"c\"}"
            }),
            json!({
                "type":"response.function_call_arguments.done",
                "output_index":20,
                "arguments":"{\"q\":\"a\"}"
            }),
            json!({
                "type":"response.function_call_arguments.done",
                "output_index":5,
                "arguments":"{\"q\":\"b\"}"
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }
        let error = translator
            .accept(
                &json!({
                    "type":"response.completed",
                    "response":{"id":"resp_1","usage":{}}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("[5, 10, 20]"));
        let rendered = String::from_utf8(out).unwrap();
        assert!(!rendered.contains("input_json_delta"));
        assert!(!rendered.contains("message_stop"));
    }

    #[test]
    fn caches_whitespace_stalled_read_repair_until_authoritative_done() {
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
        let rendered = String::from_utf8(out.clone()).unwrap();
        assert!(!rendered.contains("input_json_delta"));
        assert!(!rendered.contains("content_block_stop"));
        assert!(!rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(!rendered.contains("message_stop"));
        assert!(!translator.is_finished());

        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.output_item.done",
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
                        "type": "response.completed",
                        "response": {"id":"resp_1","usage":{}}
                    }),
                    None,
                )
                .unwrap(),
        );
        let rendered = String::from_utf8(out).unwrap();
        assert_eq!(rendered.matches("input_json_delta").count(), 1);
        assert!(rendered.contains(r#""partial_json":"{\"file_path\":\"/tmp/a\"}""#));
        assert_eq!(rendered.matches("event: content_block_stop").count(), 1);
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
        out.extend(translator.finish_after_closed_completed_tool_call(true, None));
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("content_block_start"));
        assert!(rendered.contains("input_json_delta"));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(!rendered.contains("event: error"));
    }

    #[test]
    fn complete_tool_json_without_authoritative_terminal_fails_closed_by_default() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"call_1","name":"Workflow"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"script\":\"const result = await agent('inspect');\"}"
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        assert!(
            translator
                .finish_after_closed_completed_tool_call(false, None)
                .is_empty()
        );
        out.extend(translator.error_chunk(
            "upstream closed without authoritative terminal events",
            "api_error",
            None,
        ));
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("content_block_start"));
        assert!(rendered.contains("event: error"));
        assert!(!rendered.contains("input_json_delta"));
        assert!(!rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(!rendered.contains("message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn unsafe_opt_in_finishes_complete_tool_json_before_item_done() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"call_1","name":"Workflow"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"script\":\"const result = await agent('inspect');\"}"
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        out.extend(translator.finish_after_closed_completed_tool_call(true, None));
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("content_block_start"));
        assert!(rendered.contains("input_json_delta"));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(!rendered.contains("event: error"));
        assert!(translator.is_finished());
    }

    #[test]
    fn finishes_buffered_read_with_short_trailing_whitespace_on_close() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"call_1","name":"Read"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"file_path\":\"/tmp/a\"}   "
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        out.extend(translator.finish_after_closed_completed_tool_call(true, None));
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("input_json_delta"));
        assert!(rendered.contains("/tmp/a"));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn does_not_finish_partial_tool_arguments_after_close() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        translator
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type":"function_call","call_id":"call_1","name":"Workflow"}
                }),
                None,
            )
            .unwrap();
        translator
            .accept(
                &json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": 0,
                    "delta": "{\"script\":\"unfinished"
                }),
                None,
            )
            .unwrap();

        assert!(
            translator
                .finish_after_closed_completed_tool_call(true, None)
                .is_empty()
        );
        assert!(!translator.is_finished());
    }

    #[test]
    fn transport_close_salvage_rejects_unfinished_non_tool_items() {
        for (label, unfinished_item) in [
            ("reasoning", json!({"type":"reasoning","id":"reasoning_1"})),
            (
                "web_search_call",
                json!({"type":"web_search_call","id":"search_1"}),
            ),
        ] {
            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
            for event in [
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"tool_item_1",
                        "call_id":"call_1",
                        "name":"Workflow"
                    }
                }),
                json!({
                    "type":"response.function_call_arguments.delta",
                    "output_index":0,
                    "delta":"{}"
                }),
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"tool_item_1",
                        "call_id":"call_1",
                        "name":"Workflow",
                        "arguments":"{}"
                    }
                }),
                json!({
                    "type":"response.output_item.added",
                    "output_index":1,
                    "item":unfinished_item
                }),
            ] {
                translator.accept(&event, None).unwrap();
            }

            assert!(
                translator
                    .finish_after_closed_completed_tool_call(true, None)
                    .is_empty(),
                "transport-close salvage accepted unfinished {label}"
            );
            assert!(!translator.is_finished());
        }
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
                "output_index": 1,
                "item_id": "msg_up",
                "annotation": {
                    "type": "url_citation",
                    "title": "Reasoning",
                    "url": "https://docs.x.ai/docs/guides/reasoning"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {"type": "message", "id": "msg_up"}
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
        assert!(out.contains(r#""caller":{"type":"direct"}"#));
        assert!(out.contains(r#""encrypted_content":"""#));
        assert!(out.contains(r#""page_age":null"#));
        assert!(out.contains(r#""web_search_requests":1"#));
    }

    #[test]
    fn live_web_search_ids_are_nonempty_injective_and_cross_tool_unique() {
        let mut translator = LiveStreamTranslator::new("msg_ids", "gpt-5.6-sol");
        let missing = translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"web_search_call"}
                }),
                None,
            )
            .unwrap_err();
        assert!(missing.contains("web search item id"));
        assert!(translator.output_identities.is_empty());

        let mut translator = LiveStreamTranslator::new("msg_ids", "gpt-5.6-sol");
        translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                }),
                None,
            )
            .unwrap();
        let collision = translator
            .accept(&function_call_added(1, "srvtoolu_ws_1", "Read"), None)
            .unwrap_err();
        assert!(collision.contains("duplicate downstream tool use id"));
        assert_eq!(translator.output_identities.len(), 1);

        let out = render(vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"web_search_call","id":"ws-1"}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"web_search_call","id":"ws-1","action":{"query":"a"}}
            }),
            json!({
                "type":"response.output_item.added",
                "output_index":1,
                "item":{"type":"web_search_call","id":"ws_1"}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{"type":"web_search_call","id":"ws_1","action":{"query":"b"}}
            }),
            json!({
                "type":"response.completed",
                "response":{"status":"completed","usage":{}}
            }),
        ]);
        assert!(out.contains("srvtoolu_b64_d3MtMQ"));
        assert!(out.contains("srvtoolu_ws_1"));
    }

    #[test]
    fn live_hosted_max_uses_rejects_the_next_call_before_registration() {
        let mut request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "input":[],
            "tools":[{"type":"web_search","external_web_access":true,"search_content_types":["text"]}],
            "tool_choice":"auto",
            "store":false,
            "stream":true,
            "parallel_tool_calls":true,
            "text":{}
        }))
        .unwrap();
        request.hosted_web_search_max_uses = Some(1);
        let mut translator = LiveStreamTranslator::with_schema_bridge_and_tool_policy(
            "msg_max",
            "gpt-5.6-sol",
            None,
            ToolCallPolicy::from_request(&request),
        );
        translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                }),
                None,
            )
            .unwrap();
        let error = translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":1,
                    "item":{"type":"web_search_call","id":"ws_2"}
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("max_uses"));
        assert_eq!(translator.output_identities.len(), 1);
        assert_eq!(translator.web_searches.len(), 1);
    }

    #[test]
    fn live_web_search_then_function_emits_monotonic_content_indices() {
        let out = render(vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"web_search_call","id":"ws_1"}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"web_search_call","id":"ws_1","action":{"query":"rust"}}
            }),
            function_call_added(1, "call_1", "Read"),
            json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{"type":"function_call","call_id":"call_1","name":"Read","arguments":"{}"}
            }),
            json!({
                "type":"response.completed",
                "response":{"status":"completed","usage":{}}
            }),
        ]);
        let starts: Vec<_> = crate::anthropic::sse::try_parse_sse_events(out.as_bytes())
            .unwrap()
            .into_iter()
            .filter(|event| event.event.as_deref() == Some("content_block_start"))
            .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
            .map(|value| {
                (
                    value["index"].as_u64().unwrap(),
                    value["content_block"]["type"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                (0, "server_tool_use".to_string()),
                (1, "web_search_tool_result".to_string()),
                (2, "tool_use".to_string()),
            ]
        );
    }

    #[test]
    fn live_error_closes_only_content_blocks_already_emitted_downstream() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = translator
            .accept(&function_call_added(0, "call_1", "Read"), None)
            .unwrap();
        out.extend(
            translator
                .accept(
                    &json!({
                        "type":"response.output_item.added",
                        "output_index":1,
                        "item":{"type":"web_search_call","id":"ws_1"}
                    }),
                    None,
                )
                .unwrap(),
        );
        out.extend(
            translator
                .accept(
                    &json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{
                            "type":"function_call",
                            "call_id":"call_1",
                            "name":"Read",
                            "arguments":"{}"
                        }
                    }),
                    None,
                )
                .unwrap(),
        );
        let error = translator
            .accept(
                &json!({
                    "type":"response.failed",
                    "response":{"error":{"message":"upstream timed out"}}
                }),
                None,
            )
            .unwrap_err();
        out.extend(translator.error_chunk(&error, "api_error", None));

        let values: Vec<serde_json::Value> = crate::anthropic::sse::try_parse_sse_events(&out)
            .unwrap()
            .into_iter()
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect();
        let starts: Vec<_> = values
            .iter()
            .filter(|value| value["type"] == "content_block_start")
            .collect();
        let stops: Vec<_> = values
            .iter()
            .filter(|value| value["type"] == "content_block_stop")
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(starts[0]["content_block"]["type"], "tool_use");
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0]["index"], 0);
        assert!(values.iter().any(|value| value["type"] == "error"));
        assert!(
            !values
                .iter()
                .any(|value| value["content_block"]["type"] == "server_tool_use")
        );
        assert!(!values.iter().any(|value| {
            value["type"] == "content_block_delta" && value["delta"]["type"] == "input_json_delta"
        }));
    }

    #[test]
    fn live_error_discards_never_emitted_deferred_function_block() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-sol");
        let mut out = translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                }),
                None,
            )
            .unwrap();
        out.extend(
            translator
                .accept(&function_call_added(1, "call_1", "Read"), None)
                .unwrap(),
        );
        out.extend(
            translator
                .accept(
                    &json!({
                        "type":"response.function_call_arguments.delta",
                        "output_index":1,
                        "delta":"{}"
                    }),
                    None,
                )
                .unwrap(),
        );
        let error = translator
            .accept(
                &json!({
                    "type":"response.failed",
                    "response":{"error":{"message":"upstream timed out"}}
                }),
                None,
            )
            .unwrap_err();
        out.extend(translator.error_chunk(&error, "api_error", None));

        let values: Vec<serde_json::Value> = crate::anthropic::sse::try_parse_sse_events(&out)
            .unwrap()
            .into_iter()
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect();
        assert!(!values.iter().any(|value| {
            matches!(
                value["type"].as_str(),
                Some("content_block_start" | "content_block_delta" | "content_block_stop")
            )
        }));
        assert!(values.iter().any(|value| value["type"] == "error"));
        assert!(!values.iter().any(|value| value["type"] == "message_stop"));
    }

    #[test]
    fn interleaved_web_and_function_order_matches_all_response_paths() {
        let events = vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"web_search_call","id":"ws_1"}
            }),
            function_call_added(1, "call_1", "Read"),
            json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{
                    "type":"function_call",
                    "call_id":"call_1",
                    "name":"Read",
                    "arguments":"{}"
                }
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"web_search_call",
                    "id":"ws_1",
                    "action":{
                        "query":"rust",
                        "sources":[{
                            "type":"url",
                            "title":"Rust",
                            "url":"https://www.rust-lang.org"
                        }]
                    }
                }
            }),
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","status":"completed","usage":{}}
            }),
        ];
        let upstream: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();

        let live = render(events);
        let buffered =
            super::super::stream::translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5")
                .unwrap();
        let reduced = super::super::reducer::reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let non_stream =
            super::super::accumulate::accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5")
                .unwrap();

        let streaming_starts = |bytes: &[u8]| {
            crate::anthropic::sse::try_parse_sse_events(bytes)
                .unwrap()
                .into_iter()
                .filter(|event| event.event.as_deref() == Some("content_block_start"))
                .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
                .map(|value| {
                    (
                        value["index"].as_u64().unwrap(),
                        value["content_block"]["type"].as_str().unwrap().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = vec![
            (0, "server_tool_use".to_string()),
            (1, "web_search_tool_result".to_string()),
            (2, "tool_use".to_string()),
        ];
        assert_eq!(streaming_starts(live.as_bytes()), expected);
        assert_eq!(streaming_starts(&buffered), expected);

        let mut reduced_starts = reduced
            .iter()
            .flat_map(|event| match event {
                super::super::reducer::ReducerEvent::WebSearch {
                    index,
                    result_index,
                    ..
                } => vec![
                    (*index as u64, "server_tool_use".to_string()),
                    (*result_index as u64, "web_search_tool_result".to_string()),
                ],
                super::super::reducer::ReducerEvent::ToolStart { index, .. } => {
                    vec![(*index as u64, "tool_use".to_string())]
                }
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        reduced_starts.sort_by_key(|(index, _)| *index);
        assert_eq!(reduced_starts, expected);
        assert_eq!(
            non_stream["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|block| block["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["server_tool_use", "web_search_tool_result", "tool_use"]
        );
    }

    #[test]
    fn keeps_web_search_queries_and_sources_associated_per_call() {
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
                    "action": {
                        "queries": ["alpha", "beta"],
                        "sources": [{"type":"url","title":"Source A","url":"https://a.example"}]
                    }
                }
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "web_search_call", "id": "ws_2"}
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type": "web_search_call",
                    "id": "ws_2",
                    "action": {
                        "query": "gamma",
                        "sources": [{"type":"url","title":"Source B","url":"https://b.example"}]
                    }
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"status": "completed", "usage": {}}
            }),
        ]);

        let values: Vec<serde_json::Value> =
            crate::anthropic::sse::try_parse_sse_events(out.as_bytes())
                .unwrap()
                .into_iter()
                .map(|event| serde_json::from_str(&event.data).unwrap())
                .collect();
        let tool_inputs: Vec<_> = values
            .iter()
            .filter(|value| value["delta"]["type"] == "input_json_delta")
            .filter_map(|value| value["delta"]["partial_json"].as_str())
            .map(|input| serde_json::from_str::<serde_json::Value>(input).unwrap())
            .collect();
        assert_eq!(
            tool_inputs,
            vec![json!({"query":"alpha\nbeta"}), json!({"query":"gamma"})]
        );

        let results: Vec<_> = values
            .iter()
            .filter_map(|value| {
                let block = value.get("content_block")?;
                (block.get("type")?.as_str()? == "web_search_tool_result").then_some(block)
            })
            .collect();
        let server_uses: Vec<_> = values
            .iter()
            .filter_map(|value| {
                let block = value.get("content_block")?;
                (block.get("type")?.as_str()? == "server_tool_use").then_some(block)
            })
            .collect();
        assert_eq!(server_uses.len(), 2);
        assert!(
            server_uses
                .iter()
                .all(|block| block["caller"]["type"] == "direct")
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "srvtoolu_ws_1");
        assert_eq!(results[0]["content"][0]["url"], "https://a.example");
        assert_eq!(results[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(results[0]["caller"]["type"], "direct");
        assert_eq!(results[0]["content"][0]["encrypted_content"], "");
        assert!(results[0]["content"][0]["page_age"].is_null());
        assert_eq!(results[1]["tool_use_id"], "srvtoolu_ws_2");
        assert_eq!(results[1]["content"][0]["url"], "https://b.example");
        assert_eq!(results[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(results[1]["caller"]["type"], "direct");
        assert!(out.contains(r#""web_search_requests":2"#));
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
    fn cumulative_live_input_is_bounded_across_many_valid_deltas() {
        let mut translator = LiveStreamTranslator::new("msg_limit", "gpt-5.5");
        translator
            .accept(
                &json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"message","id":"msg_up"}
                }),
                None,
            )
            .unwrap();
        let event = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "x".repeat(512 * 1024),
        });
        let event_bytes = serde_json::to_vec(&event).unwrap().len();
        let accepted =
            (MAX_LIVE_TRANSLATOR_INPUT_BYTES - translator.upstream_input_bytes) / event_bytes;
        for _ in 0..accepted {
            translator.accept(&event, None).unwrap();
        }
        let error = translator.accept(&event, None).unwrap_err();
        assert!(error.contains("cumulative size limit"));
        assert!(translator.upstream_input_bytes <= MAX_LIVE_TRANSLATOR_INPUT_BYTES);
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
                "summary_index":0,
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
