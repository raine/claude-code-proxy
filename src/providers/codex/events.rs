use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexFailureKind {
    RateLimit,
    Overloaded,
    Transient,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexStreamEventKind {
    Control,
    Structural,
    Semantic,
    TerminalSuccess,
    TerminalFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexEventFailure {
    pub kind: CodexFailureKind,
    pub explicit_status: Option<u16>,
    pub status: u16,
    pub message: String,
    pub retry_after: Option<String>,
}

impl CodexEventFailure {
    pub fn retryable(&self) -> bool {
        !matches!(self.kind, CodexFailureKind::Permanent)
    }

    pub fn client_status(&self) -> u16 {
        self.status
    }

    pub fn client_error_type(&self) -> &'static str {
        match self.client_status() {
            400 | 422 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            529 => "overloaded_error",
            _ => "api_error",
        }
    }
}

pub(crate) fn classify_stream_event(payload: &Value) -> CodexStreamEventKind {
    if classify_event_failure(payload).is_some() {
        return CodexStreamEventKind::TerminalFailure;
    }
    match payload.get("type").and_then(Value::as_str) {
        Some("response.completed" | "response.done") => CodexStreamEventKind::TerminalSuccess,
        Some(
            "keepalive"
            | "response.created"
            | "response.in_progress"
            | "codex.rate_limits"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed",
        ) => CodexStreamEventKind::Control,
        Some(
            "response.reasoning_summary_text.delta"
            | "response.output_text.delta"
            | "response.function_call_arguments.delta",
        ) if payload
            .get("delta")
            .and_then(Value::as_str)
            .is_some_and(|delta| !delta.is_empty()) =>
        {
            CodexStreamEventKind::Semantic
        }
        Some("response.output_item.done")
            if matches!(
                payload.pointer("/item/type").and_then(Value::as_str),
                Some("function_call" | "web_search_call")
            ) =>
        {
            CodexStreamEventKind::Semantic
        }
        _ => CodexStreamEventKind::Structural,
    }
}

pub(crate) fn event_is_terminal(payload: &Value) -> bool {
    matches!(
        classify_stream_event(payload),
        CodexStreamEventKind::TerminalSuccess | CodexStreamEventKind::TerminalFailure
    )
}

pub(crate) fn event_is_success_terminal(payload: &Value) -> bool {
    classify_stream_event(payload) == CodexStreamEventKind::TerminalSuccess
}

pub(crate) fn event_error(payload: &Value) -> Option<&Value> {
    payload
        .get("error")
        .filter(|error| !error.is_null())
        .or_else(|| {
            payload
                .pointer("/response/error")
                .filter(|error| !error.is_null())
        })
}

pub(crate) fn response_is_incomplete_terminal(payload: &Value) -> bool {
    let event_type = payload.get("type").and_then(Value::as_str);
    let incomplete_details = payload.pointer("/response/incomplete_details");
    matches!(
        event_type,
        Some("response.completed" | "response.incomplete" | "response.done")
    ) && (event_type == Some("response.incomplete")
        || payload.pointer("/response/status").and_then(Value::as_str) == Some("incomplete")
        || incomplete_details.is_some_and(|details| !details.is_null()))
}

pub(crate) fn is_standard_max_output_tokens_incomplete(payload: &Value) -> bool {
    let status = payload.pointer("/response/status");
    payload.get("type").and_then(Value::as_str) == Some("response.incomplete")
        && (status.is_none() || status.and_then(Value::as_str) == Some("incomplete"))
        && payload
            .pointer("/response/incomplete_details/reason")
            .and_then(Value::as_str)
            == Some("max_output_tokens")
        && event_error(payload).is_none()
}

pub(crate) fn classify_event_failure(payload: &Value) -> Option<CodexEventFailure> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    let response_status = payload.pointer("/response/status").and_then(Value::as_str);
    if event_type == "codex.rate_limits" {
        return None;
    }
    if response_is_incomplete_terminal(payload) {
        let reason = payload
            .pointer("/response/incomplete_details/reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Some(CodexEventFailure {
            kind: CodexFailureKind::Transient,
            explicit_status: None,
            status: 503,
            message: format!("Incomplete response returned, reason: {reason}"),
            retry_after: None,
        });
    }
    let error = event_error(payload);
    if !matches!(event_type, "response.failed" | "response.error" | "error")
        && response_status != Some("failed")
        && error.is_none()
    {
        return None;
    }

    let explicit_status = numeric_status(payload)
        .or_else(|| {
            error
                .and_then(|value| value.get("status"))
                .and_then(Value::as_u64)
        })
        .and_then(|status| u16::try_from(status).ok());
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str);
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    let raw_message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str);
    let message = match (code, raw_message) {
        (Some("cyber_policy"), None | Some("")) => {
            "This request has been flagged for possible cybersecurity risk.".to_string()
        }
        (Some("cyber_policy"), Some(message)) if message.trim().is_empty() => {
            "This request has been flagged for possible cybersecurity risk.".to_string()
        }
        (Some("invalid_prompt" | "bio_policy"), None) => "Invalid request.".to_string(),
        (_, Some(message)) => message.to_string(),
        _ => "Upstream error".to_string(),
    };
    let lower = message.to_ascii_lowercase();
    let context_window = code == Some("context_length_exceeded")
        || lower.contains("context window")
        || lower.contains("context length exceeded");

    let kind = if context_window
        || matches!(
            code,
            Some(
                "context_length_exceeded"
                    | "insufficient_quota"
                    | "usage_not_included"
                    | "cyber_policy"
                    | "invalid_prompt"
                    | "bio_policy"
            )
        ) {
        CodexFailureKind::Permanent
    } else if matches!(code, Some("server_is_overloaded" | "slow_down")) {
        CodexFailureKind::Overloaded
    } else if explicit_status == Some(429) || lower.contains("rate limit") {
        CodexFailureKind::RateLimit
    } else if explicit_status == Some(529)
        || code == Some("overloaded_error")
        || error_type == Some("overloaded_error")
        || lower.contains("overloaded")
    {
        CodexFailureKind::Overloaded
    } else if explicit_status.is_some_and(|status| matches!(status, 500 | 502 | 503 | 504))
        || matches!(
            code,
            Some("server_error" | "internal_server_error" | "internal_error")
        )
        || matches!(
            error_type,
            Some("server_error" | "internal_server_error" | "internal_error")
        )
        || retryable_message(&lower)
    {
        CodexFailureKind::Transient
    } else if explicit_status.is_some() || matches!(event_type, "response.error" | "error") {
        CodexFailureKind::Permanent
    } else {
        // Native Codex treats an unclassified response.failed event as
        // retryable instead of silently converting it into a successful turn.
        CodexFailureKind::Transient
    };
    let status = if context_window {
        413
    } else {
        explicit_status.unwrap_or(match code {
            Some("cyber_policy" | "invalid_prompt" | "bio_policy") => 400,
            Some("insufficient_quota") => 429,
            Some("usage_not_included") => 403,
            _ => match kind {
                CodexFailureKind::RateLimit => 429,
                CodexFailureKind::Overloaded => 529,
                CodexFailureKind::Transient => 503,
                CodexFailureKind::Permanent => 500,
            },
        })
    };
    let retry_after = error
        .and_then(|value| value.get("retry_after"))
        .and_then(scalar_string_value)
        .or_else(|| {
            error
                .and_then(|value| value.get("retry_after_seconds"))
                .and_then(scalar_string_value)
        })
        .or_else(|| scalar_string(payload.get("retry_after_seconds")))
        .or_else(|| scalar_string(payload.pointer("/headers/retry-after")))
        .or_else(|| scalar_string(payload.pointer("/headers/Retry-After")));

    Some(CodexEventFailure {
        kind,
        explicit_status,
        status,
        message,
        retry_after,
    })
}

