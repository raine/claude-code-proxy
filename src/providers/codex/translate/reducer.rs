use serde_json::Value;

use crate::providers::translate_shared::{JsonObjectError, parse_json_object};

use super::super::events::validate_terminal_snapshot_status;
use super::read_rewrite::sanitize_read_args;
use super::reasoning_signature::{PendingReasoning, ReasoningReplay, encode_reasoning_signature};
use super::request::ResponsesInputItem;
use super::tool_policy::ToolCallPolicy;
use super::web_search_compat::{WebSearchResult, server_tool_use_id_from_codex_web_search_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputItemKind {
    Reasoning,
    Message,
    FunctionCall,
    WebSearchCall,
}

impl OutputItemKind {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "reasoning" => Some(Self::Reasoning),
            "message" => Some(Self::Message),
            "function_call" => Some(Self::FunctionCall),
            "web_search_call" => Some(Self::WebSearchCall),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::Message => "message",
            Self::FunctionCall => "function_call",
            Self::WebSearchCall => "web_search_call",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct OutputItemIdentity {
    pub kind: OutputItemKind,
    pub item_id: Option<String>,
    pub call_id: Option<String>,
    pub name: Option<String>,
}

pub(super) fn event_type(payload: &Value) -> Result<&str, String> {
    payload
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| "Codex stream event is missing a non-empty type".to_string())
}

pub(crate) fn is_post_terminal_telemetry(payload: &Value) -> bool {
    payload.get("type").and_then(Value::as_str) == Some("codex.rate_limits")
}

pub(super) fn parse_output_item_added(
    payload: &Value,
) -> Result<(usize, &Value, OutputItemIdentity), String> {
    let output_index = required_output_index(payload, "response.output_item.added")?;
    let item = payload
        .get("item")
        .filter(|item| item.is_object())
        .ok_or_else(|| "response.output_item.added is missing an object item".to_string())?;
    let raw_kind = item
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| "response.output_item.added item is missing a non-empty type".to_string())?;
    let kind = OutputItemKind::from_str(raw_kind).ok_or_else(|| {
        format!("unsupported Codex output item type {raw_kind:?} at output_index {output_index}")
    })?;
    let item_id = if kind == OutputItemKind::WebSearchCall {
        Some(required_non_empty_string(item, "id", "web search item id")?)
    } else {
        optional_non_empty_string(item, "id", "output item id")?
    };
    let (call_id, name) = if kind == OutputItemKind::FunctionCall {
        (
            Some(required_non_empty_string(
                item,
                "call_id",
                "function call call_id",
            )?),
            Some(required_non_empty_string(
                item,
                "name",
                "function call name",
            )?),
        )
    } else {
        (None, None)
    };
    Ok((
        output_index,
        item,
        OutputItemIdentity {
            kind,
            item_id,
            call_id,
            name,
        },
    ))
}

pub(super) fn register_output_item(
    output_index: usize,
    identity: OutputItemIdentity,
    identities: &mut std::collections::HashMap<usize, OutputItemIdentity>,
    item_id_to_output_index: &mut std::collections::HashMap<String, usize>,
    call_id_to_output_index: &mut std::collections::HashMap<String, usize>,
) -> Result<(), String> {
    if let Some(existing) = identities.get(&output_index) {
        return Err(format!(
            "duplicate Codex output_index {output_index}: existing {} item cannot be replaced by {} item",
            existing.kind.as_str(),
            identity.kind.as_str()
        ));
    }
    if let Some(item_id) = identity.item_id.as_deref()
        && let Some(existing) = item_id_to_output_index.get(item_id)
    {
        return Err(format!(
            "Codex item_id {item_id:?} is already bound to output_index {existing}"
        ));
    }
    if let Some(call_id) = identity.call_id.as_deref()
        && let Some(existing) = call_id_to_output_index.get(call_id)
    {
        return Err(format!(
            "Codex call_id {call_id:?} is already bound to output_index {existing}"
        ));
    }

    if let Some(item_id) = identity.item_id.as_deref() {
        item_id_to_output_index.insert(item_id.to_string(), output_index);
    }
    if let Some(call_id) = identity.call_id.as_deref() {
        call_id_to_output_index.insert(call_id.to_string(), output_index);
    }
    identities.insert(output_index, identity);
    Ok(())
}

pub(super) fn resolve_semantic_output_index(
    payload: &Value,
    event: &str,
    item_id_to_output_index: &std::collections::HashMap<String, usize>,
    call_id_to_output_index: &std::collections::HashMap<String, usize>,
) -> Result<usize, String> {
    let explicit = match payload.get("output_index") {
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| format!("{event} output_index must be a non-negative integer"))?
                as usize,
        ),
        None => None,
    };
    let raw_item_id = payload
        .get("item_id")
        .or_else(|| payload.get("item").and_then(|item| item.get("id")));
    let item_id = match raw_item_id {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{event} item_id must be a non-empty string"))?,
        ),
        None => None,
    };
    let raw_call_id = payload
        .get("call_id")
        .or_else(|| payload.get("item").and_then(|item| item.get("call_id")));
    let call_id = match raw_call_id {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{event} call_id must be a non-empty string"))?,
        ),
        None => None,
    };

    let item_index = item_id.and_then(|id| item_id_to_output_index.get(id).copied());
    let call_index = call_id.and_then(|id| call_id_to_output_index.get(id).copied());
    if item_id.is_some() && item_index.is_none() {
        return Err(format!("{event} references an unknown item_id"));
    }
    if call_id.is_some() && call_index.is_none() {
        return Err(format!("{event} references an unknown call_id"));
    }

    let output_index = explicit
        .or(item_index)
        .or(call_index)
        .ok_or_else(|| format!("{event} is missing a resolvable output_index/item_id/call_id"))?;
    for (label, candidate) in [("item_id", item_index), ("call_id", call_index)] {
        if let Some(candidate) = candidate
            && candidate != output_index
        {
            return Err(format!(
                "{event} {label} resolves to output_index {candidate}, not {output_index}"
            ));
        }
    }
    Ok(output_index)
}

pub(super) fn unfinished_output_indices(
    identities: &std::collections::HashMap<usize, OutputItemIdentity>,
    completed: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    let mut unfinished: Vec<_> = identities
        .keys()
        .filter(|output_index| !completed.contains(output_index))
        .copied()
        .collect();
    unfinished.sort_unstable();
    unfinished
}

