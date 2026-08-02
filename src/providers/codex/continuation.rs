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
    socket_id: u64,
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

#[cfg(test)]
static TEST_REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) fn lock_continuation_registry_for_tests() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_REGISTRY_LOCK.blocking_lock()
}

#[cfg(test)]
pub(crate) async fn lock_continuation_registry_for_async_tests()
-> tokio::sync::MutexGuard<'static, ()> {
    TEST_REGISTRY_LOCK.lock().await
}

#[derive(Clone)]
pub struct ContinuationCandidate {
    pub turn_id: Option<u64>,
    pub previous_response_id: Option<String>,
    pub input_delta: Option<Vec<ResponsesInputItem>>,
    pub input_delta_count: usize,
    pub disabled_reason: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ContinuationReservation {
    candidate: ContinuationCandidate,
    owner: Option<ConversationIdentity>,
    origin_socket_id: Option<u64>,
}

impl ContinuationReservation {
    pub(crate) fn new(
        candidate: ContinuationCandidate,
        owner: Option<ConversationIdentity>,
        origin_socket_id: Option<u64>,
    ) -> Self {
        Self {
            candidate,
            owner,
            origin_socket_id,
        }
    }

    pub(crate) fn from_public_candidate(candidate: &ContinuationCandidate) -> Self {
        Self::new(candidate.clone(), None, None)
    }

    pub(crate) fn for_owner_turn(
        owner: Option<&ConversationIdentity>,
        turn_id: Option<u64>,
    ) -> Self {
        Self::new(
            ContinuationCandidate {
                turn_id,
                previous_response_id: None,
                input_delta: None,
                input_delta_count: 0,
                disabled_reason: None,
            },
            owner.cloned(),
            None,
        )
    }

    pub(crate) fn candidate(&self) -> &ContinuationCandidate {
        &self.candidate
    }

    pub(crate) fn owner(&self) -> Option<&ConversationIdentity> {
        self.owner.as_ref()
    }

    pub(crate) fn turn_id(&self) -> Option<u64> {
        self.candidate.turn_id
    }

    pub(crate) fn origin_socket_id(&self) -> Option<u64> {
        self.origin_socket_id
    }

    pub(crate) fn into_candidate(self) -> ContinuationCandidate {
        self.candidate
    }

    pub(crate) fn full_context_retry(&self) -> Self {
        let mut candidate = self.candidate.clone();
        candidate.previous_response_id = None;
        candidate.input_delta = None;
        candidate.disabled_reason = Some("full_context_retry".to_string());
        Self::new(candidate, self.owner.clone(), None)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[deprecated(note = "use the owner-aware provider flow for typed conversation ownership")]
pub fn continuation_candidate(
    session_id: Option<&str>,
    body: &ResponsesRequest,
    enabled: bool,
) -> ContinuationCandidate {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    continuation_candidate_inner(owner.as_ref(), body, enabled, "missing_session").into_candidate()
}

pub(crate) fn continuation_candidate_for_owner(
    owner: Option<&ConversationIdentity>,
    body: &ResponsesRequest,
    enabled: bool,
) -> ContinuationReservation {
    continuation_candidate_inner(owner, body, enabled, "missing_identity")
}

fn continuation_candidate_inner(
    owner: Option<&ConversationIdentity>,
    body: &ResponsesRequest,
    enabled: bool,
    missing_owner_reason: &str,
) -> ContinuationReservation {
    if !enabled {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: None,
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("disabled".to_string()),
            },
            owner.cloned(),
            None,
        );
    }

