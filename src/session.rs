use crate::config::AliasProvider;
use crate::registry::normalize_incoming_model;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

const SESSION_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
pub const MAX_SESSIONS: usize = 10_000;

#[derive(Debug, Clone)]
pub struct SessionState {
    pub seq: u64,
    pub affinity_provider: Option<AliasProvider>,
    pub last_seen: u64,
}

#[derive(Default)]
struct SessionStore {
    map: HashMap<String, SessionState>,
    order: VecDeque<String>,
}

static SESSIONS: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::default()));

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_millis() as u64
}

pub fn existing_session(session_id: Option<&str>, now: u64) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let state = store.map.get(id).cloned()?;
    if now.saturating_sub(state.last_seen) > SESSION_IDLE_TTL_MS {
        store.map.remove(id);
        store.order.retain(|item| item != id);
        return None;
    }
    Some(state)
}

pub fn existing_session_now(session_id: Option<&str>) -> Option<SessionState> {
    existing_session(session_id, now_millis())
}

pub fn record_session_request(
    session_id: Option<&str>,
    prior: Option<&SessionState>,
    provider_name: &str,
    model: &str,
    now: u64,
) -> Option<SessionState> {
    record_session_request_with_affinity_update(session_id, prior, provider_name, model, true, now)
}

pub(crate) fn record_session_request_with_affinity_update(
    session_id: Option<&str>,
    prior: Option<&SessionState>,
    provider_name: &str,
    model: &str,
    update_affinity: bool,
    now: u64,
) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let mut next = prior.cloned().unwrap_or(SessionState {
        seq: 0,
        affinity_provider: None,
        last_seen: now,
    });
    next.seq += 1;
    next.last_seen = now;
    if update_affinity
        && is_alias_routable_provider(provider_name)
        && !crate::registry::is_anthropic_alias(normalize_incoming_model(model).as_str())
    {
        next.affinity_provider = Some(match provider_name {
            "codex" => AliasProvider::Codex,
            "kimi" => AliasProvider::Kimi,
            _ => next.affinity_provider.unwrap_or(AliasProvider::Codex),
        });
    }

    if !store.map.contains_key(id) {
        store.order.push_back(id.to_string());
    }
    store.map.insert(id.to_string(), next.clone());

    while store.order.len() > MAX_SESSIONS {
        if let Some(evict) = store.order.pop_front() {
            store.map.remove(&evict);
        } else {
            break;
        }
    }

    Some(next)
}

fn is_alias_routable_provider(name: &str) -> bool {
    matches!(name, "codex" | "kimi")
}

#[cfg(test)]
pub fn reset_sessions_for_test() {
    let mut store = SESSIONS.lock().expect("session lock");
    store.map.clear();
    store.order.clear();
}

pub fn affinity_provider_from_session(session: &SessionState) -> Option<AliasProvider> {
    session.affinity_provider
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auxiliary_request_does_not_change_session_affinity() {
        let session_id = "session-affinity-auxiliary-request-test";
        let initial = record_session_request(Some(session_id), None, "codex", "gpt-5.6-sol", 1)
            .expect("initial session");
        assert_eq!(initial.affinity_provider, Some(AliasProvider::Codex));

        let after_review = record_session_request_with_affinity_update(
            Some(session_id),
            Some(&initial),
            "kimi",
            "kimi-for-coding",
            false,
            2,
        )
        .expect("updated session");
        assert_eq!(after_review.seq, 2);
        assert_eq!(after_review.affinity_provider, Some(AliasProvider::Codex));
    }
}
