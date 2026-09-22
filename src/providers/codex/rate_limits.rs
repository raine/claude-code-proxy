//! Publishing Codex quota state in the vocabulary Anthropic clients read.
//!
//! Codex reports how much of each subscription window is spent inside the
//! stream, as a `codex.rate_limits` event, while Anthropic clients expect that
//! state in response headers. The event arrives early — ahead of the first
//! content event — so the reading is usually ready in time for the response it
//! came from; when it is not, it travels with the next one, a gauge a turn
//! stale.
//!
//! Without this the only quota signal a client ever sees is the refusal, and a
//! limit reached with no warning beforehand is exactly the case where nobody
//! knows why the assistant stopped answering.

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Codex reports `used_percent` on a 0..100 scale; the headers carry a ratio.
const PERCENT: f64 = 100.0;

/// A window is the five hour one when it is no longer than this. Codex reports
/// 300 minutes for it (299 in older payloads), against 10080 for the weekly.
const FIVE_HOUR_MAX_MINUTES: u64 = 360;

/// Where each window starts being worth announcing. These mirror the thresholds
/// clients apply to Anthropic's own windows, so a Codex session warns at the
/// same points a Claude session does.
const FIVE_HOUR_WARN_AT: f64 = 0.9;
const SEVEN_DAY_WARN_AT: f64 = 0.75;

/// Lowers both thresholds, for a consumer that wants the spent fraction earlier
/// than the defaults publish it — the fraction only reaches a caller once a
/// threshold is declared surpassed. Interfaces that draw their own warning have
/// a floor of their own well above zero, so a low setting here feeds a watcher
/// without turning the terminal noisy.
const WARN_AT_ENV: &str = "CCP_CODEX_QUOTA_WARN_AT";

fn warn_at(default: f64) -> f64 {
    static OVERRIDE: OnceLock<Option<f64>> = OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            std::env::var(WARN_AT_ENV)
                .ok()
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| (0.0..=1.0).contains(value))
        })
        .unwrap_or(default)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowState {
    /// Spent fraction of the window. Can exceed 1.0: usage legitimately runs
    /// past a cap before the refusal lands.
    pub utilization: f64,
    pub resets_at: u64,
}

impl WindowState {
    fn surpassed(&self, threshold: f64) -> bool {
        self.utilization >= threshold
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub five_hour: Option<WindowState>,
    pub seven_day: Option<WindowState>,
}

impl Snapshot {
    /// The window a client should name when it mentions one: whichever is
    /// closest to running out, since that is the one that will stop the work.
    fn representative(&self) -> Option<(&'static str, WindowState)> {
        match (self.five_hour, self.seven_day) {
            (Some(five), Some(seven)) if seven.utilization > five.utilization => {
                Some(("seven_day", seven))
            }
            (Some(five), _) => Some(("five_hour", five)),
            (None, Some(seven)) => Some(("seven_day", seven)),
            (None, None) => None,
        }
    }

