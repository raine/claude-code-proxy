//! Cursor conversation continuation across Claude Code requests.
//!
//! A Cursor run ends with its turn, but the server's conversation survives in
//! two pieces the client holds: the last `conversation_checkpoint_update` (a
//! `ConversationStateStructure` whose turns are blob ids) and the blobs the
//! server wrote through the agent store. Sending both back under the same
//! conversation id lets the next request carry only its new messages, so the
//! upstream prompt keeps the earlier turns as a cacheable prefix. A fresh run
//! instead flattens the whole transcript into one new user message, and the
//! prompt cache never matches past Cursor's own system prompt: every turn writes
//! the full context to the cache again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use tokio::sync::OwnedMutexGuard;

use crate::anthropic::schema::{Message, MessagesRequest};
use crate::providers::cursor::request::{render_message_content, render_system};

/// A conversation nobody came back to for this long is dropped.
const MAX_IDLE: Duration = Duration::from_secs(30 * 60);

/// Upper bound on kept conversations; the least recently used goes first.
const MAX_ENTRIES: usize = 32;

/// How long a request waits for the previous run to record its final state.
const SETTLE_WAIT: Duration = Duration::from_secs(30);

/// The conversation state of one Cursor run, shared with its reader task.
#[derive(Debug, Clone)]
pub struct RunState {
    /// Sent as the run's conversation id; reused by every continued run.
    pub conversation_id: String,
    /// The raw `ConversationStateStructure` of the latest checkpoint.
    pub checkpoint: Option<Vec<u8>>,
    /// The agent store: blob id to blob, as the server wrote them.
    pub blobs: HashMap<Vec<u8>, Vec<u8>>,
}

/// A [`RunState`] the reader task updates while the caller holds it.
pub type SharedRunState = Arc<Mutex<RunState>>;

impl RunState {
    /// The state of a run that starts a new conversation.
    pub fn new_shared() -> SharedRunState {
        Arc::new(Mutex::new(Self {
            conversation_id: uuid::Uuid::new_v4().to_string(),
            checkpoint: None,
            blobs: HashMap::new(),
        }))
    }
}

/// The part of Claude Code's transcript a run has seen.
///
/// `prefix_len` counts the messages of the request that opened or last resumed
/// the run. The run's reply becomes message `prefix_len` in Claude Code's next
/// request, and everything after that reply is new to the server.
#[derive(Debug, Clone)]
pub struct Segment {
    key: String,
    prefix_len: usize,
    prefix_hash: String,
}

impl Segment {
    /// The segment a run serving `body` covers.
    pub fn for_request(session_id: &str, body: &MessagesRequest) -> Self {
        Self {
            key: conversation_key(session_id, body),
            prefix_len: body.messages.len(),
            prefix_hash: prefix_hash(&body.messages),
        }
    }
}

/// What the next run starts from.
pub enum Resume {
    /// Continue the stored conversation. `delta` is the request cut down to the
    /// messages the server has not seen, with no system prompt or tools.
    Continue {
        state: SharedRunState,
        delta: MessagesRequest,
    },
    /// Start a new conversation with the whole transcript.
    Fresh,
}

struct Stored {
    state: RunState,
    segment: Segment,
}

type Slot = Arc<tokio::sync::Mutex<Option<Stored>>>;

struct Entry {
    slot: Slot,
    touched: Instant,
}

static STORE: Lazy<Mutex<HashMap<String, Entry>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// A conversation slot whose run has not settled yet.
///
/// The next request for the same conversation waits on it. Dropping it without
/// [`Pending::complete`] marks the run as not resumable, and that request starts
/// a new conversation.
pub struct Pending {
    guard: OwnedMutexGuard<Option<Stored>>,
    segment: Segment,
}

impl Pending {
    /// Record the run's final state for the next request.
    pub fn complete(mut self, state: RunState) {
        *self.guard = Some(Stored {
            state,
            segment: self.segment,
        });
    }
}

/// Claim the slot of the conversation a run covers, replacing any older one.
pub fn reserve(segment: Segment) -> Pending {
    let slot: Slot = Arc::new(tokio::sync::Mutex::new(None));
    let guard = Arc::clone(&slot)
        .try_lock_owned()
        .expect("a new slot is unlocked");
    let mut store = STORE.lock().unwrap();
    evict(&mut store);
    store.insert(
        segment.key.clone(),
        Entry {
            slot,
            touched: Instant::now(),
        },
    );
    Pending { guard, segment }
}

