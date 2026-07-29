use crate::registry::{SessionAffinityProvider, normalize_incoming_model};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

const SESSION_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
pub const MAX_SESSIONS: usize = 10_000;

#[derive(Debug, Clone)]
pub struct SessionState {
    pub seq: u64,
    pub affinity_provider: Option<SessionAffinityProvider>,
    pub last_seen: u64,
}

pub struct SessionRoute<T> {
    pub value: T,
    pub provider_name: &'static str,
    commit: bool,
    update_affinity: bool,
}

impl<T> SessionRoute<T> {
    pub fn new(value: T, provider_name: &'static str) -> Self {
        Self {
            value,
            provider_name,
            commit: true,
            update_affinity: true,
        }
    }

    pub fn preserving_affinity(value: T, provider_name: &'static str) -> Self {
        Self {
            value,
            provider_name,
            commit: true,
            update_affinity: false,
        }
    }

    pub fn without_commit(value: T) -> Self {
        Self {
            value,
            provider_name: "",
            commit: false,
            update_affinity: false,
        }
    }
}

#[derive(Default)]
struct SessionStore {
    map: HashMap<String, SessionState>,
    order: VecDeque<String>,
}

static SESSIONS: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::default()));

pub fn route_session_request<T>(
    session_id: Option<&str>,
    model: &str,
    now: u64,
    route: impl FnOnce(Option<&SessionAffinityProvider>) -> Option<SessionRoute<T>>,
) -> (Option<T>, Option<SessionState>) {
    let Some(id) = session_id else {
        return (route(None).map(|route| route.value), None);
    };
    let mut store = SESSIONS.lock().expect("session lock");
    let prior = store.map.get(id).cloned().and_then(|state| {
        (now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS).then_some(state)
    });
    if prior.is_none() && store.map.remove(id).is_some() {
        store.order.retain(|item| item != id);
    }

    let selected = route(
        prior
            .as_ref()
            .and_then(|state| state.affinity_provider.as_ref()),
    );
    let Some(selected) = selected else {
        return (None, prior);
    };
    if !selected.commit {
        return (Some(selected.value), prior);
    }
    let mut next = prior.unwrap_or(SessionState {
        seq: 0,
        affinity_provider: None,
        last_seen: now,
    });
    next.seq += 1;
    next.last_seen = next.last_seen.max(now);
    if selected.update_affinity
        && !crate::registry::is_anthropic_alias(normalize_incoming_model(model).as_str())
        && let Some(provider) = session_affinity_provider(selected.provider_name)
    {
        next.affinity_provider = Some(provider);
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

    (Some(selected.value), Some(next))
}

fn session_affinity_provider(name: &str) -> Option<SessionAffinityProvider> {
    match name {
        "codex" => Some(SessionAffinityProvider::Codex),
        "grok" => Some(SessionAffinityProvider::Grok),
        _ => None,
    }
}

#[cfg(test)]
pub fn reset_sessions_for_test() {
    let mut store = SESSIONS.lock().expect("session lock");
    store.map.clear();
    store.order.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    static SESSION_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn concurrent_requests_receive_unique_monotonic_sequences() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        const REQUESTS: usize = 256;
        let barrier = Arc::new(Barrier::new(REQUESTS));
        let mut threads = Vec::new();
        for _ in 0..REQUESTS {
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let (_, state) =
                    route_session_request(Some("shared-session"), "gpt-5.6-sol", 1, |_| {
                        Some(SessionRoute::new((), "codex"))
                    });
                state.unwrap().seq
            }));
        }
        let sequences: Vec<u64> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        let unique: HashSet<u64> = sequences.iter().copied().collect();
        assert_eq!(unique.len(), REQUESTS);
        assert_eq!(unique.iter().copied().min(), Some(1));
        assert_eq!(unique.iter().copied().max(), Some(REQUESTS as u64));
    }

    #[test]
    fn routing_and_affinity_update_are_atomic() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        let (_, first) = route_session_request(Some("affinity-session"), "gpt-5.6-sol", 1, |_| {
            Some(SessionRoute::new((), "codex"))
        });
        assert_eq!(
            first.unwrap().affinity_provider,
            Some(SessionAffinityProvider::Codex)
        );

        let (seen, second) =
            route_session_request(Some("affinity-session"), "sonnet", 2, |affinity| {
                Some(SessionRoute::new(affinity.copied(), "codex"))
            });
        assert_eq!(seen, Some(Some(SessionAffinityProvider::Codex)));
        assert_eq!(second.unwrap().seq, 2);
    }

    #[test]
    fn explicit_grok_model_establishes_alias_affinity() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        let (_, first) =
            route_session_request(Some("grok-affinity-session"), "grok-4.5", 1, |_| {
                Some(SessionRoute::new((), "grok"))
            });
        assert_eq!(
            first.unwrap().affinity_provider,
            Some(SessionAffinityProvider::Grok)
        );

        let (seen, second) = route_session_request(
            Some("grok-affinity-session"),
            "claude-opus-5",
            2,
            |affinity| Some(SessionRoute::new(affinity.copied(), "grok")),
        );
        assert_eq!(seen, Some(Some(SessionAffinityProvider::Grok)));
        assert_eq!(second.unwrap().seq, 2);
    }

    #[test]
    fn expired_session_starts_a_new_sequence() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        let (_, first) = route_session_request(Some("expired-session"), "gpt-5.6-sol", 1, |_| {
            Some(SessionRoute::new((), "codex"))
        });
        assert_eq!(first.unwrap().seq, 1);

        let (_, next) = route_session_request(
            Some("expired-session"),
            "gpt-5.6-sol",
            SESSION_IDLE_TTL_MS + 2,
            |_| Some(SessionRoute::new((), "codex")),
        );
        assert_eq!(next.unwrap().seq, 1);
    }

    #[test]
    fn unroutable_request_does_not_advance_or_rewind_session_state() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        let (_, first) = route_session_request(Some("stable-session"), "gpt-5.6-sol", 10, |_| {
            Some(SessionRoute::new((), "codex"))
        });
        assert_eq!(first.as_ref().unwrap().seq, 1);

        let (selected, unchanged) =
            route_session_request::<()>(Some("stable-session"), "unknown-model", 5, |_| None);
        assert!(selected.is_none());
        assert_eq!(unchanged.as_ref().unwrap().seq, 1);
        assert_eq!(unchanged.unwrap().last_seen, 10);

        let (_, next) = route_session_request(Some("stable-session"), "gpt-5.6-sol", 4, |_| {
            Some(SessionRoute::new((), "codex"))
        });
        assert_eq!(next.as_ref().unwrap().seq, 2);
        assert_eq!(next.unwrap().last_seen, 10);
    }

    #[test]
    fn rejected_route_does_not_advance_sequence_or_change_affinity() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        let (_, first) = route_session_request(Some("gate-session"), "gpt-5.6-sol", 1, |_| {
            Some(SessionRoute::new((), "codex"))
        });
        let prior = first.unwrap();

        let (rejected, unchanged) =
            route_session_request(Some("gate-session"), "gpt-5.6-sol", 2, |_| {
                Some(SessionRoute::without_commit("provider saturated"))
            });
        assert_eq!(rejected, Some("provider saturated"));
        assert_eq!(unchanged.as_ref().map(|state| state.seq), Some(prior.seq));
        assert_eq!(
            unchanged.and_then(|state| state.affinity_provider),
            prior.affinity_provider
        );
    }

    #[test]
    fn preserving_affinity_still_commits_sequence_and_last_seen() {
        let _guard = SESSION_TEST_LOCK.lock().expect("session test lock");
        reset_sessions_for_test();
        let (_, first) = route_session_request(Some("preserved-session"), "grok-4.5", 10, |_| {
            Some(SessionRoute::new((), "grok"))
        });
        assert_eq!(
            first.as_ref().and_then(|state| state.affinity_provider),
            Some(SessionAffinityProvider::Grok)
        );

        let (_, second) =
            route_session_request(Some("preserved-session"), "gpt-5.6-terra", 20, |_| {
                Some(SessionRoute::preserving_affinity((), "codex"))
            });
        let second = second.expect("preserving route must commit session state");
        assert_eq!(second.seq, 2);
        assert_eq!(second.last_seen, 20);
        assert_eq!(
            second.affinity_provider,
            Some(SessionAffinityProvider::Grok)
        );
    }
}
