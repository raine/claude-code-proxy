use std::collections::HashMap;
use std::sync::Mutex;

use super::translate::request::{ResponsesInputItem, ResponsesRequest};

const TTL_MS: u64 = 30 * 60 * 1000;
const MAX_STATES: usize = 10_000;
const MAX_SESSION_TRANSCRIPT_BYTES: u64 = 2_000_000;
const MAX_TOTAL_TRANSCRIPT_BYTES: u64 = 20_000_000;

#[derive(Clone)]
struct ContinuationState {
    response_id: String,
    prompt_signature: String,
    transcript: Vec<ResponsesInputItem>,
    transcript_bytes: u64,
    updated_at: u64,
}

static STATES: Mutex<Option<HashMap<String, ContinuationState>>> = Mutex::new(None);
static TOTAL_TRANSCRIPT_BYTES: Mutex<u64> = Mutex::new(0);

#[derive(Clone)]
pub struct ContinuationCandidate {
    pub previous_response_id: Option<String>,
    pub input_delta: Option<Vec<ResponsesInputItem>>,
    pub input_delta_count: usize,
    pub disabled_reason: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn continuation_candidate(
    session_id: Option<&str>,
    body: &ResponsesRequest,
    enabled: bool,
) -> ContinuationCandidate {
    let now = now_ms();

    if !enabled {
        return ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("disabled".to_string()),
        };
    }

    let session_id = match session_id {
        Some(s) => s,
        None => {
            return ContinuationCandidate {
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("missing_session".to_string()),
            };
        }
    };

    let state = {
        let guard = STATES.lock().unwrap();
        guard.as_ref().and_then(|m| m.get(session_id).cloned())
    };
    let state = match state {
        Some(s) if now - s.updated_at <= TTL_MS => s,
        Some(_) => {
            clear_continuation(Some(session_id));
            return ContinuationCandidate {
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("missing_state".to_string()),
            };
        }
        None => {
            return ContinuationCandidate {
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("missing_state".to_string()),
            };
        }
    };

    let signature = prompt_signature(body);
    if signature != state.prompt_signature {
        clear_continuation(Some(session_id));
        return ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: body.input.len(),
            disabled_reason: Some("prompt_changed".to_string()),
        };
    }

    let suffix = input_suffix_after_prefix(&body.input, &state.transcript);
    let suffix = match suffix {
        Some(s) => s,
        None => {
            clear_continuation(Some(session_id));
            return ContinuationCandidate {
                previous_response_id: None,
                input_delta: None,
                input_delta_count: body.input.len(),
                disabled_reason: Some("not_append_only".to_string()),
            };
        }
    };

    if suffix.is_empty() {
        return ContinuationCandidate {
            previous_response_id: None,
            input_delta: None,
            input_delta_count: 0,
            disabled_reason: Some("empty_delta".to_string()),
        };
    }

    ContinuationCandidate {
        previous_response_id: Some(state.response_id),
        input_delta: Some(suffix.clone()),
        input_delta_count: suffix.len(),
        disabled_reason: None,
    }
}

pub fn record_continuation(
    session_id: Option<&str>,
    request_body: &ResponsesRequest,
    response_id: Option<&str>,
    output_items: &[ResponsesInputItem],
) {
    let session_id = match session_id {
        Some(s) => s,
        None => return,
    };

    let response_id = match response_id {
        Some(id) => id.to_string(),
        None => {
            clear_continuation(Some(session_id));
            return;
        }
    };

    let mut transcript: Vec<ResponsesInputItem> = request_body.input.clone();
    transcript.extend_from_slice(output_items);

    let transcript_json = serde_json::to_string(&transcript).unwrap_or_default();
    let transcript_bytes = transcript_json.len() as u64;

    if transcript_bytes > MAX_SESSION_TRANSCRIPT_BYTES {
        clear_continuation(Some(session_id));
        return;
    }

    clear_continuation(Some(session_id));

    let state = ContinuationState {
        response_id,
        prompt_signature: prompt_signature(request_body),
        transcript,
        transcript_bytes,
        updated_at: now_ms(),
    };

    {
        let mut guard = TOTAL_TRANSCRIPT_BYTES.lock().unwrap();
        *guard += transcript_bytes;
    }
    {
        let mut guard = STATES.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(session_id.to_string(), state);
    }
    evict_oldest();
}

pub fn clear_continuation(session_id: Option<&str>) {
    let session_id = match session_id {
        Some(s) => s,
        None => return,
    };
    let mut guard = STATES.lock().unwrap();
    if let Some(map) = guard.as_mut()
        && let Some(existing) = map.remove(session_id)
    {
        let mut bytes_guard = TOTAL_TRANSCRIPT_BYTES.lock().unwrap();
        *bytes_guard = bytes_guard.saturating_sub(existing.transcript_bytes);
    }
}

pub fn has_continuation_for_tests(session_id: &str) -> bool {
    let guard = STATES.lock().unwrap();
    guard.as_ref().is_some_and(|m| m.contains_key(session_id))
}

