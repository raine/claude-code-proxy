use crate::anthropic::sse::parse_sse_events;

use super::read_rewrite::sanitize_read_args;
use super::reasoning_signature::{PendingReasoning, ReasoningReplay, encode_reasoning_signature};
use super::request::ResponsesInputItem;

#[derive(Debug, Clone)]
pub struct UpstreamStreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
    pub diagnostics: Option<UpstreamStreamDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    RateLimit,
    Overloaded,
    Transient,
    Failed,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpstreamStreamDiagnostics {
    pub event_count: usize,
    pub last_event_type: Option<String>,
    pub saw_terminal_event: bool,
    pub open_blocks: Vec<OpenBlockDiagnostic>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenBlockDiagnostic {
    pub output_index: usize,
    pub anthropic_index: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub input_tokens_details_cached: Option<u64>,
    pub output_tokens_details_reasoning: Option<u64>,
}

pub type StopReason = &'static str;
pub const STOP_END_TURN: &str = "end_turn";
pub const STOP_TOOL_USE: &str = "tool_use";
pub const STOP_MAX_TOKENS: &str = "max_tokens";

pub type TerminalType = &'static str;
pub const TERM_COMPLETED: &str = "response.completed";
pub const TERM_INCOMPLETE: &str = "response.incomplete";
pub const TERM_DONE: &str = "response.done";

const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES: usize = 1_024;
const BUFFERED_TOOL_MAX_ARGS_BYTES: usize = 5_000_000;

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    ThinkingStart {
        index: usize,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ThinkingSignature {
        index: usize,
        signature: String,
    },
    ThinkingStop {
        index: usize,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    TextStop {
        index: usize,
    },
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolDelta {
        index: usize,
        partial_json: String,
    },
    ToolStop {
        index: usize,
    },
    ToolProgress {
        index: usize,
    },
    Progress,
    WebSearch {
        index: usize,
        result_index: usize,
        id: String,
        query: String,
    },
    Finish {
        stop_reason: StopReason,
        terminal_type: String,
        continuation_eligible: bool,
        usage: Option<CodexUsage>,
        web_search_requests: usize,
        response_id: Option<String>,
        output_items: Vec<ResponsesInputItem>,
    },
}

#[derive(Debug, Clone)]
pub struct FinishMetadata {
    pub continuation_eligible: bool,
    pub response_id: Option<String>,
    pub output_items: Vec<ResponsesInputItem>,
}

enum BlockState {
    Text {
        index: usize,
        text_accum: String,
    },
    Tool {
        index: usize,
        #[allow(dead_code)]
        output_index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        had_delta: bool,
        buffer_until_done: bool,
        emitted_args: bool,
    },
}

#[derive(Clone, Copy)]
struct ActiveThinking {
    output_index: usize,
    anthropic_index: usize,
}

fn reasoning_input_item(replay: ReasoningReplay) -> ResponsesInputItem {
    ResponsesInputItem::Reasoning {
        id: replay.id,
        summary: Vec::new(),
        encrypted_content: replay.encrypted_content,
    }
}

fn finalize_active_thinking(
    active: ActiveThinking,
    out: &mut Vec<ReducerEvent>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some(replay) = reasoning_by_output_index
        .remove(&active.output_index)
        .and_then(|pending| pending.replay())
        && let Some(signature) = encode_reasoning_signature(&replay)
    {
        out.push(ReducerEvent::ThinkingSignature {
            index: active.anthropic_index,
            signature,
        });
        output_items_by_index.insert(active.output_index, reasoning_input_item(replay));
    }
    out.push(ReducerEvent::ThinkingStop {
        index: active.anthropic_index,
    });
}

fn close_thinking(
    out: &mut Vec<ReducerEvent>,
    active_thinking: &mut Option<ActiveThinking>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some(active) = active_thinking.take() {
        finalize_active_thinking(
            active,
            out,
            reasoning_by_output_index,
            output_items_by_index,
        );
    }
}

fn emit_signature_only_reasoning(
    output_index: usize,
    anthropic_index: &mut usize,
    out: &mut Vec<ReducerEvent>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    let Some(replay) = reasoning_by_output_index
        .remove(&output_index)
        .and_then(|pending| pending.replay())
    else {
        return;
    };
    let Some(signature) = encode_reasoning_signature(&replay) else {
        return;
    };
    let index = *anthropic_index;
    *anthropic_index += 1;
    out.push(ReducerEvent::ThinkingStart { index });
    out.push(ReducerEvent::ThinkingSignature { index, signature });
    out.push(ReducerEvent::ThinkingStop { index });
    output_items_by_index.insert(output_index, reasoning_input_item(replay));
}

pub fn finish_metadata_from_upstream(
    input: &[u8],
) -> Result<Option<FinishMetadata>, UpstreamStreamError> {
    let events = reduce_upstream_bytes(input)?;
    Ok(events.into_iter().rev().find_map(|event| match event {
        ReducerEvent::Finish {
            continuation_eligible,
            response_id,
            output_items,
            ..
        } => Some(FinishMetadata {
            continuation_eligible,
            response_id,
            output_items,
        }),
        _ => None,
    }))
}

pub fn reduce_upstream_bytes(input: &[u8]) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    let sse_events = parse_sse_events(input);
    let mut out = Vec::new();

    let mut blocks_by_output_index: std::collections::HashMap<usize, BlockState> =
        std::collections::HashMap::new();
    let mut output_items_by_index: std::collections::BTreeMap<usize, ResponsesInputItem> =
        std::collections::BTreeMap::new();
    let mut item_id_to_output_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut reasoning_by_output_index: std::collections::HashMap<usize, PendingReasoning> =
        std::collections::HashMap::new();
    let mut anthropic_index = 0usize;
    let mut active_thinking: Option<ActiveThinking> = None;
    let mut saw_tool_use = false;
    let mut final_usage: Option<CodexUsage> = None;
    let mut response_id: Option<String> = None;
    let mut terminal_type: Option<String> = None;
    let mut continuation_eligible = false;
    let mut incomplete = false;
    let mut web_search_requests = 0usize;
    let mut _saw_terminal = false;
    let mut event_count = 0usize;
    let mut last_event_type: Option<String> = None;

    fn capture_output_item(
        output_index: usize,
        state: &BlockState,
        items: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
    ) {
        match state {
            BlockState::Text {
                index: _,
                text_accum,
            } => {
                if text_accum.is_empty() {
                    return;
                }
                items.insert(
                    output_index,
                    ResponsesInputItem::Message {
                        role: "assistant".to_string(),
                        content: vec![super::request::ResponsesContentPart::OutputText {
                            text: text_accum.clone(),
                        }],
                    },
                );
            }
            BlockState::Tool {
                args_accum,
                name,
                call_id,
                ..
            } => {
                items.insert(
                    output_index,
                    ResponsesInputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: args_accum.clone(),
                    },
                );
            }
        }
    }

    for evt in &sse_events {
        let data = evt.data.trim();
        if data.is_empty() {
            continue;
        }

        let p: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let t = p
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        event_count += 1;
        last_event_type = Some(t.clone());

        if t == "codex.rate_limits" {
            crate::providers::codex::rate_limits::record_event(&p);
            if let Some(true) = p
                .get("rate_limits")
                .and_then(|r| r.get("limit_reached"))
                .and_then(|v| v.as_bool())
            {
                let retry_after = p
                    .get("rate_limits")
                    .and_then(|r| r.get("primary"))
                    .and_then(|r| r.get("reset_after_seconds"))
                    .and_then(|v| v.as_f64());
                return Err(UpstreamStreamError {
                    kind: UpstreamErrorKind::RateLimit,
                    message: "rate limit reached".to_string(),
                    retry_after_seconds: retry_after.map(|f| f as u64),
                    diagnostics: None,
                });
            }
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "keepalive" {
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "response.web_search_call.in_progress"
            || t == "response.web_search_call.searching"
            || t == "response.web_search_call.completed"
        {
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "response.failed" || t == "response.error" || t == "error" {
            let msg = p
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    p.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("Upstream error");
            let kind = upstream_failure_kind(&p, msg);
            let retry_after = retry_after_from_payload(&p);
            return Err(UpstreamStreamError {
                kind,
                message: msg.to_string(),
                retry_after_seconds: retry_after,
                diagnostics: None,
            });
        }

        if t == "response.output_item.added" {
            let item = match p.get("item") {
                Some(v) => v,
                None => continue,
            };
            let output_index: usize =
                p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "reasoning" {
                reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
                continue;
            }
            if item_type == "web_search_call" {
                out.push(ReducerEvent::Progress);
                continue;
            }

            if item_type == "message" {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let idx = anthropic_index;
                anthropic_index += 1;
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    item_id_to_output_index.insert(id.to_string(), output_index);
                }
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Text {
                        index: idx,
                        text_accum: String::new(),
                    },
                );
                out.push(ReducerEvent::TextStart { index: idx });
                continue;
            }

            if item_type == "function_call" {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                saw_tool_use = true;
                let idx = anthropic_index;
                anthropic_index += 1;
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
                let buffer_until_done = should_buffer_tool_args(&name);
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Tool {
                        index: idx,
                        output_index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        had_delta: false,
                        buffer_until_done,
                        emitted_args: false,
                    },
                );
                out.push(ReducerEvent::ToolStart {
                    index: idx,
                    id: call_id,
                    name,
                });
                continue;
            }

            continue;
        }

        if t == "response.reasoning_summary_part.added" {
            let output_index = p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(active) =
                active_thinking.filter(|active| active.output_index == output_index)
            {
                out.push(ReducerEvent::ThinkingDelta {
                    index: active.anthropic_index,
                    text: "\n\n".to_string(),
                });
            }
            continue;
        }

        if t == "response.reasoning_summary_text.delta" {
            let output_index = p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            if active_thinking.map(|active| active.output_index) != Some(output_index) {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let index = anthropic_index;
                anthropic_index += 1;
                active_thinking = Some(ActiveThinking {
                    output_index,
                    anthropic_index: index,
                });
                out.push(ReducerEvent::ThinkingStart { index });
            }
            out.push(ReducerEvent::ThinkingDelta {
                index: active_thinking
                    .expect("thinking block was started")
                    .anthropic_index,
                text: delta.to_string(),
            });
            continue;
        }

        if t == "response.output_text.delta" {
            close_thinking(
                &mut out,
                &mut active_thinking,
                &mut reasoning_by_output_index,
                &mut output_items_by_index,
            );
            let output_index = p
                .get("output_index")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let item_id = p.get("item_id").and_then(|v| v.as_str());
            let state = if let Some(oi) = output_index {
                blocks_by_output_index.get_mut(&oi)
            } else if let Some(id) = item_id {
                item_id_to_output_index
                    .get(id)
                    .and_then(|oi| blocks_by_output_index.get_mut(oi))
            } else {
                None
            };
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            match state {
                Some(BlockState::Text { index, text_accum }) => {
                    text_accum.push_str(delta);
                    out.push(ReducerEvent::TextDelta {
                        index: *index,
                        text: delta.to_string(),
                    });
                }
                _ => continue,
            }
            continue;
        }

        if t == "response.function_call_arguments.delta" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            let state = match blocks_by_output_index.get_mut(&output_index) {
                Some(s) => s,
                None => continue,
            };
            let mut repaired_read: Option<(usize, String)> = None;
            match state {
                BlockState::Tool {
                    args_accum,
                    had_delta,
                    buffer_until_done,
                    emitted_args,
                    name,
                    index,
                    call_id,
                    ..
                } => {
                    args_accum.push_str(delta);
                    *had_delta = true;
                    if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                        return Err(UpstreamStreamError {
                            kind: UpstreamErrorKind::Failed,
                            message: format!("Buffered {name} tool arguments exceeded safe limits"),
                            retry_after_seconds: None,
                            diagnostics: None,
                        });
                    }

                    if *buffer_until_done {
                        if let Some(repaired) = repair_whitespace_stalled_read_args(
                            name,
                            args_accum,
                            Some(call_id.as_str()),
                        ) {
                            *args_accum = repaired.clone();
                            *emitted_args = true;
                            repaired_read = Some((*index, repaired));
                        } else {
                            out.push(ReducerEvent::ToolProgress { index: *index });
                        }
                    } else {
                        *emitted_args = true;
                        out.push(ReducerEvent::ToolDelta {
                            index: *index,
                            partial_json: delta.to_string(),
                        });
                    }
                }
                _ => continue,
            }
            if let Some((index, repaired)) = repaired_read {
                if let Some(state) = blocks_by_output_index.remove(&output_index) {
                    capture_output_item(output_index, &state, &mut output_items_by_index);
                }
                out.push(ReducerEvent::ToolDelta {
                    index,
                    partial_json: repaired,
                });
                out.push(ReducerEvent::ToolStop { index });
                let output_items: Vec<ResponsesInputItem> =
                    output_items_by_index.into_values().collect();
                out.push(ReducerEvent::Finish {
                    stop_reason: STOP_TOOL_USE,
                    terminal_type: TERM_INCOMPLETE.to_string(),
                    continuation_eligible: false,
                    usage: None,
                    web_search_requests,
                    response_id: None,
                    output_items,
                });
                return Ok(out);
            }
            continue;
        }

        if t == "response.function_call_arguments.done" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            if let Some(BlockState::Tool { args_accum, .. }) =
                blocks_by_output_index.get_mut(&output_index)
                && let Some(args) = p.get("arguments").and_then(|v| v.as_str())
                && args_accum.is_empty()
            {
                *args_accum = args.to_string();
            }
            continue;
        }

        if t == "response.output_item.done" {
            let output_index: usize =
                p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let item = p.get("item");

            if let Some(item_val) = item
                && item_val.get("type").and_then(|v| v.as_str()) == Some("reasoning")
            {
                reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item_val);
                let had_active_summary =
                    active_thinking.is_some_and(|active| active.output_index == output_index);
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                if !had_active_summary {
                    emit_signature_only_reasoning(
                        output_index,
                        &mut anthropic_index,
                        &mut out,
                        &mut reasoning_by_output_index,
                        &mut output_items_by_index,
                    );
                }
                continue;
            }

            if let Some(item_val) = item
                && item_val.get("type").and_then(|v| v.as_str()) == Some("web_search_call")
            {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let idx = anthropic_index;
                anthropic_index += 1;
                let result_index = anthropic_index;
                anthropic_index += 1;
                web_search_requests += 1;
                let id_val = item_val.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let id = server_tool_use_id_from_codex_web_search_id(id_val);
                let query = web_search_query(item_val);
                out.push(ReducerEvent::WebSearch {
                    index: idx,
                    result_index,
                    id,
                    query,
                });
                continue;
            }

            let state = blocks_by_output_index.remove(&output_index);
            let mut state = match state {
                Some(s) => s,
                None => continue,
            };

            if let Some(item_val) = item {
                if let BlockState::Tool {
                    args_accum,
                    name,
                    call_id,
                    buffer_until_done,
                    emitted_args,
                    had_delta,
                    index,
                    ..
                } = &mut state
                {
                    if let Some(final_args) = item_val
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        if !args_accum.is_empty() && !*emitted_args {
                            // Already have accum from deltas - skip
                        } else if *had_delta {
                            // Already emitted deltas
                        } else {
                            *args_accum = final_args.to_string();
                        }
                    }

                    if !args_accum.is_empty() {
                        let sanitized =
                            sanitize_read_args(name, args_accum, Some(call_id.as_str()));
                        *args_accum = sanitized;
                        if *buffer_until_done || !*emitted_args {
                            *emitted_args = true;
                            out.push(ReducerEvent::ToolDelta {
                                index: *index,
                                partial_json: args_accum.clone(),
                            });
                        }
                    }
                }
            }

            capture_output_item(output_index, &state, &mut output_items_by_index);

            match &state {
                BlockState::Text { index, .. } => {
                    out.push(ReducerEvent::TextStop { index: *index });
                }
                BlockState::Tool { index, .. } => {
                    out.push(ReducerEvent::ToolStop { index: *index });
                }
            }
            continue;
        }

        if t == "response.completed" || t == "response.incomplete" || t == "response.done" {
            _saw_terminal = true;
            terminal_type = Some(t.clone());
            response_id = p
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            final_usage = p.get("response").map(parse_codex_usage);
            if response_is_incomplete(&p, &t) {
                incomplete = true;
            }
            continuation_eligible =
                (t == "response.completed" || t == "response.done") && !incomplete;
            continue;
        }
    }

    let open_blocks = describe_open_blocks(&blocks_by_output_index);
    if !_saw_terminal || !open_blocks.is_empty() {
        let diagnostics = UpstreamStreamDiagnostics {
            event_count,
            last_event_type,
            saw_terminal_event: _saw_terminal,
            open_blocks,
        };
        return Err(UpstreamStreamError {
            kind: UpstreamErrorKind::Transient,
            message: if diagnostics.saw_terminal_event {
                "upstream stream ended with open Codex output blocks".to_string()
            } else {
                "upstream stream ended before terminal Codex response event".to_string()
            },
            retry_after_seconds: None,
            diagnostics: Some(diagnostics),
        });
    }

    close_thinking(
        &mut out,
        &mut active_thinking,
        &mut reasoning_by_output_index,
        &mut output_items_by_index,
    );

    let stop_reason: StopReason = if incomplete {
        STOP_MAX_TOKENS
    } else if saw_tool_use {
        STOP_TOOL_USE
    } else {
        STOP_END_TURN
    };

    let output_items: Vec<ResponsesInputItem> = output_items_by_index.into_values().collect();

    out.push(ReducerEvent::Finish {
        stop_reason,
        terminal_type: terminal_type.unwrap_or_else(|| TERM_INCOMPLETE.to_string()),
        continuation_eligible,
        usage: final_usage,
        web_search_requests,
        response_id,
        output_items,
    });

    Ok(out)
}