/// Take the stored conversation `body` continues, if any.
///
/// Waits for a run that is still settling. Any doubt -- no entry, a run that
/// could not settle, a transcript that changed under it -- starts fresh, which
/// is what every request did before continuation existed.
pub async fn resume(session_id: &str, body: &MessagesRequest) -> Resume {
    let key = conversation_key(session_id, body);
    let entry = STORE.lock().unwrap().remove(&key);
    let (resume, reason) = match entry {
        None => (Resume::Fresh, "no_conversation"),
        Some(entry) => match tokio::time::timeout(SETTLE_WAIT, entry.slot.lock_owned()).await {
            Err(_) => (Resume::Fresh, "settle_timeout"),
            Ok(mut guard) => match guard.take() {
                None => (Resume::Fresh, "not_resumable"),
                Some(stored) => match delta_request(&stored.segment, body) {
                    None => (Resume::Fresh, "history_changed"),
                    Some(delta) => (
                        Resume::Continue {
                            state: Arc::new(Mutex::new(stored.state)),
                            delta,
                        },
                        "continued",
                    ),
                },
            },
        },
    };
    log_resume(&resume, reason, body);
    resume
}

fn log_resume(resume: &Resume, reason: &str, body: &MessagesRequest) {
    let mut fields = serde_json::Map::new();
    fields.insert("outcome".into(), reason.into());
    fields.insert("messages".into(), body.messages.len().into());
    if let Resume::Continue { delta, .. } = resume {
        fields.insert("deltaMessages".into(), delta.messages.len().into());
    }
    crate::logging::create_logger("cursor").info("cursor_conversation", Some(fields));
}

/// The request cut down to what the server has not seen.
///
/// The stored run saw `messages[..prefix_len]` and wrote the reply Claude Code
/// stored as `messages[prefix_len]`. Everything after it must come from the
/// client side: tool results, a new prompt, and the `system` messages Claude
/// Code appends after a user turn (hooks, output styles). Another assistant
/// message means the transcript moved on without this conversation.
fn delta_request(segment: &Segment, body: &MessagesRequest) -> Option<MessagesRequest> {
    let messages = &body.messages;
    let reply = messages.get(segment.prefix_len)?;
    let delta = &messages[segment.prefix_len + 1..];
    if reply.role != "assistant"
        || delta.is_empty()
        || delta.iter().any(|message| message.role == "assistant")
        || prefix_hash(&messages[..segment.prefix_len]) != segment.prefix_hash
    {
        return None;
    }
    let mut request = body.clone();
    request.messages = delta.to_vec();
    request.extra.remove("system");
    request.extra.remove("tools");
    Some(request)
}

