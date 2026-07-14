use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model_allowlist::{KIMI_DEFAULT_MODEL, assert_allowed_model, resolve_model};
use crate::anthropic::schema::MessagesRequest;
use crate::providers::translate_shared::{
    ContentBlock, flatten_system_text, image_block_to_url, image_source_to_url, normalize_content,
    read_effort,
};

// ---------------------------------------------------------------------------
// Kimi OpenAI-compatible chat-completions types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiChatRequest {
    pub model: String,
    pub messages: Vec<KimiMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<KimiTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<KimiToolChoice>,
    pub stream: bool,
    pub stream_options: KimiStreamOptions,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<KimiThinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiStreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiThinking {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KimiToolChoice {
    Auto,
    None,
    Required,
    Function {
        #[serde(rename = "type")]
        kind: String,
        function: KimiToolChoiceFunction,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KimiMessage {
    System {
        role: String,
        content: String,
    },
    User {
        role: String,
        content: serde_json::Value,
    },
    Assistant {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<KimiAssistantToolCall>>,
    },
    Tool {
        role: String,
        tool_call_id: String,
        content: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiAssistantToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: KimiToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: KimiToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiToolFunction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

pub struct TranslateOptions {
    pub session_id: Option<String>,
}

const DEFAULT_MAX_TOKENS: u32 = 32000;

// ---------------------------------------------------------------------------
// Translation entry point
// ---------------------------------------------------------------------------

pub fn translate_request(
    req: &MessagesRequest,
    opts: TranslateOptions,
) -> Result<KimiChatRequest, anyhow::Error> {
    let model = req.model.as_deref().unwrap_or(KIMI_DEFAULT_MODEL);
    let resolved = resolve_model(model);
    assert_allowed_model(&resolved).map_err(|e| anyhow::anyhow!("{e}"))?;

    let messages = build_messages(req)?;
    let tools = read_tools(req)?;
    let tool_choice = read_tool_choice(req)?;

    let mut out = KimiChatRequest {
        model: resolved,
        messages,
        stream: true,
        stream_options: KimiStreamOptions {
            include_usage: true,
        },
        max_tokens: clamp_max_tokens(req.max_tokens),
        reasoning_effort: Some(map_reasoning_effort(read_effort(req)?)),
        thinking: Some(KimiThinking {
            kind: "enabled".to_string(),
        }),
        tools: if tools.is_empty() { None } else { Some(tools) },
        tool_choice,
        prompt_cache_key: opts.session_id,
    };

    // Collapse auto tool_choice to None (default behavior)
    if matches!(out.tool_choice, Some(KimiToolChoice::Auto)) {
        out.tool_choice = None;
    }

    Ok(out)
}

fn clamp_max_tokens(requested: Option<u32>) -> u32 {
    match requested {
        Some(v) if v > 0 => v.min(DEFAULT_MAX_TOKENS),
        _ => DEFAULT_MAX_TOKENS,
    }
}

fn map_reasoning_effort(effort: Option<&str>) -> String {
    match effort {
        Some("max" | "xhigh") => "high".to_string(),
        Some(v) => v.to_string(),
        None => "medium".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tool & tool_choice reading
// ---------------------------------------------------------------------------

fn map_tool_choice(choice: &serde_json::Map<String, Value>) -> KimiToolChoice {
    match choice.get("type").and_then(|v| v.as_str()) {
        Some("auto") => KimiToolChoice::Auto,
        Some("none") => KimiToolChoice::None,
        Some("any") => KimiToolChoice::Required,
        Some("tool") => {
            if let Some(name) = choice.get("name").and_then(|v| v.as_str()) {
                KimiToolChoice::Function {
                    kind: "function".to_string(),
                    function: KimiToolChoiceFunction {
                        name: name.to_string(),
                    },
                }
            } else {
                KimiToolChoice::Required
            }
        }
        _ => KimiToolChoice::Auto,
    }
}

fn read_tool_choice(req: &MessagesRequest) -> Result<Option<KimiToolChoice>, anyhow::Error> {
    match req.extra.get("tool_choice") {
        Some(Value::Object(choice)) => Ok(Some(map_tool_choice(choice))),
        Some(Value::String(s)) => Ok(Some(match s.as_str() {
            "auto" => KimiToolChoice::Auto,
            "none" => KimiToolChoice::None,
            "any" | "required" => KimiToolChoice::Required,
            _ => KimiToolChoice::Auto,
        })),
        _ => Ok(None),
    }
}

fn read_tools(req: &MessagesRequest) -> Result<Vec<KimiTool>, anyhow::Error> {
    let Some(tools) = req.extra.get("tools") else {
        return Ok(Vec::new());
    };
    let tools_arr = match tools {
        Value::Array(a) => a,
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for tool in tools_arr {
        let name = tool
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let description = tool
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let parameters = tool
            .get("input_schema")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        out.push(KimiTool {
            kind: "function".to_string(),
            function: KimiToolFunction {
                name,
                description,
                parameters,
            },
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Message building
// ---------------------------------------------------------------------------

fn build_messages(req: &MessagesRequest) -> Result<Vec<KimiMessage>, anyhow::Error> {
    let mut out: Vec<KimiMessage> = Vec::new();

    // System message
    if let Some(system) = flatten_system_text(req.extra.get("system")) {
        out.push(KimiMessage::System {
            role: "system".to_string(),
            content: system,
        });
    }

    // Convert each message
    for msg in &req.messages {
        let blocks = normalize_content(&msg.content, serde_json::json!({}));
        match msg.role.as_str() {
            "user" => push_user_messages(&mut out, &blocks),
            "assistant" => push_assistant_message(&mut out, &blocks),
            other => {
                anyhow::bail!("unexpected message role: {other}");
            }
        }
    }

    Ok(out)
}

fn push_user_messages(out: &mut Vec<KimiMessage>, blocks: &[ContentBlock]) {
    let mut buffer: Vec<KimiUserContentPart> = Vec::new();

    let flush_buffer = |out: &mut Vec<KimiMessage>, buffer: &mut Vec<KimiUserContentPart>| {
        if buffer.is_empty() {
            return;
        }
        let all_text = buffer
            .iter()
            .all(|p| matches!(p, KimiUserContentPart::Text { .. }));
        if all_text {
            let joined: String = buffer
                .iter()
                .map(|p| match p {
                    KimiUserContentPart::Text { text } => text.as_str(),
                    _ => "",
                })
                .collect();
            out.push(KimiMessage::User {
                role: "user".to_string(),
                content: Value::String(joined),
            });
        } else {
            let parts: Vec<KimiUserContentPart> = std::mem::take(buffer);
            out.push(KimiMessage::User {
                role: "user".to_string(),
                content: serde_json::to_value(parts).unwrap_or_default(),
            });
            return;
        }
        buffer.clear();
    };

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                buffer.push(KimiUserContentPart::Text { text: text.clone() });
            }
            ContentBlock::Image { source } => {
                buffer.push(KimiUserContentPart::ImageUrl {
                    image_url: KimiImageUrl {
                        url: image_source_to_url(source),
                    },
                });
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                // flush any buffered user content first
                let mut temp = Vec::new();
                std::mem::swap(&mut buffer, &mut temp);
                flush_buffer(out, &mut temp);

                out.push(KimiMessage::Tool {
                    role: "tool".to_string(),
                    tool_call_id: tool_use_id.clone(),
                    content: tool_result_content(content, *is_error),
                });
            }
            _ => {}
        }
    }

    // flush remaining buffer
    flush_buffer(out, &mut buffer);
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum KimiUserContentPart {
    Text { text: String },
    ImageUrl { image_url: KimiImageUrl },
}

#[derive(Debug, Clone, Serialize)]
struct KimiImageUrl {
    url: String,
}

fn tool_result_content(content: &Value, is_error: Option<bool>) -> Value {
    let prefix = if is_error.unwrap_or(false) {
        "[tool execution error]\n"
    } else {
        ""
    };

    match content {
        Value::String(s) => Value::String(format!("{prefix}{s}")),
        Value::Array(arr) => {
            let mut parts: Vec<KimiToolResultPart> = Vec::new();
            if !prefix.is_empty() {
                parts.push(KimiToolResultPart::Text {
                    text: prefix.to_string(),
                });
            }
            for b in arr {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        let text = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        parts.push(KimiToolResultPart::Text {
                            text: text.to_string(),
                        });
                    }
                    Some("image") => {
                        let url = image_block_to_url(b);
                        parts.push(KimiToolResultPart::ImageUrl {
                            image_url: KimiImageUrl { url },
                        });
                    }
                    Some(other) => {
                        parts.push(KimiToolResultPart::Text {
                            text: format!("[unsupported content block omitted: {other}]"),
                        });
                    }
                    None => {}
                }
            }

            // Collapse to string when only one text part
            if parts.len() == 1
                && let KimiToolResultPart::Text { text } = &parts[0]
            {
                return Value::String(text.clone());
            }

            serde_json::to_value(parts).unwrap_or(Value::String(prefix.to_string()))
        }
        _ => Value::String(prefix.to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum KimiToolResultPart {
    Text { text: String },
    ImageUrl { image_url: KimiImageUrl },
}

fn push_assistant_message(out: &mut Vec<KimiMessage>, blocks: &[ContentBlock]) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<KimiAssistantToolCall> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    text_parts.push(text.clone());
                }
            }
            ContentBlock::Thinking { thinking, .. } => {
                if !thinking.is_empty() {
                    thinking_parts.push(thinking.clone());
                }
            }
            ContentBlock::ToolUse { id, name, input } => {
                let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(KimiAssistantToolCall {
                    id: id.clone(),
                    kind: "function".to_string(),
                    function: KimiToolCallFunction {
                        name: name.clone(),
                        arguments: args,
                    },
                });
            }
            // Image blocks from assistant are dropped
            _ => {}
        }
    }

    if text_parts.is_empty() && tool_calls.is_empty() && thinking_parts.is_empty() {
        return;
    }

    let content = if text_parts.is_empty() {
        Some(String::new())
    } else {
        Some(text_parts.join(""))
    };

    let reasoning_content = if thinking_parts.is_empty() {
        None
    } else {
        Some(thinking_parts.join("\n\n"))
    };

    let tool_calls_val = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    out.push(KimiMessage::Assistant {
        role: "assistant".to_string(),
        content,
        reasoning_content,
        tool_calls: tool_calls_val,
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translate_text_request_defaults_like_reference() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "haiku",
            "max_tokens": 10,
            "system": "sys",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"name":"search","description":"Search","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool", "name":"search"},
            "output_config": {"effort":"max"}
        }))
        .unwrap();
        let translated = translate_request(
            &req,
            TranslateOptions {
                session_id: Some("sid".into()),
            },
        )
        .unwrap();
        assert_eq!(translated.model, "kimi-for-coding");
        assert_eq!(translated.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(translated.prompt_cache_key.as_deref(), Some("sid"));
        assert_eq!(translated.max_tokens, 10);
    }

    #[test]
    fn translate_tool_result_with_unsupported_blocks() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-k2",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [
                        {"type": "text", "text": "visible output"},
                        {"type": "thinking", "thinking": "hidden thought"}
                    ]
                }]
            }]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        // Should have one tool message
        assert_eq!(translated.messages.len(), 1);
        match &translated.messages[0] {
            KimiMessage::Tool {
                role,
                tool_call_id,
                content,
            } => {
                assert_eq!(role, "tool");
                assert_eq!(tool_call_id, "toolu_1");
                // content should be an array with text parts
                let parts: Vec<&Value> = match content {
                    Value::Array(a) => a.iter().collect(),
                    _ => panic!("expected array content"),
                };
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[0].get("text").and_then(|v| v.as_str()),
                    Some("visible output")
                );
                assert_eq!(
                    parts[1].get("text").and_then(|v| v.as_str()),
                    Some("[unsupported content block omitted: thinking]")
                );
            }
            _ => panic!("expected Tool message"),
        }
    }

    #[test]
    fn translate_tool_result_with_image() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-k2",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [
                        {"type": "text", "text": "caption"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}
                    ]
                }]
            }]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.messages.len(), 1);
        match &translated.messages[0] {
            KimiMessage::Tool {
                role,
                tool_call_id,
                content,
            } => {
                assert_eq!(role, "tool");
                assert_eq!(tool_call_id, "toolu_1");
                let parts: Vec<&Value> = match content {
                    Value::Array(a) => a.iter().collect(),
                    _ => panic!("expected array"),
                };
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[1]
                        .get("image_url")
                        .and_then(|u| u.get("url"))
                        .and_then(|v| v.as_str()),
                    Some("data:image/png;base64,abc")
                );
            }
            _ => panic!("expected Tool message"),
        }
    }

    #[test]
    fn translate_assistant_with_thinking_tool_use_and_text() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "let me think..."},
                    {"type": "text", "text": "here's the answer"},
                    {"type": "tool_use", "id": "tu_1", "name": "search", "input": {"q": "rust"}}
                ]
            }]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.messages.len(), 1);
        match &translated.messages[0] {
            KimiMessage::Assistant {
                role,
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(role, "assistant");
                assert_eq!(content.as_deref(), Some("here's the answer"));
                assert_eq!(reasoning_content.as_deref(), Some("let me think..."));
                assert!(tool_calls.is_some());
                let tcs = tool_calls.as_ref().unwrap();
                assert_eq!(tcs.len(), 1);
                assert_eq!(tcs[0].function.name, "search");
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn translate_empty_assistant_content_emits_empty_string() {
        // When there are no text blocks and no tool calls but there is thinking
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "thinking..."}
                ]
            }]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.messages.len(), 1);
        match &translated.messages[0] {
            KimiMessage::Assistant {
                content,
                reasoning_content,
                ..
            } => {
                assert_eq!(content.as_deref(), Some(""));
                assert_eq!(reasoning_content.as_deref(), Some("thinking..."));
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn translate_user_text_and_image_collapse_correctly() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe this"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "xyz"}}
                ]
            }]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.messages.len(), 1);
        match &translated.messages[0] {
            KimiMessage::User { role, content } => {
                assert_eq!(role, "user");
                // Mixed text+image produces array, not string
                assert!(content.is_array());
                let parts = content.as_array().unwrap();
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[0].get("text").and_then(|v| v.as_str()),
                    Some("describe this")
                );
                assert!(parts[1].get("image_url").is_some());
            }
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn translate_text_only_user_collapses_to_string() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{
                "role": "user",
                "content": "hello"
            }]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        match &translated.messages[0] {
            KimiMessage::User { role, content } => {
                assert_eq!(role, "user");
                assert_eq!(content.as_str(), Some("hello"));
            }
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn max_tokens_defaults_to_32000() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.max_tokens, 32000);
    }

    #[test]
    fn max_tokens_clamps_at_32000() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "max_tokens": 99999,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.max_tokens, 32000);
    }

    #[test]
    fn invalid_effort_rejected() {
        let req: Result<MessagesRequest, _> = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "output_config": {"effort": "extreme"}
        }));
        // The serde flatten extra captures it, so it parses. Error comes at translate time.
        let req = req.unwrap();
        let result = translate_request(&req, TranslateOptions { session_id: None });
        assert!(result.is_err());
    }

    #[test]
    fn effort_xhigh_maps_to_high() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert_eq!(translated.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn auto_tool_choice_is_collapsed() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "auto"}
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert!(translated.tool_choice.is_none());
    }

    #[test]
    fn any_tool_choice_becomes_required() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name":"search","input_schema":{"type":"object"}}],
            "tool_choice": {"type": "any"}
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        assert!(matches!(
            translated.tool_choice,
            Some(KimiToolChoice::Required)
        ));
    }

    #[test]
    fn system_text_excludes_billing_headers() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "kimi-for-coding",
            "system": [
                {"type": "text", "text": "You are a helpful assistant."},
                {"type": "text", "text": "x-anthropic-billing-header: secret"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let translated = translate_request(&req, TranslateOptions { session_id: None }).unwrap();
        let system_msg = translated
            .messages
            .iter()
            .find(|m| matches!(m, KimiMessage::System { .. }));
        assert!(system_msg.is_some());
        if let Some(KimiMessage::System { content, .. }) = system_msg {
            assert!(!content.contains("x-anthropic-billing-header"));
            assert!(content.contains("helpful"));
        }
    }
}