pub(super) fn completed_tool_counts(
    identities: &std::collections::HashMap<usize, OutputItemIdentity>,
    completed: &std::collections::HashSet<usize>,
) -> (usize, usize) {
    identities
        .iter()
        .filter(|(output_index, _)| completed.contains(output_index))
        .fold(
            (0, 0),
            |(function_calls, hosted_calls), (_, identity)| match identity.kind {
                OutputItemKind::FunctionCall => (function_calls + 1, hosted_calls),
                OutputItemKind::WebSearchCall => (function_calls, hosted_calls + 1),
                OutputItemKind::Reasoning | OutputItemKind::Message => {
                    (function_calls, hosted_calls)
                }
            },
        )
}

pub(super) fn registered_tool_count(
    identities: &std::collections::HashMap<usize, OutputItemIdentity>,
) -> usize {
    identities
        .values()
        .filter(|identity| {
            matches!(
                identity.kind,
                OutputItemKind::FunctionCall | OutputItemKind::WebSearchCall
            )
        })
        .count()
}

pub(super) fn registered_hosted_tool_count(
    identities: &std::collections::HashMap<usize, OutputItemIdentity>,
) -> usize {
    identities
        .values()
        .filter(|identity| identity.kind == OutputItemKind::WebSearchCall)
        .count()
}

pub(super) fn validate_output_item_done<'a>(
    payload: &'a Value,
    output_index: usize,
    identity: &OutputItemIdentity,
) -> Result<&'a Value, String> {
    let item = payload
        .get("item")
        .filter(|item| item.is_object())
        .ok_or_else(|| "response.output_item.done is missing an object item".to_string())?;
    let raw_kind = item
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| "response.output_item.done item is missing a non-empty type".to_string())?;
    let kind = OutputItemKind::from_str(raw_kind).ok_or_else(|| {
        format!("unsupported Codex output item type {raw_kind:?} at output_index {output_index}")
    })?;
    if kind != identity.kind {
        return Err(format!(
            "response.output_item.done type {} conflicts with added {} item at output_index {output_index}",
            kind.as_str(),
            identity.kind.as_str()
        ));
    }
    if let Some(expected) = identity.item_id.as_deref() {
        let actual = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "response.output_item.done is missing the non-empty id from output_item.added"
                    .to_string()
            })?;
        if actual != expected {
            return Err(format!(
                "response.output_item.done item id {actual:?} conflicts with {expected:?}"
            ));
        }
    }
    if kind == OutputItemKind::FunctionCall {
        let actual_call_id = required_non_empty_string(
            item,
            "call_id",
            "response.output_item.done function call call_id",
        )?;
        let actual_name = required_non_empty_string(
            item,
            "name",
            "response.output_item.done function call name",
        )?;
        if identity.call_id.as_deref() != Some(actual_call_id.as_str())
            || identity.name.as_deref() != Some(actual_name.as_str())
        {
            return Err(format!(
                "response.output_item.done function identity conflicts at output_index {output_index}"
            ));
        }
    }
    Ok(item)
}

pub(super) fn require_output_kind(
    identities: &std::collections::HashMap<usize, OutputItemIdentity>,
    output_index: usize,
    expected: OutputItemKind,
    event: &str,
) -> Result<(), String> {
    let identity = identities.get(&output_index).ok_or_else(|| {
        format!("{event} references output_index {output_index} before output_item.added")
    })?;
    if identity.kind != expected {
        return Err(format!(
            "{event} expected {} at output_index {output_index}, found {}",
            expected.as_str(),
            identity.kind.as_str()
        ));
    }
    Ok(())
}

pub(super) fn require_active_output_kind(
    identities: &std::collections::HashMap<usize, OutputItemIdentity>,
    completed: &std::collections::HashSet<usize>,
    output_index: usize,
    expected: OutputItemKind,
    event: &str,
) -> Result<(), String> {
    require_output_kind(identities, output_index, expected, event)?;
    if completed.contains(&output_index) {
        return Err(format!(
            "{event} references completed output_index {output_index}"
        ));
    }
    Ok(())
}

pub(super) fn reconcile_output_text_snapshot<'a>(
    accumulated: &str,
    snapshot: &'a str,
    event: &str,
) -> Result<&'a str, String> {
    snapshot.strip_prefix(accumulated).ok_or_else(|| {
        format!(
            "{event} text snapshot disagrees with accumulated output at byte {}",
            accumulated.len()
        )
    })
}

pub(super) fn required_output_index(payload: &Value, event: &str) -> Result<usize, String> {
    payload
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .ok_or_else(|| format!("{event} is missing a non-negative integer output_index"))
}

fn required_non_empty_string(value: &Value, key: &str, label: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("Codex {label} must be a non-empty string"))
}

fn optional_non_empty_string(
    value: &Value,
    key: &str,
    label: &str,
) -> Result<Option<String>, String> {
    match value.get(key) {
        Some(value) => value
            .as_str()
            .filter(|value| !value.is_empty())
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| format!("Codex {label} must be a non-empty string")),
        None => Ok(None),
    }
}

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

fn protocol_error(message: impl Into<String>) -> UpstreamStreamError {
    UpstreamStreamError {
        kind: UpstreamErrorKind::Failed,
        message: message.into(),
        retry_after_seconds: None,
        diagnostics: None,
    }
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
pub const STOP_REFUSAL: &str = "refusal";

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
        sources: Vec<WebSearchResult>,
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
        call_id: String,
        name: String,
        args_accum: String,
        done_snapshot: Option<String>,
        repair_candidate: Option<String>,
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
    finish_metadata_from_upstream_with_tool_policy(input, &ToolCallPolicy::permissive())
}

pub(crate) fn finish_metadata_from_upstream_with_tool_policy(
    input: &[u8],
    tool_policy: &ToolCallPolicy,
) -> Result<Option<FinishMetadata>, UpstreamStreamError> {
    let events = reduce_upstream_bytes_with_tool_policy(input, tool_policy)?;
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
    reduce_upstream_bytes_with_tool_policy(input, &ToolCallPolicy::permissive())
}

