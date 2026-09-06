use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::providers::codex::translate::{
    model_allowlist::{
        allowed_models_display, assert_allowed_model, resolve_model_request, uses_responses_lite,
    },
    request::{Effort, resolve_effort_override, to_codex_effort},
};
use crate::{config, registry::normalize_incoming_model};

use super::ChatError;

const SUPPORTED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "reasoning_effort",
    "response_format",
    "temperature",
    "top_p",
    "user",
];

#[derive(Debug, Clone)]
pub struct TranslatedRequest {
    pub upstream: Value,
    pub requested_model: String,
    pub model: String,
    pub effort: Option<String>,
    pub stream: bool,
    pub include_usage: bool,
    pub use_responses_lite: bool,
}

pub fn translate_request(body: Value) -> Result<TranslatedRequest, ChatError> {
    translate_request_with_override(body, config::codex_effort().as_deref())
}

fn translate_request_with_override(
    body: Value,
    effort_override: Option<&str>,
) -> Result<TranslatedRequest, ChatError> {
    let object = body
        .as_object()
        .ok_or_else(|| ChatError::invalid("Request body must be a JSON object", None, None))?;
    reject_unsupported_fields(object)?;

    let requested_model = required_string(object, "model")?;
    let normalized = normalize_incoming_model(&requested_model);
    let resolved = resolve_model_request(&normalized);
    assert_allowed_model(&resolved.model).map_err(|error| {
        ChatError::invalid(
            format!(
                "Model '{requested_model}' resolves to unsupported model '{}'. Supported: {}",
                error.model,
                allowed_models_display()
            ),
            Some("model"),
            Some("model_not_supported"),
        )
    })?;
    let use_responses_lite = uses_responses_lite(&resolved.model);

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ChatError::invalid("Missing or invalid 'messages'", Some("messages"), None)
        })?;
    if messages.is_empty() {
        return Err(ChatError::invalid(
            "'messages' must contain at least one message",
            Some("messages"),
            None,
        ));
    }
    let input = messages
        .iter()
        .enumerate()
        .map(translate_message)
        .collect::<Result<Vec<_>, _>>()?;

    let stream = optional_bool(object, "stream")?.unwrap_or(false);
    let include_usage = translate_stream_options(object.get("stream_options"))?;
    if include_usage && !stream {
        return Err(ChatError::invalid(
            "'stream_options' is only supported when 'stream' is true",
            Some("stream_options"),
            None,
        ));
    }

    let request_effort = match object.get("reasoning_effort") {
        None | Some(Value::Null) => Some(Effort::Medium),
        Some(Value::String(value)) => parse_effort(value)?,
        Some(_) => {
            return Err(ChatError::invalid(
                "'reasoning_effort' must be a string",
                Some("reasoning_effort"),
                None,
            ));
        }
    };
    let effort = resolve_effort_override(request_effort, effort_override).map_err(|error| {
        ChatError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            error.to_string(),
            None,
            None,
        )
    })?;

    let mut text = Map::from_iter([("verbosity".to_string(), json!("low"))]);
    if let Some(format) = translate_response_format(object.get("response_format"))? {
        text.insert("format".to_string(), format);
    }

    let mut upstream = Map::from_iter([
        ("model".to_string(), json!(&resolved.model)),
        ("input".to_string(), Value::Array(input)),
        ("store".to_string(), json!(false)),
        ("stream".to_string(), json!(true)),
        ("parallel_tool_calls".to_string(), json!(false)),
        ("client_metadata".to_string(), json!({"lite":"true"})),
        ("text".to_string(), Value::Object(text)),
    ]);
    if let Some(tier) = resolved.service_tier {
        upstream.insert(
            "service_tier".to_string(),
            serde_json::to_value(tier).unwrap(),
        );
    }
    let mut reasoning = Map::from_iter([("context".to_string(), json!("all_turns"))]);
    if let Some(effort) = effort.as_ref().filter(|effort| **effort != Effort::None) {
        reasoning.insert("effort".to_string(), json!(effort));
    }
    upstream.insert("reasoning".to_string(), Value::Object(reasoning));

    for param in ["temperature", "top_p"] {
        if let Some(value) = object.get(param).filter(|value| !value.is_null()) {
            if use_responses_lite {
                return Err(ChatError::unsupported(param));
            }
            validate_sampling_value(param, value)?;
            upstream.insert(param.to_string(), value.clone());
        }
    }
    if let Some(user) = object.get("user").filter(|value| !value.is_null()) {
        let user = user
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 64)
            .ok_or_else(|| {
                ChatError::invalid(
                    "'user' must be a non-empty string of at most 64 bytes",
                    Some("user"),
                    None,
                )
            })?;
        upstream.insert("safety_identifier".to_string(), json!(user));
    }

    Ok(TranslatedRequest {
        upstream: Value::Object(upstream),
        requested_model,
        model: resolved.model,
        effort: effort
            .map(|effort| effort.to_string())
            .filter(|effort| effort != "none"),
        stream,
        include_usage,
        use_responses_lite,
    })
}

