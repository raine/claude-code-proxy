use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::anthropic::sse::parse_sse_events;
use crate::provider::RequestContext;
use crate::providers::codex::client::{CodexError, CodexHttpClient};

use super::translate::request::{
    ResponsesContentPart, ResponsesInputItem, ResponsesRequest, is_compact_message_text,
};

const RETAINED_MESSAGE_TOKEN_BUDGET: u64 = 20_000;
const STATE_TTL_MS: u64 = 30 * 60 * 1_000;
const MAX_STATES: usize = 1_000;
const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_STATE_BYTES: usize = 20_000_000;
const MIN_PORTABLE_SUMMARY_BYTES: usize = 32;

#[derive(Debug)]
pub enum CompactionError {
    Upstream(CodexError),
    InvalidResponse(String),
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upstream(error) => write!(f, "{error}"),
            Self::InvalidResponse(message) => f.write_str(message),
        }
    }
}

enum CompactionPhase {
    Preparing,
    Unconfirmed,
    Anchored { portable_summary: String },
}

struct CompactionState {
    attempt: CompactionAttempt,
    model: String,
    native_history: Vec<ResponsesInputItem>,
    phase: CompactionPhase,
    updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionAttempt(u64);

pub struct CompactionReplay {
    pub request: ResponsesRequest,
    pub attempt: CompactionAttempt,
}

#[derive(Default)]
struct CompactionRegistry {
    states: HashMap<String, CompactionState>,
    total_bytes: usize,
}

static REGISTRY: Mutex<Option<CompactionRegistry>> = Mutex::new(None);
static NEXT_ATTEMPT_ID: AtomicU64 = AtomicU64::new(1);

pub async fn request_compaction(
    client: &CodexHttpClient,
    request: &ResponsesRequest,
    ctx: &RequestContext,
) -> Result<Vec<ResponsesInputItem>, CompactionError> {
    let (envelope, conversation) = split_input_envelope(&request.input);
    let conversation = without_compaction_instruction(conversation);
    let mut compaction_request = request.clone();
    compaction_request.instructions = None;
    compaction_request.input = envelope
        .iter()
        .filter(|item| matches!(item, ResponsesInputItem::AdditionalTools { .. }))
        .cloned()
        .chain(conversation.iter().cloned())
        .chain(std::iter::once(ResponsesInputItem::CompactionTrigger))
        .collect();
    compaction_request.include = Some(vec!["reasoning.encrypted_content".to_string()]);

    let response = client
        .post_codex_for_owner(&compaction_request, ctx, None)
        .await
        .map_err(CompactionError::Upstream)?;
    let compaction = parse_compaction_response(&response.body)?;
    Ok(build_compacted_history(&conversation, compaction))
}

pub fn begin_compaction(session_id: &str, model: &str) -> CompactionAttempt {
    let attempt = CompactionAttempt(NEXT_ATTEMPT_ID.fetch_add(1, Ordering::Relaxed));
    let state = CompactionState {
        attempt,
        model: model.to_string(),
        native_history: Vec::new(),
        phase: CompactionPhase::Preparing,
        updated_at: now_ms(),
    };
    let now = state.updated_at;
    let mut guard = REGISTRY.lock().unwrap();
    let registry = guard.get_or_insert_with(CompactionRegistry::default);
    evict_states(registry, now);
    registry.states.insert(session_id.to_string(), state);
    evict_states(registry, now);
    attempt
}

pub fn store_compaction(
    session_id: &str,
    attempt: CompactionAttempt,
    native_history: Vec<ResponsesInputItem>,
) -> bool {
    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return false;
    };
    evict_states(registry, now);
    let Some(state) = registry.states.get_mut(session_id) else {
        return false;
    };
    if state.attempt != attempt || !matches!(state.phase, CompactionPhase::Preparing) {
        return false;
    }
    state.native_history = native_history;
    state.phase = CompactionPhase::Unconfirmed;
    state.updated_at = now;
    if state_size(session_id, state) > MAX_STATE_BYTES {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return false;
    }
    evict_states(registry, now);
    registry
        .states
        .get(session_id)
        .is_some_and(|state| state.attempt == attempt)
}

