//! Incremental, bounded SSE parsing plus SIWC completion/tool integrity checks.
use super::{
    auth::{Auth, read_json},
    client::{ApiError, ResponseContext, ResponseInfo, response_error_status},
};

pub(super) type UsagePause = std::sync::Arc<std::sync::Mutex<Option<ApiError>>>;

#[derive(Clone, Default)]
pub(crate) struct StreamOutcome(std::sync::Arc<std::sync::Mutex<Option<String>>>);
impl StreamOutcome {
    pub(crate) fn failure(&self) -> Option<String> {
        self.0.lock().expect("SIWC stream outcome").clone()
    }
    fn fail(&self) {
        *self.0.lock().expect("SIWC stream outcome") =
            Some("SIWC stream failed; inspect the client error event".into());
    }
}
fn pause_on_limit(pause: &UsagePause, error: &ApiError) {
    if error.body.pointer("/error/code").and_then(Value::as_str)
        == Some("subscription_sharing_usage_limit_exceeded")
    {
        *pause.lock().expect("usage pause") = Some(error.clone());
    }
}
use crate::providers::codex::translate::{
    accumulate::accumulate_response, live_stream::LiveStreamTranslator,
};
use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::stream;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_BUFFERED_RESPONSE: usize = 32 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

fn safe_event_type(kind: &str) -> &'static str {
    match kind {
        "response.created" => "response.created",
        "response.in_progress" => "response.in_progress",
        "response.output_text.delta" => "response.output_text.delta",
        "response.output_item.added" => "response.output_item.added",
        "response.completed" => "response.completed",
        "response.failed" => "response.failed",
        "response.incomplete" => "response.incomplete",
        "error" => "error",
        _ => "other_event",
    }
}

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: Vec<String>,
    size: usize,
    skip_lf: bool,
    event: Option<String>,
    started: bool,
    pub events: VecDeque<Value>,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ApiError> {
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                let line = String::from_utf8(std::mem::take(&mut self.line))
                    .map_err(|_| protocol("Invalid UTF-8 in SSE"))?;
                let line = if !self.started {
                    self.started = true;
                    line.trim_start_matches('\u{feff}')
                } else {
                    &line
                };
                if line.is_empty() {
                    if !self.data.is_empty() {
                        let data = self.data.join("\n");
                        if data == "[DONE]" {
                            return Err(protocol(
                                "SSE ended with [DONE] instead of response.completed",
                            ));
                        }
                        let mut value: Value = serde_json::from_str(&data)
                            .map_err(|_| protocol("Invalid JSON in SSE event"))?;
                        if !value.is_object() {
                            return Err(protocol("SSE event data must be an object"));
                        }
                        if value.get("type").is_none()
                            && let Some(event) = &self.event
                        {
                            value["type"] = json!(event);
                        }
                        self.events.push_back(value);
                    }
                    self.data.clear();
                    self.event = None;
                    self.size = 0;
                } else if !line.starts_with(':') {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "data" => self.data.push(value.into()),
                        "event" => self.event = Some(value.into()),
                        _ => {}
                    }
                }
            } else {
                self.line.push(byte);
            }
            self.size += 1;
            if self.size > MAX_FRAME {
                return Err(protocol("SSE event exceeds size limit"));
            }
        }
        Ok(())
    }
}

fn protocol(message: &str) -> ApiError {
    ApiError::local(StatusCode::BAD_GATEWAY, "siwc_stream_error", message)
}

fn with_request_id(mut response: Response, info: &ResponseInfo) -> Response {
    response
        .headers_mut()
        .insert("x-should-retry", "false".parse().expect("static header"));
    if let Some(id) = &info.request_id
        && let Ok(id) = id.parse()
    {
        response.headers_mut().insert("x-openai-request-id", id);
    }
    response
}

struct Call {
    id: String,
    name: String,
    item_id: String,
    args: String,
    done: bool,
}