/// Which known upstream quota window ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexLimitWindow {
    FiveHour,
    SevenDay,
}

impl CodexLimitWindow {
    pub(crate) fn claim(self) -> &'static str {
        match self {
            CodexLimitWindow::FiveHour => "five_hour",
            CodexLimitWindow::SevenDay => "seven_day",
        }
    }
}

/// Quota exhaustion reported by Codex, together with the reset clock upstream
/// sends alongside it. Unlike a transient rate limit this does not clear on a
/// backoff, so the reset time is the only useful thing to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageLimit {
    pub message: String,
    pub resets_at: Option<u64>,
    pub window: Option<CodexLimitWindow>,
}

/// Recognise the `usage_limit_reached` error Codex emits when a subscription
/// window is spent. Upstream puts the clock both in the error body
/// (`resets_at`, `resets_in_seconds`) and in `X-Codex-*` headers mirrored into
/// the event payload.
pub(crate) fn usage_limit_from_event(payload: &Value) -> Option<CodexUsageLimit> {
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("response.failed" | "response.error" | "error")
    ) {
        return None;
    }
    usage_limit_from_payload(payload)
}

/// Read a quota exhaustion event while consulting the current HTTP response
/// headers for metadata that is absent from the event payload.
pub(crate) fn usage_limit_from_event_with_headers(
    payload: &Value,
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("response.failed" | "response.error" | "error")
    ) {
        return None;
    }
    usage_limit_from_payload_with_headers(payload, headers)
}