/// One conversation per Claude Code session, model, system prompt and tool set.
///
/// Side requests share the session id (the auto-mode classifier, subagents) but
/// not the prompt, so each keeps its own conversation. A changed tool list
/// starts a new one: the tools travel in the first prompt only.
fn conversation_key(session_id: &str, body: &MessagesRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update([0]);
    hasher.update(body.model.as_deref().unwrap_or("").as_bytes());
    hasher.update([0]);
    hasher.update(render_system(body).unwrap_or_default().as_bytes());
    hasher.update([0]);
    if let Some(tools) = body.extra.get("tools") {
        hasher.update(tools.to_string().as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Hash of the rendered messages: the text the server saw, so markers Claude
/// Code moves between requests (`cache_control`) do not count as a change.
fn prefix_hash(messages: &[Message]) -> String {
    let mut hasher = Sha256::new();
    for message in messages {
        hasher.update(message.role.as_bytes());
        hasher.update([0]);
        hasher.update(
            render_message_content(message)
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn evict(store: &mut HashMap<String, Entry>) {
    store.retain(|_, entry| entry.touched.elapsed() < MAX_IDLE);
    while store.len() >= MAX_ENTRIES {
        let Some(oldest) = store
            .iter()
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        store.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(messages: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "cursor:claude-opus-5-5-high",
            "stream": true,
            "system": [{"type": "text", "text": "be brief"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}],
            "messages": messages,
        }))
        .unwrap()
    }

    fn first_turn() -> MessagesRequest {
        request(serde_json::json!([{"role": "user", "content": "first question"}]))
    }

    fn second_turn() -> MessagesRequest {
        request(serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "first question",
                "cache_control": {"type": "ephemeral"}}]},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "second question"}
        ]))
    }

    fn state(conversation_id: &str) -> RunState {
        RunState {
            conversation_id: conversation_id.to_string(),
            checkpoint: Some(vec![1, 2, 3]),
            blobs: HashMap::from([(b"blob".to_vec(), b"data".to_vec())]),
        }
    }

    #[test]
    fn delta_keeps_only_the_messages_after_the_reply() {
        let segment = Segment::for_request("session-delta", &first_turn());

        let delta = delta_request(&segment, &second_turn()).expect("continues");

        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0].content, "second question");
        assert!(delta.extra.get("system").is_none());
        assert!(delta.extra.get("tools").is_none());
    }

    #[test]
    fn delta_carries_the_system_messages_claude_code_appends() {
        let first = request(serde_json::json!([
            {"role": "user", "content": "first question"},
            {"role": "system", "content": [{"type": "text", "text": "style reminder",
                "cache_control": {"type": "ephemeral"}}]}
        ]));
        let segment = Segment::for_request("session-system", &first);
        let second = request(serde_json::json!([
            {"role": "user", "content": "first question"},
            {"role": "system", "content": "style reminder"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "second question"},
            {"role": "system", "content": "style reminder"}
        ]));

        let delta = delta_request(&segment, &second).expect("continues");

        let roles: Vec<&str> = delta.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "system"]);
    }

    #[test]
    fn delta_rejects_an_edited_history() {
        let segment = Segment::for_request("session-edited", &first_turn());
        let edited = request(serde_json::json!([
            {"role": "user", "content": "another first question"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "second question"}
        ]));

        assert!(delta_request(&segment, &edited).is_none());
    }

    #[test]
    fn delta_rejects_a_second_assistant_reply() {
        let segment = Segment::for_request("session-two-replies", &first_turn());
        let moved_on = request(serde_json::json!([
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "second question"},
            {"role": "assistant", "content": "an answer this conversation never gave"},
            {"role": "user", "content": "third question"}
        ]));

        assert!(delta_request(&segment, &moved_on).is_none());
    }

    #[test]
    fn delta_rejects_a_request_without_new_user_turns() {
        let segment = Segment::for_request("session-short", &first_turn());
        let reply_only = request(serde_json::json!([
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "first answer"}
        ]));

        assert!(delta_request(&segment, &reply_only).is_none());
    }

    #[test]
    fn key_separates_requests_with_another_system_prompt() {
        let mut classifier = second_turn();
        classifier.extra.insert(
            "system".into(),
            serde_json::json!([{"type": "text", "text": "classify"}]),
        );

        assert_ne!(
            conversation_key("session-key", &second_turn()),
            conversation_key("session-key", &classifier)
        );
        assert_eq!(
            conversation_key("session-key", &first_turn()),
            conversation_key("session-key", &second_turn())
        );
    }

    #[tokio::test]
    async fn resume_returns_the_completed_conversation() {
        reserve(Segment::for_request("session-resume", &first_turn()))
            .complete(state("conv-resume"));

        let Resume::Continue { state, delta } = resume("session-resume", &second_turn()).await
        else {
            panic!("expected a continued conversation");
        };

        let state = state.lock().unwrap();
        assert_eq!(state.conversation_id, "conv-resume");
        assert_eq!(state.checkpoint, Some(vec![1, 2, 3]));
        assert_eq!(delta.messages.len(), 1);
    }

    #[tokio::test]
    async fn resume_waits_for_a_run_that_is_still_settling() {
        let pending = reserve(Segment::for_request("session-settle", &first_turn()));
        let settle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            pending.complete(state("conv-settle"));
        });

        let resumed = resume("session-settle", &second_turn()).await;
        settle.await.unwrap();

        assert!(matches!(resumed, Resume::Continue { .. }));
    }

    #[tokio::test]
    async fn resume_starts_fresh_when_the_run_could_not_settle() {
        drop(reserve(Segment::for_request(
            "session-dropped",
            &first_turn(),
        )));

        assert!(matches!(
            resume("session-dropped", &second_turn()).await,
            Resume::Fresh
        ));
    }

    #[tokio::test]
    async fn a_conversation_is_resumed_once() {
        reserve(Segment::for_request("session-once", &first_turn())).complete(state("conv-once"));

        assert!(matches!(
            resume("session-once", &second_turn()).await,
            Resume::Continue { .. }
        ));
        assert!(matches!(
            resume("session-once", &second_turn()).await,
            Resume::Fresh
        ));
    }
}
