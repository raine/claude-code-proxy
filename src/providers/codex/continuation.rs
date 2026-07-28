use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::timeutil::now_ms;

use super::translate::request::{ResponsesInputItem, ResponsesRequest};

const TTL_MS: u64 = 30 * 60 * 1000;
const MAX_STATES: usize = 10_000;
pub(super) const MAX_SESSION_TRANSCRIPT_BYTES: u64 = 2_000_000;
pub(super) const MAX_TOTAL_TRANSCRIPT_BYTES: u64 = 20_000_000;

#[derive(Clone)]
struct ContinuationState {
    response_id: String,
    prompt_signature: String,
    transcript: Vec<ResponsesInputItem>,
    transcript_bytes: u64,
    updated_at: u64,
}

struct SessionState {
    current_turn: u64,
    continuation: Option<ContinuationState>,
    updated_at: u64,
}

#[derive(Default)]
struct ContinuationRegistry {
    sessions: HashMap<String, SessionState>,
    total_transcript_bytes: u64,
}

static REGISTRY: Mutex<Option<ContinuationRegistry>> = Mutex::new(None);
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ContinuationCandidate {
    pub turn_id: Option<u64>,
    pub previous_response_id: Option<String>,
    pub input_delta: Option<Vec<ResponsesInputItem>>,
    pub input_delta_count: usize,
    pub disabled_reason: Option<String>,
}

pub fn continuation_candidate(
    session_id: Option<&str>,
    body: &ResponsesRequest,
    enabled: bool,
) -> ContinuationCandidate {
    if !enabled {
        return ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("disabled".to_string()),
        };
    }

    let Some(session_id) = session_id else {
        return ContinuationCandidate {
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("missing_session".to_string()),
        };
    };

    let turn_id = NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    let (state, superseded_turn) = {
        let mut guard = REGISTRY.lock().unwrap();
        let registry = guard.get_or_insert_with(ContinuationRegistry::default);
        let existing = registry.sessions.remove(session_id);
        let superseded_turn = existing.is_some();
        let state = existing.and_then(|session| session.continuation);
        if let Some(state) = &state {
            registry.total_transcript_bytes = registry
                .total_transcript_bytes
                .saturating_sub(state.transcript_bytes);
        }
        registry.sessions.insert(
            session_id.to_string(),
            SessionState {
                current_turn: turn_id,
                continuation: None,
                updated_at: now,
            },
        );
        evict_oldest(registry);
        (state, superseded_turn)
    };

    continuation_candidate_from_state(turn_id, body, state, superseded_turn, now)
}

fn continuation_candidate_from_state(
    turn_id: u64,
    body: &ResponsesRequest,
    state: Option<ContinuationState>,
    superseded_turn: bool,
    now: u64,
) -> ContinuationCandidate {
    let state = match state {
        Some(state) if now.saturating_sub(state.updated_at) <= TTL_MS => state,
        Some(_) | None => {
            return ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some(if superseded_turn {
                    "superseded_turn".to_string()
                } else {
                    "missing_state".to_string()
                }),
            };
        }
    };

    let signature = prompt_signature(body);
    if signature != state.prompt_signature {
        return ContinuationCandidate {
            turn_id: Some(turn_id),
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("prompt_changed".to_string()),
        };
    }

    let Some(suffix) = input_suffix_after_prefix(&body.input, &state.transcript) else {
        return ContinuationCandidate {
            turn_id: Some(turn_id),
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("not_append_only".to_string()),
        };
    };

    if suffix.is_empty() {
        return ContinuationCandidate {
            turn_id: Some(turn_id),
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 0,
            disabled_reason: Some("empty_delta".to_string()),
        };
    }

    ContinuationCandidate {
        turn_id: Some(turn_id),
        previous_response_id: Some(state.response_id),
        input_delta_count: suffix.len(),
        input_delta: Some(suffix),
        disabled_reason: None,
    }
}