/// Read quota exhaustion from either a JSON error response or an SSE event
/// body. HTTP response headers are included because Codex does not always
/// mirror its quota clocks into the error payload.
pub(crate) fn usage_limit_from_response(
    body: &[u8],
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    let direct = serde_json::from_slice::<Value>(body).ok().into_iter();
    let events = crate::anthropic::sse::parse_sse_events(body)
        .into_iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok());
    direct.chain(events).find_map(|payload| {
        // Parse JSON before checking the event shape so escaped strings and all
        // other valid JSON forms retain serde_json's normal semantics. Header
        // lookup is only needed for an actual quota event.
        is_usage_limit_payload(&payload)
            .then(|| usage_limit_from_payload_with_headers(&payload, headers))
            .flatten()
    })
}

fn is_usage_limit_payload(payload: &Value) -> bool {
    event_error(payload)
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        == Some("usage_limit_reached")
}

fn usage_limit_from_payload(payload: &Value) -> Option<CodexUsageLimit> {
    usage_limit_from_payload_with_headers(payload, &[])
}

fn usage_limit_from_payload_with_headers(
    payload: &Value,
    response_headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    let error = event_error(payload)?;
    if error.get("type").and_then(Value::as_str) != Some("usage_limit_reached") {
        return None;
    }

    let resets_at = numeric_value(error.get("resets_at"));
    let resets_in_seconds = numeric_value(error.get("resets_in_seconds"));
    let limiting_prefix =
        limiting_window_prefix(payload, response_headers, resets_in_seconds, resets_at);
    let resets_at = resets_at.or_else(|| {
        let prefix = limiting_prefix?;
        header_number(
            payload,
            response_headers,
            &format!("X-Codex-{prefix}-Reset-At"),
        )
    });
    let window = limiting_prefix.and_then(|prefix| {
        match header_number(
            payload,
            response_headers,
            &format!("X-Codex-{prefix}-Window-Minutes"),
        ) {
            Some(300) => Some(CodexLimitWindow::FiveHour),
            Some(10_080) => Some(CodexLimitWindow::SevenDay),
            _ => None,
        }
    });

    Some(CodexUsageLimit {
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Usage limit reached")
            .to_string(),
        resets_at,
        window,
    })
}

/// Codex sends the clock for both windows on every limit error, so the one that
/// actually ran out is the one whose countdown or reset epoch matches the
/// error's own clock.
fn limiting_window_prefix(
    payload: &Value,
    response_headers: &[(String, String)],
    resets_in_seconds: Option<u64>,
    resets_at: Option<u64>,
) -> Option<&'static str> {
    let by_countdown = resets_in_seconds.and_then(|actual| {
        closest_window(
            actual,
            header_number(
                payload,
                response_headers,
                "X-Codex-Primary-Reset-After-Seconds",
            ),
            header_number(
                payload,
                response_headers,
                "X-Codex-Secondary-Reset-After-Seconds",
            ),
        )
    });
    let by_epoch = resets_at.and_then(|actual| {
        closest_window(
            actual,
            header_number(payload, response_headers, "X-Codex-Primary-Reset-At"),
            header_number(payload, response_headers, "X-Codex-Secondary-Reset-At"),
        )
    });
    by_countdown.or(by_epoch).or_else(|| {
        match (
            window_headers_present(payload, response_headers, "Primary"),
            window_headers_present(payload, response_headers, "Secondary"),
        ) {
            (true, false) => Some("Primary"),
            (false, true) => Some("Secondary"),
            _ => None,
        }
    })
}

fn closest_window(
    actual: u64,
    primary: Option<u64>,
    secondary: Option<u64>,
) -> Option<&'static str> {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => {
            match (actual.abs_diff(primary), actual.abs_diff(secondary)) {
                (primary_distance, secondary_distance) if primary_distance < secondary_distance => {
                    Some("Primary")
                }
                (primary_distance, secondary_distance) if secondary_distance < primary_distance => {
                    Some("Secondary")
                }
                _ => None,
            }
        }
        (Some(_), None) => Some("Primary"),
        (None, Some(_)) => Some("Secondary"),
        (None, None) => None,
    }
}

