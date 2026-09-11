//! Codex quota telemetry, re-exposed to the local client.
//!
//! Codex reports how much of a ChatGPT subscription is spent in two places: the
//! `x-codex-*` headers on an HTTP response, and the `codex.rate_limits` event on
//! the WebSocket transport. Neither reaches the client, because the proxy
//! answers in Anthropic's format and that format has no equivalent field. A tool
//! placed in front of the proxy — a rotating multi-account gateway, a quota
//! readout — therefore has no way to see the limit it is about to hit.
//!
//! The newest snapshot is kept here and stamped onto every Codex response under
//! the header names Codex itself uses, so a client that already reads them needs
//! no second format. Codex sends the event ahead of the first generated output
//! on both transports, and the response head is built from that output, so a
//! response normally carries the numbers of the request it answers. A response
//! that arrives before any telemetry has — an early error, the first request of
//! a run that failed upstream — carries the previous snapshot, or none.

use std::sync::{LazyLock, RwLock};

use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

/// Header suffixes that carry a window's numbers.
const WINDOW_SUFFIXES: [&str; 6] = [
    "-used-percent",
    "-window-minutes",
    "-reset-at",
    "-reset-after-seconds",
    "-limit-name",
    "-over-secondary-limit-percent",
];

/// Whole header names that describe the subscription rather than a window.
const PLAN_HEADERS: [&str; 2] = ["x-codex-plan-type", "x-codex-active-limit"];