pub fn activate_compaction(
    session_id: Option<&str>,
    attempt: Option<CompactionAttempt>,
    model: &str,
    output: &[ResponsesInputItem],
) -> bool {
    let (Some(session_id), Some(attempt)) = (session_id, attempt) else {
        return false;
    };
    let Some(portable_summary) = portable_summary_text(output) else {
        abort_compaction_attempt(Some(session_id), Some(attempt));
        return false;
    };

    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return false;
    };
    evict_states(registry, now);
    let Some(state) = registry.states.get_mut(session_id) else {
        return false;
    };
    if state.attempt != attempt {
        return false;
    }
    if state.model != model || !matches!(state.phase, CompactionPhase::Unconfirmed) {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return false;
    }
    state.phase = CompactionPhase::Anchored { portable_summary };
    state.updated_at = now;
    if state_size(session_id, state) > MAX_STATE_BYTES {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return false;
    }
    evict_states(registry, now);
    registry.states.contains_key(session_id)
}

pub fn apply_compaction_replay(
    session_id: Option<&str>,
    request: &ResponsesRequest,
) -> Option<CompactionReplay> {
    let session_id = session_id?;
    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let registry = guard.as_mut()?;
    evict_states(registry, now);
    let state = registry.states.get_mut(session_id)?;
    if !matches!(state.phase, CompactionPhase::Anchored { .. }) {
        return None;
    }
    if state.model != request.model {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return None;
    }
    let CompactionPhase::Anchored { portable_summary } = &state.phase else {
        return None;
    };

    let (envelope, conversation) = split_input_envelope(&request.input);
    let summary_item = conversation.first()?;
    let Some(text) = message_text(summary_item) else {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return None;
    };
    if text.match_indices(portable_summary).count() != 1 {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return None;
    }
    if conversation.len() == 1 {
        return None;
    }

    let mut replay = request.clone();
    replay.input = envelope
        .iter()
        .cloned()
        .chain(state.native_history.iter().cloned())
        .chain(conversation[1..].iter().cloned())
        .collect();
    if serialized_size(&replay.input) > MAX_STATE_BYTES {
        registry.states.remove(session_id);
        update_total_bytes(registry);
        return None;
    }
    state.updated_at = now;
    Some(CompactionReplay {
        request: replay,
        attempt: state.attempt,
    })
}

pub fn abort_compaction_attempt(session_id: Option<&str>, attempt: Option<CompactionAttempt>) {
    let (Some(session_id), Some(attempt)) = (session_id, attempt) else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if registry
        .states
        .get(session_id)
        .is_some_and(|state| state.attempt == attempt)
    {
        registry.states.remove(session_id);
        update_total_bytes(registry);
    }
}

pub fn clear_compaction(session_id: &str) {
    let mut guard = REGISTRY.lock().unwrap();
    if let Some(registry) = guard.as_mut() {
        registry.states.remove(session_id);
        update_total_bytes(registry);
    }
}

fn split_input_envelope(
    input: &[ResponsesInputItem],
) -> (&[ResponsesInputItem], &[ResponsesInputItem]) {
    let prefix_len = input
        .iter()
        .take_while(|item| is_envelope_item(item))
        .count();
    input.split_at(prefix_len)
}

fn without_compaction_instruction(input: &[ResponsesInputItem]) -> Vec<ResponsesInputItem> {
    let mut input = input.to_vec();
    let remove_empty_message = if let Some(ResponsesInputItem::Message { role, content }) =
        input.last_mut()
        && role == "user"
    {
        content.retain(|part| {
            !matches!(
                part,
                ResponsesContentPart::InputText { text }
                    if is_compact_message_text(text)
            )
        });
        content.is_empty()
    } else {
        false
    };
    if remove_empty_message {
        input.pop();
    }
    input
}

fn is_envelope_item(item: &ResponsesInputItem) -> bool {
    match item {
        ResponsesInputItem::AdditionalTools { .. } => true,
        ResponsesInputItem::Message { role, .. } => role == "developer",
        _ => false,
    }
}