fn window_headers_present(
    payload: &Value,
    response_headers: &[(String, String)],
    prefix: &str,
) -> bool {
    ["Reset-After-Seconds", "Reset-At", "Window-Minutes"]
        .into_iter()
        .any(|suffix| {
            header_number(
                payload,
                response_headers,
                &format!("X-Codex-{prefix}-{suffix}"),
            )
            .is_some()
        })
}

fn header_number(
    payload: &Value,
    response_headers: &[(String, String)],
    name: &str,
) -> Option<u64> {
    payload
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| numeric_value(Some(value)))
        })
        .or_else(|| {
            response_headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.parse().ok())
        })
}

fn numeric_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

pub(crate) fn first_retryable_failure(body: &[u8]) -> Option<CodexEventFailure> {
    first_event_failure(body).filter(CodexEventFailure::retryable)
}

pub(crate) fn first_event_failure(body: &[u8]) -> Option<CodexEventFailure> {
    for event in crate::anthropic::sse::parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if let Some(failure) = classify_event_failure(&payload) {
            return Some(failure);
        }
    }
    None
}

pub(crate) fn numeric_status(payload: &Value) -> Option<u64> {
    payload
        .get("status")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("status_code").and_then(Value::as_u64))
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    value.and_then(scalar_string_value)
}