    /// The headers that carry this snapshot, as name/value pairs.
    pub(crate) fn headers(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        let Some((claim, representative)) = self.representative() else {
            return out;
        };

        out.push(("anthropic-ratelimit-unified-status", "allowed".to_string()));
        out.push((
            "anthropic-ratelimit-unified-reset",
            representative.resets_at.to_string(),
        ));
        out.push((
            "anthropic-ratelimit-unified-representative-claim",
            claim.to_string(),
        ));

        for (window, threshold, utilization_header, reset_header, threshold_header) in [
            (
                self.five_hour,
                FIVE_HOUR_WARN_AT,
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-5h-reset",
                "anthropic-ratelimit-unified-5h-surpassed-threshold",
            ),
            (
                self.seven_day,
                SEVEN_DAY_WARN_AT,
                "anthropic-ratelimit-unified-7d-utilization",
                "anthropic-ratelimit-unified-7d-reset",
                "anthropic-ratelimit-unified-7d-surpassed-threshold",
            ),
        ] {
            let Some(state) = window else { continue };
            let threshold = warn_at(threshold);
            out.push((utilization_header, format!("{:.4}", state.utilization)));
            out.push((reset_header, state.resets_at.to_string()));
            // Clients report the spent fraction to their caller only once a
            // threshold is declared surpassed, so a window worth warning about
            // has to say which line it crossed.
            if state.surpassed(threshold) {
                out.push((threshold_header, format!("{threshold:.2}")));
            }
        }

        out
    }
}

static LATEST: OnceLock<Mutex<Option<Snapshot>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Snapshot>> {
    LATEST.get_or_init(|| Mutex::new(None))
}

/// Record the quota state carried by a `codex.rate_limits` event. Any other
/// event is ignored, so this can be handed every frame off the stream.
pub(crate) fn observe_event(payload: &Value) {
    if payload.get("type").and_then(Value::as_str) != Some("codex.rate_limits") {
        return;
    }
    let Some(snapshot) = snapshot_from_event(payload) else {
        return;
    };
    if let Ok(mut latest) = cell().lock() {
        *latest = Some(snapshot);
    }
}

/// The most recent snapshot, with any window that has since rolled over left
/// out.
///
/// A reading can outlive the window it describes — nothing new arrives while a
/// process sits idle — and the turn after a window reopens would then carry the
/// spent figure from before the reset, announcing an allowance as nearly gone
/// at the moment it came back. A window whose reset time has passed says
/// nothing about the one now running.
pub(crate) fn latest() -> Option<Snapshot> {
    let snapshot = cell().lock().ok().and_then(|latest| latest.clone())?;
    still_running(snapshot, now())
}

fn still_running(snapshot: Snapshot, now: u64) -> Option<Snapshot> {
    let fresh = |window: Option<WindowState>| window.filter(|state| state.resets_at > now);
    let snapshot = Snapshot {
        five_hour: fresh(snapshot.five_hour),
        seven_day: fresh(snapshot.seven_day),
    };

    (snapshot != Snapshot::default()).then_some(snapshot)
}

fn snapshot_from_event(payload: &Value) -> Option<Snapshot> {
    let limits = payload.get("rate_limits")?;
    let mut snapshot = Snapshot::default();
    // Codex names the windows by rank rather than by length, and which rank
    // holds which length has changed between payload versions — so the window
    // is decided by the length it reports, not by the slot it arrived in.
    for slot in ["primary", "secondary"] {
        let Some(window) = limits.get(slot) else {
            continue;
        };
        let Some((minutes, state)) = window_state(window) else {
            continue;
        };
        if minutes <= FIVE_HOUR_MAX_MINUTES {
            snapshot.five_hour = Some(state);
        } else {
            snapshot.seven_day = Some(state);
        }
    }

    (snapshot != Snapshot::default()).then_some(snapshot)
}

fn window_state(window: &Value) -> Option<(u64, WindowState)> {
    let minutes = number(window.get("window_minutes"))? as u64;
    let used_percent = number(window.get("used_percent"))?;
    // The stream names the moment `reset_at`; the same window reserialised into
    // a Codex CLI session log calls it `resets_at`. Both spellings are read so
    // a payload from either side parses.
    let resets_at = match first(window, &["reset_at", "resets_at"]) {
        Some(at) => at as u64,
        // Some payloads count down instead of naming the moment.
        None => now() + first(window, &["reset_after_seconds", "resets_in_seconds"])? as u64,
    };

    Some((
        minutes,
        WindowState {
            utilization: used_percent / PERCENT,
            resets_at,
        },
    ))
}

fn first(window: &Value, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| number(window.get(*name)))
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape recorded off a live stream, down to the field names.
    fn event(primary_percent: f64, secondary_percent: f64) -> Value {
        serde_json::json!({
            "type": "codex.rate_limits",
            "plan_type": "plus",
            "credits": {"balance": "0", "has_credits": false, "unlimited": false},
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {
                    "used_percent": primary_percent,
                    "window_minutes": 300,
                    "reset_after_seconds": 16394,
                    "reset_at": 1788879437u64
                },
                "secondary": {
                    "used_percent": secondary_percent,
                    "window_minutes": 10080,
                    "reset_after_seconds": 584857,
                    "reset_at": 1789466238u64
                }
            }
        })
    }

    fn headers_of(payload: &Value) -> std::collections::HashMap<&'static str, String> {
        snapshot_from_event(payload)
            .expect("snapshot")
            .headers()
            .into_iter()
            .collect()
    }

    #[test]
    fn maps_windows_by_their_length() {
        let headers = headers_of(&event(42.0, 16.0));
        assert_eq!(
            headers["anthropic-ratelimit-unified-5h-utilization"],
            "0.4200"
        );
        assert_eq!(
            headers["anthropic-ratelimit-unified-5h-reset"],
            "1788879437"
        );
        assert_eq!(
            headers["anthropic-ratelimit-unified-7d-utilization"],
            "0.1600"
        );
        assert_eq!(headers["anthropic-ratelimit-unified-status"], "allowed");
    }

    #[test]
    fn stays_quiet_below_the_thresholds() {
        let headers = headers_of(&event(42.0, 16.0));
        assert!(!headers.contains_key("anthropic-ratelimit-unified-5h-surpassed-threshold"));
        assert!(!headers.contains_key("anthropic-ratelimit-unified-7d-surpassed-threshold"));
    }

    #[test]
    fn declares_the_threshold_a_window_crossed() {
        let headers = headers_of(&event(93.0, 16.0));
        assert_eq!(
            headers["anthropic-ratelimit-unified-5h-surpassed-threshold"],
            "0.90"
        );
        assert!(!headers.contains_key("anthropic-ratelimit-unified-7d-surpassed-threshold"));

        let weekly = headers_of(&event(10.0, 80.0));
        assert_eq!(
            weekly["anthropic-ratelimit-unified-7d-surpassed-threshold"],
            "0.75"
        );
    }

    #[test]
    fn names_the_window_closest_to_running_out() {
        let headers = headers_of(&event(10.0, 80.0));
        assert_eq!(
            headers["anthropic-ratelimit-unified-representative-claim"],
            "seven_day"
        );
        assert_eq!(headers["anthropic-ratelimit-unified-reset"], "1789466238");

        let five = headers_of(&event(80.0, 10.0));
        assert_eq!(
            five["anthropic-ratelimit-unified-representative-claim"],
            "five_hour"
        );
    }

    #[test]
    fn accepts_the_spelling_a_session_log_uses() {
        // The same window, reserialised by the Codex CLI into its rollout file.
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "primary": {
                    "used_percent": 5.0,
                    "window_minutes": 299,
                    "resets_at": 1788879437u64
                }
            }
        });
        let snapshot = snapshot_from_event(&payload).expect("snapshot");
        assert_eq!(snapshot.five_hour.expect("five hour").resets_at, 1788879437);
    }

    #[test]
    fn accepts_a_countdown_instead_of_a_moment() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "primary": {
                    "used_percent": 5.0,
                    "window_minutes": 299,
                    "resets_in_seconds": 17940
                }
            }
        });
        let snapshot = snapshot_from_event(&payload).expect("snapshot");
        let five = snapshot.five_hour.expect("five hour window");
        assert!(five.resets_at > now());
        assert!(five.resets_at <= now() + 17940);
    }

    #[test]
    fn drops_a_window_that_has_since_rolled_over() {
        let spent = snapshot_from_event(&event(100.0, 80.0)).expect("snapshot");
        // Both windows reset long before this moment.
        assert_eq!(still_running(spent.clone(), 1_800_000_000), None);

        // The weekly window is still running; the five hour one has rolled over.
        let surviving = still_running(spent, 1_789_000_000).expect("weekly window survives");
        assert!(surviving.five_hour.is_none());
        assert!(surviving.seven_day.is_some());
        let headers: std::collections::HashMap<_, _> = surviving.headers().into_iter().collect();
        assert!(!headers.contains_key("anthropic-ratelimit-unified-5h-utilization"));
        assert_eq!(
            headers["anthropic-ratelimit-unified-representative-claim"],
            "seven_day"
        );
    }

    #[test]
    fn ignores_frames_that_are_not_quota_telemetry() {
        observe_event(&serde_json::json!({"type": "response.completed"}));
        assert!(snapshot_from_event(&serde_json::json!({"type": "codex.rate_limits"})).is_none());
    }
}
