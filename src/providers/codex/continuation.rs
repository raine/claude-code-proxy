use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::request_identity::ConversationIdentity;

use super::translate::request::{ResponsesInputItem, ResponsesRequest};

const TTL_MS: u64 = 30 * 60 * 1000;
const MAX_STATES: usize = 10_000;
const MAX_OWNER_TRANSCRIPT_BYTES: u64 = 2_000_000;
const MAX_TOTAL_TRANSCRIPT_BYTES: u64 = 20_000_000;

#[derive(Clone)]
struct ContinuationState {
    response_id: String,
    prompt_signature: String,
    transcript: Vec<ResponsesInputItem>,
    transcript_bytes: u64,
    updated_at: u64,
}

struct OwnerState {
    current_turn: u64,
    continuation: Option<ContinuationState>,
    updated_at: u64,
}

#[derive(Default)]
struct ContinuationRegistry {
    owners: HashMap<ConversationIdentity, OwnerState>,
    total_transcript_bytes: u64,
}

static REGISTRY: Mutex<Option<ContinuationRegistry>> = Mutex::new(None);
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ContinuationCandidate {
    pub owner: Option<ConversationIdentity>,
    pub turn_id: Option<u64>,
    pub previous_response_id: Option<String>,
    pub input_delta: Option<Vec<ResponsesInputItem>>,
    pub input_delta_count: usize,
    pub disabled_reason: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn continuation_candidate(
    owner: Option<&ConversationIdentity>,
    body: &ResponsesRequest,
    enabled: bool,
) -> ContinuationCandidate {
    if !enabled {
        return ContinuationCandidate {
            owner: owner.cloned(),
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("disabled".to_string()),
        };
    }

    let Some(owner) = owner else {
        return ContinuationCandidate {
            owner: None,
            turn_id: None,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("missing_identity".to_string()),
        };
    };

    let turn_id = NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    let (state, superseded_turn) = {
        let mut guard = REGISTRY.lock().unwrap();
        let registry = guard.get_or_insert_with(ContinuationRegistry::default);
        let existing = registry.owners.remove(owner);
        let superseded_turn = existing.is_some();
        let state = existing.and_then(|owner| owner.continuation);
        if let Some(state) = &state {
            registry.total_transcript_bytes = registry
                .total_transcript_bytes
                .saturating_sub(state.transcript_bytes);
        }
        registry.owners.insert(
            owner.clone(),
            OwnerState {
                current_turn: turn_id,
                continuation: None,
                updated_at: now,
            },
        );
        evict_oldest(registry);
        (state, superseded_turn)
    };

    continuation_candidate_from_state(owner, turn_id, body, state, superseded_turn, now)
}

fn continuation_candidate_from_state(
    owner: &ConversationIdentity,
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
                owner: Some(owner.clone()),
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
            owner: Some(owner.clone()),
            turn_id: Some(turn_id),
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("prompt_changed".to_string()),
        };
    }

    let Some(suffix) = input_suffix_after_prefix(&body.input, &state.transcript) else {
        return ContinuationCandidate {
            owner: Some(owner.clone()),
            turn_id: Some(turn_id),
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("not_append_only".to_string()),
        };
    };

    if suffix.is_empty() {
        return ContinuationCandidate {
            owner: Some(owner.clone()),
            turn_id: Some(turn_id),
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 0,
            disabled_reason: Some("empty_delta".to_string()),
        };
    }

    ContinuationCandidate {
        owner: Some(owner.clone()),
        turn_id: Some(turn_id),
        previous_response_id: Some(state.response_id),
        input_delta_count: suffix.len(),
        input_delta: Some(suffix),
        disabled_reason: None,
    }
}

