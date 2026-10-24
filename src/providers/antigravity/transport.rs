//! Bounded native HTTP error diagnostics and SSE collection. No generation retry.
use super::replay::{self, Result};
use crate::anthropic::json_error;
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::time::Instant;

const MAX_ERROR_BODY: usize = 8 * 1024;
const MAX_RETRY_SECONDS: u32 = 86400;
const UNCLASSIFIED: &str = "unclassified";
const GOOGLE_STATUSES: &[&str] = &[
    "CANCELLED",
    "UNKNOWN",
    "INVALID_ARGUMENT",
    "DEADLINE_EXCEEDED",
    "NOT_FOUND",
    "ALREADY_EXISTS",
    "PERMISSION_DENIED",
    "RESOURCE_EXHAUSTED",
    "FAILED_PRECONDITION",
    "ABORTED",
    "OUT_OF_RANGE",
    "UNIMPLEMENTED",
    "INTERNAL",
    "UNAVAILABLE",
    "DATA_LOSS",
    "UNAUTHENTICATED",
];
// Supported exact literals, not claims about which reasons Argon actually emits.
const GOOGLE_REASONS: &[&str] = &[
    "RATE_LIMIT_EXCEEDED",
    "QUOTA_EXCEEDED",
    "RESOURCE_EXHAUSTED",
    "MODEL_CAPACITY_EXHAUSTED",
    "MODEL_NOT_FOUND",
    "SERVICE_DISABLED",
    "API_DISABLED",
    "ACCESS_TOKEN_EXPIRED",
    "ACCESS_TOKEN_SCOPE_INSUFFICIENT",
    "CONSUMER_INVALID",
    "PROJECT_INVALID",
];
const GOOGLE_DOMAINS: &[&str] = &[
    "googleapis.com",
    "cloudcode-pa.googleapis.com",
    "generativelanguage.googleapis.com",
    "aiplatform.googleapis.com",
];

#[derive(Clone, Copy)]
enum RetryHint {
    Absent,
    Invalid,
    Conflicting,
    Valid(u32),
}
impl RetryHint {
    fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Invalid => "invalid",
            Self::Conflicting => "conflicting",
            Self::Valid(_) => "valid",
        }
    }
    fn seconds(self) -> String {
        match self {
            Self::Valid(seconds) => seconds.to_string(),
            _ => "none".into(),
        }
    }
}
struct RetryHeader {
    hint: RetryHint,
    value: Option<HeaderValue>,
    date: bool,
}
impl RetryHeader {
    fn parse(headers: &HeaderMap, now: time::OffsetDateTime) -> Self {
        let mut values = headers.get_all("retry-after").iter();
        let Some(value) = values.next() else {
            return Self {
                hint: RetryHint::Absent,
                value: None,
                date: false,
            };
        };
        if values.next().is_none()
            && value.as_bytes().len() <= 128
            && let Some((seconds, normalized, date)) =
                value.to_str().ok().and_then(|v| retry_after(v, now))
            && let Ok(value) = HeaderValue::from_str(&normalized)
        {
            return Self {
                hint: RetryHint::Valid(seconds),
                value: Some(value),
                date,
            };
        }
        Self {
            hint: RetryHint::Invalid,
            value: None,
            date: false,
        }
    }
}

fn retry_after(raw: &str, now: time::OffsetDateTime) -> Option<(u32, String, bool)> {
    if (1..=5).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: u32 = raw.parse().ok()?;
        return (seconds <= MAX_RETRY_SECONDS).then(|| (seconds, seconds.to_string(), false));
    }
    if raw.len() != 29 {
        return None;
    }
    let format = time::format_description::parse_borrowed::<3>(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
    )
    .ok()?;
    let date = time::PrimitiveDateTime::parse(raw, &format)
        .ok()?
        .assume_utc();
    let canonical = date.format(&format).ok()?;
    // Round-tripping also verifies weekday, exact case, spacing and GMT.
    if canonical != raw {
        return None;
    }
    let nanos = (date.unix_timestamp_nanos() - now.unix_timestamp_nanos()).max(0);
    if nanos > i128::from(MAX_RETRY_SECONDS) * 1_000_000_000 {
        return None;
    }
    let seconds = u32::try_from((nanos + 999_999_999) / 1_000_000_000).ok()?;
    Some((seconds, canonical, true))
}