    let Some(owner) = owner else {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: None,
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some(missing_owner_reason.to_string()),
            },
            None,
            None,
        );
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
) -> ContinuationReservation {
    let state = match state {
        Some(state) if now.saturating_sub(state.updated_at) <= TTL_MS => state,
        Some(_) | None => {
            return ContinuationReservation::new(
                ContinuationCandidate {
                    turn_id: Some(turn_id),
                    previous_response_id: None,
                    input_delta: None,
                    input_delta_count: body.input.len(),
                    disabled_reason: Some(if superseded_turn {
                        "superseded_turn".to_string()
                    } else {
                        "missing_state".to_string()
                    }),
                },
                Some(owner.clone()),
                None,
            );
        }
    };

    let signature = prompt_signature(body);
    if signature != state.prompt_signature {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("prompt_changed".to_string()),
            },
            Some(owner.clone()),
            None,
        );
    }

    let Some(suffix) = input_suffix_after_prefix(&body.input, &state.transcript) else {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("not_append_only".to_string()),
            },
            Some(owner.clone()),
            None,
        );
    };

    if suffix.is_empty() {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                input_delta_count: 0,
                disabled_reason: Some("empty_delta".to_string()),
            },
            Some(owner.clone()),
            None,
        );
    }

    ContinuationReservation::new(
        ContinuationCandidate {
            turn_id: Some(turn_id),
            previous_response_id: Some(state.response_id),
            input_delta_count: suffix.len(),
            input_delta: Some(suffix),
            disabled_reason: None,
        },
        Some(owner.clone()),
        Some(state.socket_id),
    )
}

#[deprecated(note = "recording without typed socket provenance is not reusable")]
pub fn record_continuation(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    request_body: &ResponsesRequest,
    response_id: Option<&str>,
    output_items: &[ResponsesInputItem],
) {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    let reservation = ContinuationReservation::new(
        ContinuationCandidate {
            turn_id,
            previous_response_id: None,
            input_delta: None,
            input_delta_count: request_body.input.len(),
            disabled_reason: Some("legacy_recording_without_socket".to_string()),
        },
        owner,
        None,
    );
    record_continuation_for_owner(&reservation, request_body, response_id, None, output_items);
}