pub(crate) fn reduce_upstream_bytes_with_tool_policy(
    input: &[u8],
    tool_policy: &ToolCallPolicy,
) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    let sse_events = super::super::events::parse_codex_sse_events(input).map_err(|message| {
        UpstreamStreamError {
            kind: UpstreamErrorKind::Failed,
            message,
            retry_after_seconds: None,
            diagnostics: None,
        }
    })?;
    let mut out = Vec::new();

    let mut blocks_by_output_index: std::collections::HashMap<usize, BlockState> =
        std::collections::HashMap::new();
    let mut output_items_by_index: std::collections::BTreeMap<usize, ResponsesInputItem> =
        std::collections::BTreeMap::new();
    let mut item_id_to_output_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut call_id_to_output_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut downstream_tool_ids = std::collections::HashSet::new();
    let mut output_identities: std::collections::HashMap<usize, OutputItemIdentity> =
        std::collections::HashMap::new();
    let mut completed_output_indices = std::collections::HashSet::new();
    let mut reasoning_by_output_index: std::collections::HashMap<usize, PendingReasoning> =
        std::collections::HashMap::new();
    let mut anthropic_index = 0usize;
    let mut active_thinking: Option<ActiveThinking> = None;
    let mut saw_tool_use = false;
    let mut final_usage: Option<CodexUsage> = None;
    let mut response_id: Option<String> = None;
    let mut terminal_type: Option<String> = None;
    let mut continuation_eligible = false;
    let mut incomplete_stop_reason: Option<StopReason> = None;
    let mut rewrote_output_items = false;
    let mut web_search_requests = 0usize;
    let mut web_search_output_indices = std::collections::HashSet::new();
    let mut web_search_anthropic_indices: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
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

        if data == "[DONE]" {
            if _saw_terminal {
                continue;
            }
            return Err(protocol_error(
                "Codex [DONE] sentinel arrived before a terminal response event",
            ));
        }

        let p: serde_json::Value = serde_json::from_str(data).map_err(|error| {
            protocol_error(if _saw_terminal {
                format!("invalid Codex event after terminal response event: {error}")
            } else {
                format!("malformed Codex semantic event: {error}")
            })
        })?;

        let t = event_type(&p).map_err(protocol_error)?.to_string();
        validate_terminal_snapshot_status(&p).map_err(protocol_error)?;
        event_count += 1;
        last_event_type = Some(t.clone());

        if _saw_terminal && is_post_terminal_telemetry(&p) {
            // A successful Codex terminal is authoritative. The upstream may append a final
            // quota snapshot, including `limit_reached=true`, after the response has already
            // completed. Treat that tail as telemetry instead of retroactively failing the
            // completed request.
            continue;
        }
        if _saw_terminal {
            return Err(protocol_error(format!(
                "unexpected Codex event {t:?} after terminal response event"
            )));
        }

        if t == "codex.rate_limits" {
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

        if matches!(t.as_str(), "keepalive" | "codex.response.metadata") {
            // `codex.response.metadata` carries response-scoped gateway
            // headers. It is an acknowledged control frame, not model output.
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "response.web_search_call.in_progress"
            || t == "response.web_search_call.searching"
            || t == "response.web_search_call.completed"
        {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::WebSearchCall,
                &t,
            )
            .map_err(protocol_error)?;
            out.push(ReducerEvent::Progress);
            continue;
        }

        if matches!(
            t.as_str(),
            "response.failed" | "response.error" | "response.cancelled" | "error"
        ) {
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
            let (output_index, item, identity) =
                parse_output_item_added(&p).map_err(protocol_error)?;
            if identity.kind == OutputItemKind::FunctionCall {
                tool_policy
                    .validate_function_name(
                        identity.name.as_deref().expect("validated function name"),
                    )
                    .map_err(protocol_error)?;
            } else if identity.kind == OutputItemKind::WebSearchCall {
                tool_policy
                    .validate_hosted_call(registered_hosted_tool_count(&output_identities))
                    .map_err(protocol_error)?;
            }
            if matches!(
                identity.kind,
                OutputItemKind::FunctionCall | OutputItemKind::WebSearchCall
            ) {
                tool_policy
                    .validate_next_tool_call(registered_tool_count(&output_identities))
                    .map_err(protocol_error)?;
            }
            let item_type = identity.kind;
            let downstream_tool_id = match identity.kind {
                OutputItemKind::FunctionCall => identity.call_id.clone(),
                OutputItemKind::WebSearchCall => Some(
                    server_tool_use_id_from_codex_web_search_id(
                        identity
                            .item_id
                            .as_deref()
                            .expect("validated web search item id"),
                    )
                    .map_err(protocol_error)?,
                ),
                OutputItemKind::Reasoning | OutputItemKind::Message => None,
            };
            if let Some(id) = downstream_tool_id.as_deref()
                && downstream_tool_ids.contains(id)
            {
                return Err(protocol_error(format!(
                    "Codex produced duplicate downstream tool use id {id:?}"
                )));
            }
            register_output_item(
                output_index,
                identity,
                &mut output_identities,
                &mut item_id_to_output_index,
                &mut call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            if let Some(id) = downstream_tool_id {
                downstream_tool_ids.insert(id);
            }

            if item_type == OutputItemKind::Reasoning {
                reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
                continue;
            }
            if item_type == OutputItemKind::WebSearchCall {
                let index = anthropic_index;
                anthropic_index += 1;
                let result_index = anthropic_index;
                anthropic_index += 1;
                web_search_anthropic_indices.insert(output_index, (index, result_index));
                out.push(ReducerEvent::Progress);
                continue;
            }

            if item_type == OutputItemKind::Message {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let idx = anthropic_index;
                anthropic_index += 1;
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

            if item_type == OutputItemKind::FunctionCall {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                saw_tool_use = true;
                let idx = anthropic_index;
                anthropic_index += 1;
                let identity = output_identities
                    .get(&output_index)
                    .expect("registered output item identity");
                let call_id = identity
                    .call_id
                    .clone()
                    .expect("validated function call_id");
                let name = identity.name.clone().expect("validated function name");
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Tool {
                        index: idx,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        done_snapshot: None,
                        repair_candidate: None,
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
        }

        if t == "response.reasoning_summary_part.added" {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            p.get("summary_index")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    protocol_error(format!(
                        "{t} is missing a non-negative integer summary_index"
                    ))
                })?;
            let part_type = p
                .get("part")
                .filter(|part| part.is_object())
                .and_then(|part| part.get("type"))
                .and_then(Value::as_str);
            if part_type != Some("summary_text") {
                return Err(protocol_error(format!(
                    "{t} is missing a summary_text part"
                )));
            }
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::Reasoning,
                &t,
            )
            .map_err(protocol_error)?;
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
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::Reasoning,
                &t,
            )
            .map_err(protocol_error)?;
            p.get("summary_index")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    protocol_error(format!(
                        "{t} is missing a non-negative integer summary_index"
                    ))
                })?;
            let delta = p
                .get("delta")
                .and_then(|v| v.as_str())
                .ok_or_else(|| protocol_error(format!("{t} is missing a string delta")))?;
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
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::Message,
                &t,
            )
            .map_err(protocol_error)?;
            let delta = p
                .get("delta")
                .and_then(|v| v.as_str())
                .ok_or_else(|| protocol_error(format!("{t} is missing a string delta")))?;
            if delta.is_empty() {
                continue;
            }
            match blocks_by_output_index.get_mut(&output_index) {
                Some(BlockState::Text { index, text_accum }) => {
                    text_accum.push_str(delta);
                    out.push(ReducerEvent::TextDelta {
                        index: *index,
                        text: delta.to_string(),
                    });
                }
                Some(BlockState::Tool { .. }) => {
                    return Err(protocol_error(format!(
                        "{t} resolved to a function call at output_index {output_index}"
                    )));
                }
                None => {
                    return Err(protocol_error(format!(
                        "{t} references closed or missing output_index {output_index}"
                    )));
                }
            }
            continue;
        }

        if t == "response.function_call_arguments.delta" {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::FunctionCall,
                &t,
            )
            .map_err(protocol_error)?;
            let delta = p
                .get("delta")
                .and_then(|v| v.as_str())
                .ok_or_else(|| protocol_error(format!("{t} is missing a string delta")))?;
            if delta.is_empty() {
                continue;
            }
            let state = match blocks_by_output_index.get_mut(&output_index) {
                Some(s) => s,
                None => {
                    return Err(protocol_error(format!(
                        "{t} references closed or missing output_index {output_index}"
                    )));
                }
            };
            match state {
                BlockState::Tool {
                    args_accum,
                    done_snapshot,
                    name,
                    index,
                    call_id,
                    repair_candidate,
                    ..
                } => {
                    // The done event is an authoritative boundary. A later
                    // delta is an upstream ordering violation, not a replay to
                    // silently discard.
                    if done_snapshot.is_some() {
                        return Err(protocol_error(format!(
                            "{t} arrived after arguments.done at output_index {output_index}"
                        )));
                    }
                    args_accum.push_str(delta);
                    if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                        return Err(UpstreamStreamError {
                            kind: UpstreamErrorKind::Failed,
                            message: format!("Buffered {name} tool arguments exceeded safe limits"),
                            retry_after_seconds: None,
                            diagnostics: None,
                        });
                    }

                    // Cache a repairable Read snapshot, but do not turn it
                    // into a committed tool call before authoritative
                    // output_item.done.
                    *repair_candidate = repair_whitespace_stalled_read_args(
                        name,
                        args_accum,
                        Some(call_id.as_str()),
                    );
                    out.push(ReducerEvent::ToolProgress { index: *index });
                }
                BlockState::Text { .. } => {
                    return Err(protocol_error(format!(
                        "{t} resolved to a message at output_index {output_index}"
                    )));
                }
            }
            continue;
        }

        if t == "response.function_call_arguments.done" {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::FunctionCall,
                &t,
            )
            .map_err(protocol_error)?;
            let args = p
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol_error(format!("{t} is missing string arguments")))?;
            let Some(BlockState::Tool {
                name,
                done_snapshot,
                ..
            }) = blocks_by_output_index.get_mut(&output_index)
            else {
                return Err(protocol_error(format!(
                    "{t} references closed or missing output_index {output_index}"
                )));
            };
            if done_snapshot.is_some() {
                return Err(protocol_error(format!(
                    "duplicate {t} for output_index {output_index}"
                )));
            }
            if args.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                return Err(protocol_error(format!(
                    "Buffered {name} tool arguments exceeded safe limits"
                )));
            }
            *done_snapshot = Some(args.to_string());
            continue;
        }

        if t == "response.output_item.done" {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            let identity = output_identities
                .get(&output_index)
                .cloned()
                .ok_or_else(|| {
                    protocol_error(format!(
                        "{t} references output_index {output_index} before output_item.added"
                    ))
                })?;
            if completed_output_indices.contains(&output_index) {
                return Err(protocol_error(format!(
                    "duplicate {t} for output_index {output_index}"
                )));
            }
            let item_val =
                validate_output_item_done(&p, output_index, &identity).map_err(protocol_error)?;

            if identity.kind == OutputItemKind::Reasoning {
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
                completed_output_indices.insert(output_index);
                continue;
            }

            if identity.kind == OutputItemKind::WebSearchCall {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                if !web_search_output_indices.insert(output_index) {
                    return Err(protocol_error(format!(
                        "duplicate web search completion at output_index {output_index}"
                    )));
                }
                let (idx, result_index) = web_search_anthropic_indices
                    .remove(&output_index)
                    .ok_or_else(|| {
                        protocol_error(format!(
                            "{t} references missing web search indices at output_index {output_index}"
                        ))
                    })?;
                web_search_requests += 1;
                let id = server_tool_use_id_from_codex_web_search_id(
                    identity
                        .item_id
                        .as_deref()
                        .expect("validated web search item id"),
                )
                .map_err(protocol_error)?;
                let query = web_search_query(item_val);
                let sources = web_search_sources(item_val);
                out.push(ReducerEvent::WebSearch {
                    index: idx,
                    result_index,
                    id,
                    query,
                    sources,
                });
                completed_output_indices.insert(output_index);
                continue;
            }

            let mut state = blocks_by_output_index
                .remove(&output_index)
                .ok_or_else(|| {
                    protocol_error(format!(
                        "{t} references closed or missing output_index {output_index}"
                    ))
                })?;

            if let BlockState::Tool {
                args_accum,
                name,
                call_id,
                done_snapshot,
                repair_candidate,
                emitted_args,
                index,
                ..
            } = &mut state
            {
                let item_snapshot = item_val.get("arguments").and_then(|v| v.as_str());
                let used_repair_candidate = done_snapshot.is_none()
                    && item_snapshot.is_none()
                    && repair_candidate.is_some();
                let authoritative = done_snapshot
                    .as_deref()
                    .or(item_snapshot)
                    .or(repair_candidate.as_deref())
                    .unwrap_or(args_accum)
                    .to_string();
                let sanitized = if authoritative.is_empty() {
                    authoritative.clone()
                } else {
                    sanitize_read_args(name, &authoritative, Some(call_id.as_str()))
                };
                rewrote_output_items |= used_repair_candidate || sanitized != authoritative;
                *args_accum = sanitized;

                validate_tool_arguments(name, call_id, args_accum).map_err(|message| {
                    UpstreamStreamError {
                        kind: UpstreamErrorKind::Failed,
                        message,
                        retry_after_seconds: None,
                        diagnostics: None,
                    }
                })?;

                if !*emitted_args {
                    *emitted_args = true;
                    out.push(ReducerEvent::ToolDelta {
                        index: *index,
                        partial_json: args_accum.clone(),
                    });
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
            completed_output_indices.insert(output_index);
            continue;
        }

        if t == "response.completed" || t == "response.incomplete" || t == "response.done" {
            if !p.get("response").is_some_and(Value::is_object) {
                return Err(protocol_error(format!(
                    "{t} is missing an object response snapshot"
                )));
            }
            let unfinished =
                unfinished_output_indices(&output_identities, &completed_output_indices);
            if !unfinished.is_empty() {
                return Err(protocol_error(format!(
                    "{t} arrived with unfinished Codex output items at output_index(es) {unfinished:?}"
                )));
            }
            let terminal_stop_reason = stop_reason_for_incomplete_response(&p, &t);
            if (t == "response.completed" || t == "response.done") && terminal_stop_reason.is_none()
            {
                let (completed_function_calls, completed_hosted_calls) =
                    completed_tool_counts(&output_identities, &completed_output_indices);
                tool_policy
                    .validate_success_terminal(completed_function_calls, completed_hosted_calls)
                    .map_err(protocol_error)?;
            }
            _saw_terminal = true;
            terminal_type = Some(t.clone());
            response_id = p
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            final_usage = p.get("response").map(parse_codex_usage);
            incomplete_stop_reason = terminal_stop_reason;
            continuation_eligible = (t == "response.completed" || t == "response.done")
                && incomplete_stop_reason.is_none();
            continue;
        }

        if matches!(
            t.as_str(),
            "response.created" | "response.in_progress" | "response.queued"
        ) {
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "response.output_text.done" {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::Message,
                &t,
            )
            .map_err(protocol_error)?;
            let snapshot = p
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol_error(format!("{t} is missing a string text snapshot")))?;
            let Some(BlockState::Text { index, text_accum }) =
                blocks_by_output_index.get_mut(&output_index)
            else {
                return Err(protocol_error(format!(
                    "{t} references closed or missing output_index {output_index}"
                )));
            };
            let suffix =
                reconcile_output_text_snapshot(text_accum, snapshot, &t).map_err(protocol_error)?;
            if !suffix.is_empty() {
                text_accum.push_str(suffix);
                out.push(ReducerEvent::TextDelta {
                    index: *index,
                    text: suffix.to_string(),
                });
            }
            continue;
        }

        if matches!(
            t.as_str(),
            "response.content_part.added"
                | "response.content_part.done"
                | "response.output_text.annotation.added"
        ) {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::Message,
                &t,
            )
            .map_err(protocol_error)?;
            if t == "response.output_text.annotation.added" {
                let annotation = p
                    .get("annotation")
                    .filter(|annotation| annotation.is_object())
                    .ok_or_else(|| {
                        protocol_error(format!("{t} is missing an object annotation"))
                    })?;
                if annotation.get("type").and_then(Value::as_str) != Some("url_citation")
                    || annotation
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.is_empty())
                        .is_none()
                {
                    return Err(protocol_error(format!(
                        "{t} contains an invalid url_citation annotation"
                    )));
                }
            }
            out.push(ReducerEvent::Progress);
            continue;
        }

        if matches!(
            t.as_str(),
            "response.reasoning_summary_part.done" | "response.reasoning_summary_text.done"
        ) {
            let output_index = resolve_semantic_output_index(
                &p,
                &t,
                &item_id_to_output_index,
                &call_id_to_output_index,
            )
            .map_err(protocol_error)?;
            require_active_output_kind(
                &output_identities,
                &completed_output_indices,
                output_index,
                OutputItemKind::Reasoning,
                &t,
            )
            .map_err(protocol_error)?;
            out.push(ReducerEvent::Progress);
            continue;
        }

        return Err(protocol_error(format!(
            "unsupported Codex semantic event {t:?}"
        )));
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

    let stop_reason: StopReason = if let Some(reason) = incomplete_stop_reason {
        reason
    } else if saw_tool_use {
        STOP_TOOL_USE
    } else {
        STOP_END_TURN
    };

    let output_items: Vec<ResponsesInputItem> = output_items_by_index.into_values().collect();
    continuation_eligible &= !rewrote_output_items;

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