pub fn record_continuation(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    request_body: &ResponsesRequest,
    response_id: Option<&str>,
    output_items: &[ResponsesInputItem],
) {
    let (session_id, turn_id) = match (session_id, turn_id) {
        (Some(session_id), Some(turn_id)) => (session_id, turn_id),
        _ => return,
    };

    let response_id = match response_id {
        Some(id) => id.to_string(),
        None => {
            abort_continuation(Some(session_id), Some(turn_id));
            return;
        }
    };

    let mut transcript: Vec<ResponsesInputItem> = request_body.input.clone();
    transcript.extend_from_slice(output_items);

    let transcript_json = serde_json::to_string(&transcript).unwrap_or_default();
    let transcript_bytes = transcript_json.len() as u64;

    if transcript_bytes > MAX_SESSION_TRANSCRIPT_BYTES {
        abort_continuation(Some(session_id), Some(turn_id));
        return;
    }

    let state = ContinuationState {
        response_id,
        prompt_signature: prompt_signature(request_body),
        transcript,
        transcript_bytes,
        updated_at: now_ms(),
    };

    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    let Some(session) = registry.sessions.get_mut(session_id) else {
        return;
    };
    if session.current_turn != turn_id {
        return;
    }
    if let Some(existing) = session.continuation.replace(state) {
        registry.total_transcript_bytes = registry
            .total_transcript_bytes
            .saturating_sub(existing.transcript_bytes);
    }
    registry.total_transcript_bytes += transcript_bytes;
    evict_oldest(registry);
}

pub fn abort_continuation(session_id: Option<&str>, turn_id: Option<u64>) {
    let (Some(session_id), Some(turn_id)) = (session_id, turn_id) else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if registry
        .sessions
        .get(session_id)
        .is_some_and(|session| session.current_turn == turn_id)
        && let Some(session) = registry.sessions.remove(session_id)
        && let Some(state) = session.continuation
    {
        registry.total_transcript_bytes = registry
            .total_transcript_bytes
            .saturating_sub(state.transcript_bytes);
    }
}

pub fn if_current_turn<T>(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let (Some(session_id), Some(turn_id)) = (session_id, turn_id) else {
        return Some(action());
    };
    let guard = REGISTRY.lock().unwrap();
    let current = guard
        .as_ref()
        .and_then(|registry| registry.sessions.get(session_id))
        .is_some_and(|session| session.current_turn == turn_id);
    current.then(action)
}

pub fn with_current_turn(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    action: impl FnOnce(),
) -> bool {
    if_current_turn(session_id, turn_id, action).is_some()
}

pub fn is_current_turn(session_id: Option<&str>, turn_id: Option<u64>) -> bool {
    let (Some(session_id), Some(turn_id)) = (session_id, turn_id) else {
        return false;
    };
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.sessions.get(session_id))
        .is_some_and(|session| session.current_turn == turn_id)
}

pub fn clear_continuation(session_id: Option<&str>) {
    let Some(session_id) = session_id else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if let Some(session) = registry.sessions.remove(session_id)
        && let Some(state) = session.continuation
    {
        registry.total_transcript_bytes = registry
            .total_transcript_bytes
            .saturating_sub(state.transcript_bytes);
    }
}

pub fn has_continuation_for_tests(session_id: &str) -> bool {
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.sessions.get(session_id))
        .is_some_and(|session| session.continuation.is_some())
}

pub fn clear_all_continuations_for_tests() {
    let mut guard = REGISTRY.lock().unwrap();
    *guard = None;
}

fn input_suffix_after_prefix(
    input: &[ResponsesInputItem],
    prefix: &[ResponsesInputItem],
) -> Option<Vec<ResponsesInputItem>> {
    if prefix.len() > input.len() {
        return None;
    }
    for i in 0..prefix.len() {
        let a = serde_json::to_value(&input[i]).unwrap_or_default();
        let b = serde_json::to_value(&prefix[i]).unwrap_or_default();
        if a != b {
            return None;
        }
    }
    Some(input[prefix.len()..].to_vec())
}

fn prompt_signature(body: &ResponsesRequest) -> String {
    let value = serde_json::to_value(body).unwrap_or_default();
    let obj = match value.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let mut entries: Vec<(&String, &serde_json::Value)> =
        obj.iter().filter(|(k, _)| *k != "input").collect();
    entries.sort_by_key(|(a, _)| *a);
    let mut sig = String::from("{");
    for (i, (key, val)) in entries.iter().enumerate() {
        if i > 0 {
            sig.push(',');
        }
        sig.push_str(&format!("\"{}\":{}", key, stable_json(val)));
    }
    sig.push('}');
    sig
}

fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(stable_json).collect();
            format!("[{}]", items.join(","))
        }
        serde_json::Value::Object(obj) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
            entries.sort_by_key(|(a, _)| *a);
            let items: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        stable_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