pub fn record_continuation(
    owner: Option<&ConversationIdentity>,
    turn_id: Option<u64>,
    request_body: &ResponsesRequest,
    response_id: Option<&str>,
    output_items: &[ResponsesInputItem],
) {
    let (owner, turn_id) = match (owner, turn_id) {
        (Some(owner), Some(turn_id)) => (owner, turn_id),
        _ => return,
    };

    let response_id = match response_id {
        Some(id) => id.to_string(),
        None => {
            abort_continuation(Some(owner), Some(turn_id));
            return;
        }
    };

    let mut transcript: Vec<ResponsesInputItem> = request_body.input.clone();
    transcript.extend_from_slice(output_items);

    let transcript_json = serde_json::to_string(&transcript).unwrap_or_default();
    let transcript_bytes = transcript_json.len() as u64;

    if transcript_bytes > MAX_OWNER_TRANSCRIPT_BYTES {
        abort_continuation(Some(owner), Some(turn_id));
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
    let Some(owner_state) = registry.owners.get_mut(owner) else {
        return;
    };
    if owner_state.current_turn != turn_id {
        return;
    }
    if let Some(existing) = owner_state.continuation.replace(state) {
        registry.total_transcript_bytes = registry
            .total_transcript_bytes
            .saturating_sub(existing.transcript_bytes);
    }
    registry.total_transcript_bytes += transcript_bytes;
    evict_oldest(registry);
}

pub fn abort_continuation(owner: Option<&ConversationIdentity>, turn_id: Option<u64>) {
    let (Some(owner), Some(turn_id)) = (owner, turn_id) else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if registry
        .owners
        .get(owner)
        .is_some_and(|state| state.current_turn == turn_id)
        && let Some(state) = registry.owners.remove(owner)
        && let Some(continuation) = state.continuation
    {
        registry.total_transcript_bytes = registry
            .total_transcript_bytes
            .saturating_sub(continuation.transcript_bytes);
    }
}

pub fn if_current_turn<T>(
    owner: Option<&ConversationIdentity>,
    turn_id: Option<u64>,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let (Some(owner), Some(turn_id)) = (owner, turn_id) else {
        return None;
    };
    let guard = REGISTRY.lock().unwrap();
    let current = guard
        .as_ref()
        .and_then(|registry| registry.owners.get(owner))
        .is_some_and(|state| state.current_turn == turn_id);
    current.then(action)
}

pub fn with_current_turn(
    owner: Option<&ConversationIdentity>,
    turn_id: Option<u64>,
    action: impl FnOnce(),
) -> bool {
    if_current_turn(owner, turn_id, action).is_some()
}

pub fn is_current_turn(owner: Option<&ConversationIdentity>, turn_id: Option<u64>) -> bool {
    let (Some(owner), Some(turn_id)) = (owner, turn_id) else {
        return false;
    };
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.owners.get(owner))
        .is_some_and(|state| state.current_turn == turn_id)
}

pub fn clear_continuation(owner: Option<&ConversationIdentity>) {
    let Some(owner) = owner else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if let Some(state) = registry.owners.remove(owner)
        && let Some(continuation) = state.continuation
    {
        registry.total_transcript_bytes = registry
            .total_transcript_bytes
            .saturating_sub(continuation.transcript_bytes);
    }
}

