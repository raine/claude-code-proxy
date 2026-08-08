use http::StatusCode;
use serde_json::{Value, json};

use crate::anthropic::sse::parse_sse_events;
use crate::providers::codex::events::classify_event_failure;

use super::ChatError;

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    pub fn value(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens.saturating_add(self.completion_tokens),
        })
    }
}

#[derive(Debug, Clone)]
pub struct CompletionState {
    pub id: String,
    pub created: u64,
    pub model: String,
    pub text: String,
    pub usage: Usage,
    pub finish_reason: &'static str,
    pub completed: bool,
}

impl CompletionState {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: unix_seconds(),
            model: model.to_string(),
            text: String::new(),
            usage: Usage::default(),
            finish_reason: "stop",
            completed: false,
        }
    }

    pub fn observe(&mut self, event: &Value) -> Result<Option<String>, ChatError> {
        let kind = event.get("type").and_then(Value::as_str);
        if let Some(failure) = classify_event_failure(event) {
            let status =
                StatusCode::from_u16(failure.client_status()).unwrap_or(StatusCode::BAD_GATEWAY);
            return Err(ChatError::new(
                status,
                failure.client_error_type(),
                failure.message,
                None,
                None,
            ));
        }
        match kind {
            Some("response.created" | "response.in_progress") => {
                if let Some(response) = event.get("response") {
                    self.update_metadata(response);
                }
            }
            Some("response.output_text.delta") => {
                let delta = event.get("delta").and_then(Value::as_str).ok_or_else(|| {
                    ChatError::upstream("Codex output-text delta did not contain text")
                })?;
                self.text.push_str(delta);
                return Ok(Some(delta.to_string()));
            }
            Some("response.completed" | "response.done") => {
                let response = event.get("response").ok_or_else(|| {
                    ChatError::upstream("Codex completion event did not contain a response")
                })?;
                self.update_metadata(response);
                if response.get("status").and_then(Value::as_str) == Some("failed") {
                    return Err(event_error(event));
                }
                self.completed = true;
            }
            _ => {
                if event.get("error").is_some_and(|error| !error.is_null()) {
                    return Err(event_error(event));
                }
            }
        }
        Ok(None)
    }

    fn update_metadata(&mut self, response: &Value) {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.id = chat_completion_id(id);
        }
        if let Some(model) = response.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        if let Some(created) = response
            .get("created_at")
            .or_else(|| response.get("created"))
            .and_then(Value::as_f64)
            .filter(|created| *created >= 0.0)
        {
            self.created = created as u64;
        }
        if let Some(usage) = response.get("usage") {
            self.usage.prompt_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.prompt_tokens);
            self.usage.completion_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.completion_tokens);
        }
    }
}

pub fn aggregate_sse(body: &[u8], requested_model: &str) -> Result<Value, ChatError> {
    let events = parse_sse_events(body);
    if events.is_empty() {
        return Err(ChatError::upstream(
            "Codex returned an empty or malformed event stream",
        ));
    }
    let mut state = CompletionState::new(requested_model);
    for event in events {
        if event.data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| {
            ChatError::upstream("Codex returned malformed JSON in its event stream")
        })?;
        state.observe(&value)?;
    }
    if !state.completed {
        return Err(ChatError::upstream(
            "Codex event stream ended before completion",
        ));
    }
    if state.text.is_empty() {
        return Err(ChatError::upstream("Codex completed without output text"));
    }
    Ok(completion_value(&state))
}

pub fn completion_value(state: &CompletionState) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion",
        "created": state.created,
        "model": state.model,
        "choices": [{
            "index": 0,
            "message": {"role":"assistant", "content":state.text},
            "finish_reason": state.finish_reason,
        }],
        "usage": state.usage.value(),
    })
}

pub fn event_error(event: &Value) -> ChatError {
    let message = event
        .pointer("/response/error/message")
        .or_else(|| event.pointer("/error/message"))
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Codex response generation failed");
    ChatError::upstream(message)
}

pub fn chat_completion_id(response_id: &str) -> String {
    if response_id.starts_with("chatcmpl-") {
        response_id.to_string()
    } else if let Some(suffix) = response_id.strip_prefix("resp_") {
        format!("chatcmpl-{suffix}")
    } else {
        format!("chatcmpl-{response_id}")
    }
}

fn unix_seconds() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_ordered_deltas_and_usage() {
        let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"model\":\"gpt-5.6-sol\",\"status\":\"completed\",\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n";
        let value = aggregate_sse(body, "requested").unwrap();
        assert_eq!(value["id"], "chatcmpl-123");
        assert_eq!(value["choices"][0]["message"]["content"], "hello");
        assert_eq!(value["usage"]["total_tokens"], 6);
    }

    #[test]
    fn incomplete_is_an_upstream_error() {
        let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n\n";
        let error = aggregate_sse(body, "model").unwrap_err();
        assert!(error.message.contains("Incomplete response"));
    }

    #[test]
    fn buffered_chat_preserves_typed_terminal_failure() {
        let body = b"data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"input exceeds context window\"}}}\n\n";
        let error = aggregate_sse(body, "model").unwrap_err();
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.kind, "request_too_large");
        assert_eq!(error.message, "input exceeds context window");
    }

    #[test]
    fn rejects_failed_and_truncated_streams() {
        let failed = b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"bad generation\"}}}\n\n";
        assert!(
            aggregate_sse(failed, "model")
                .unwrap_err()
                .message
                .contains("bad generation")
        );
        let truncated =
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
        assert!(
            aggregate_sse(truncated, "model")
                .unwrap_err()
                .message
                .contains("before completion")
        );
    }
}