pub(super) struct Reader {
    response: reqwest::Response,
    pending: Option<Bytes>,
    decoder: Decoder,
    names: HashSet<String>,
    calls: HashMap<u64, Call>,
    call_ids: HashSet<String>,
    pub complete: bool,
    context: ResponseContext,
    auth: Auth,
    last_check: tokio::time::Instant,
    last_chunk: tokio::time::Instant,
    last_ping: tokio::time::Instant,
}
impl Reader {
    pub async fn new(
        response: reqwest::Response,
        names: HashSet<String>,
        auth: Auth,
    ) -> Result<Self, ApiError> {
        let mut context = ResponseContext::from_response(&response);
        // HTTP admission errors must be classified before applying the stream contract.
        if !response.status().is_success() {
            context.info.response_class = Some("http_error");
            let raw = tokio::time::timeout(Duration::from_secs(15), read_json(response))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(Value::Null);
            return Err(context.http_error(raw));
        }
        if context.info.content_type.as_deref().is_some_and(|s| {
            !s.split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        }) {
            let raw = tokio::time::timeout(Duration::from_secs(15), read_json(response))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(Value::Null);
            context.info.response_class = Some(if raw.get("error").is_some() {
                "json_error"
            } else if !raw.is_null() {
                "json"
            } else {
                "non_sse"
            });
            return Err(context.unexpected_type(raw));
        }
        let missing_type = context.info.content_type.is_none();
        let mut reader = Self {
            response,
            pending: None,
            decoder: Decoder::default(),
            names,
            calls: HashMap::new(),
            call_ids: HashSet::new(),
            complete: false,
            context,
            auth,
            last_check: tokio::time::Instant::now() - Duration::from_secs(1),
            last_chunk: tokio::time::Instant::now(),
            last_ping: tokio::time::Instant::now(),
        };
        if missing_type {
            // The first response.created can echo large instructions/tools. Its bound
            // must match the ordinary SSE frame bound, not a smaller HTTP-chunk cap.
            reader.detect_sse().await?;
            reader.report_classification();
        }
        Ok(reader)
    }
    async fn detect_sse(&mut self) -> Result<(), ApiError> {
        let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
        let mut prefix = Vec::new();
        loop {
            let result = tokio::time::timeout_at(deadline, self.response.chunk()).await;
            let classification = match &result {
                Err(_) => "probe_timeout",
                Ok(Err(_)) => "transport_error",
                Ok(Ok(None)) if prefix.is_empty() => "empty",
                _ => "incomplete_sse",
            };
            let chunk = result.ok().and_then(Result::ok).flatten();
            let Some(chunk) = chunk else {
                return Err(self.classification_error(classification, &prefix));
            };
            // Feed complete line segments only until the first complete event. Keep
            // the rest of this transport chunk for next(), including later failures.
            let mut offset = 0;
            while offset < chunk.len() {
                let end = chunk[offset..]
                    .iter()
                    .position(|b| matches!(b, b'\r' | b'\n'))
                    .map(|i| offset + i + 1)
                    .unwrap_or(chunk.len());
                if prefix.len() + end - offset > MAX_FRAME {
                    return Err(self.classification_error("probe_limit", &prefix));
                }
                prefix.extend_from_slice(&chunk[offset..end]);
                let first = prefix.iter().copied().find(|b| !b.is_ascii_whitespace());
                if matches!(first, Some(b'{' | b'[')) {
                    if serde_json::from_slice::<Value>(&prefix).is_ok() {
                        return Err(self.classification_error("json", &prefix));
                    }
                } else {
                    if first == Some(b'<') {
                        return Err(self.classification_error("html_or_xml", &prefix));
                    }
                    if self.decoder.push(&chunk[offset..end]).is_err() {
                        return Err(self.classification_error("invalid_sse", &prefix));
                    }
                    if let Some(event) = self.decoder.events.front() {
                        let kind = event["type"].as_str().unwrap_or("");
                        if !(kind.starts_with("response.") || kind == "error") {
                            return Err(self.classification_error("unrecognized_event", &prefix));
                        }
                        self.context.info.response_class = Some("responses_sse");
                        self.context.info.first_event_type = Some(safe_event_type(kind));
                        self.context.info.probe_bytes = prefix.len();
                        self.pending = (end < chunk.len()).then(|| chunk.slice(end..));
                        self.last_chunk = tokio::time::Instant::now();
                        return Ok(());
                    }
                }
                offset = end;
            }
        }
    }
    fn classification_error(&mut self, classification: &'static str, prefix: &[u8]) -> ApiError {
        let raw = serde_json::from_slice::<Value>(prefix).unwrap_or(Value::Null);
        self.context.info.response_class = Some(if raw.get("error").is_some() {
            "json_error"
        } else {
            classification
        });
        self.context.info.probe_bytes = prefix.len();
        self.context.unexpected_type(raw)
    }
    fn report_classification(&self) {
        let diagnostic = ApiError::local(StatusCode::OK, "", "").with_upstream(&self.context.info);
        let logger = crate::logging::create_logger("siwc");
        if self.context.info.content_type.is_none() {
            logger.warn(
                "siwc_response_classification",
                diagnostic.diagnostics().as_object().cloned(),
            );
        } else {
            logger.info(
                "siwc_response_classification",
                diagnostic.diagnostics().as_object().cloned(),
            );
        }
    }
    pub async fn next(&mut self) -> Result<Option<Value>, ApiError> {
        self.next_inner().await.map_err(|mut error| {
            error.body = self.context.sanitize(error.body);
            error = error.with_upstream(&self.context.info);
            self.context.remember_failure(&error);
            error.report();
            error
        })
    }
    async fn next_inner(&mut self) -> Result<Option<Value>, ApiError> {
        if self.complete {
            return Ok(None);
        }
        loop {
            if self.last_check.elapsed() >= Duration::from_secs(1) {
                if !self.auth.session_active().await.unwrap_or(false) {
                    return Err(protocol(
                        "SIWC account signed out or permission removed; stream stopped",
                    ));
                }
                self.last_check = tokio::time::Instant::now();
            }
            if let Some(mut value) = self.decoder.events.pop_front() {
                if self.context.info.first_event_type.is_none() {
                    self.context.info.response_class = Some("responses_sse");
                    self.context.info.first_event_type =
                        Some(safe_event_type(value["type"].as_str().unwrap_or("")));
                    self.report_classification();
                }
                if value["type"] == "response.refusal.delta" {
                    value["type"] = json!("response.output_text.delta");
                }
                self.validate(&value)?;
                return Ok(Some(value));
            }
            if let Some(bytes) = self.pending.take() {
                self.decoder.push(&bytes)?;
                continue;
            }
            if self.last_chunk.elapsed() >= Duration::from_secs(90) {
                return Err(protocol(
                    "SIWC stream idle timeout before response.completed",
                ));
            }
            let chunk = match tokio::time::timeout(Duration::from_secs(1), self.response.chunk())
                .await
            {
                Ok(result) => result
                    .map_err(|_| protocol("SIWC stream disconnected before response.completed"))?,
                Err(_) => {
                    if self.last_ping.elapsed() >= Duration::from_secs(15) {
                        self.last_ping = tokio::time::Instant::now();
                        return Ok(Some(json!({"type":"keepalive"})));
                    }
                    continue;
                }
            };
            self.last_chunk = tokio::time::Instant::now();
            match chunk {
                Some(bytes) => self.decoder.push(&bytes)?,
                None => return Err(protocol("SIWC stream ended without response.completed")),
            }
        }
    }
    fn validate(&mut self, value: &Value) -> Result<(), ApiError> {
        let kind = value["type"]
            .as_str()
            .ok_or_else(|| protocol("SSE event missing type"))?;
        match kind {
            "error" | "response.failed" | "response.incomplete" => {
                let error = value.get("error").or_else(|| value.pointer("/response/error")).filter(|v| !v.is_null()).or_else(|| (kind == "error" && (value.get("code").is_some() || value.get("message").is_some())).then_some(value)).cloned().unwrap_or_else(|| json!({"code":"response_incomplete","message":"SIWC response did not complete"}));
                let status = response_error_status(error["code"].as_str().unwrap_or(""));
                return Err(ApiError {
                    status,
                    body: json!({"error":error,"event":value}),
                    request_id: self.context.info.request_id.clone(),
                    retry_after: None,
                    upstream: Some(Box::new(self.context.info.clone())),
                });
            }
            "response.done" => {
                return Err(protocol(
                    "Expected response.completed, received legacy response.done",
                ));
            }
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| protocol("Tool call missing output_index"))?;
                let id = value["item"]["call_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| protocol("Tool call missing call_id"))?;
                let name = value["item"]["name"].as_str().unwrap_or("");
                if !self.names.contains(name)
                    || !self.call_ids.insert(id.into())
                    || self.calls.contains_key(&index)
                {
                    return Err(protocol("Unknown tool or duplicate tool call ID/index"));
                }
                let args = value["item"]["arguments"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                self.calls.insert(
                    index,
                    Call {
                        id: id.into(),
                        name: name.into(),
                        item_id: value["item"]["id"].as_str().unwrap_or("").into(),
                        args,
                        done: false,
                    },
                );
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let call = value["output_index"]
                    .as_u64()
                    .and_then(|i| self.calls.get_mut(&i))
                    .ok_or_else(|| protocol("Tool arguments reference an unknown call"))?;
                if value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != call.item_id)
                    || call.done
                {
                    return Err(protocol("Tool argument item ID/order mismatch"));
                }
                if kind.ends_with(".delta") {
                    call.args.push_str(
                        value["delta"]
                            .as_str()
                            .ok_or_else(|| protocol("Invalid tool argument delta"))?,
                    );
                } else if let Some(args) = value["arguments"].as_str() {
                    if !call.args.is_empty() && call.args != args {
                        return Err(protocol("Tool argument completion does not match deltas"));
                    }
                    call.args = args.into();
                }
                if call.args.len() > MAX_FRAME {
                    return Err(protocol("Tool arguments exceed size limit"));
                }
            }
            "response.output_item.done" if value["item"]["type"] == "function_call" => {
                let call = value["output_index"]
                    .as_u64()
                    .and_then(|i| self.calls.get_mut(&i))
                    .ok_or_else(|| protocol("Tool completion has no matching call"))?;
                if call.done
                    || value["item"]["call_id"] != call.id
                    || value["item"]["name"] != call.name
                    || value["item"]["id"] != call.item_id
                {
                    return Err(protocol("Tool completion identity mismatch"));
                }
                let args = value["item"]["arguments"]
                    .as_str()
                    .ok_or_else(|| protocol("Tool completion missing arguments"))?;
                if !call.args.is_empty() && call.args != args {
                    return Err(protocol("Tool completion arguments mismatch"));
                }
                if !serde_json::from_str::<Value>(args).is_ok_and(|v| v.is_object()) {
                    return Err(protocol("Tool arguments must be a valid JSON object"));
                }
                call.args = args.into();
                call.done = true;
            }
            "response.output_item.added" | "response.output_item.done"
                if !matches!(
                    value["item"]["type"].as_str(),
                    Some("message" | "reasoning")
                ) =>
            {
                return Err(protocol("Unsupported Responses output item"));
            }
            "response.completed" => {
                if value
                    .pointer("/response/status")
                    .is_some_and(|s| s != "completed")
                    || self.calls.values().any(|c| !c.done)
                {
                    return Err(protocol("Response completed with incomplete tools/status"));
                }
                if let Some(output) = value.pointer("/response/output").and_then(Value::as_array) {
                    let mut terminal_calls = HashSet::new();
                    for item in output {
                        match item["type"].as_str() {
                            Some("function_call") => {
                                if !terminal_calls.insert(item["call_id"].as_str().unwrap_or(""))
                                    || !self.calls.values().any(|c| {
                                        c.done
                                            && item["id"] == c.item_id
                                            && item["call_id"] == c.id
                                            && item["name"] == c.name
                                            && item["arguments"] == c.args
                                    })
                                {
                                    return Err(protocol(
                                        "Terminal response contains an unmatched or changed tool call",
                                    ));
                                }
                            }
                            Some("message" | "reasoning") => {}
                            _ => return Err(protocol("Unsupported terminal output item")),
                        }
                    }
                }
                self.complete = true;
            }
            _ => {}
        }
        Ok(())
    }
}