static SNAPSHOT: LazyLock<RwLock<Vec<(HeaderName, HeaderValue)>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// True for a header that reports quota rather than protocol state. Codex sends
/// several limit families (`x-codex-*`, `x-codex-<limit id>-*`) and a few opaque
/// continuation headers that share the prefix; only the former are quota.
fn is_quota_header(name: &str) -> bool {
    if !name.starts_with("x-codex-") {
        return false;
    }
    PLAN_HEADERS.contains(&name)
        || name.starts_with("x-codex-credits-")
        || WINDOW_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The quota headers of an upstream response head.
pub fn telemetry_headers(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| is_quota_header(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// The same headers, rebuilt from a `codex.rate_limits` event. The event names
/// its extra limit families by label where the headers name them by id, so only
/// the default family and the plan are reconstructed.
pub fn event_headers(payload: &Value) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = Vec::new();
    for window in ["primary", "secondary"] {
        let Some(values) = payload.pointer(&format!("/rate_limits/{window}")) else {
            continue;
        };
        for (field, suffix) in [
            ("used_percent", "used-percent"),
            ("window_minutes", "window-minutes"),
            ("reset_at", "reset-at"),
            ("reset_after_seconds", "reset-after-seconds"),
        ] {
            let Some(number) = values.get(field).and_then(Value::as_f64) else {
                continue;
            };
            push_header(
                &mut headers,
                &format!("x-codex-{window}-{suffix}"),
                &format_number(number),
            );
        }
    }
    if let Some(plan) = payload.get("plan_type").and_then(Value::as_str) {
        push_header(&mut headers, "x-codex-plan-type", plan);
    }
    headers
}

/// Keeps a snapshot as the newest one. An empty snapshot is ignored: a response
/// that carried no telemetry says nothing about the quota, so the previous
/// answer stands.
pub fn record(headers: Vec<(HeaderName, HeaderValue)>) {
    if headers.is_empty() {
        return;
    }
    if let Ok(mut snapshot) = SNAPSHOT.write() {
        *snapshot = headers;
    }
}

/// Adds the newest snapshot to an outgoing response, leaving any header the
/// response already set alone.
pub fn stamp(target: &mut HeaderMap) {
    let Ok(snapshot) = SNAPSHOT.read() else {
        return;
    };
    for (name, value) in snapshot.iter() {
        if !target.contains_key(name) {
            target.insert(name.clone(), value.clone());
        }
    }
}

/// Stamps an outgoing Codex response, whatever shape it took.
pub fn stamp_response(mut response: Response) -> Response {
    stamp(response.headers_mut());
    response
}

/// Codex sends whole numbers as integers; JSON parsing turns them into floats.
fn format_number(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn push_header(headers: &mut Vec<(HeaderName, HeaderValue)>, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.push((name, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn value_of(headers: &[(HeaderName, HeaderValue)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| key.as_str() == name)
            .map(|(_, value)| value.to_str().unwrap().to_string())
    }

    // A ChatGPT Pro subscription reports its weekly limit as the primary window,
    // so the window's declared length matters to the client and travels with it.
    #[test]
    fn keeps_the_quota_headers_of_a_response_head() {
        let captured = telemetry_headers(&header_map(&[
            ("x-codex-primary-used-percent", "1"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", "1788765936"),
            ("x-codex-plan-type", "pro"),
        ]));

        assert_eq!(
            value_of(&captured, "x-codex-primary-used-percent").as_deref(),
            Some("1")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-window-minutes").as_deref(),
            Some("10080")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-reset-at").as_deref(),
            Some("1788765936")
        );
        assert_eq!(
            value_of(&captured, "x-codex-plan-type").as_deref(),
            Some("pro")
        );
    }

    // `x-codex-turn-state` is continuation state, and a large opaque blob at
    // that. Forwarding the whole prefix would put it on every response.
    #[test]
    fn drops_headers_that_are_not_quota() {
        let captured = telemetry_headers(&header_map(&[
            ("x-codex-primary-used-percent", "1"),
            ("x-codex-turn-state", "gAAAAAB-opaque"),
            ("content-type", "text/event-stream"),
        ]));

        assert_eq!(captured.len(), 1);
        assert_eq!(
            value_of(&captured, "x-codex-primary-used-percent").as_deref(),
            Some("1")
        );
    }

    // The WebSocket transport has no response headers at all, so the same
    // numbers have to come out of the event instead.
    #[test]
    fn rebuilds_the_headers_from_a_rate_limits_event() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "plan_type": "pro",
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {
                    "used_percent": 1.0,
                    "window_minutes": 10080,
                    "reset_at": 1788765936_i64,
                    "reset_after_seconds": 603581_i64
                },
                "secondary": null
            }
        });

        let captured = event_headers(&payload);

        assert_eq!(
            value_of(&captured, "x-codex-primary-used-percent").as_deref(),
            Some("1")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-window-minutes").as_deref(),
            Some("10080")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-reset-at").as_deref(),
            Some("1788765936")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-reset-after-seconds").as_deref(),
            Some("603581")
        );
        assert_eq!(
            value_of(&captured, "x-codex-plan-type").as_deref(),
            Some("pro")
        );
        assert_eq!(value_of(&captured, "x-codex-secondary-used-percent"), None);
    }

    // A window the plan does not meter must stay absent rather than arrive as
    // zero, which a client would read as a bucket that is empty and usable.
    #[test]
    fn omits_a_window_the_event_reports_as_absent() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": { "primary": null, "secondary": null }
        });

        assert!(event_headers(&payload).is_empty());
    }

    // The snapshot is one process-wide value, so tests that touch it record the
    // same one: they run in parallel, and each `record` replaces the last.
    fn record_sample() {
        record(vec![
            (
                HeaderName::from_static("x-codex-primary-used-percent"),
                HeaderValue::from_static("42"),
            ),
            (
                HeaderName::from_static("x-codex-primary-window-minutes"),
                HeaderValue::from_static("10080"),
            ),
        ]);
    }

    #[test]
    fn stamps_the_newest_snapshot_onto_a_response() {
        record_sample();

        let mut response = HeaderMap::new();
        stamp(&mut response);

        assert_eq!(
            response
                .get("x-codex-primary-used-percent")
                .and_then(|value| value.to_str().ok()),
            Some("42")
        );
    }

    // A streaming response is stamped on its head, before any of the body is
    // written, so a client reading headers never has to wait for the stream.
    #[test]
    fn stamps_a_streaming_response_before_its_body() {
        record_sample();

        let response = stamp_response(
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from("event: message_start\n"))
                .unwrap(),
        );

        assert_eq!(
            response
                .headers()
                .get("x-codex-primary-window-minutes")
                .and_then(|value| value.to_str().ok()),
            Some("10080")
        );
    }
}