fn reject_unsupported_fields(object: &Map<String, Value>) -> Result<(), ChatError> {
    for key in object.keys() {
        if !SUPPORTED_FIELDS.contains(&key.as_str()) {
            return Err(ChatError::unsupported(key));
        }
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, key: &'static str) -> Result<String, ChatError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ChatError::invalid(format!("Missing or invalid '{key}'"), Some(key), None))
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<bool>, ChatError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ChatError::invalid(
            format!("'{key}' must be a boolean"),
            Some(key),
            None,
        )),
    }
}

fn translate_message((index, message): (usize, &Value)) -> Result<Value, ChatError> {
    let param = format!("messages[{index}]");
    let object = message
        .as_object()
        .ok_or_else(|| ChatError::invalid("Each message must be an object", Some(&param), None))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "role" | "content" | "name") {
            return Err(ChatError::unsupported(format!("{param}.{key}")));
        }
    }
    let role = object.get("role").and_then(Value::as_str).ok_or_else(|| {
        ChatError::invalid(
            "Each message requires a role",
            Some(&format!("{param}.role")),
            None,
        )
    })?;
    let role = match role {
        "system" | "developer" => "developer",
        "user" => "user",
        "assistant" => "assistant",
        _ => {
            return Err(ChatError::invalid(
                format!("Unsupported message role: {role}"),
                Some(&format!("{param}.role")),
                Some("unsupported_value"),
            ));
        }
    };
    let parts = translate_content(object.get("content"), index)?;
    Ok(json!({"type":"message", "role":role, "content":parts}))
}

fn translate_content(content: Option<&Value>, index: usize) -> Result<Vec<Value>, ChatError> {
    let param = format!("messages[{index}].content");
    let parts = match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type":"input_text", "text":text})]
        }
        Some(Value::Array(parts)) if !parts.is_empty() => parts
            .iter()
            .enumerate()
            .map(|(part_index, part)| {
                let part_param = format!("{param}[{part_index}]");
                let object = part.as_object().ok_or_else(|| {
                    ChatError::invalid(
                        "Message content parts must be objects",
                        Some(&part_param),
                        None,
                    )
                })?;
                if object.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(ChatError::invalid(
                        "Only text message content is supported",
                        Some(&format!("{part_param}.type")),
                        Some("unsupported_value"),
                    ));
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        ChatError::invalid(
                            "Text content must not be empty",
                            Some(&format!("{part_param}.text")),
                            None,
                        )
                    })?;
                Ok(json!({"type":"input_text", "text":text}))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(ChatError::invalid(
                "Message content must contain text",
                Some(&param),
                None,
            ));
        }
    };
    Ok(parts)
}

fn parse_effort(value: &str) -> Result<Option<Effort>, ChatError> {
    if value == "none" {
        return Ok(Some(Effort::None));
    }
    to_codex_effort(Some(value)).map(Some).ok_or_else(|| {
        ChatError::invalid(
            format!(
                "Invalid reasoning effort '{value}'. Supported: none, low, medium, high, xhigh, max"
            ),
            Some("reasoning_effort"),
            Some("unsupported_value"),
        )
    })
}

fn translate_response_format(value: Option<&Value>) -> Result<Option<Value>, ChatError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value.as_object().ok_or_else(|| {
        ChatError::invalid(
            "'response_format' must be an object",
            Some("response_format"),
            None,
        )
    })?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => Ok(None),
        Some("json_object") => Ok(Some(json!({"type":"json_object"}))),
        Some("json_schema") => {
            let format = object
                .get("json_schema")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ChatError::invalid(
                        "'response_format.json_schema' must be an object",
                        Some("response_format.json_schema"),
                        None,
                    )
                })?;
            let name = format
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ChatError::invalid(
                        "JSON Schema output requires a name",
                        Some("response_format.json_schema.name"),
                        None,
                    )
                })?;
            let schema = format
                .get("schema")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| {
                    ChatError::invalid(
                        "JSON Schema output requires an object schema",
                        Some("response_format.json_schema.schema"),
                        None,
                    )
                })?;
            let strict = match format.get("strict") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(value)) => Some(*value),
                Some(_) => {
                    return Err(ChatError::invalid(
                        "JSON Schema strict must be a boolean",
                        Some("response_format.json_schema.strict"),
                        None,
                    ));
                }
            };
            let mut translated = Map::from_iter([
                ("type".to_string(), json!("json_schema")),
                ("name".to_string(), json!(name)),
                ("schema".to_string(), schema.clone()),
            ]);
            if let Some(strict) = strict {
                translated.insert("strict".to_string(), json!(strict));
            }
            Ok(Some(Value::Object(translated)))
        }
        Some(kind) => Err(ChatError::invalid(
            format!("Unsupported response format: {kind}"),
            Some("response_format.type"),
            Some("unsupported_value"),
        )),
        None => Err(ChatError::invalid(
            "'response_format.type' is required",
            Some("response_format.type"),
            None,
        )),
    }
}