fn scalar_string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn retryable_message(message: &str) -> bool {
    [
        "server error",
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "temporarily unavailable",
        "you can retry your request",
        "socket connection was closed unexpectedly",
        "connection closed unexpectedly",
        "operation timed out",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "econnreset",
        "epipe",
        "etimedout",
        "und_err_socket",
        "fetch failed",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape recorded from a live turn that exhausted the five hour window: the
    /// clock arrives both in the error body and in the mirrored `X-Codex-*`
    /// headers, and the primary window is the one that ran out.
    fn spent_five_hour_window() -> Value {
        serde_json::json!({
            "type": "error",
            "status_code": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "plus",
                "resets_at": 1788879437u64,
                "resets_in_seconds": 9568u64
            },
            "headers": {
                "X-Codex-Primary-Used-Percent": "100",
                "X-Codex-Primary-Window-Minutes": "300",
                "X-Codex-Primary-Reset-After-Seconds": "9569",
                "X-Codex-Primary-Reset-At": "1788879438",
                "X-Codex-Secondary-Used-Percent": "16",
                "X-Codex-Secondary-Window-Minutes": "10080",
                "X-Codex-Secondary-Reset-After-Seconds": "596369",
                "X-Codex-Secondary-Reset-At": "1789466238"
            }
        })
    }

    #[test]
    fn reads_usage_limit_reset_clock() {
        let limit = usage_limit_from_event(&spent_five_hour_window()).expect("usage limit");
        assert_eq!(limit.message, "The usage limit has been reached");
        assert_eq!(limit.resets_at, Some(1788879437));
        assert_eq!(limit.window, Some(CodexLimitWindow::FiveHour));
        assert_eq!(limit.window.unwrap().claim(), "five_hour");
    }

    #[test]
    fn attributes_the_window_whose_clock_matches() {
        let mut payload = spent_five_hour_window();
        // Same error, but it is the weekly window that ran out.
        payload["error"]["resets_in_seconds"] = serde_json::json!(596_368u64);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn falls_back_to_header_clock_when_body_omits_it() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_at");
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1788879438));
    }

    #[test]
    fn attributes_window_by_body_epoch_when_countdown_is_missing() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_in_seconds");
        payload["error"]["resets_at"] = serde_json::json!(1789466238u64);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn preserves_ambiguous_body_epoch_without_claim() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_in_seconds");
        payload["error"]["resets_at"] = serde_json::json!(150u64);
        payload["headers"]["X-Codex-Primary-Reset-At"] = serde_json::json!(100u64);
        payload["headers"]["X-Codex-Secondary-Reset-At"] = serde_json::json!(200u64);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(150));
        assert_eq!(limit.window, None);
    }

    #[test]
    fn omits_header_reset_and_claim_without_identifying_body_clock() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_at");
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_in_seconds");
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, None);
        assert_eq!(limit.window, None);
    }

    #[test]
    fn ignores_errors_that_are_not_usage_limits() {
        assert!(
            usage_limit_from_event(&serde_json::json!({
                "type": "error",
                "status_code": 429,
                "error": {
                    "type": "rate_limit_exceeded",
                    "message": "slow down",
                    "resets_at": 1788879437u64,
                    "resets_in_seconds": 60
                }
            }))
            .is_none()
        );
        assert!(
            usage_limit_from_event(&serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "hello"
            }))
            .is_none()
        );
    }

    #[test]
    fn omits_claim_for_unknown_window_duration() {
        let mut payload = spent_five_hour_window();
        payload["headers"]["X-Codex-Primary-Window-Minutes"] = serde_json::json!(60);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1788879437));
        assert_eq!(limit.window, None);
    }

    #[test]
    fn reads_usage_limit_from_http_body_and_headers() {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "weekly limit reached",
                "resets_in_seconds": 90
            }
        }))
        .unwrap();
        let headers = vec![
            (
                "x-codex-secondary-reset-after-seconds".to_string(),
                "90".to_string(),
            ),
            (
                "x-codex-secondary-reset-at".to_string(),
                "1789466238".to_string(),
            ),
            (
                "x-codex-secondary-window-minutes".to_string(),
                "10080".to_string(),
            ),
        ];
        let limit = usage_limit_from_response(&body, &headers).expect("usage limit");
        assert_eq!(limit.message, "weekly limit reached");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn parses_header_only_sse_quota_metadata_without_byte_matching() {
        let body = br#"data: {"type":"error","error":{"type":"usage_limit_reached","message":"weekly \"limit\" reached","resets_in_seconds":90}}

"#;
        let headers = vec![
            (
                "X-Codex-Secondary-Reset-After-Seconds".to_string(),
                "90".to_string(),
            ),
            (
                "X-Codex-Secondary-Reset-At".to_string(),
                "1789466238".to_string(),
            ),
            (
                "X-Codex-Secondary-Window-Minutes".to_string(),
                "10080".to_string(),
            ),
        ];

        let limit = usage_limit_from_response(body, &headers).expect("usage limit");
        assert_eq!(limit.message, "weekly \"limit\" reached");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn ignores_quota_text_in_successful_buffered_events() {
        let body = br#"data: {"type":"response.output_text.delta","delta":"the text says \"usage_limit_reached\" but is not an error"}

data: {"type":"response.completed","response":{"status":"completed"}}

"#;
        let headers = vec![(
            "X-Codex-Secondary-Window-Minutes".to_string(),
            "10080".to_string(),
        )];

        assert!(usage_limit_from_response(body, &headers).is_none());
    }

    #[test]
    fn classifies_retryable_failure_kinds() {
        let overload = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"type": "overloaded_error", "message": "busy"}}
        }))
        .unwrap();
        assert_eq!(overload.status, 529);
        assert!(overload.retryable());
    }

    #[test]
    fn rate_limit_snapshots_are_always_telemetry() {
        assert!(
            classify_event_failure(&serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": true},
                "credits": {"has_credits": false, "unlimited": false}
            }))
            .is_none()
        );
    }

    #[test]
    fn ignores_informational_and_permanent_events() {
        assert!(
            classify_event_failure(&serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": false}
            }))
            .is_none()
        );
        let failure = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"status": 400, "message": "bad request"}
        }))
        .unwrap();
        assert!(!failure.retryable());
    }

    #[test]
    fn classifies_progress_and_terminal_semantics() {
        assert_eq!(
            classify_stream_event(&serde_json::json!({"type":"response.created"})),
            CodexStreamEventKind::Control
        );
        assert_eq!(
            classify_stream_event(&serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"function_call"}
            })),
            CodexStreamEventKind::Structural
        );
        assert_eq!(
            classify_stream_event(&serde_json::json!({
                "type":"response.function_call_arguments.delta",
                "delta":"{}"
            })),
            CodexStreamEventKind::Semantic
        );
        assert_eq!(
            classify_stream_event(&serde_json::json!({"type":"response.incomplete"})),
            CodexStreamEventKind::TerminalFailure
        );
        assert_eq!(
            classify_stream_event(&serde_json::json!({
                "type":"response.completed",
                "error": null,
                "response": {
                    "status":"failed",
                    "error":{"status":400,"code":"invalid_prompt","message":"rejected"}
                }
            })),
            CodexStreamEventKind::TerminalFailure
        );
    }

    #[test]
    fn nested_response_error_survives_null_top_level_error() {
        let failure = classify_event_failure(&serde_json::json!({
            "type":"response.completed",
            "error": null,
            "response": {
                "status":"failed",
                "error":{"status":400,"code":"invalid_prompt","message":"nested rejection"}
            }
        }))
        .unwrap();
        assert_eq!(failure.client_status(), 400);
        assert_eq!(failure.client_error_type(), "invalid_request_error");
        assert_eq!(failure.message, "nested rejection");
        assert!(!failure.retryable());
    }

    #[test]
    fn completed_event_with_nested_error_is_not_success() {
        let payload = serde_json::json!({
            "type":"response.completed",
            "error": null,
            "response": {
                "status":"completed",
                "error":{"status":502,"code":"server_error","message":"late failure"}
            }
        });
        let failure = classify_event_failure(&payload).unwrap();
        assert_eq!(failure.client_status(), 502);
        assert!(failure.retryable());
        assert_eq!(
            classify_stream_event(&payload),
            CodexStreamEventKind::TerminalFailure
        );
    }

    #[test]
    fn completed_event_with_nested_null_error_is_success() {
        let payload = serde_json::json!({
            "type":"response.completed",
            "error": null,
            "response": {"status":"completed", "error":null}
        });
        assert!(classify_event_failure(&payload).is_none());
        assert_eq!(
            classify_stream_event(&payload),
            CodexStreamEventKind::TerminalSuccess
        );
    }

    #[test]
    fn native_fatal_codes_are_not_retried_without_numeric_status() {
        for (code, status, error_type) in [
            ("context_length_exceeded", 413, "request_too_large"),
            ("cyber_policy", 400, "invalid_request_error"),
            ("invalid_prompt", 400, "invalid_request_error"),
            ("bio_policy", 400, "invalid_request_error"),
            ("insufficient_quota", 429, "rate_limit_error"),
            ("usage_not_included", 403, "permission_error"),
        ] {
            let failure = classify_event_failure(&serde_json::json!({
                "type":"response.failed",
                "response":{"status":"failed","error":{"code":code,"message":"rejected"}}
            }))
            .unwrap();
            assert!(!failure.retryable(), "{code}");
            assert_eq!(failure.client_status(), status, "{code}");
            assert_eq!(failure.client_error_type(), error_type, "{code}");
        }
    }

    #[test]
    fn native_overload_codes_remain_retryable_without_numeric_status() {
        for code in ["server_is_overloaded", "slow_down"] {
            let failure = classify_event_failure(&serde_json::json!({
                "type":"response.failed",
                "response":{"status":"failed","error":{"code":code,"message":"busy"}}
            }))
            .unwrap();
            assert!(failure.retryable(), "{code}");
            assert_eq!(failure.client_status(), 529, "{code}");
            assert_eq!(failure.client_error_type(), "overloaded_error", "{code}");
        }
    }

    #[test]
    fn context_window_message_overrides_generic_upstream_400() {
        let failure = classify_event_failure(&serde_json::json!({
            "type":"error",
            "status":400,
            "error":{"type":"invalid_request_error","message":"input exceeds context window"}
        }))
        .unwrap();
        assert!(!failure.retryable());
        assert_eq!(failure.explicit_status, Some(400));
        assert_eq!(failure.client_status(), 413);
        assert_eq!(failure.client_error_type(), "request_too_large");
    }

    #[test]
    fn incomplete_policy_only_accepts_consistent_max_output_terminal() {
        let allowed = serde_json::json!({
            "type":"response.incomplete",
            "response": {
                "status":"incomplete",
                "error":null,
                "incomplete_details":{"reason":"max_output_tokens"}
            }
        });
        assert!(response_is_incomplete_terminal(&allowed));
        assert!(is_standard_max_output_tokens_incomplete(&allowed));
        let allowed_without_status = serde_json::json!({
            "type":"response.incomplete",
            "response": {
                "incomplete_details":{"reason":"max_output_tokens"}
            }
        });
        assert!(is_standard_max_output_tokens_incomplete(
            &allowed_without_status
        ));

        for rejected in [
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}
            }),
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":"incomplete","incomplete_details":{}}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{"status":"completed","incomplete_details":{"reason":"max_output_tokens"}}
            }),
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":null,"incomplete_details":{"reason":"max_output_tokens"}}
            }),
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":123,"incomplete_details":{"reason":"max_output_tokens"}}
            }),
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":"completed","incomplete_details":{"reason":"max_output_tokens"}}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{"status":"completed","incomplete_details":{}}
            }),
        ] {
            assert!(response_is_incomplete_terminal(&rejected));
            assert!(!is_standard_max_output_tokens_incomplete(&rejected));
            assert!(classify_event_failure(&rejected).is_some());
        }
    }
}
