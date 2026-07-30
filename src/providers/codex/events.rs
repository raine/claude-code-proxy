use serde_json::Value;

const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexTerminalKind {
    Completed,
    Done,
    Incomplete,
    Failed,
    ResponseError,
    Cancelled,
    Error,
}

impl CodexTerminalKind {
    pub(crate) fn from_payload(payload: &Value) -> Option<Self> {
        match payload.get("type").and_then(Value::as_str) {
            Some("response.completed") => Some(Self::Completed),
            Some("response.done") => Some(Self::Done),
            Some("response.incomplete") => Some(Self::Incomplete),
            Some("response.failed") => Some(Self::Failed),
            Some("response.error") => Some(Self::ResponseError),
            Some("response.cancelled") => Some(Self::Cancelled),
            Some("error") => Some(Self::Error),
            _ => None,
        }
    }

    pub(crate) fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::ResponseError | Self::Cancelled | Self::Error
        )
    }

    pub(crate) fn is_reusable(self) -> bool {
        matches!(self, Self::Completed | Self::Done)
    }
}

/// Validate an optional terminal response snapshot status against the event that carries it.
///
/// Codex has emitted both full Responses events and the Lite `response.done` success alias.
/// Omitted and null statuses remain compatible with both shapes, but a provided status must never
/// contradict the terminal event and turn an upstream failure into a successful Anthropic
/// response. This is deliberately the Codex Responses contract, not Realtime's multi-outcome
/// `response.done` event.
pub(crate) fn validate_terminal_snapshot_status(payload: &Value) -> Result<(), String> {
    let Some(kind) = CodexTerminalKind::from_payload(payload) else {
        return Ok(());
    };
    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    let expected = match kind {
        CodexTerminalKind::Completed | CodexTerminalKind::Done => "completed",
        CodexTerminalKind::Incomplete => "incomplete",
        CodexTerminalKind::Failed => "failed",
        // `error` is an event name, not a valid Responses snapshot status. A
        // response snapshot carried by either error event describes a failed
        // response.
        CodexTerminalKind::ResponseError | CodexTerminalKind::Error => "failed",
        CodexTerminalKind::Cancelled => "cancelled",
    };
    match payload.pointer("/response/status") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(status)) if status == expected => Ok(()),
        Some(Value::String(status)) => Err(format!(
            "{event_type} response.status must be {expected:?} when provided, got {status:?}"
        )),
        Some(_) => Err(format!(
            "{event_type} response.status must be {expected:?} or null when provided"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexFailureKind {
    ContextOverflow,
    RateLimit,
    Overloaded,
    Transient,
    Permanent,
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
        !matches!(
            self.kind,
            CodexFailureKind::ContextOverflow | CodexFailureKind::Permanent
        )
    }
}

pub(crate) fn classify_event_failure(payload: &Value) -> Option<CodexEventFailure> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    if event_type == "codex.rate_limits" {
        if payload
            .pointer("/rate_limits/limit_reached")
            .and_then(Value::as_bool)
            != Some(true)
        {
            return None;
        }
        return Some(CodexEventFailure {
            kind: CodexFailureKind::RateLimit,
            explicit_status: Some(429),
            status: 429,
            message: "rate limit reached".to_string(),
            retry_after: scalar_string(payload.pointer("/rate_limits/primary/reset_after_seconds")),
        });
    }
    if !CodexTerminalKind::from_payload(payload).is_some_and(CodexTerminalKind::is_failure) {
        return None;
    }
    if validate_terminal_snapshot_status(payload).is_err() {
        // A contradictory terminal snapshot is protocol corruption. It must be
        // surfaced by the reducers, never promoted into a replayable service
        // failure by the retry classifier.
        return None;
    }

    let error = payload
        .get("error")
        .or_else(|| payload.pointer("/response/error"));
    let explicit_status = structured_error_status(payload);
    let raw_message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            error
                .and_then(|value| value.get("detail"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Upstream error");
    let message = sanitized_error_message(payload);
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str);
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    let lower = raw_message.to_ascii_lowercase();

    let kind = if is_context_overflow_error_with_status(payload, explicit_status) {
        CodexFailureKind::ContextOverflow
    } else if explicit_status == Some(429)
        || code.is_some_and(is_rate_limit_error_code)
        || error_type.is_some_and(is_rate_limit_error_code)
        || lower.contains("rate limit")
    {
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
    } else {
        CodexFailureKind::Permanent
    };
    let status = if kind == CodexFailureKind::ContextOverflow {
        413
    } else {
        explicit_status.unwrap_or(match kind {
            CodexFailureKind::ContextOverflow => unreachable!("handled above"),
            CodexFailureKind::RateLimit => 429,
            CodexFailureKind::Overloaded => 529,
            CodexFailureKind::Transient => 503,
            CodexFailureKind::Permanent => 500,
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

const CONTEXT_OVERFLOW_CODES: &[&str] = &[
    "context_length_exceeded",
    "context_window_exceeded",
    "max_context_length_exceeded",
    "prompt_too_long",
    "input_too_large",
];

fn error_object(payload: &Value) -> Option<&Value> {
    payload
        .get("error")
        .or_else(|| payload.pointer("/response/error"))
}

fn structured_error_str<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    error_object(payload)
        .filter(|error| error.is_object())
        .unwrap_or(payload)
        .get(field)
        .and_then(Value::as_str)
}

fn is_context_overflow_code(value: &str) -> bool {
    CONTEXT_OVERFLOW_CODES
        .iter()
        .any(|allowed| value.eq_ignore_ascii_case(allowed))
}

fn is_rate_limit_error_code(value: &str) -> bool {
    ["rate_limit", "rate_limit_error", "rate_limit_exceeded"]
        .iter()
        .any(|known| value.trim().eq_ignore_ascii_case(known))
}

fn is_known_non_context_error_code(value: &str) -> bool {
    [
        "rate_limit",
        "rate_limit_error",
        "rate_limit_exceeded",
        "overloaded_error",
        "authentication_error",
        "invalid_api_key",
        "unauthorized",
        "permission_denied",
        "forbidden",
        "safety_policy_violation",
        "content_policy_violation",
        "policy_violation",
        "server_error",
        "internal_server_error",
        "internal_error",
    ]
    .iter()
    .any(|known| value.trim().eq_ignore_ascii_case(known))
}

fn structured_error_status(payload: &Value) -> Option<u16> {
    numeric_status(payload)
        .or_else(|| {
            error_object(payload)
                .and_then(|value| value.get("status"))
                .and_then(Value::as_u64)
        })
        .and_then(|status| u16::try_from(status).ok())
}

pub(crate) fn is_authoritative_non_context_status(status: u16) -> bool {
    (400..600).contains(&status) && !matches!(status, 400 | 413 | 422)
}

pub(crate) fn is_context_overflow_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "exceeds the context window",
        "exceeds context window",
        "exceeded the context window",
        "exceeded context window",
        "context window exceeded",
        "maximum context length",
        "max context length",
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "(code: context_length_exceeded",
        "(code: context_window_exceeded",
        "(code: max_context_length_exceeded",
        "(code: prompt_too_long",
        "(code: input_too_large",
        "(type: context_length_exceeded",
        "(type: context_window_exceeded",
        "(type: max_context_length_exceeded",
        "(type: prompt_too_long",
        "(type: input_too_large",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

/// Classify only an allowlisted structured code/type, then use conservative
/// English message patterns as a compatibility fallback.
pub(crate) fn is_context_overflow_error(payload: &Value) -> bool {
    is_context_overflow_error_with_status(payload, structured_error_status(payload))
}

/// Structured context codes remain authoritative when an intermediary wraps
/// them in a generic status. Free-form prose is weaker evidence and must not
/// override an explicit authentication, rate-limit, overload, or server status.
pub(crate) fn is_context_overflow_error_with_status(
    payload: &Value,
    outer_status: Option<u16>,
) -> bool {
    let code = structured_error_str(payload, "code");
    if code.is_some_and(is_context_overflow_code) {
        return true;
    }
    // Known provider categories are more authoritative than free-form prose.
    // In particular, rate-limit errors can mention a per-minute token budget;
    // treating those as a context overflow would suppress a safe retry and
    // incorrectly return 413 to Claude Code. Unknown and generic codes still
    // reach the conservative message fallback because private endpoint
    // vocabularies can drift.
    if code.is_some_and(is_known_non_context_error_code) {
        return false;
    }

    let error_type = structured_error_str(payload, "type");
    if error_type.is_some_and(is_context_overflow_code) {
        return true;
    }
    if error_type.is_some_and(is_known_non_context_error_code) {
        return false;
    }
    let payload_status = structured_error_status(payload);
    if payload_status == Some(413) {
        return true;
    }
    if payload_status.is_some_and(is_authoritative_non_context_status) {
        return false;
    }
    if outer_status == Some(413) {
        return true;
    }
    if outer_status.is_some_and(is_authoritative_non_context_status) {
        return false;
    }

    error_object(payload)
        .and_then(|error| {
            error.as_str().or_else(|| {
                error
                    .get("message")
                    .or_else(|| error.get("detail"))
                    .and_then(Value::as_str)
            })
        })
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .or_else(|| payload.get("detail").and_then(Value::as_str))
        .is_some_and(is_context_overflow_message)
}

/// Render a safe, bounded diagnostic while retaining structured fields that
/// remain useful after the raw provider body is discarded.
pub(crate) fn sanitized_error_message(payload: &Value) -> String {
    let error = error_object(payload);
    let raw_message = error
        .and_then(|value| {
            value.as_str().or_else(|| {
                value
                    .get("message")
                    .or_else(|| value.get("detail"))
                    .and_then(Value::as_str)
            })
        })
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .or_else(|| payload.get("detail").and_then(Value::as_str))
        .unwrap_or("Upstream error");
    let message = crate::providers::translate_shared::sanitize_external_error_detail(raw_message)
        .unwrap_or_else(|| "Upstream error".to_string());

    let mut descriptors = Vec::new();
    for field in ["code", "type", "param"] {
        if field == "type"
            && error.filter(|error| error.is_object()).is_none()
            && CodexTerminalKind::from_payload(payload).is_some()
        {
            // `type` names the SSE event here, not the provider's structured
            // error type. Do not expose it as a misleading error descriptor.
            continue;
        }
        let Some(value) = error
            .filter(|error| error.is_object())
            .unwrap_or(payload)
            .get(field)
            .and_then(|value| match value {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            })
        else {
            continue;
        };
        let Some(value) =
            crate::providers::translate_shared::sanitize_external_error_detail(&value)
        else {
            continue;
        };
        let descriptor = format!("{field}: {value}");
        if !message.contains(&descriptor) {
            descriptors.push(descriptor);
        }
    }
    if descriptors.is_empty() {
        message
    } else {
        let descriptors = descriptors.join(", ");
        let combined = format!("{message} ({descriptors})");
        if combined.len() <= crate::providers::translate_shared::MAX_EXTERNAL_ERROR_DETAIL_BYTES {
            combined
        } else {
            // Keep the actionable structured fields at the front when an
            // adversarially long message forces a second, whole-detail bound.
            crate::providers::translate_shared::sanitize_external_error_detail(&format!(
                "({descriptors}) {message}"
            ))
            .unwrap_or_else(|| "Upstream error".to_string())
        }
    }
}

pub(crate) fn first_retryable_failure(body: &[u8]) -> Option<CodexEventFailure> {
    let mut first_failure = None;
    let mut hosted_side_effect_started = false;
    // Malformed protocol bytes must never become the reason to replay a model
    // request. The reducer will surface the decode failure to the caller.
    for event in parse_codex_sse_events(body).ok()? {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if starts_hosted_side_effect(&payload) {
            hosted_side_effect_started = true;
            first_failure = None;
        }
        if CodexTerminalKind::from_payload(&payload).is_some_and(|kind| {
            matches!(
                kind,
                CodexTerminalKind::Completed
                    | CodexTerminalKind::Done
                    | CodexTerminalKind::Incomplete
            )
        }) {
            // A non-error response terminal is authoritative. Providers may append
            // quota telemetry after it; that tail must not turn either a completed
            // response or a valid truncated response into a whole-request replay.
            return None;
        }
        if let Some(failure) = classify_event_failure(&payload)
            && failure.retryable()
            && !hosted_side_effect_started
            && first_failure.is_none()
        {
            first_failure = Some(failure);
        }
    }
    (!hosted_side_effect_started)
        .then_some(first_failure)
        .flatten()
}

/// Once Codex exposes a hosted search lifecycle event, replaying the logical
/// request may repeat an external search even when no Anthropic bytes were
/// emitted yet. Keep this separate from downstream stream commitment.
pub(crate) fn starts_hosted_side_effect(payload: &Value) -> bool {
    match payload.get("type").and_then(Value::as_str) {
        Some(
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed",
        ) => true,
        Some("response.output_item.added" | "response.output_item.done") => {
            payload
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("web_search_call")
        }
        _ => false,
    }
}

/// Strict Codex SSE parsing with the protocol's one allowed stream-start BOM.
/// A later BOM could prefix `data:` and otherwise be treated as an unknown SSE
/// field, silently dropping a model event.
pub(crate) fn parse_codex_sse_events(
    body: &[u8],
) -> Result<Vec<crate::anthropic::sse::SseEvent>, String> {
    let body = body.strip_prefix(UTF8_BOM).unwrap_or(body);
    if body
        .windows(UTF8_BOM.len())
        .any(|window| window == UTF8_BOM)
    {
        return Err("Codex SSE response contained a UTF-8 BOM after stream start".to_string());
    }
    crate::anthropic::sse::try_parse_sse_events(body).map_err(|error| {
        format!(
            "Codex SSE response contained invalid UTF-8 at byte {}",
            error.valid_up_to()
        )
    })
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

    #[test]
    fn terminal_kinds_are_classified_consistently() {
        let cases = [
            (
                "response.completed",
                CodexTerminalKind::Completed,
                true,
                false,
            ),
            ("response.done", CodexTerminalKind::Done, true, false),
            (
                "response.incomplete",
                CodexTerminalKind::Incomplete,
                false,
                false,
            ),
            ("response.failed", CodexTerminalKind::Failed, false, true),
            (
                "response.error",
                CodexTerminalKind::ResponseError,
                false,
                true,
            ),
            (
                "response.cancelled",
                CodexTerminalKind::Cancelled,
                false,
                true,
            ),
            ("error", CodexTerminalKind::Error, false, true),
        ];

        for (event_type, expected, reusable, failure) in cases {
            let payload = serde_json::json!({"type": event_type});
            let actual = CodexTerminalKind::from_payload(&payload);
            assert_eq!(actual, Some(expected), "event type {event_type}");
            assert_eq!(expected.is_reusable(), reusable, "event type {event_type}");
            assert_eq!(expected.is_failure(), failure, "event type {event_type}");
        }

        assert_eq!(
            CodexTerminalKind::from_payload(
                &serde_json::json!({"type": "response.output_text.delta"})
            ),
            None
        );
    }

    #[test]
    fn terminal_snapshot_status_must_match_its_event_when_provided() {
        for (event_type, accepted, rejected) in [
            ("response.completed", "completed", "failed"),
            ("response.done", "completed", "incomplete"),
            ("response.incomplete", "incomplete", "completed"),
            ("response.failed", "failed", "completed"),
            ("response.error", "failed", "completed"),
            ("response.cancelled", "cancelled", "failed"),
            ("error", "failed", "completed"),
        ] {
            for status in [
                None,
                Some(Value::Null),
                Some(Value::String(accepted.into())),
            ] {
                let mut response = serde_json::json!({});
                if let Some(status) = status {
                    response["status"] = status;
                }
                let payload = serde_json::json!({"type":event_type, "response":response});
                validate_terminal_snapshot_status(&payload)
                    .unwrap_or_else(|error| panic!("{event_type}: {error}"));
            }

            let mismatch = serde_json::json!({
                "type":event_type,
                "response":{"status":rejected}
            });
            let error = validate_terminal_snapshot_status(&mismatch).unwrap_err();
            assert!(error.contains("response.status"), "{event_type}: {error}");

            let wrong_type = serde_json::json!({
                "type":event_type,
                "response":{"status":42}
            });
            assert!(validate_terminal_snapshot_status(&wrong_type).is_err());
        }
    }

    #[test]
    fn classifies_retryable_failure_kinds() {
        let rate = classify_event_failure(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true, "primary": {"reset_after_seconds": 1.5}}
        }))
        .unwrap();
        assert_eq!(rate.kind, CodexFailureKind::RateLimit);
        assert_eq!(rate.retry_after.as_deref(), Some("1.5"));

        let overload = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"type": "overloaded_error", "message": "busy"}}
        }))
        .unwrap();
        assert_eq!(overload.status, 529);
        assert!(overload.retryable());
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
    fn context_overflow_prefers_allowlisted_code_and_keeps_safe_descriptors() {
        let payload = serde_json::json!({
            "type":"response.failed",
            "response":{
                "status":"failed",
                "error":{
                    "status":400,
                    "code":"context_length_exceeded",
                    "type":"invalid_request_error",
                    "param":"input",
                    "message":"request rejected"
                }
            }
        });
        let failure = classify_event_failure(&payload).unwrap();

        assert_eq!(failure.kind, CodexFailureKind::ContextOverflow);
        assert_eq!(failure.status, 413);
        assert!(!failure.retryable());
        assert!(failure.message.contains("code: context_length_exceeded"));
        assert!(failure.message.contains("type: invalid_request_error"));
        assert!(failure.message.contains("param: input"));
    }

    #[test]
    fn context_overflow_message_fallback_is_conservative() {
        assert!(is_context_overflow_error(&serde_json::json!({
            "error":{"message":"Prompt is too long for this model"}
        })));
        assert!(is_context_overflow_error(&serde_json::json!({
            "code":"input_too_large",
            "message":"request rejected"
        })));
        assert!(!is_context_overflow_error(&serde_json::json!({
            "error":{"message":"context window telemetry is temporarily unavailable"}
        })));
        assert!(!is_context_overflow_error(&serde_json::json!({
            "error":{"code":"rate_limit_exceeded","message":"too many requests"}
        })));
        assert!(!is_context_overflow_error(&serde_json::json!({
            "error":{
                "code":"rate_limit_exceeded",
                "type":"rate_limit_error",
                "message":"too many tokens for this minute"
            }
        })));
        let rate_limit = classify_event_failure(&serde_json::json!({
            "type":"response.failed",
            "response":{
                "error":{
                    "code":"rate_limit_exceeded",
                    "type":"rate_limit_error",
                    "message":"too many tokens for this minute"
                }
            }
        }))
        .expect("structured rate-limit code is an event failure");
        assert_eq!(rate_limit.kind, CodexFailureKind::RateLimit);
        assert_eq!(rate_limit.status, 429);
        assert!(rate_limit.retryable());
        assert!(!is_context_overflow_error(&serde_json::json!({
            "error":{
                "code":"safety_policy_violation",
                "message":"Prompt is too long for the safety classifier"
            }
        })));
        assert!(is_context_overflow_error(&serde_json::json!({
            "error":{
                "type":"invalid_request_error",
                "message":"Prompt is too long for this model"
            }
        })));
        assert!(is_context_overflow_error(&serde_json::json!({
            "error":{
                "code":"invalid_request_error",
                "type":"invalid_request_error",
                "message":"Prompt is too long for this model"
            }
        })));
        assert!(is_context_overflow_error(&serde_json::json!({
            "error":{
                "code":"future_private_gateway_code",
                "message":"The input exceeds the context window"
            }
        })));
        for status in [401, 403, 404, 429, 500, 501, 502, 503, 504, 520, 529] {
            let payload = serde_json::json!({
                "type":"response.failed",
                "status":status,
                "response":{
                    "status":"failed",
                    "error":{
                        "type":"invalid_request_error",
                        "message":"Prompt is too long for this model"
                    }
                }
            });
            assert!(
                !is_context_overflow_error(&payload),
                "explicit non-context status {status} must veto prose fallback"
            );
        }
        assert!(is_context_overflow_error(&serde_json::json!({
            "type":"response.failed",
            "status":429,
            "response":{
                "status":"failed",
                "error":{
                    "code":"context_length_exceeded",
                    "message":"request rejected"
                }
            }
        })));
        let embedded_rate_limit = serde_json::json!({
            "type":"response.failed",
            "response":{
                "status":"failed",
                "error":{
                    "status":429,
                    "type":"invalid_request_error",
                    "message":"Prompt is too long for this minute's token quota"
                }
            }
        });
        assert!(!is_context_overflow_error_with_status(
            &embedded_rate_limit,
            Some(200)
        ));
    }

    #[test]
    fn failure_message_redacts_secret_but_preserves_safe_code() {
        let payload = serde_json::json!({
            "error":{
                "code":"context_length_exceeded",
                "message":"Authorization: Bearer top-secret"
            }
        });
        let message = sanitized_error_message(&payload);
        assert!(!message.contains("top-secret"));
        assert!(message.contains("[redacted upstream error detail]"));
        assert!(message.contains("code: context_length_exceeded"));

        let long = sanitized_error_message(&serde_json::json!({
            "error":{
                "code":"prompt_too_long",
                "message":"x".repeat(2_048)
            }
        }));
        assert!(long.starts_with("(code: prompt_too_long)"));
        assert!(long.len() <= crate::providers::translate_shared::MAX_EXTERNAL_ERROR_DETAIL_BYTES);
        assert!(long.is_char_boundary(long.len()));

        assert_eq!(
            sanitized_error_message(&serde_json::json!({"error":{"code":42}})),
            "Upstream error (code: 42)"
        );
        assert_eq!(
            sanitized_error_message(&serde_json::json!({"type":"response.failed"})),
            "Upstream error"
        );
    }

    #[test]
    fn completed_response_wins_over_trailing_rate_limit_telemetry() {
        let body = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{}}}\n\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":0}}}\n\n";

        assert!(first_retryable_failure(body).is_none());
    }

    #[test]
    fn incomplete_response_wins_over_trailing_rate_limit_telemetry() {
        let body = b"data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{}}}\n\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":0}}}\n\n";

        assert!(first_retryable_failure(body).is_none());
    }

    #[test]
    fn rate_limit_without_completed_response_remains_retryable() {
        let body = b"data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":0}}}\n\n";

        assert_eq!(
            first_retryable_failure(body).map(|failure| failure.status),
            Some(429)
        );
    }

    #[test]
    fn response_metadata_does_not_close_buffered_retry_prescan() {
        let body = b"data: {\"type\":\"codex.response.metadata\",\"headers\":{\"openai-model\":\"gpt-5.6-sol\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"status\":503,\"message\":\"busy\"}}}\n\n";

        assert_eq!(
            first_retryable_failure(body).map(|failure| failure.status),
            Some(503)
        );
    }

    #[test]
    fn hosted_side_effect_blocks_buffered_model_replay() {
        for hosted in [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1"}}"#,
            r#"{"type":"response.web_search_call.searching","output_index":0}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1"}}"#,
        ] {
            let body = format!(
                "data: {hosted}\n\ndata: {{\"type\":\"response.failed\",\"response\":{{\"status\":\"failed\",\"error\":{{\"status\":503,\"message\":\"busy\"}}}}}}\n\n"
            );
            assert!(first_retryable_failure(body.as_bytes()).is_none());
        }
    }

    #[test]
    fn malformed_protocol_bytes_never_trigger_retry_prescan() {
        let invalid_utf8 = b"data: {\"type\":\"response.failed\",\"error\":{\"status\":503,\"message\":\"busy\xff\"}}\n\n";
        assert!(first_retryable_failure(invalid_utf8).is_none());

        let later_bom = b"data: {\"type\":\"response.created\"}\n\n\xef\xbb\xbfdata: {\"type\":\"response.failed\",\"error\":{\"status\":503,\"message\":\"busy\"}}\n\n";
        assert!(first_retryable_failure(later_bom).is_none());
    }
}