fn retry_duration(raw: &str) -> Option<u32> {
    let duration = raw.strip_suffix('s')?;
    let (seconds, fraction) = duration
        .split_once('.')
        .map_or((duration, None), |(s, f)| (s, Some(f)));
    if !(1..=5).contains(&seconds.len())
        || !seconds.bytes().all(|b| b.is_ascii_digit())
        || (seconds.len() > 1 && seconds.starts_with('0'))
    {
        return None;
    }
    let seconds: u32 = seconds.parse().ok()?;
    let nanos = match fraction {
        None => 0,
        Some(f) if (1..=9).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()) => f
            .parse::<u32>()
            .ok()?
            .checked_mul(10_u32.pow(9 - f.len() as u32))?,
        _ => return None,
    };
    let rounded = seconds.checked_add(u32::from(nanos > 0))?;
    (rounded <= MAX_RETRY_SECONDS).then_some(rounded)
}

struct ErrorDiagnostics {
    status: &'static str,
    reason: &'static str,
    domain: &'static str,
    quota: &'static str,
    retry: RetryHint,
}
impl ErrorDiagnostics {
    fn unclassified() -> Self {
        Self {
            status: UNCLASSIFIED,
            reason: UNCLASSIFIED,
            domain: UNCLASSIFIED,
            quota: UNCLASSIFIED,
            retry: RetryHint::Invalid,
        }
    }
    fn parse(raw: &[u8]) -> Option<Self> {
        let value = replay::parse_json(raw, MAX_ERROR_BODY).ok()?;
        let error = value.get("error")?.as_object()?;
        let details = match error.get("details") {
            Some(v) => v.as_array()?.as_slice(),
            None => &[],
        };
        if details.len() > 16 {
            return None;
        }
        let mut infos = Vec::new();
        let mut quotas = Vec::new();
        let mut retries = Vec::new();
        for detail in details {
            let detail = detail.as_object()?;
            match detail.get("@type")?.as_str()? {
                "type.googleapis.com/google.rpc.ErrorInfo" => infos.push(detail),
                "type.googleapis.com/google.rpc.QuotaFailure" => quotas.push(detail),
                "type.googleapis.com/google.rpc.RetryInfo" => retries.push(detail),
                _ => {} // Unknown detail types never contribute free-form diagnostics.
            }
        }
        let mut out = Self::unclassified();
        out.status = allowed(error.get("status"), GOOGLE_STATUSES);
        if let [info] = infos.as_slice()
            && info.get("reason").is_some_and(Value::is_string)
        {
            out.domain = allowed(info.get("domain"), GOOGLE_DOMAINS);
            if out.domain != UNCLASSIFIED {
                out.reason = allowed(info.get("reason"), GOOGLE_REASONS);
            }
        }
        out.quota = match quotas.as_slice() {
            [] => "absent",
            [quota]
                if quota
                    .get("violations")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty() && v.iter().all(Value::is_object)) =>
            {
                "reported"
            }
            _ => UNCLASSIFIED,
        };
        out.retry = RetryHint::Absent;
        for retry in &retries {
            match retry
                .get("retryDelay")
                .and_then(Value::as_str)
                .and_then(retry_duration)
            {
                Some(seconds) => out.retry = RetryHint::Valid(seconds),
                None => {
                    out.retry = RetryHint::Invalid;
                    return Some(out);
                }
            }
        }
        if retries.len() > 1 {
            out.retry = RetryHint::Conflicting;
        }
        Some(out)
    }
}
fn allowed(value: Option<&Value>, whitelist: &'static [&'static str]) -> &'static str {
    whitelist
        .iter()
        .copied()
        .find(|v| Some(*v) == value.and_then(Value::as_str))
        .unwrap_or(UNCLASSIFIED)
}
fn combine_retry(header: &RetryHeader, body: RetryHint) -> RetryHint {
    use RetryHint::*;
    match (header.hint, body) {
        (Invalid, _) | (_, Invalid) => Invalid,
        (Conflicting, _) | (_, Conflicting) => Conflicting,
        (Valid(h), Valid(b)) if header.date => Valid(h.max(b)),
        (Valid(h), Valid(b)) if h != b => Conflicting,
        (Valid(h), _) => Valid(h),
        (_, Valid(b)) => Valid(b),
        (Absent, Absent) => Absent,
    }
}