fn portable_summary_text(output: &[ResponsesInputItem]) -> Option<String> {
    let text = output
        .iter()
        .filter_map(|item| match item {
            ResponsesInputItem::Message { role, content } if role == "assistant" => Some(content),
            _ => None,
        })
        .flat_map(|content| content.iter())
        .filter_map(|part| match part {
            ResponsesContentPart::InputText { text }
            | ResponsesContentPart::OutputText { text } => Some(text.as_str()),
            ResponsesContentPart::InputImage { .. } => None,
        })
        .collect::<String>();
    let trimmed = text.trim();
    let summary = trimmed
        .split_once("<summary>")
        .and_then(|(_, rest)| rest.split_once("</summary>"))
        .map(|(summary, _)| summary.trim())
        .filter(|summary| !summary.is_empty())
        .unwrap_or(trimmed);
    (summary.len() >= MIN_PORTABLE_SUMMARY_BYTES).then(|| summary.to_string())
}

fn message_text(item: &ResponsesInputItem) -> Option<String> {
    let ResponsesInputItem::Message { content, .. } = item else {
        return None;
    };
    Some(
        content
            .iter()
            .filter_map(|part| match part {
                ResponsesContentPart::InputText { text }
                | ResponsesContentPart::OutputText { text } => Some(text.as_str()),
                ResponsesContentPart::InputImage { .. } => None,
            })
            .collect(),
    )
}

fn parse_compaction_response(body: &[u8]) -> Result<ResponsesInputItem, CompactionError> {
    let mut completed = false;
    let mut compacted = Vec::new();

    for event in parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("error" | "response.error" | "response.failed") => {
                let message = payload
                    .pointer("/response/error/message")
                    .or_else(|| payload.pointer("/error/message"))
                    .or_else(|| payload.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("remote compaction failed");
                return Err(CompactionError::InvalidResponse(message.to_string()));
            }
            Some("response.output_item.done") => {
                let Some(item) = payload.get("item") else {
                    continue;
                };
                if item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
                    && let Some(encrypted_content) = item
                        .get("encrypted_content")
                        .and_then(serde_json::Value::as_str)
                {
                    compacted.push(ResponsesInputItem::Compaction {
                        encrypted_content: encrypted_content.to_string(),
                    });
                }
            }
            Some("response.completed") => completed = true,
            _ => {}
        }
    }

    if !completed {
        return Err(CompactionError::InvalidResponse(
            "remote compaction stream ended before response.completed".to_string(),
        ));
    }
    if compacted.len() != 1 {
        return Err(CompactionError::InvalidResponse(format!(
            "remote compaction expected exactly one compaction item, got {}",
            compacted.len()
        )));
    }
    Ok(compacted.pop().expect("validated one compaction item"))
}