pub fn clear_all_continuations_for_tests() {
    let mut guard = STATES.lock().unwrap();
    *guard = None;
    let mut bytes_guard = TOTAL_TRANSCRIPT_BYTES.lock().unwrap();
    *bytes_guard = 0;
}

fn input_suffix_after_prefix(
    input: &[ResponsesInputItem],
    prefix: &[ResponsesInputItem],
) -> Option<Vec<ResponsesInputItem>> {
    if prefix.len() > input.len() {
        return None;
    }
    for i in 0..prefix.len() {
        let a = serde_json::to_value(&input[i]).unwrap_or_default();
        let b = serde_json::to_value(&prefix[i]).unwrap_or_default();
        if a != b {
            return None;
        }
    }
    Some(input[prefix.len()..].to_vec())
}

fn prompt_signature(body: &ResponsesRequest) -> String {
    let value = serde_json::to_value(body).unwrap_or_default();
    let obj = match value.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let mut entries: Vec<(&String, &serde_json::Value)> =
        obj.iter().filter(|(k, _)| *k != "input").collect();
    entries.sort_by_key(|(a, _)| *a);
    let mut sig = String::from("{");
    for (i, (key, val)) in entries.iter().enumerate() {
        if i > 0 {
            sig.push(',');
        }
        sig.push_str(&format!("\"{}\":{}", key, stable_json(val)));
    }
    sig.push('}');
    sig
}

fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(stable_json).collect();
            format!("[{}]", items.join(","))
        }
        serde_json::Value::Object(obj) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
            entries.sort_by_key(|(a, _)| *a);
            let items: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        stable_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

fn evict_oldest() {
    let mut guard = STATES.lock().unwrap();
    let map = match guard.as_mut() {
        Some(m) => m,
        None => return,
    };
    let mut bytes_guard = TOTAL_TRANSCRIPT_BYTES.lock().unwrap();
    while map.len() > MAX_STATES || *bytes_guard > MAX_TOTAL_TRANSCRIPT_BYTES {
        let key = map.keys().next().cloned();
        match key {
            Some(k) => {
                if let Some(existing) = map.remove(&k) {
                    *bytes_guard = bytes_guard.saturating_sub(existing.transcript_bytes);
                }
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request_with_input(
        input: Vec<ResponsesInputItem>,
        extra: Option<serde_json::Value>,
    ) -> ResponsesRequest {
        let mut fields = serde_json::Map::new();
        fields.insert("model".into(), json!("gpt-5.5"));
        fields.insert("input".into(), json!(input));
        fields.insert("store".into(), json!(false));
        fields.insert("stream".into(), json!(true));
        fields.insert("text".into(), json!({"verbosity": "low"}));
        fields.insert("parallel_tool_calls".into(), json!(true));
        if let Some(extras) = extra
            && let Some(obj) = extras.as_object()
        {
            for (k, v) in obj {
                fields.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(serde_json::Value::Object(fields)).unwrap()
    }

    #[test]
    fn continuation_behaviors() {
        // All tests run in sequence to avoid global state interference

        // disabled_when_not_enabled
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input, None);
        let result = continuation_candidate(Some("s1"), &req, false);
        assert_eq!(result.disabled_reason, Some("disabled".to_string()));
        assert_eq!(result.input_delta_count, 1);

        // missing_session
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input, None);
        let result = continuation_candidate(None, &req, true);
        assert_eq!(result.disabled_reason, Some("missing_session".to_string()));

        // uses_previous_response_id_for_append_only
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input, None);
        record_continuation(Some("s1"), &req, Some("resp_1"), &[]);

        let input2 = vec![
            ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "one".to_string(),
                    },
                ],
            },
            ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "two".to_string(),
                    },
                ],
            },
        ];
        let req2 = request_with_input(input2, None);
        let result = continuation_candidate(Some("s1"), &req2, true);
        assert_eq!(result.previous_response_id, Some("resp_1".to_string()));
        assert_eq!(result.input_delta_count, 1);

        // clears_state_when_prompt_signature_changes
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input.clone(), None);
        record_continuation(Some("s1"), &req, Some("resp_1"), &[]);

        let req2 = request_with_input(input, Some(json!({"service_tier": "flex"})));
        let result = continuation_candidate(Some("s1"), &req2, true);
        assert_eq!(result.disabled_reason, Some("prompt_changed".to_string()));
        assert!(!has_continuation_for_tests("s1"));

        // clears_state_when_missing_response_id
        clear_all_continuations_for_tests();
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![
                super::super::translate::request::ResponsesContentPart::InputText {
                    text: "one".to_string(),
                },
            ],
        }];
        let req = request_with_input(input.clone(), None);
        record_continuation(Some("s1"), &req, Some("resp_1"), &[]);
        assert!(has_continuation_for_tests("s1"));

        record_continuation(Some("s1"), &req, None, &[]);
        assert!(!has_continuation_for_tests("s1"));
    }
}