fn translate_stream_options(value: Option<&Value>) -> Result<bool, ChatError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(false);
    };
    let object = value.as_object().ok_or_else(|| {
        ChatError::invalid(
            "'stream_options' must be an object",
            Some("stream_options"),
            None,
        )
    })?;
    for key in object.keys() {
        if key != "include_usage" {
            return Err(ChatError::unsupported(format!("stream_options.{key}")));
        }
    }
    match object.get("include_usage") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ChatError::invalid(
            "'stream_options.include_usage' must be a boolean",
            Some("stream_options.include_usage"),
            None,
        )),
    }
}

fn validate_sampling_value(param: &'static str, value: &Value) -> Result<(), ChatError> {
    let number = value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| {
            ChatError::invalid(format!("'{param}' must be a number"), Some(param), None)
        })?;
    let valid = match param {
        "temperature" => (0.0..=2.0).contains(&number),
        _ => (0.0..=1.0).contains(&number),
    };
    if valid {
        Ok(())
    } else {
        Err(ChatError::invalid(
            format!("'{param}' is out of range"),
            Some(param),
            Some("invalid_value"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({"model":"gpt-5.6-sol","messages":[{"role":"system","content":"rules"},{"role":"user","content":[{"type":"text","text":"hello"}]}]})
    }

    #[test]
    fn translates_messages_and_responses_lite_fields() {
        let translated = translate_request(base()).unwrap();
        assert_eq!(translated.upstream["input"][0]["role"], "developer");
        assert_eq!(
            translated.upstream["input"][1]["content"][0]["text"],
            "hello"
        );
        assert_eq!(translated.upstream["store"], false);
        assert_eq!(translated.upstream["stream"], true);
        assert_eq!(translated.upstream["reasoning"]["effort"], "medium");
        assert_eq!(translated.upstream["reasoning"]["context"], "all_turns");
    }

    #[test]
    fn none_effort_retains_context() {
        let mut body = base();
        body["reasoning_effort"] = json!("none");
        let translated = translate_request(body).unwrap();
        assert!(translated.upstream["reasoning"].get("effort").is_none());
        assert_eq!(translated.upstream["reasoning"]["context"], "all_turns");
    }

    #[test]
    fn translates_strict_json_schema() {
        let mut body = base();
        body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"answer","strict":true,"schema":{"type":"object"}}});
        let translated = translate_request(body).unwrap();
        assert_eq!(translated.upstream["text"]["format"]["name"], "answer");
        assert_eq!(translated.upstream["text"]["format"]["strict"], true);
    }

    #[test]
    fn rejects_empty_content_and_unsupported_controls() {
        let mut empty = base();
        empty["messages"][0]["content"] = json!("");
        assert_eq!(
            translate_request(empty).unwrap_err().param.as_deref(),
            Some("messages[0].content")
        );
        let mut tokens = base();
        tokens["max_tokens"] = json!(100);
        let error = translate_request(tokens).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("unsupported_parameter"));
        assert_eq!(error.param.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn requires_model_and_nonempty_messages() {
        assert_eq!(
            translate_request(json!({"messages":[{"role":"user","content":"hello"}]}))
                .unwrap_err()
                .param
                .as_deref(),
            Some("model")
        );
        assert_eq!(
            translate_request(json!({"model":"gpt-5.6-sol","messages":[]}))
                .unwrap_err()
                .param
                .as_deref(),
            Some("messages")
        );
    }

    #[test]
    fn accepts_all_efforts_and_forced_override_wins() {
        for effort in ["none", "low", "medium", "high", "xhigh", "max"] {
            let mut body = base();
            body["reasoning_effort"] = json!(effort);
            assert!(translate_request_with_override(body, None).is_ok());
        }
        let mut body = base();
        body["reasoning_effort"] = json!("low");
        let translated = translate_request_with_override(body, Some("high")).unwrap();
        assert_eq!(translated.upstream["reasoning"]["effort"], "high");
        assert_eq!(translated.effort.as_deref(), Some("high"));
    }

    #[test]
    fn validates_stream_options_and_sampling_controls() {
        let mut options = base();
        options["stream_options"] = json!({"include_usage":true});
        assert_eq!(
            translate_request(options).unwrap_err().param.as_deref(),
            Some("stream_options")
        );

        let mut lite = base();
        lite["temperature"] = json!(0.2);
        assert_eq!(
            translate_request(lite).unwrap_err().code.as_deref(),
            Some("unsupported_parameter")
        );

        let mut full = base();
        full["model"] = json!("gpt-5.4");
        full["temperature"] = json!(0.2);
        full["top_p"] = json!(0.9);
        let translated = translate_request(full).unwrap();
        assert_eq!(translated.upstream["temperature"], 0.2);
        assert_eq!(translated.upstream["top_p"], 0.9);
    }
}