pub(crate) fn record_continuation_for_owner(
    reservation: &ContinuationReservation,
    request_body: &ResponsesRequest,
    response_id: Option<&str>,
    socket_id: Option<u64>,
    output_items: &[ResponsesInputItem],
) {
    let (owner, turn_id) = match (reservation.owner(), reservation.turn_id()) {
        (Some(owner), Some(turn_id)) => (owner, turn_id),
        _ => return,
    };

    let (response_id, socket_id) = match (response_id, socket_id) {
        (Some(response_id), Some(socket_id)) if socket_id != 0 => {
            (response_id.to_string(), socket_id)
        }
        _ => {
            abort_continuation_inner(Some(owner), Some(turn_id));
            return;
        }
    };
    let mut transcript: Vec<ResponsesInputItem> = request_body.input.clone();
    transcript.extend_from_slice(output_items);

    let transcript_json = serde_json::to_string(&transcript).unwrap_or_default();
    let transcript_bytes = transcript_json.len() as u64;

    if transcript_bytes > MAX_OWNER_TRANSCRIPT_BYTES {
        abort_continuation_inner(Some(owner), Some(turn_id));
        return;
    }

    let state = ContinuationState {
        response_id,
        socket_id,
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

#[deprecated(note = "use the owner-aware provider flow for typed conversation ownership")]
pub fn abort_continuation(session_id: Option<&str>, turn_id: Option<u64>) {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    abort_continuation_inner(owner.as_ref(), turn_id);
}

pub(crate) fn abort_continuation_for_owner(reservation: &ContinuationReservation) {
    abort_continuation_inner(reservation.owner(), reservation.turn_id());
}

fn abort_continuation_inner(owner: Option<&ConversationIdentity>, turn_id: Option<u64>) {
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

#[deprecated(note = "use the owner-aware provider flow for typed conversation ownership")]
pub fn if_current_turn<T>(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    if_current_turn_inner(owner.as_ref(), turn_id, action)
}

pub(crate) fn if_current_turn_for_owner<T>(
    reservation: &ContinuationReservation,
    action: impl FnOnce() -> T,
) -> Option<T> {
    if_current_turn_inner(reservation.owner(), reservation.turn_id(), action)
}

fn if_current_turn_inner<T>(
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

#[deprecated(note = "use the owner-aware provider flow for typed conversation ownership")]
pub fn with_current_turn(
    session_id: Option<&str>,
    turn_id: Option<u64>,
    action: impl FnOnce(),
) -> bool {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    if_current_turn_inner(owner.as_ref(), turn_id, action).is_some()
}

pub(crate) fn with_current_turn_for_owner(
    reservation: &ContinuationReservation,
    action: impl FnOnce(),
) -> bool {
    if_current_turn_for_owner(reservation, action).is_some()
}

#[deprecated(note = "use the owner-aware provider flow for typed conversation ownership")]
pub fn is_current_turn(session_id: Option<&str>, turn_id: Option<u64>) -> bool {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    is_current_turn_inner(owner.as_ref(), turn_id)
}

#[allow(dead_code)]
pub(crate) fn is_current_turn_for_owner(reservation: &ContinuationReservation) -> bool {
    is_current_turn_inner(reservation.owner(), reservation.turn_id())
}

fn is_current_turn_inner(owner: Option<&ConversationIdentity>, turn_id: Option<u64>) -> bool {
    let (Some(owner), Some(turn_id)) = (owner, turn_id) else {
        return false;
    };
    let guard = REGISTRY.lock().unwrap();
    guard
        .as_ref()
        .and_then(|registry| registry.owners.get(owner))
        .is_some_and(|state| state.current_turn == turn_id)
}

#[deprecated(note = "use the owner-aware provider flow for typed conversation ownership")]
pub fn clear_continuation(session_id: Option<&str>) {
    let owner = session_id.map(|session_id| ConversationIdentity::Main(session_id.to_owned()));
    clear_continuation_for_owner(owner.as_ref());
}

pub(crate) fn clear_continuation_for_owner(owner: Option<&ConversationIdentity>) {
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

#[deprecated(note = "use the owner-aware test helper for typed conversation ownership")]
pub fn has_continuation_for_tests(session_id: &str) -> bool {
    let owner = ConversationIdentity::Main(session_id.to_owned());
    has_continuation_for_owner_for_tests(&owner)
}

pub(crate) fn has_continuation_for_owner_for_tests(owner: &ConversationIdentity) -> bool {
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

    fn lock_registry() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = lock_continuation_registry_for_tests();
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
        let reservation = continuation_candidate_for_owner(Some(owner), request, true);
        record_continuation_for_owner(&reservation, request, Some(response_id), Some(1), &[]);
    }

    #[test]
    #[allow(deprecated)]
    fn disabled_and_missing_identity_requests_are_stateless() {
        let _registry_guard = lock_registry();
        let request = request_with_input(vec![input("one")], None);
        let owner = main_owner("session-a");

        let disabled = continuation_candidate_for_owner(Some(&owner), &request, false);
        assert_eq!(disabled.owner(), Some(&owner));
        assert_eq!(disabled.turn_id(), None);
        assert_eq!(disabled.candidate().input_delta_count, request.input.len());
        assert_eq!(
            disabled.candidate().disabled_reason.as_deref(),
            Some("disabled")
        );

        let missing = continuation_candidate_for_owner(None, &request, true);
        assert_eq!(missing.owner(), None);
        assert_eq!(missing.turn_id(), None);
        assert_eq!(missing.candidate().input_delta_count, request.input.len());
        assert_eq!(
            missing.candidate().disabled_reason.as_deref(),
            Some("missing_identity")
        );

        let legacy_missing = continuation_candidate(None, &request, true);
        assert_eq!(
            legacy_missing.disabled_reason.as_deref(),
            Some("missing_session")
        );
    }

    #[test]
    fn sibling_agents_reserve_and_publish_independently() {
        let _registry_guard = lock_registry();
        let sibling_one = agent_owner("session-a", "agent-one");
        let sibling_two = agent_owner("session-a", "agent-two");
        let first_request = request_with_input(vec![input("one")], None);

        let first = continuation_candidate_for_owner(Some(&sibling_one), &first_request, true);
        let second = continuation_candidate_for_owner(Some(&sibling_two), &first_request, true);
        assert_ne!(first.turn_id(), second.turn_id());
        record_continuation_for_owner(&first, &first_request, Some("resp_one"), Some(11), &[]);
        record_continuation_for_owner(&second, &first_request, Some("resp_two"), Some(22), &[]);
        assert!(has_continuation_for_owner_for_tests(&sibling_one));
        assert!(has_continuation_for_owner_for_tests(&sibling_two));

        let next_request = request_with_input(vec![input("one"), input("two")], None);
        let first_next = continuation_candidate_for_owner(Some(&sibling_one), &next_request, true);
        let second_next = continuation_candidate_for_owner(Some(&sibling_two), &next_request, true);
        assert_eq!(
            first_next.candidate().previous_response_id.as_deref(),
            Some("resp_one")
        );
        assert_eq!(first_next.origin_socket_id(), Some(11));
        assert_eq!(
            second_next.candidate().previous_response_id.as_deref(),
            Some("resp_two")
        );
        assert_eq!(second_next.origin_socket_id(), Some(22));
    }

    #[test]
    fn different_owner_completion_order_cannot_interfere() {
        let _registry_guard = lock_registry();
        let main = main_owner("session-a");
        let agent = agent_owner("session-a", "agent-a");
        let request = request_with_input(vec![input("one")], None);
        let main_reservation = continuation_candidate_for_owner(Some(&main), &request, true);
        let agent_reservation = continuation_candidate_for_owner(Some(&agent), &request, true);

        record_continuation_for_owner(
            &agent_reservation,
            &request,
            Some("resp_agent"),
            Some(1),
            &[],
        );
        record_continuation_for_owner(&main_reservation, &request, Some("resp_main"), Some(1), &[]);
        abort_continuation_for_owner(&ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: agent_reservation.turn_id(),
                previous_response_id: None,
                input_delta: None,
                input_delta_count: 0,
                disabled_reason: None,
            },
            Some(main.clone()),
            None,
        ));

        assert!(has_continuation_for_owner_for_tests(&main));
        assert!(has_continuation_for_owner_for_tests(&agent));
        let next = request_with_input(vec![input("one"), input("two")], None);
        assert_eq!(
            continuation_candidate_for_owner(Some(&agent), &next, true)
                .candidate()
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

        let reservation = continuation_candidate_for_owner(Some(&owner), &request, true);
        record_continuation_for_owner(&reservation, &request, None, Some(1), &[]);

        assert!(!has_continuation_for_owner_for_tests(&owner));
        assert!(has_continuation_for_owner_for_tests(&sibling));
    }

    #[test]
    fn missing_socket_id_does_not_publish_reusable_state() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-no-socket");
        let request = request_with_input(vec![input("one")], None);
        let reservation = continuation_candidate_for_owner(Some(&owner), &request, true);

        record_continuation_for_owner(
            &reservation,
            &request,
            Some("resp_without_socket"),
            None,
            &[],
        );

        assert!(!has_continuation_for_owner_for_tests(&owner));
        let next = continuation_candidate_for_owner(Some(&owner), &request, true);
        assert_eq!(next.candidate().previous_response_id, None);
        assert_eq!(next.origin_socket_id(), None);
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_recording_without_provenance_publishes_no_reusable_state() {
        let _registry_guard = lock_registry();
        let session_id = "legacy-no-provenance";
        let request = request_with_input(vec![input("one")], None);
        let candidate = continuation_candidate(Some(session_id), &request, true);

        record_continuation(
            Some(session_id),
            candidate.turn_id,
            &request,
            Some("resp_legacy"),
            &[],
        );

        assert!(!has_continuation_for_tests(session_id));
        let next = continuation_candidate(Some(session_id), &request, true);
        assert_eq!(next.previous_response_id, None);
    }

    #[test]
    fn same_owner_stale_turn_cannot_publish_clear_or_run_actions() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_1");

        let stale = continuation_candidate_for_owner(Some(&owner), &request, true);
        let current = continuation_candidate_for_owner(Some(&owner), &request, true);
        assert_eq!(
            current.candidate().disabled_reason.as_deref(),
            Some("superseded_turn")
        );
        record_continuation_for_owner(&stale, &request, Some("resp_stale"), Some(1), &[]);
        assert!(!has_continuation_for_owner_for_tests(&owner));
        record_continuation_for_owner(&current, &request, Some("resp_current"), Some(1), &[]);
        assert!(has_continuation_for_owner_for_tests(&owner));
        abort_continuation_for_owner(&stale);
        assert!(has_continuation_for_owner_for_tests(&owner));

        let mut ran = false;
        assert_eq!(if_current_turn_for_owner(&stale, || ran = true), None);
        assert!(!ran);
    }

    #[test]
    #[allow(deprecated)]
    fn missing_owner_or_turn_mutations_are_hard_noops() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_1");

        let missing_owner = ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(1),
                previous_response_id: None,
                input_delta: None,
                input_delta_count: 1,
                disabled_reason: None,
            },
            None,
            None,
        );
        let missing_turn = ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: None,
                previous_response_id: None,
                input_delta: None,
                input_delta_count: 1,
                disabled_reason: None,
            },
            Some(owner.clone()),
            None,
        );
        record_continuation_for_owner(&missing_owner, &request, Some("ignored"), Some(1), &[]);
        record_continuation_for_owner(&missing_turn, &request, Some("ignored"), Some(1), &[]);
        abort_continuation_for_owner(&missing_owner);
        abort_continuation_for_owner(&missing_turn);
        clear_continuation_for_owner(None);
        assert!(has_continuation_for_owner_for_tests(&owner));

        let mut runs = 0;
        assert_eq!(
            if_current_turn_for_owner(&missing_owner, || runs += 1),
            None
        );
        assert_eq!(if_current_turn_for_owner(&missing_turn, || runs += 1), None);
        assert!(!with_current_turn_for_owner(&missing_owner, || runs += 1));
        assert!(!with_current_turn_for_owner(&missing_turn, || runs += 1));
        assert_eq!(if_current_turn(None, Some(1), || runs += 1), None);
        assert!(!with_current_turn(None, Some(1), || runs += 1));
        assert_eq!(runs, 0);
    }

    #[test]
    fn append_only_and_prompt_guards_stay_owner_scoped() {
        let _registry_guard = lock_registry();
        let owner = main_owner("session-a");
        let request = request_with_input(vec![input("one")], None);
        start_and_record(&owner, &request, "resp_1");

        let appended = request_with_input(vec![input("one"), input("two")], None);
        let reservation = continuation_candidate_for_owner(Some(&owner), &appended, true);
        assert_eq!(
            reservation.candidate().previous_response_id.as_deref(),
            Some("resp_1")
        );
        assert_eq!(reservation.candidate().input_delta_count, 1);

        let full_context = reservation.full_context_retry();
        assert_eq!(full_context.owner(), Some(&owner));
        assert_eq!(full_context.turn_id(), reservation.turn_id());
        assert_eq!(full_context.candidate().previous_response_id, None);
        assert!(full_context.candidate().input_delta.is_none());
        assert_eq!(full_context.origin_socket_id(), None);

        record_continuation_for_owner(&reservation, &appended, Some("resp_2"), Some(1), &[]);
        let changed = request_with_input(
            vec![input("one"), input("two"), input("three")],
            Some(json!({"service_tier": "flex"})),
        );
        let reservation = continuation_candidate_for_owner(Some(&owner), &changed, true);
        assert_eq!(
            reservation.candidate().disabled_reason.as_deref(),
            Some("prompt_changed")
        );
        assert!(!has_continuation_for_owner_for_tests(&owner));
    }
}