async fn error_body(mut upstream: reqwest::Response, deadline: Instant) -> Option<Vec<u8>> {
    if upstream
        .content_length()
        .is_some_and(|n| n > MAX_ERROR_BODY as u64)
        || Instant::now() >= deadline
    {
        return None;
    }
    tokio::time::timeout_at(deadline, async move {
        let mut body = Vec::with_capacity(MAX_ERROR_BODY);
        while let Some(chunk) = upstream.chunk().await.ok()? {
            if chunk.len() > MAX_ERROR_BODY - body.len() {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        Some(body)
    })
    .await
    .ok()
    .flatten()
}

/// Initial native failures remain HTTP JSON errors even when the caller requested SSE.
/// Only static labels and bounded integers escape; never log the response/body/parse errors.
/// This future owns the upstream body, so dropping the request cancels inspection.
pub(crate) async fn http_error(upstream: reqwest::Response, header_deadline: Instant) -> Response {
    let deadline = header_deadline.min(Instant::now() + Duration::from_secs(1));
    let status = upstream.status();
    let retry = RetryHeader::parse(upstream.headers(), time::OffsetDateTime::now_utc());
    let body = error_body(upstream, deadline).await;
    let diagnostics = match body.as_deref().and_then(ErrorDiagnostics::parse) {
        Some(mut diagnostics) => {
            diagnostics.retry = combine_retry(&retry, diagnostics.retry);
            diagnostics
        }
        None => {
            let mut diagnostics = ErrorDiagnostics::unclassified();
            // An unusable body cannot establish absence, but a validated header is independent.
            if let RetryHint::Valid(_) = retry.hint {
                diagnostics.retry = retry.hint;
            }
            diagnostics
        }
    };
    let mut response = json_error(
        status,
        match status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            _ => "api_error",
        },
        format!(
            "Antigravity upstream returned HTTP {}; no retry or fallback attempted; google_status={}; reason={}; domain={}; quota_failure={}; retry_hint={}; retry_s={}",
            status.as_u16(),
            diagnostics.status,
            diagnostics.reason,
            diagnostics.domain,
            diagnostics.quota,
            diagnostics.retry.label(),
            diagnostics.retry.seconds()
        ),
    );
    // Body RetryInfo is advisory metadata only, never a synthesized HTTP header.
    if let Some(value) = retry.value {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

const MAX_TURN: usize = 8 * 1024 * 1024;
const MAX_FRAME: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct Reducer {
    bytes: usize,
    line: Vec<u8>,
    data: Vec<u8>,
    contents: Vec<Value>,
    usage: Map<String, Value>,
    terminal: Option<String>,
    parts: usize,
}

pub(crate) struct Turn {
    pub contents: Value,
    pub usage: Value,
    pub stop_reason: String,
}

impl Reducer {
    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(chunk.len())
            .ok_or("native turn byte limit")?;
        if self.bytes > MAX_TURN {
            return Err("native turn byte limit");
        }
        for byte in chunk {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    self.frame()?;
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if self.data.len() + data.len() + 1 > MAX_FRAME {
                        return Err("native SSE frame byte limit");
                    }
                    self.data.extend_from_slice(data);
                    self.data.push(b'\n');
                }
            } else {
                self.line.push(*byte);
                if self.line.len() > MAX_FRAME {
                    return Err("native SSE line byte limit");
                }
            }
        }
        Ok(())
    }
    fn frame(&mut self) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        if data == b"[DONE]" {
            return if self.terminal.is_some() {
                Ok(())
            } else {
                Err("native stream missing terminal state")
            };
        }
        let value = replay::parse_json(&data, MAX_FRAME)?;
        if value.get("error").is_some() {
            return Err("native upstream error");
        }
        let response = value
            .get("response")
            .ok_or("invalid native response envelope")?;
        if response.get("error").is_some()
            || response
                .get("promptFeedback")
                .is_some_and(|p| p.get("blockReason").is_some())
        {
            return Err("native refusal/error");
        }
        if let Some(usage) = response.get("usageMetadata") {
            let usage = usage.as_object().ok_or("invalid native usage")?;
            for key in [
                "promptTokenCount",
                "cachedContentTokenCount",
                "candidatesTokenCount",
                "thoughtsTokenCount",
                "totalTokenCount",
            ] {
                if let Some(value) = usage.get(key) {
                    let number = value
                        .as_u64()
                        .filter(|n| *n <= 1_000_000_000)
                        .ok_or("invalid native usage count")?;
                    if self
                        .usage
                        .get(key)
                        .and_then(Value::as_u64)
                        .is_some_and(|old| number < old)
                    {
                        return Err("native cumulative usage decreased");
                    }
                    self.usage.insert(key.into(), value.clone());
                }
            }
        }
        if let Some(candidates) = response.get("candidates") {
            let candidates = candidates
                .as_array()
                .filter(|c| c.len() <= 1)
                .ok_or("multiple/invalid native candidates")?;
            for candidate in candidates {
                if candidate
                    .get("index")
                    .is_some_and(|v| v.as_u64() != Some(0))
                {
                    return Err("unsupported candidate index");
                }
                if let Some(content) = candidate.get("content") {
                    if content["role"] != "model" {
                        return Err("invalid native content role");
                    }
                    let parts = content["parts"].as_array().ok_or("invalid native parts")?;
                    self.parts += parts.len();
                    if self.parts > 4096 || self.contents.len() >= 512 {
                        return Err("native content/part limit");
                    }
                    for part in parts {
                        if !part.is_object()
                            || part.get("text").is_some_and(|v| !v.is_string())
                            || part.get("thought").is_some_and(|v| !v.is_boolean())
                            || part.get("thoughtSignature").is_some_and(|v| !v.is_string())
                        {
                            return Err("invalid native part");
                        }
                        if [
                            "inlineData",
                            "fileData",
                            "executableCode",
                            "codeExecutionResult",
                            "functionResponse",
                        ]
                        .iter()
                        .any(|k| part.get(*k).is_some())
                        {
                            return Err("unsupported native content/media");
                        }
                        if let Some(call) = part.get("functionCall") {
                            replay::keys(call, &["name", "args", "id"], &["name"])?;
                            replay::call_arguments(call)?;
                        }
                        if !["text", "functionCall", "thoughtSignature"]
                            .iter()
                            .any(|k| part.get(*k).is_some())
                        {
                            return Err("unsupported native part");
                        }
                    }
                    self.contents.push(content.clone());
                }
                if let Some(reason) = candidate.get("finishReason") {
                    let reason = reason
                        .as_str()
                        .filter(|r| matches!(*r, "STOP" | "MAX_TOKENS"))
                        .ok_or("unsupported native termination/refusal")?;
                    if self.terminal.as_ref().is_some_and(|old| old != reason) {
                        return Err("conflicting native terminal state");
                    }
                    self.terminal = Some(reason.to_string());
                }
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Turn> {
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err("truncated native SSE frame");
        }
        let reason = self
            .terminal
            .ok_or("native stream missing terminal state")?;
        if self.contents.is_empty() {
            return Err("empty native turn");
        }
        let prompt = self
            .usage
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .ok_or("native prompt usage missing")?;
        let candidates = self
            .usage
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .ok_or("native output usage missing")?;
        let thoughts = self
            .usage
            .get("thoughtsTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = candidates
            .checked_add(thoughts)
            .ok_or("invalid native usage total")?;
        if self
            .usage
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .is_some_and(|total| total != prompt + output)
        {
            return Err("ambiguous native usage total");
        }
        let cache = self
            .usage
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(prompt);
        let contents = json!(self.contents);
        let has_calls = !replay::calls(&contents)?.is_empty();
        Ok(Turn {
            contents,
            usage: json!({"input_tokens":prompt-cache,"cache_read_input_tokens":cache,"output_tokens":output}),
            stop_reason: if reason == "MAX_TOKENS" {
                "max_tokens"
            } else if has_calls {
                "tool_use"
            } else {
                "end_turn"
            }
            .to_string(),
        })
    }
}

pub(crate) async fn collect(mut response: reqwest::Response) -> Result<Turn> {
    let mut reducer = Reducer::default();
    while let Some(chunk) =
        tokio::time::timeout(std::time::Duration::from_secs(30), response.chunk())
            .await
            .map_err(|_| "native stream idle timeout")?
            .map_err(|_| "native stream transport failed")?
    {
        reducer.push(&chunk)?;
    }
    reducer.finish()
}

pub(crate) fn message(blocks: Value, model: &str, turn: &Turn) -> Value {
    json!({"id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,
        "content":blocks,"stop_reason":turn.stop_reason,"stop_sequence":null,"usage":turn.usage})
}

pub(crate) fn sse(message: &Value) -> Result<Vec<u8>> {
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    let mut events = vec![json!({"type":"message_start","message":start})];
    for (index, block) in message["content"]
        .as_array()
        .ok_or("invalid projected message")?
        .iter()
        .enumerate()
    {
        let (initial, deltas) = match block["type"].as_str() {
            Some("thinking") => (
                json!({"type":"thinking","thinking":"","signature":""}),
                vec![
                    json!({"type":"thinking_delta","thinking":block["thinking"]}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ],
            ),
            Some("text") => (
                json!({"type":"text","text":""}),
                vec![json!({"type":"text_delta","text":block["text"]})],
            ),
            Some("tool_use") => (
                json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                vec![
                    json!({"type":"input_json_delta","partial_json":serde_json::to_string(&block["input"]).map_err(|_| "invalid tool arguments")?}),
                ],
            ),
            _ => return Err("invalid projected block"),
        };
        events.push(json!({"type":"content_block_start","index":index,"content_block":initial}));
        for delta in deltas {
            events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":message["stop_reason"],"stop_sequence":null},"usage":message["usage"]}));
    events.push(json!({"type":"message_stop"}));
    let mut wire = String::new();
    for event in events {
        wire.push_str(&format!(
            "event: {}\ndata: {}\n\n",
            event["type"].as_str().unwrap(),
            event
        ));
    }
    Ok(wire.into_bytes())
}