fn build_compacted_history(
    input: &[ResponsesInputItem],
    compaction: ResponsesInputItem,
) -> Vec<ResponsesInputItem> {
    let retained = input
        .iter()
        .filter(|item| {
            matches!(
                item,
                ResponsesInputItem::Message { role, .. }
                    if matches!(role.as_str(), "user" | "developer" | "system")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut retained = truncate_retained_messages(retained, RETAINED_MESSAGE_TOKEN_BUDGET);
    retained.push(compaction);
    retained
}

fn truncate_retained_messages(
    items: Vec<ResponsesInputItem>,
    max_tokens: u64,
) -> Vec<ResponsesInputItem> {
    let mut remaining = max_tokens;
    let mut retained = Vec::new();
    for item in items.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = message_tokens(&item).max(1);
        if tokens <= remaining {
            retained.push(item);
            remaining -= tokens;
        } else if let Some(item) = truncate_message(item, remaining) {
            retained.push(item);
            remaining = 0;
        }
    }
    retained.reverse();
    retained
}

fn message_tokens(item: &ResponsesInputItem) -> u64 {
    let ResponsesInputItem::Message { content, .. } = item else {
        return 0;
    };
    content
        .iter()
        .map(|part| match part {
            ResponsesContentPart::InputText { text }
            | ResponsesContentPart::OutputText { text } => text.len().div_ceil(4) as u64,
            ResponsesContentPart::InputImage { .. } => 2_000,
        })
        .sum()
}

fn truncate_message(item: ResponsesInputItem, max_tokens: u64) -> Option<ResponsesInputItem> {
    let ResponsesInputItem::Message { role, content } = item else {
        return Some(item);
    };
    let mut remaining_chars = max_tokens.saturating_mul(4) as usize;
    let mut truncated = Vec::new();
    for part in content {
        match part {
            ResponsesContentPart::InputImage { .. } => truncated.push(part),
            ResponsesContentPart::InputText { text } => {
                let text = truncate_text(text, &mut remaining_chars);
                if !text.is_empty() {
                    truncated.push(ResponsesContentPart::InputText { text });
                }
            }
            ResponsesContentPart::OutputText { text } => {
                let text = truncate_text(text, &mut remaining_chars);
                if !text.is_empty() {
                    truncated.push(ResponsesContentPart::OutputText { text });
                }
            }
        }
    }
    (!truncated.is_empty()).then_some(ResponsesInputItem::Message {
        role,
        content: truncated,
    })
}

fn truncate_text(mut text: String, remaining_chars: &mut usize) -> String {
    if *remaining_chars == 0 {
        return String::new();
    }
    if text.len() > *remaining_chars {
        let mut boundary = *remaining_chars;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    *remaining_chars -= text.len();
    text
}

fn serialized_size(items: &[ResponsesInputItem]) -> usize {
    serde_json::to_vec(items).map_or(usize::MAX, |value| value.len())
}

fn state_size(session_id: &str, state: &CompactionState) -> usize {
    let summary_len = match &state.phase {
        CompactionPhase::Preparing | CompactionPhase::Unconfirmed => 0,
        CompactionPhase::Anchored { portable_summary } => portable_summary.len(),
    };
    session_id.len() + state.model.len() + summary_len + serialized_size(&state.native_history)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn update_total_bytes(registry: &mut CompactionRegistry) {
    registry.total_bytes = registry
        .states
        .iter()
        .map(|(session_id, state)| state_size(session_id, state))
        .sum();
}

fn evict_states(registry: &mut CompactionRegistry, now: u64) {
    registry
        .states
        .retain(|_, state| now.saturating_sub(state.updated_at) <= STATE_TTL_MS);
    update_total_bytes(registry);
    while registry.states.len() > MAX_STATES || registry.total_bytes > MAX_TOTAL_STATE_BYTES {
        let oldest = registry
            .states
            .iter()
            .min_by_key(|(_, state)| state.updated_at)
            .map(|(session_id, _)| session_id.clone());
        let Some(oldest) = oldest else {
            break;
        };
        registry.states.remove(&oldest);
        update_total_bytes(registry);
    }
}

pub fn clear_all_compactions_for_tests() {
    *REGISTRY.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUMMARY: &str =
        "portable summary with enough detail to identify this compacted conversation";
    const STALE_SUMMARY: &str =
        "stale portable summary from an older overlapping compaction attempt";
    static TEST_REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    fn request(input: serde_json::Value) -> ResponsesRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": input,
            "store": false,
            "stream": true,
            "parallel_tool_calls": false,
            "client_metadata": {"lite":"true"},
            "text": {"verbosity":"low"}
        }))
        .unwrap()
    }

    fn output(text: &str) -> Vec<ResponsesInputItem> {
        serde_json::from_value(json!([{
            "type":"message",
            "role":"assistant",
            "content":[{"type":"output_text","text":text}]
        }]))
        .unwrap()
    }

    fn stored_compaction(
        session_id: &str,
        native_history: Vec<ResponsesInputItem>,
    ) -> CompactionAttempt {
        let attempt = begin_compaction(session_id, "gpt-5.6-sol");
        assert!(store_compaction(session_id, attempt, native_history));
        attempt
    }

    #[test]
    fn parses_exactly_one_completed_compaction_item() {
        let body = b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        assert!(matches!(
            parse_compaction_response(body).unwrap(),
            ResponsesInputItem::Compaction { encrypted_content } if encrypted_content == "opaque"
        ));
    }

    #[test]
    fn rejects_incomplete_or_ambiguous_compaction_streams() {
        let incomplete = b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n";
        assert!(
            parse_compaction_response(incomplete)
                .unwrap_err()
                .to_string()
                .contains("before response.completed")
        );
        let missing = b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        assert!(
            parse_compaction_response(missing)
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn replay_requires_activation_and_wrapped_summary_anchor() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let attempt = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "opaque".to_string(),
            }],
        );
        let next = request(json!([
            {"type":"additional_tools","role":"developer","tools":[]},
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"instructions"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":format!("<summary>{SUMMARY}</summary>")}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
        ]));
        assert!(apply_compaction_replay(Some("session"), &next).is_none());
        assert!(activate_compaction(
            Some("session"),
            Some(attempt),
            "gpt-5.6-sol",
            &output(&format!(
                "<analysis>summary preparation</analysis>\n<summary>\n{SUMMARY}\n</summary>"
            ))
        ));

        let replay = apply_compaction_replay(Some("session"), &next)
            .unwrap()
            .request;
        assert!(matches!(
            replay.input[0],
            ResponsesInputItem::AdditionalTools { .. }
        ));
        assert!(
            matches!(replay.input[1], ResponsesInputItem::Message { ref role, .. } if role == "developer")
        );
        assert!(matches!(
            replay.input[2],
            ResponsesInputItem::Compaction { .. }
        ));
        assert_eq!(replay.client_metadata, next.client_metadata);
    }

    #[test]
    fn stale_activation_cannot_anchor_newer_native_history() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let older = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "older-native-history".to_string(),
            }],
        );
        let newer = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "newer-native-history".to_string(),
            }],
        );

        assert!(!activate_compaction(
            Some("session"),
            Some(older),
            "gpt-5.6-sol",
            &output(STALE_SUMMARY),
        ));
        assert!(activate_compaction(
            Some("session"),
            Some(newer),
            "gpt-5.6-sol",
            &output(SUMMARY),
        ));
    }

    #[test]
    fn stale_store_cannot_replace_newer_compaction() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let older = begin_compaction("session", "gpt-5.6-sol");
        let newer = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "newer-native-history".to_string(),
            }],
        );

        assert!(!store_compaction(
            "session",
            older,
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "late-older-native-history".to_string(),
            }],
        ));
        assert!(activate_compaction(
            Some("session"),
            Some(newer),
            "gpt-5.6-sol",
            &output(SUMMARY),
        ));
        let next = request(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":SUMMARY}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
        ]));
        let replay = apply_compaction_replay(Some("session"), &next)
            .unwrap()
            .request;
        assert!(replay.input.iter().any(|item| matches!(
            item,
            ResponsesInputItem::Compaction { encrypted_content }
                if encrypted_content == "newer-native-history"
        )));
    }

    #[test]
    fn preparing_compaction_survives_model_mismatched_replay_check() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let attempt = begin_compaction("session", "gpt-5.6-sol");
        let mut mismatched = request(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
        ]));
        mismatched.model = "gpt-5.6-terra".to_string();

        assert!(apply_compaction_replay(Some("session"), &mismatched).is_none());
        assert!(store_compaction(
            "session",
            attempt,
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "native-history".to_string(),
            }],
        ));
    }

    #[test]
    fn stale_abort_cannot_clear_newer_compaction() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let older = begin_compaction("session", "gpt-5.6-sol");
        let newer = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "newer-native-history".to_string(),
            }],
        );

        abort_compaction_attempt(Some("session"), Some(older));

        assert!(activate_compaction(
            Some("session"),
            Some(newer),
            "gpt-5.6-sol",
            &output(SUMMARY),
        ));
    }

    #[test]
    fn stale_replay_abort_cannot_clear_newer_compaction() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let older = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "older-native-history".to_string(),
            }],
        );
        assert!(activate_compaction(
            Some("session"),
            Some(older),
            "gpt-5.6-sol",
            &output(STALE_SUMMARY),
        ));
        let older_next = request(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":STALE_SUMMARY}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue old"}]}
        ]));
        let older_replay = apply_compaction_replay(Some("session"), &older_next).unwrap();

        let newer = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "newer-native-history".to_string(),
            }],
        );
        assert!(activate_compaction(
            Some("session"),
            Some(newer),
            "gpt-5.6-sol",
            &output(SUMMARY),
        ));

        abort_compaction_attempt(Some("session"), Some(older_replay.attempt));

        let newer_next = request(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":SUMMARY}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue new"}]}
        ]));
        let replay = apply_compaction_replay(Some("session"), &newer_next)
            .unwrap()
            .request;
        assert!(replay.input.iter().any(|item| matches!(
            item,
            ResponsesInputItem::Compaction { encrypted_content }
                if encrypted_content == "newer-native-history"
        )));
    }

    #[test]
    fn invalid_stale_summary_cannot_clear_newer_compaction() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let older = begin_compaction("session", "gpt-5.6-sol");
        let newer = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "newer-native-history".to_string(),
            }],
        );

        assert!(!activate_compaction(
            Some("session"),
            Some(older),
            "gpt-5.6-sol",
            &output("too short"),
        ));
        assert!(activate_compaction(
            Some("session"),
            Some(newer),
            "gpt-5.6-sol",
            &output(SUMMARY),
        ));
    }

    #[test]
    fn replay_clears_on_missing_or_duplicate_anchor() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        for text in [
            "different conversation without the expected summary".to_string(),
            format!("{SUMMARY} and {SUMMARY}"),
        ] {
            clear_all_compactions_for_tests();
            let attempt = stored_compaction(
                "session",
                vec![ResponsesInputItem::Compaction {
                    encrypted_content: "opaque".to_string(),
                }],
            );
            activate_compaction(
                Some("session"),
                Some(attempt),
                "gpt-5.6-sol",
                &output(SUMMARY),
            );
            let changed = request(json!([
                {"type":"message","role":"user","content":[{"type":"input_text","text":text}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
            ]));
            assert!(apply_compaction_replay(Some("session"), &changed).is_none());
            assert!(apply_compaction_replay(Some("session"), &changed).is_none());
        }
    }

    #[test]
    fn compacted_history_excludes_lite_envelope() {
        let input: Vec<ResponsesInputItem> = serde_json::from_value(json!([
            {"type":"additional_tools","role":"developer","tools":[]},
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"summarize"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"remember me"}]}
        ])).unwrap();
        let (_, conversation) = split_input_envelope(&input);
        let history = build_compacted_history(
            conversation,
            ResponsesInputItem::Compaction {
                encrypted_content: "opaque".to_string(),
            },
        );
        assert_eq!(history.len(), 2);
        assert!(
            matches!(history[0], ResponsesInputItem::Message { ref role, .. } if role == "user")
        );
    }

    #[test]
    fn failed_replay_clears_anchored_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let attempt = stored_compaction(
            "failed-replay",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "opaque".to_string(),
            }],
        );
        activate_compaction(
            Some("failed-replay"),
            Some(attempt),
            "gpt-5.6-sol",
            &output(SUMMARY),
        );
        let next = request(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":SUMMARY}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
        ]));
        let replay = apply_compaction_replay(Some("failed-replay"), &next).unwrap();

        abort_compaction_attempt(Some("failed-replay"), Some(replay.attempt));

        assert!(apply_compaction_replay(Some("failed-replay"), &next).is_none());
    }

    #[test]
    fn replay_clears_on_model_change() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_compactions_for_tests();
        let attempt = stored_compaction(
            "session",
            vec![ResponsesInputItem::Compaction {
                encrypted_content: "opaque".to_string(),
            }],
        );
        activate_compaction(
            Some("session"),
            Some(attempt),
            "gpt-5.6-sol",
            &output(SUMMARY),
        );
        let mut changed = request(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":SUMMARY}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
        ]));
        changed.model = "gpt-5.4".to_string();
        assert!(apply_compaction_replay(Some("session"), &changed).is_none());
    }

    #[test]
    fn retained_history_obeys_token_budget_at_utf8_boundary() {
        let text = "é".repeat((RETAINED_MESSAGE_TOKEN_BUDGET as usize + 10) * 4);
        let input: Vec<ResponsesInputItem> = serde_json::from_value(json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":text}]}
        ]))
        .unwrap();
        let history = build_compacted_history(
            &input,
            ResponsesInputItem::Compaction {
                encrypted_content: "opaque".to_string(),
            },
        );
        let ResponsesInputItem::Message { content, .. } = &history[0] else {
            panic!("expected retained message");
        };
        let ResponsesContentPart::InputText { text } = &content[0] else {
            panic!("expected retained text");
        };
        assert!(text.len() <= RETAINED_MESSAGE_TOKEN_BUDGET as usize * 4);
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }
}