fn evict_oldest(registry: &mut ContinuationRegistry) {
    while registry.sessions.len() > MAX_STATES
        || registry.total_transcript_bytes > MAX_TOTAL_TRANSCRIPT_BYTES
    {
        let key = registry
            .sessions
            .iter()
            .min_by_key(|(_, session)| session.updated_at)
            .map(|(key, _)| key.clone());
        let Some(key) = key else {
            break;
        };
        if let Some(session) = registry.sessions.remove(&key)
            && let Some(state) = session.continuation
        {
            registry.total_transcript_bytes = registry
                .total_transcript_bytes
                .saturating_sub(state.transcript_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request_with_input(
        input: Vec<ResponsesInputItem>,
        extra: Option<serde_json::Value>,
    ) -> ResponsesRequest {
        let mut fields = serde_json::Map::new();
        fields.insert("model".into(), json!("gpt-5.5"));
        fields.insert("input".into(), json!(input));
        fields.insert("store".into(), json!(false));
        fields.insert("stream".into(), json!(true));
        fields.insert("text".into(), json!({"verbosity": "low"}));
        fields.insert("parallel_tool_calls".into(), json!(true));
        if let Some(extras) = extra
            && let Some(obj) = extras.as_object()
        {
            for (k, v) in obj {
                fields.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(serde_json::Value::Object(fields)).unwrap()
    }

    fn start_and_record(session_id: &str, request: &ResponsesRequest, response_id: Option<&str>) {
        let candidate = continuation_candidate(Some(session_id), request, true);
        record_continuation(
            Some(session_id),
            candidate.turn_id,
            request,
            response_id,
            &[],
        );
    }

    #[test]
    fn continuation_behaviors() {
        let _state_guard = super::super::CODEX_STATE_TEST_LOCK.blocking_lock();
        // All tests run in sequence to avoid global state interference

        // disabled_when_not_enabled
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input, None);
        let result = continuation_candidate(Some("s1"), &req, false);
        assert_eq!(result.disabled_reason, Some("disabled".to_string()));
        assert_eq!(result.input_delta_count, 1);

        // missing_session
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input, None);
        let result = continuation_candidate(None, &req, true);
        assert_eq!(result.disabled_reason, Some("missing_session".to_string()));

        // uses_previous_response_id_for_append_only
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input, None);
        start_and_record("s1", &req, Some("resp_1"));

        let input2 = vec![
            ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "one".to_string(),
                    },
                ],
            },
            ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "two".to_string(),
                    },
                ],
            },
        ];
        let req2 = request_with_input(input2, None);
        let result = continuation_candidate(Some("s1"), &req2, true);
        assert_eq!(result.previous_response_id, Some("resp_1".to_string()));
        assert_eq!(result.input_delta_count, 1);

        // clears_state_when_prompt_signature_changes
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input.clone(), None);
        start_and_record("s1", &req, Some("resp_1"));

        let req2 = request_with_input(input, Some(json!({"service_tier": "flex"})));
        let result = continuation_candidate(Some("s1"), &req2, true);
        assert_eq!(result.disabled_reason, Some("prompt_changed".to_string()));
        assert!(!has_continuation_for_tests("s1"));

        // clears_state_when_missing_response_id
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input.clone(), None);
        start_and_record("s1", &req, Some("resp_1"));
        assert!(has_continuation_for_tests("s1"));

        let candidate = continuation_candidate(Some("s1"), &req, true);
        record_continuation(Some("s1"), candidate.turn_id, &req, None, &[]);
        assert!(!has_continuation_for_tests("s1"));

        // stale turns cannot publish or clear a newer turn
        clear_all_continuations_for_tests();
        let first = continuation_candidate(Some("s1"), &req, true);
        record_continuation(Some("s1"), first.turn_id, &req, Some("resp_1"), &[]);
        let second = continuation_candidate(Some("s1"), &req, true);
        let third = continuation_candidate(Some("s1"), &req, true);
        assert_eq!(third.disabled_reason.as_deref(), Some("superseded_turn"));

        record_continuation(Some("s1"), second.turn_id, &req, Some("resp_2"), &[]);
        assert!(!has_continuation_for_tests("s1"));
        record_continuation(Some("s1"), third.turn_id, &req, Some("resp_3"), &[]);
        assert!(has_continuation_for_tests("s1"));
        abort_continuation(Some("s1"), second.turn_id);
        assert!(has_continuation_for_tests("s1"));
    }
}
