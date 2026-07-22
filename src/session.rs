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
    update_affinity: bool,
) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let mut next = store
        .map
        .get(id)
        .cloned()
        .or_else(|| prior.cloned())
        .unwrap_or(SessionState {
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
    fn request_can_advance_session_without_changing_latest_affinity() {
        reset_sessions_for_test();
        let first = record_session_request(
            Some("compaction-affinity-test"),
            None,
            "codex",
            "gpt-5.6-sol",
            1_000,
            true,
        )
        .unwrap();
        assert_eq!(first.affinity_provider, Some(AliasProvider::Codex));

        let ordinary = record_session_request(
            Some("compaction-affinity-test"),
            Some(&first),
            "kimi",
            "kimi-for-coding",
            1_500,
            true,
        )
        .unwrap();
        assert_eq!(ordinary.affinity_provider, Some(AliasProvider::Kimi));

        let compacted = record_session_request(
            Some("compaction-affinity-test"),
            Some(&first),
            "codex",
            "gpt-5.6-terra",
            2_000,
            false,
        )
        .unwrap();
        assert_eq!(compacted.seq, 3);
        assert_eq!(compacted.last_seen, 2_000);
        assert_eq!(compacted.affinity_provider, Some(AliasProvider::Kimi));
    }
}