fn describe_open_blocks(
    blocks: &std::collections::HashMap<usize, BlockState>,
) -> Vec<OpenBlockDiagnostic> {
    let mut out: Vec<_> = blocks
        .iter()
        .map(|(output_index, state)| match state {
            BlockState::Text { index, text_accum } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "text".to_string(),
                name: None,
                call_id: None,
                text_bytes: Some(text_accum.len()),
                argument_bytes: None,
            },
            BlockState::Tool {
                index,
                call_id,
                name,
                args_accum,
                ..
            } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "tool".to_string(),
                name: Some(name.clone()),
                call_id: Some(call_id.clone()),
                text_bytes: None,
                argument_bytes: Some(args_accum.len()),
            },
        })
        .collect();
    out.sort_by_key(|block| block.output_index);
    out
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

fn response_is_incomplete(payload: &serde_json::Value, event_type: &str) -> bool {
    event_type == "response.incomplete"
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

fn should_buffer_tool_args(name: &str) -> bool {
    name == "Read"
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

fn web_search_query(item: &serde_json::Value) -> String {
    let action = match item.get("action") {
        Some(v) => v,
        None => return String::new(),
    };
    if let Some(query) = action.get("query").and_then(|v| v.as_str()) {
        return query.to_string();
    }
    if let Some(queries) = action.get("queries").and_then(|v| v.as_array()) {
        for q in queries {
            if let Some(s) = q.as_str() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn server_tool_use_id_from_codex_web_search_id(id: &str) -> String {
    let suffix: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("srvtoolu_{suffix}")
}

fn upstream_failure_kind(payload: &serde_json::Value, message: &str) -> UpstreamErrorKind {
    let status = payload
        .get("status")
        .or_else(|| payload.get("status_code"))
        .and_then(|v| v.as_u64());
    let code = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("code"))
        .or_else(|| payload.get("error").and_then(|e| e.get("code")))
        .and_then(|v| v.as_str());
    let err_type = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("type"))
        .or_else(|| payload.get("error").and_then(|e| e.get("type")))
        .and_then(|v| v.as_str());
    let lower_msg = message.to_lowercase();

    if status == Some(529)
        || code == Some("overloaded_error")
        || err_type == Some("overloaded_error")
        || lower_msg.contains("overloaded")
    {
        return UpstreamErrorKind::Overloaded;
    }

    if (status.is_some_and(|s| (500..600).contains(&s)))
        || code == Some("server_error")
        || code == Some("internal_server_error")
        || code == Some("internal_error")
        || err_type == Some("server_error")
        || err_type == Some("internal_server_error")
        || err_type == Some("internal_error")
        || is_retryable_transport_message(&lower_msg)
    {
        return UpstreamErrorKind::Transient;
    }

    UpstreamErrorKind::Failed
}

fn retry_after_from_payload(payload: &serde_json::Value) -> Option<u64> {
    let raw = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("retry_after_seconds"))
        .or_else(|| {
            payload
                .get("error")
                .and_then(|e| e.get("retry_after_seconds"))
        })
        .or_else(|| payload.get("retry_after_seconds"))
        .or_else(|| payload.get("headers").and_then(|h| h.get("retry-after")))
        .or_else(|| payload.get("headers").and_then(|h| h.get("Retry-After")));
    let value = match raw {
        Some(v) if v.is_number() => v.as_f64(),
        Some(v) if v.is_string() => v.as_str().and_then(|s| s.parse::<f64>().ok()),
        _ => None,
    };
    value.map(|f| f as u64)
}

fn is_retryable_transport_message(msg: &str) -> bool {
    msg.contains("you can retry your request")
        || msg.contains("socket connection was closed unexpectedly")
        || msg.contains("connection closed unexpectedly")
        || msg.contains("connection reset")
        || msg.contains("operation timed out")
        || msg.contains("econnreset")
        || msg.contains("epipe")
        || msg.contains("etimedout")
        || msg.contains("und_err_socket")
        || msg.contains("fetch failed")
}

pub fn map_codex_usage_to_anthropic(
    u: &Option<CodexUsage>,
    web_search_requests: Option<usize>,
) -> AnthropicUsage {
    let usage = match u {
        Some(u) => u,
        None => return AnthropicUsage::default(),
    };
    let cached = usage.input_tokens_details_cached.unwrap_or(0);
    let total_input = usage.input_tokens.unwrap_or(0);
    let input_tokens = total_input.saturating_sub(cached);

    let mut result = AnthropicUsage {
        input_tokens,
        output_tokens: usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        server_tool_use: None,
    };

    if let Some(requests) = web_search_requests
        && requests > 0
    {
        result.server_tool_use = Some(WebSearchUsage {
            web_search_requests: requests,
        });
    }

    result
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<WebSearchUsage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebSearchUsage {
    pub web_search_requests: usize,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse(type_name: &str, payload: serde_json::Value) -> String {
        let mut obj = payload.as_object().cloned().unwrap_or_default();
        obj.insert("type".into(), json!(type_name));
        format!("data: {}\n\n", serde_json::to_string(&obj).unwrap())
    }

    #[test]
    fn reduce_text_response() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","status":"completed","incomplete_details":null,"usage":{"input_tokens":5,"output_tokens":1}}
                })
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            stop_reason,
            response_id,
            usage,
            ..
        } = last
        {
            assert_eq!(*stop_reason, "end_turn");
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert_eq!(usage.as_ref().unwrap().input_tokens, Some(5));
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_tool_use_response() {
        let upstream = format!(
            "{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                })
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,"delta":"{\"file_path\":"
                })
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,"delta":"\"/tmp/a\"}"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"/tmp/a\"}"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish { stop_reason, .. } = last {
            assert_eq!(*stop_reason, "tool_use");
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_rate_limit_throws() {
        let upstream = sse(
            "codex.rate_limits",
            json!({"rate_limits":{"limit_reached":true,"primary":{"reset_after_seconds":30}}}),
        );
        let result = reduce_upstream_bytes(upstream.as_bytes());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, UpstreamErrorKind::RateLimit);
    }

    #[test]
    fn reduce_repairs_whitespace_stalled_read_args() {
        let upstream = format!(
            "{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                })
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,
                    "delta": format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
                })
            )
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(out.iter().any(|event| {
            matches!(
                event,
                ReducerEvent::ToolDelta { partial_json, .. }
                    if partial_json == "{\"file_path\":\"/tmp/a\"}"
            )
        }));
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            stop_reason,
            continuation_eligible,
            ..
        } = last
        {
            assert_eq!(*stop_reason, "tool_use");
            assert!(!continuation_eligible);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_upstream_error_event() {
        let upstream = sse("error", json!({"error":{"message":"upstream failure"}}));
        let result = reduce_upstream_bytes(upstream.as_bytes());
        assert!(result.is_err());
        match result.unwrap_err().kind {
            UpstreamErrorKind::Failed => {}
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn reduce_web_search_output() {
        let upstream = format!(
            "{}{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1","action":{"query":"test query"}}
                })
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":1,"delta":"result"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":3}}
                })
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let has_web_search = out
            .iter()
            .any(|e| matches!(e, ReducerEvent::WebSearch { .. }));
        assert!(has_web_search, "expected WebSearch event");
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            web_search_requests,
            ..
        } = last
        {
            assert_eq!(*web_search_requests, 1);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_missing_terminal_is_error() {
        let upstream = format!(
            "{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"partial"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
        );
        let err = reduce_upstream_bytes(upstream.as_bytes()).unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::Transient);
        assert!(err.message.contains("terminal"));
    }

    #[test]
    fn reduce_incomplete_is_max_tokens() {
        let upstream = sse(
            "response.incomplete",
            json!({"response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{}}}),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            stop_reason,
            continuation_eligible,
            ..
        } = last
        {
            assert_eq!(*stop_reason, "max_tokens");
            assert!(!continuation_eligible);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_completed_with_null_incomplete_details_is_end_turn() {
        let upstream = sse(
            "response.completed",
            json!({"response":{"id":"resp_1","status":"completed","incomplete_details":null,"usage":{}}}),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            stop_reason,
            continuation_eligible,
            ..
        } = last
        {
            assert_eq!(*stop_reason, "end_turn");
            assert!(continuation_eligible);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_completed_is_continuation_eligible() {
        let upstream = sse(
            "response.completed",
            json!({"response":{"id":"resp_1","usage":{}}}),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            continuation_eligible,
            ..
        } = last
        {
            assert!(continuation_eligible);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn finish_metadata_extracts_continuation_state() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let metadata = finish_metadata_from_upstream(upstream.as_bytes())
            .unwrap()
            .unwrap();
        assert!(metadata.continuation_eligible);
        assert_eq!(metadata.response_id.as_deref(), Some("resp_1"));
        assert_eq!(metadata.output_items.len(), 1);
    }

    #[test]
    fn sanitize_tool_args_removes_empty_pages() {
        let args = r#"{"file_path":"/tmp/a","pages":""}"#;
        let sanitized = sanitize_read_args("Read", args, None);
        let parsed: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert!(parsed.get("pages").is_none());
        assert_eq!(
            parsed.get("file_path").and_then(|v| v.as_str()),
            Some("/tmp/a")
        );
    }

    #[test]
    fn map_usage_reports_cached_prompt_tokens() {
        let usage = CodexUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            input_tokens_details_cached: Some(20),
            output_tokens_details_reasoning: None,
        };
        let mapped = map_codex_usage_to_anthropic(&Some(usage), None);
        assert_eq!(mapped.input_tokens, 80);
        assert_eq!(mapped.output_tokens, 50);
        assert_eq!(mapped.cache_read_input_tokens, 20);
    }

    #[test]
    fn reduce_reasoning_summary_before_text() {
        let upstream = format!(
            "{}{}{}{}{}{}",
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":0,"delta":"Plan"})
            ),
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":0,"delta":"ning"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.output_item.added",
                json!({"output_index":1,"item":{"type":"message","id":"msg_up"}})
            ),
            sse(
                "response.output_text.delta",
                json!({"output_index":1,"delta":"answer"})
            ),
            format!(
                "{}{}",
                sse(
                    "response.output_item.done",
                    json!({"output_index":1,"item":{"type":"message"}})
                ),
                sse(
                    "response.completed",
                    json!({"response":{"id":"resp_1","usage":{}}})
                )
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(matches!(
            out.iter()
                .find(|event| matches!(event, ReducerEvent::ThinkingStart { .. })),
            Some(ReducerEvent::ThinkingStart { index: 0 })
        ));
        let thinking_text: String = out
            .iter()
            .filter_map(|event| match event {
                ReducerEvent::ThinkingDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking_text, "Planning");
        let thinking_stop = out
            .iter()
            .position(|event| matches!(event, ReducerEvent::ThinkingStop { .. }))
            .unwrap();
        let text_start = out
            .iter()
            .position(|event| matches!(event, ReducerEvent::TextStart { .. }))
            .unwrap();
        assert!(thinking_stop < text_start);
        assert!(matches!(
            out.iter()
                .find(|event| matches!(event, ReducerEvent::TextStart { .. })),
            Some(ReducerEvent::TextStart { index: 1 })
        ));
    }

    #[test]
    fn reduce_empty_reasoning_summary_emits_no_thinking() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.output_item.added",
                json!({"output_index":1,"item":{"type":"message","id":"msg_up"}})
            ),
            format!(
                "{}{}{}",
                sse(
                    "response.output_text.delta",
                    json!({"output_index":1,"delta":"answer"})
                ),
                sse(
                    "response.output_item.done",
                    json!({"output_index":1,"item":{"type":"message"}})
                ),
                sse(
                    "response.completed",
                    json!({"response":{"id":"resp_1","usage":{}}})
                )
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(!out.iter().any(|event| matches!(
            event,
            ReducerEvent::ThinkingStart { .. }
                | ReducerEvent::ThinkingDelta { .. }
                | ReducerEvent::ThinkingStop { .. }
        )));
    }

    #[test]
    fn reduce_multiple_reasoning_summary_parts() {
        let upstream = format!(
            "{}{}{}{}{}",
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":0,"delta":"part one"})
            ),
            sse(
                "response.reasoning_summary_part.added",
                json!({"output_index":0,"summary_index":1,"part":{"type":"summary_text","text":""}})
            ),
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":1,"delta":"part two"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let deltas: Vec<&str> = out
            .iter()
            .filter_map(|event| match event {
                ReducerEvent::ThinkingDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["part one", "\n\n", "part two"]);
        assert_eq!(
            out.iter()
                .filter(|event| matches!(event, ReducerEvent::ThinkingStop { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn reduce_two_reasoning_items() {
        let upstream = format!(
            "{}{}{}{}{}",
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":0,"delta":"first"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":1,"summary_index":0,"delta":"second"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":1,"item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert_eq!(
            out.iter()
                .filter(|event| matches!(event, ReducerEvent::ThinkingStart { .. }))
                .count(),
            2
        );
        assert_eq!(
            out.iter()
                .filter(|event| matches!(event, ReducerEvent::ThinkingStop { .. }))
                .count(),
            2
        );
        let deltas: Vec<&str> = out
            .iter()
            .filter_map(|event| match event {
                ReducerEvent::ThinkingDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["first", "second"]);
    }

    #[test]
    fn reasoning_signature_precedes_stop_and_enters_continuation_transcript() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"}
                })
            ),
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":0,"delta":"plan"})
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1","summary":[]}
                })
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            ),
        );
        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let signature_index = events
            .iter()
            .position(|event| matches!(event, ReducerEvent::ThinkingSignature { .. }))
            .unwrap();
        let stop_index = events
            .iter()
            .position(|event| matches!(event, ReducerEvent::ThinkingStop { .. }))
            .unwrap();
        assert!(signature_index < stop_index);

        let ReducerEvent::ThinkingSignature { signature, .. } = &events[signature_index] else {
            unreachable!();
        };
        let replay =
            super::super::reasoning_signature::decode_reasoning_signature(signature).unwrap();
        assert_eq!(replay.id, "rs_1");
        assert_eq!(replay.encrypted_content, "opaque");

        let ReducerEvent::Finish { output_items, .. } = events.last().unwrap() else {
            panic!("expected Finish");
        };
        assert!(matches!(
            output_items.as_slice(),
            [ResponsesInputItem::Reasoning {
                id,
                encrypted_content,
                ..
            }] if id == "rs_1" && encrypted_content == "opaque"
        ));
    }
}