pub fn has_continuation_for_tests(owner: &ConversationIdentity) -> bool {
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.owners.get(owner))
        .is_some_and(|state| state.continuation.is_some())
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
    while registry.owners.len() > MAX_STATES
        || registry.total_transcript_bytes > MAX_TOTAL_TRANSCRIPT_BYTES
    {
        let owner = registry
            .owners
            .iter()
            .min_by_key(|(_, state)| state.updated_at)
            .map(|(owner, _)| owner.clone());
        let Some(owner) = owner else {
            break;
        };
        if let Some(state) = registry.owners.remove(&owner)
            && let Some(continuation) = state.continuation
        {
            registry.total_transcript_bytes = registry
                .total_transcript_bytes
                .saturating_sub(continuation.transcript_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static TEST_REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    fn lock_registry() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_continuations_for_tests();
        guard
    }

    fn main_owner(session_id: &str) -> ConversationIdentity {
        ConversationIdentity::Main(session_id.to_string())
    }

    fn agent_owner(session_id: &str, agent_id: &str) -> ConversationIdentity {
        ConversationIdentity::Agent(session_id.to_string(), agent_id.to_string())
    }

    fn input(text: &str) -> ResponsesInputItem {
        ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: text.to_string(),
                },
            ],
        }
    }

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
            for (key, value) in obj {
                fields.insert(key.clone(), value.clone());
            }
        }
        serde_json::from_value(serde_json::Value::Object(fields)).unwrap()
    }

    fn start_and_record(
        owner: &ConversationIdentity,
        request: &ResponsesRequest,
        response_id: &str,
    ) {
        let candidate = continuation_candidate(Some(owner), request, true);
        record_continuation(
            candidate.owner.as_ref(),
            candidate.turn_id,
            request,
            Some(response_id),
            &[],
        );
    }

    #[test]
    fn disabled_and_missing_identity_requests_are_stateless() {
        let _registry_guard = lock_registry();
        let request = request_with_input(vec![input("one")], None);
        let owner = main_owner("session-a");

        let disabled = continuation_candidate(Some(&owner), &request, false);
        assert_eq!(disabled.owner.as_ref(), Some(&owner));
        assert_eq!(disabled.turn_id, None);
        assert_eq!(disabled.input_delta_count, request.input.len());
        assert_eq!(disabled.disabled_reason.as_deref(), Some("disabled"));

        let missing = continuation_candidate(None, &request, true);
        assert_eq!(missing.owner, None);
        assert_eq!(missing.turn_id, None);
        assert_eq!(missing.input_delta_count, request.input.len());
        assert_eq!(missing.disabled_reason.as_deref(), Some("missing_identity"));
    }

    #[test]
    fn sibling_agents_reserve_and_publish_independently() {
        let _registry_guard = lock_registry();
        let sibling_one = agent_owner("session-a", "agent-one");
        let sibling_two = agent_owner("session-a", "agent-two");
        let first_request = request_with_input(vec![input("one")], None);

        let first = continuation_candidate(Some(&sibling_one), &first_request, true);
        let second = continuation_candidate(Some(&sibling_two), &first_request, true);
        assert_ne!(first.turn_id, second.turn_id);
        record_continuation(
            first.owner.as_ref(),
            first.turn_id,
            &first_request,
            Some("resp_one"),
            &[],
        );
        record_continuation(
            second.owner.as_ref(),
            second.turn_id,
            &first_request,
            Some("resp_two"),
            &[],
        );
        assert!(has_continuation_for_tests(&sibling_one));
        assert!(has_continuation_for_tests(&sibling_two));

        let next_request = request_with_input(vec![input("one"), input("two")], None);
        let first_next = continuation_candidate(Some(&sibling_one), &next_request, true);
        let second_next = continuation_candidate(Some(&sibling_two), &next_request, true);
        assert_eq!(first_next.previous_response_id.as_deref(), Some("resp_one"));
        assert_eq!(
            second_next.previous_response_id.as_deref(),
            Some("resp_two")
        );
    }

    #[test]
    fn different_owner_completion_order_cannot_interfere() {
        let _registry_guard = lock_registry();
        let main = main_owner("session-a");
        let agent = agent_owner("session-a", "agent-a");
        let request = request_with_input(vec![input("one")], None);
        let main_candidate = continuation_candidate(Some(&main), &request, true);
        let agent_candidate = continuation_candidate(Some(&agent), &request, true);

        record_continuation(
            agent_candidate.owner.as_ref(),
            agent_candidate.turn_id,
            &request,
            Some("resp_agent"),
            &[],
        );
        record_continuation(
            main_candidate.owner.as_ref(),
            main_candidate.turn_id,
            &request,
            Some("resp_main"),
            &[],
        );
        abort_continuation(Some(&main), agent_candidate.turn_id);

        assert!(has_continuation_for_tests(&main));
        assert!(has_continuation_for_tests(&agent));
        let next = request_with_input(vec![input("one"), input("two")], None);
        assert_eq!(
            continuation_candidate(Some(&agent), &next, true)
                .previous_response_id
                .as_deref(),
            Some("resp_agent")
        );
    }

    #[test]
    fn missing_response_id_aborts_only_the_current_owner() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let sibling = agent_owner("session-a", "agent-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_main");
        start_and_record(&sibling, &request, "resp_agent");

        let candidate = continuation_candidate(Some(&owner), &request, true);
        record_continuation(
            candidate.owner.as_ref(),
            candidate.turn_id,
            &request,
            None,
            &[],
        );

        assert!(!has_continuation_for_tests(&owner));
        assert!(has_continuation_for_tests(&sibling));
    }

    #[test]
    fn same_owner_stale_turn_cannot_publish_clear_or_run_actions() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_1");

        let stale = continuation_candidate(Some(&owner), &request, true);
        let current = continuation_candidate(Some(&owner), &request, true);
        assert_eq!(current.disabled_reason.as_deref(), Some("superseded_turn"));
        record_continuation(
            stale.owner.as_ref(),
            stale.turn_id,
            &request,
            Some("resp_stale"),
            &[],
        );
        assert!(!has_continuation_for_tests(&owner));
        record_continuation(
            current.owner.as_ref(),
            current.turn_id,
            &request,
            Some("resp_current"),
            &[],
        );
        assert!(has_continuation_for_tests(&owner));
        abort_continuation(stale.owner.as_ref(), stale.turn_id);
        assert!(has_continuation_for_tests(&owner));

        let mut ran = false;
        assert_eq!(
            if_current_turn(stale.owner.as_ref(), stale.turn_id, || ran = true),
            None
        );
        assert!(!ran);
    }

    #[test]
    fn missing_owner_or_turn_mutations_are_hard_noops() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_1");

        record_continuation(None, Some(1), &request, Some("ignored"), &[]);
        record_continuation(Some(&owner), None, &request, Some("ignored"), &[]);
        abort_continuation(None, Some(1));
        abort_continuation(Some(&owner), None);
        clear_continuation(None);
        assert!(has_continuation_for_tests(&owner));

        let mut runs = 0;
        assert_eq!(if_current_turn(None, Some(1), || runs += 1), None);
        assert_eq!(if_current_turn(Some(&owner), None, || runs += 1), None);
        assert!(!with_current_turn(None, Some(1), || runs += 1));
        assert!(!with_current_turn(Some(&owner), None, || runs += 1));
        assert_eq!(runs, 0);
    }

    #[test]
    fn append_only_and_prompt_guards_stay_owner_scoped() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_1");

        let appended = request_with_input(vec![input("one"), input("two")], None);
        let candidate = continuation_candidate(Some(&owner), &appended, true);
        assert_eq!(candidate.previous_response_id.as_deref(), Some("resp_1"));
        assert_eq!(candidate.input_delta_count, 1);

        record_continuation(
            candidate.owner.as_ref(),
            candidate.turn_id,
            &appended,
            Some("resp_2"),
            &[],
        );
        let changed = request_with_input(
            vec![input("one"), input("two"), input("three")],
            Some(json!({"service_tier": "flex"})),
        );
        let candidate = continuation_candidate(Some(&owner), &changed, true);
        assert_eq!(candidate.disabled_reason.as_deref(), Some("prompt_changed"));
        assert!(!has_continuation_for_tests(&owner));
    }
}