pub(super) async fn response(
    response: reqwest::Response,
    names: HashSet<String>,
    model: String,
    stream_requested: bool,
    estimated: u64,
    pause: UsagePause,
    auth: Auth,
) -> Response {
    let reader = match Reader::new(response, names, auth).await {
        Ok(reader) => reader,
        Err(e) => {
            pause_on_limit(&pause, &e);
            e.report();
            return e.response();
        }
    };
    let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let info = reader.context.info.clone();
    if !stream_requested {
        let mut reader = reader;
        let mut bytes = Vec::new();
        loop {
            match reader.next().await {
                Ok(Some(event)) => {
                    bytes.extend(crate::anthropic::sse::encode_sse_event(
                        None,
                        &event.to_string(),
                    ));
                    if bytes.len() > MAX_BUFFERED_RESPONSE {
                        let error = protocol("Buffered response exceeds limit; use streaming")
                            .with_upstream(&info);
                        reader.context.remember_failure(&error);
                        return error.response();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    pause_on_limit(&pause, &e);
                    return e.response();
                }
            }
        }
        let response = match accumulate_response(&bytes, &id, &model) {
            Ok(value) => axum::Json(value).into_response(),
            Err(_) => {
                let error = protocol("Could not translate completed response").with_upstream(&info);
                reader.context.remember_failure(&error);
                error.response()
            }
        };
        return with_request_id(response, &info);
    }
    struct Live {
        reader: Reader,
        translator: LiveStreamTranslator,
        done: bool,
        pause: UsagePause,
        outcome: StreamOutcome,
    }
    let outcome = StreamOutcome::default();
    let live = Live {
        reader,
        translator: LiveStreamTranslator::with_estimated_input_tokens(id, model, estimated)
            .with_read_stall_repair(false),
        done: false,
        pause,
        outcome: outcome.clone(),
    };
    let stream = stream::unfold(live, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            match state.reader.next().await {
                Ok(Some(event)) => match state.translator.accept(&event, None) {
                    Ok(bytes) if !bytes.is_empty() => {
                        return Some((
                            Ok::<_, std::convert::Infallible>(Bytes::from(bytes)),
                            state,
                        ));
                    }
                    Ok(_) => {}
                    Err(_) => {
                        state.done = true;
                        state.outcome.fail();
                        let e = protocol("Could not translate Responses stream")
                            .with_upstream(&state.reader.context.info);
                        state.reader.context.remember_failure(&e);
                        e.report();
                        let bytes = crate::anthropic::sse::encode_sse_event(
                            Some("error"),
                            &e.envelope().to_string(),
                        );
                        return Some((Ok(Bytes::from(bytes)), state));
                    }
                },
                Ok(None) => return None,
                Err(e) => {
                    state.done = true;
                    state.outcome.fail();
                    pause_on_limit(&state.pause, &e);
                    let bytes = crate::anthropic::sse::encode_sse_event(
                        Some("error"),
                        &e.envelope().to_string(),
                    );
                    return Some((Ok(Bytes::from(bytes)), state));
                }
            }
        }
    });
    let mut response = (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
            ("x-accel-buffering", "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response();
    response.extensions_mut().insert(outcome);
    with_request_id(response, &info)
}