pub fn stop_reason_for_incomplete_response(
    payload: &serde_json::Value,
    event_type: &str,
) -> Option<StopReason> {
    let response = payload.get("response");
    let reason = response
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(|value| value.as_str());
    let incomplete = event_type == "response.incomplete"
        || payload
            .get("response")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str())
            == Some("incomplete")
        || reason.is_some();
    if !incomplete {
        return None;
    }

    Some(match reason {
        Some("max_output_tokens" | "max_tokens" | "length") | None => STOP_MAX_TOKENS,
        Some("content_filter") => STOP_REFUSAL,
        Some(_) => STOP_REFUSAL,
    })
}

pub(super) fn validate_tool_arguments(
    name: &str,
    call_id: &str,
    arguments: &str,
) -> Result<(), String> {
    match parse_json_object(arguments) {
        Ok(_) => Ok(()),
        Err(JsonObjectError::Empty) => Err(format!(
            "Codex tool call {name} ({call_id}) completed without JSON arguments"
        )),
        Err(JsonObjectError::Invalid(error)) => Err(format!(
            "Codex tool call {name} ({call_id}) returned invalid JSON arguments: {error}"
        )),
        Err(JsonObjectError::NotObject) => Err(format!(
            "Codex tool call {name} ({call_id}) returned non-object JSON arguments"
        )),
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

fn web_search_query(item: &serde_json::Value) -> String {
    let action = match item.get("action") {
        Some(v) => v,
        None => return String::new(),
    };
    if let Some(query) = action.get("query").and_then(|v| v.as_str()) {
        return query.to_string();
    }
    if let Some(queries) = action.get("queries").and_then(|v| v.as_array()) {
        return queries
            .iter()
            .filter_map(|query| query.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn web_search_sources(item: &serde_json::Value) -> Vec<WebSearchResult> {
    let Some(sources) = item
        .get("action")
        .and_then(|action| action.get("sources"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for source in sources {
        let Some(url) = source.get("url").and_then(Value::as_str) else {
            continue;
        };
        if results
            .iter()
            .any(|result: &WebSearchResult| result.url == url)
        {
            continue;
        }
        let title = source
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        results.push(WebSearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }
    results
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
    use super::super::request::{ResponsesRequest, ResponsesToolChoice, ResponsesToolChoiceMode};
    use super::*;
    use serde_json::json;

    fn sse(type_name: &str, payload: serde_json::Value) -> String {
        let mut obj = payload.as_object().cloned().unwrap_or_default();
        obj.insert("type".into(), json!(type_name));
        format!("data: {}\n\n", serde_json::to_string(&obj).unwrap())
    }

    fn response_tool_policy(
        function_names: &[&str],
        hosted_web_search: bool,
        tool_choice: ResponsesToolChoice,
    ) -> ToolCallPolicy {
        response_tool_policy_with_parallel(function_names, hosted_web_search, tool_choice, true)
    }

    fn response_tool_policy_with_parallel(
        function_names: &[&str],
        hosted_web_search: bool,
        tool_choice: ResponsesToolChoice,
        parallel_tool_calls: bool,
    ) -> ToolCallPolicy {
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
        ToolCallPolicy::from_request(&request)
    }

    #[test]
    fn buffered_tool_policy_matches_live_fail_closed_behavior() {
        let hosted = sse(
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "search_1"}
            }),
        );
        let none = response_tool_policy(
            &[],
            true,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None),
        );
        let error = reduce_upstream_bytes_with_tool_policy(hosted.as_bytes(), &none).unwrap_err();
        assert!(error.message.contains("tool_choice forbids"));

        let required = response_tool_policy(
            &["Read"],
            false,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
        );
        let terminal = sse(
            "response.completed",
            json!({
                "response": {
                    "id": "resp_empty",
                    "status": "completed",
                    "incomplete_details": null,
                    "usage": {}
                }
            }),
        );
        let error =
            reduce_upstream_bytes_with_tool_policy(terminal.as_bytes(), &required).unwrap_err();
        assert!(error.message.contains("required by tool_choice"));
    }

    #[test]
    fn buffered_tool_policy_allows_declared_parallel_calls() {
        let upstream = format!(
            "{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 3,
                    "item": {"type": "function_call", "call_id": "call_read", "name": "Read"}
                })
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 7,
                    "item": {"type": "function_call", "call_id": "call_bash", "name": "Bash"}
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index": 7,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_bash",
                        "name": "Bash",
                        "arguments": "{\"command\":\"pwd\"}"
                    }
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index": 3,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_read",
                        "name": "Read",
                        "arguments": "{\"file_path\":\"/tmp/a\"}"
                    }
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response": {
                        "id": "resp_parallel",
                        "status": "completed",
                        "incomplete_details": null,
                        "usage": {}
                    }
                })
            )
        );
        let policy = response_tool_policy(
            &["Read", "Bash"],
            false,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
        );
        let events = reduce_upstream_bytes_with_tool_policy(upstream.as_bytes(), &policy).unwrap();
        let tool_starts = events
            .iter()
            .filter(|event| matches!(event, ReducerEvent::ToolStart { .. }))
            .count();
        assert_eq!(tool_starts, 2);
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                stop_reason: STOP_TOOL_USE,
                ..
            })
        ));
    }

    #[test]
    fn buffered_tool_policy_rejects_multiple_calls_when_parallel_is_disabled() {
        let policy = response_tool_policy_with_parallel(
            &["Read"],
            true,
            ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
            false,
        );
        for second in [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"function_call", "call_id":"call_2", "name":"Read"}
                }),
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"web_search_call", "id":"search_1"}
                }),
            ),
        ] {
            let upstream = format!(
                "{}{}",
                sse(
                    "response.output_item.added",
                    json!({
                        "output_index":0,
                        "item":{"type":"function_call", "call_id":"call_1", "name":"Read"}
                    }),
                ),
                second
            );
            let error =
                reduce_upstream_bytes_with_tool_policy(upstream.as_bytes(), &policy).unwrap_err();
            assert!(
                error.message.contains("parallel_tool_calls is false"),
                "{}",
                error.message
            );
        }
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
                    "output_index":0,"item":{"type":"message","id":"msg_up"}
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
    fn buffered_reducer_ignores_codex_response_metadata_control_frame() {
        let upstream = format!(
            "{}{}{}{}{}{}",
            sse(
                "codex.response.metadata",
                json!({
                    "headers": {
                        "openai-model": "gpt-5.6-sol",
                        "x-codex-safety-buffering-enabled": "true"
                    }
                })
            ),
            sse(
                "response.created",
                json!({"response":{"id":"resp_1","status":"in_progress"}})
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({"output_index":0,"delta":"metadata survived"})
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","status":"completed","usage":{}}
                })
            ),
        );

        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(out.iter().any(|event| matches!(
            event,
            ReducerEvent::TextDelta { text, .. } if text == "metadata survived"
        )));
        assert!(matches!(out.last(), Some(ReducerEvent::Finish { .. })));
    }

    #[test]
    fn buffered_reducer_accepts_stream_start_bom_and_rejects_invalid_utf8() {
        let with_bom = b"\xef\xbb\xbfdata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\n";
        let reduced = reduce_upstream_bytes(with_bom).unwrap();
        assert!(matches!(reduced.last(), Some(ReducerEvent::Finish { .. })));

        let invalid = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\xff\"}\n\n";
        let error = reduce_upstream_bytes(invalid).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::Failed);
        assert!(error.message.contains("invalid UTF-8"));
    }

    #[test]
    fn buffered_reducer_rejects_bom_after_stream_start_instead_of_dropping_event() {
        let input = b"data: {\"type\":\"response.created\"}\n\n\xef\xbb\xbfdata: {\"type\":\"response.output_text.delta\",\"delta\":\"missing\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\n";

        let error = reduce_upstream_bytes(input).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::Failed);
        assert!(error.message.contains("BOM after stream start"));
    }

    #[test]
    fn codex_buffered_malformed_semantic_event_fails() {
        let input = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"truncated\"\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\n",
        );

        let error = reduce_upstream_bytes(input.as_bytes()).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::Failed);
        assert!(error.message.contains("malformed Codex semantic event"));
    }

    #[test]
    fn codex_buffered_unresolved_delta_fails() {
        let input = format!(
            "{}{}",
            sse(
                "response.output_text.delta",
                json!({"item_id":"missing_item","delta":"lost"})
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            )
        );

        let error = reduce_upstream_bytes(input.as_bytes()).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::Failed);
        assert!(error.message.contains("unknown item_id"));
    }

    #[test]
    fn codex_buffered_rejects_duplicate_output_index_and_missing_function_identity() {
        let duplicate = [
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"message","id":"msg_1"}}),
            ),
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"Read"}}),
            ),
        ]
        .concat();
        let error = reduce_upstream_bytes(duplicate.as_bytes()).unwrap_err();
        assert!(error.message.contains("duplicate Codex output_index 0"));

        for item in [
            json!({"type":"function_call","name":"Read"}),
            json!({"type":"function_call","call_id":"call_1"}),
            json!({"type":"function_call","call_id":"","name":"Read"}),
        ] {
            let input = sse(
                "response.output_item.added",
                json!({"output_index":0,"item":item}),
            );
            let error = reduce_upstream_bytes(input.as_bytes()).unwrap_err();
            assert!(error.message.contains("must be a non-empty string"));
        }
    }

    #[test]
    fn codex_buffered_rejects_done_before_added_and_item_index_conflict() {
        let done_before_added = sse(
            "response.output_item.done",
            json!({"output_index":0,"item":{"type":"message","id":"msg_1"}}),
        );
        let error = reduce_upstream_bytes(done_before_added.as_bytes()).unwrap_err();
        assert!(
            error.message.contains("before output_item.added")
                || error.message.contains("unknown item_id")
        );

        let conflict = [
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"message","id":"msg_1"}}),
            ),
            sse(
                "response.output_item.added",
                json!({"output_index":1,"item":{"type":"message","id":"msg_2"}}),
            ),
            sse(
                "response.output_text.delta",
                json!({"output_index":1,"item_id":"msg_1","delta":"wrong target"}),
            ),
        ]
        .concat();
        let error = reduce_upstream_bytes(conflict.as_bytes()).unwrap_err();
        assert!(error.message.contains("resolves to output_index 0, not 1"));

        let missing_done_id = [
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"message","id":"msg_1"}}),
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"message"}}),
            ),
        ]
        .concat();
        let error = reduce_upstream_bytes(missing_done_id.as_bytes()).unwrap_err();
        assert!(error.message.contains("missing the non-empty id"));
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
    fn reducer_uses_done_snapshot_over_item_snapshot() {
        let upstream = [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"Bash"}
                }),
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({"item_id":"item_1","delta":"{\"command\":\"from delta\"}"}),
            ),
            sse(
                "response.function_call_arguments.done",
                json!({"call_id":"call_1","arguments":"{\"command\":\"from done\"}"}),
            ),
            sse(
                "response.output_item.done",
                json!({
                    "item":{
                        "type":"function_call",
                        "id":"item_1",
                        "call_id":"call_1",
                        "name":"Bash",
                        "arguments":"{\"command\":\"from item\"}"
                    }
                }),
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}}),
            ),
        ]
        .concat();

        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ReducerEvent::ToolDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["{\"command\":\"from done\"}"]);

        let Some(ReducerEvent::Finish { output_items, .. }) = events.last() else {
            panic!("expected Finish");
        };
        let Some(ResponsesInputItem::FunctionCall { arguments, .. }) = output_items.first() else {
            panic!("expected function call continuation item");
        };
        assert_eq!(arguments, "{\"command\":\"from done\"}");
    }

    #[test]
    fn reducer_rejects_delta_after_arguments_done() {
        let upstream = [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"Bash"}
                }),
            ),
            sse(
                "response.function_call_arguments.done",
                json!({"output_index":0,"arguments":"{\"command\":\"done\"}"}),
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({"output_index":0,"delta":"{late garbage"}),
            ),
        ]
        .concat();

        let error = reduce_upstream_bytes(upstream.as_bytes()).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::Failed);
        assert!(error.message.contains("after arguments.done"));
    }

    #[test]
    fn reducer_rejects_malformed_done_snapshot_even_when_item_snapshot_is_valid() {
        let upstream = [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Bash"}
                }),
            ),
            sse(
                "response.function_call_arguments.done",
                json!({"output_index":0,"arguments":"{\"command\":"}),
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "call_id":"call_1",
                        "name":"Bash",
                        "arguments":"{\"command\":\"valid but non-authoritative\"}"
                    }
                }),
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}}),
            ),
        ]
        .concat();

        let error = reduce_upstream_bytes(upstream.as_bytes()).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::Failed);
        assert!(error.message.contains("invalid JSON arguments"));
    }

    #[test]
    fn reducer_rejects_invalid_tool_arguments_before_nonstream_accumulation() {
        for (arguments, expected) in [
            ("", "completed without JSON arguments"),
            ("{\"command\":", "invalid JSON arguments"),
            ("[]", "non-object JSON arguments"),
        ] {
            let upstream = format!(
                "{}{}{}",
                sse(
                    "response.output_item.added",
                    json!({
                        "output_index": 0,
                        "item": {"type":"function_call","call_id":"call_1","name":"Bash"}
                    })
                ),
                sse(
                    "response.output_item.done",
                    json!({
                        "output_index": 0,
                        "item": {
                            "type":"function_call",
                            "call_id":"call_1",
                            "name":"Bash",
                            "arguments": arguments
                        }
                    })
                ),
                sse(
                    "response.completed",
                    json!({"response":{"id":"resp_1","usage":{}}})
                ),
            );

            let error = reduce_upstream_bytes(upstream.as_bytes()).unwrap_err();
            assert_eq!(error.kind, UpstreamErrorKind::Failed);
            assert!(
                error.message.contains(expected),
                "unexpected error for {arguments:?}: {}",
                error.message
            );
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
    fn terminal_tail_only_allows_rate_limit_telemetry() {
        let terminal = sse(
            "response.completed",
            json!({"response":{"id":"resp_1","usage":{}}}),
        );
        let telemetry = sse(
            "codex.rate_limits",
            json!({"rate_limits":{"limit_reached":true,"primary":{"reset_after_seconds":30}}}),
        );
        let completed_with_telemetry = format!("{terminal}{telemetry}");
        assert!(reduce_upstream_bytes(completed_with_telemetry.as_bytes()).is_ok());

        for tail in [
            sse(
                "response.output_text.delta",
                json!({"output_index":0,"delta":"late"}),
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                }),
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_2","usage":{}}}),
            ),
            sse(
                "codex.response.metadata",
                json!({"headers":{"openai-model":"gpt-5.6-sol"}}),
            ),
            sse("future.semantic.event", json!({"value":1})),
        ] {
            let input = format!("{terminal}{tail}");
            let error = reduce_upstream_bytes(input.as_bytes()).unwrap_err();
            assert_eq!(error.kind, UpstreamErrorKind::Failed);
            assert!(error.message.contains("after terminal"));
        }
    }

    #[test]
    fn reduce_commits_whitespace_stalled_read_repair_only_after_authoritative_done() {
        let partial = format!(
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
        let error = reduce_upstream_bytes(partial.as_bytes()).unwrap_err();
        assert!(
            error
                .message
                .contains("before terminal Codex response event")
        );

        let upstream = format!(
            "{}{}{}",
            partial,
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                })
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
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
    fn buffered_read_repair_does_not_hide_unfinished_non_tool_items() {
        for (label, unfinished_item) in [
            ("reasoning", json!({"type":"reasoning","id":"reasoning_1"})),
            (
                "web_search_call",
                json!({"type":"web_search_call","id":"search_1"}),
            ),
        ] {
            let upstream = [
                sse(
                    "response.output_item.added",
                    json!({
                        "output_index":0,
                        "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                    }),
                ),
                sse(
                    "response.function_call_arguments.delta",
                    json!({
                        "output_index":0,
                        "delta":format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
                    }),
                ),
                sse(
                    "response.output_item.added",
                    json!({"output_index":1,"item":unfinished_item}),
                ),
            ]
            .concat();

            let error = reduce_upstream_bytes(upstream.as_bytes()).unwrap_err();
            assert!(
                error
                    .message
                    .contains("before terminal Codex response event"),
                "{label}: {}",
                error.message
            );
        }
    }

    #[test]
    fn reducer_read_repair_keeps_parallel_peer_and_finishes_once() {
        let upstream = [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"read_1","name":"Read"}
                }),
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"function_call","call_id":"bash_1","name":"Bash"}
                }),
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({"output_index":1,"delta":"{\"command\":\"echo peer\"}"}),
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,
                    "delta":format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
                }),
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "call_id":"read_1",
                        "name":"Read"
                    }
                }),
            ),
            sse(
                "response.function_call_arguments.done",
                json!({"output_index":1,"arguments":"{\"command\":\"echo peer\"}"}),
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":1,
                    "item":{
                        "type":"function_call",
                        "call_id":"bash_1",
                        "name":"Bash",
                        "arguments":"{\"command\":\"echo peer\"}"
                    }
                }),
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}}),
            ),
        ]
        .concat();

        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let stops: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ReducerEvent::ToolStop { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(stops, vec![0, 1]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ReducerEvent::Finish { .. }))
                .count(),
            1
        );
        let Some(ReducerEvent::Finish { output_items, .. }) = events.last() else {
            panic!("expected Finish");
        };
        assert_eq!(output_items.len(), 2);
    }

    #[test]
    fn web_search_query_preserves_all_queries() {
        assert_eq!(
            web_search_query(&json!({"action":{"queries":["alpha","beta","gamma"]}})),
            "alpha\nbeta\ngamma"
        );
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
                    "item":{"type":"web_search_call","id":"ws_1","action":{
                        "query":"test query",
                        "sources":[{"title":"Bound source","url":"https://bound.example"}]
                    }}
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
                    "output_index":1,"item":{"type":"message","id":"msg_up"}
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
        let search = out.iter().find_map(|event| match event {
            ReducerEvent::WebSearch { query, sources, .. } => Some((query, sources)),
            _ => None,
        });
        let (query, sources) = search.unwrap();
        assert_eq!(query, "test query");
        assert_eq!(
            sources,
            &vec![WebSearchResult {
                title: "Bound source".to_string(),
                url: "https://bound.example".to_string(),
            }]
        );
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
    fn buffered_web_search_ids_are_required_and_cross_tool_unique() {
        let missing = sse(
            "response.output_item.added",
            json!({"output_index":0,"item":{"type":"web_search_call"}}),
        );
        let error = reduce_upstream_bytes(missing.as_bytes()).unwrap_err();
        assert!(error.message.contains("web search item id"));

        let collision = [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                }),
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"function_call","call_id":"srvtoolu_ws_1","name":"Read"}
                }),
            ),
        ]
        .concat();
        let error = reduce_upstream_bytes(collision.as_bytes()).unwrap_err();
        assert!(error.message.contains("duplicate downstream tool use id"));
    }

    #[test]
    fn buffered_hosted_max_uses_rejects_the_next_call() {
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
        let policy = ToolCallPolicy::from_request(&request);
        let upstream = [
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                }),
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"web_search_call","id":"ws_2"}
                }),
            ),
        ]
        .concat();

        let error =
            reduce_upstream_bytes_with_tool_policy(upstream.as_bytes(), &policy).unwrap_err();
        assert!(error.message.contains("max_uses"));
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
                    "output_index":0,"item":{"type":"message","id":"msg_up"}
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
    fn reduce_content_filter_incomplete_is_refusal() {
        let upstream = sse(
            "response.incomplete",
            json!({"response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"content_filter"},"usage":{}}}),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let Some(ReducerEvent::Finish {
            stop_reason,
            continuation_eligible,
            ..
        }) = out.last()
        else {
            panic!("expected Finish");
        };
        assert_eq!(*stop_reason, "refusal");
        assert!(!continuation_eligible);
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
            "{}{}{}{}{}",
            sse(
                "codex.response.metadata",
                json!({
                    "headers": {
                        "openai-model": "gpt-5.6-sol",
                        "x-codex-safety-buffering-enabled": "true"
                    }
                })
            ),
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
                    "output_index":0,"item":{"type":"message","id":"msg_up"}
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
            "{}{}{}{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"enc"}})
            ),
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
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.output_item.added",
                json!({"output_index":1,"item":{"type":"message","id":"msg_up"}})
            ),
            sse(
                "response.output_text.delta",
                json!({"output_index":1,"delta":"answer"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":1,"item":{"type":"message","id":"msg_up"}})
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            )
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
            "{}{}{}{}{}{}",
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
            sse(
                "response.output_text.delta",
                json!({"output_index":1,"delta":"answer"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":1,"item":{"type":"message","id":"msg_up"}})
            ),
            sse(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            )
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
            "{}{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"enc"}})
            ),
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
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"enc"}})
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
            "{}{}{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"summary_index":0,"delta":"first"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.output_item.added",
                json!({"output_index":1,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc"}})
            ),
            sse(
                "response.reasoning_summary_text.delta",
                json!({"output_index":1,"summary_index":0,"delta":"second"})
            ),
            sse(
                "response.output_item.done",
                json!({"output_index":1,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc"}})
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
