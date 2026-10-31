use crate::{
    anthropic::schema::MessagesRequest,
    providers::codex::translate::request::{
        ResponsesInputItem, translate_openai_compatible_request,
    },
};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::HashSet;

const FORBIDDEN: &[&str] = &[
    "background",
    "conversation",
    "max_output_tokens",
    "max_tool_calls",
    "moderation",
    "multi_agent",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "top_logprobs",
    "truncation",
    "user",
    "previous_response_id",
    "client_metadata",
    "service_tier",
    "additional_tools",
    "input",
    "instructions",
    "store",
];

pub(super) struct Prepared {
    pub body: Value,
    pub estimated_tokens: u64,
    pub tool_names: HashSet<String>,
}

/// Only structural counts and fixed categories; never prompts, names or arguments.
pub(super) fn request_shape(body: &Value) -> Value {
    let input = body["input"].as_array().cloned().unwrap_or_default();
    let tools: Vec<_> = input
        .iter()
        .filter(|v| v["type"] == "additional_tools")
        .filter_map(|v| v["tools"].as_array())
        .flatten()
        .collect();
    let count = |kind: &str| input.iter().filter(|v| v["type"] == kind).count();
    let role_count = |role: &str| input.iter().filter(|v| v["role"] == role).count();
    let choice = match body["tool_choice"]
        .as_str()
        .or_else(|| body["tool_choice"]["type"].as_str())
    {
        Some("auto") => "auto",
        Some("none") => "none",
        Some("required") => "required",
        Some("function") => "function",
        None => "absent",
        _ => "other",
    };
    // A caller-provided unknown model must not become arbitrary diagnostic text.
    let model = body["model"].as_str().filter(|s| {
        s.len() <= 128
            && s.starts_with("gpt-")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
    });
    json!({"model":model,"store":body["store"].as_bool(),"stream":body["stream"].as_bool(),
        "body_bytes":body.to_string().len(),"input_count":input.len(),"instructions_bytes":body["instructions"].as_str().map(str::len).unwrap_or(0),
        "message_count":count("message"),"function_call_count":count("function_call"),"function_output_count":count("function_call_output"),
        "system_items":role_count("system"),"developer_items":role_count("developer"),"user_items":role_count("user"),"assistant_items":role_count("assistant"),
        "additional_tools_items":count("additional_tools"),"tool_count":tools.len(),
        "tool_schema_bytes":tools.iter().map(|v| v["parameters"].to_string().len()).sum::<usize>(),
        "top_level_tools":body.get("tools").is_some(),"tool_choice":choice})
}

pub(super) fn prepare(request: &MessagesRequest, model: String) -> Result<Prepared> {
    for key in FORBIDDEN {
        ensure!(
            !request.extra.contains_key(*key),
            "SIWC does not support request field '{key}'"
        );
    }
    // Anthropic max_tokens, metadata, temperature, top_p, top_k, stop_sequences
    // and cache controls are accepted for client compatibility, never forwarded.
    let mut request = request.clone();
    let mut tool_names = HashSet::new();
    if let Some(tools) = request.extra.get_mut("tools") {
        for tool in tools
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?
        {
            let kind = tool.get("type").and_then(Value::as_str).unwrap_or("custom");
            ensure!(
                matches!(
                    kind,
                    "custom"
                        | "function"
                        | "bash_20250124"
                        | "text_editor_20250124"
                        | "text_editor_20250429"
                ),
                "SIWC unsupported hosted tool type '{kind}'; use client-executed function tools"
            );
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                "Invalid function tool name"
            );
            ensure!(
                tool_names.insert(name.to_string()),
                "Duplicate function tool name"
            );
            ensure!(
                tool.get("input_schema").is_some_and(Value::is_object),
                "Function tool requires input_schema; implicit hosted tool schemas are unsupported"
            );
            ensure!(
                tool.get("allowed_callers").is_none_or(|v| v
                    .as_array()
                    .is_some_and(|a| a.iter().all(|v| v == "direct"))),
                "Programmatic tool calling is unsupported"
            );
            // All advertised local tools are supplied together. Claude executes them.
            if let Some(tool) = tool.as_object_mut() {
                tool.remove("defer_loading");
            }
        }
    }
    if let Some(choice) = request.extra.get("tool_choice") {
        let kind = choice
            .as_str()
            .or_else(|| choice.get("type").and_then(Value::as_str))
            .unwrap_or("");
        ensure!(
            matches!(kind, "auto" | "none" | "any" | "required" | "tool"),
            "Unsupported tool_choice"
        );
        if kind == "tool" {
            ensure!(
                choice
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| tool_names.contains(n)),
                "tool_choice references an unregistered tool"
            );
        }
    }
    validate_content(&mut request)?;
    let mut translated = translate_openai_compatible_request(&request, model, None)?;
    // Reuse only the pure translation entry point, with no Codex configuration.
    // Signed reasoning from another provider is not a portable conversation item.
    translated
        .input
        .retain(|item| !matches!(item, ResponsesInputItem::Reasoning { .. }));
    let estimated_tokens =
        crate::providers::codex::count_tokens::count_translated_tokens(&translated);
    let mut input = serde_json::to_value(&translated.input)?
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(tools) = translated.tools {
        input.insert(
            0,
            json!({"type":"additional_tools", "role":"developer", "tools":tools}),
        );
    }
    let mut body = json!({"model":translated.model,"input":input,"store":false,"stream":true,"parallel_tool_calls":translated.parallel_tool_calls});
    if let Some(instructions) = translated.instructions {
        body["instructions"] = json!(instructions);
    }
    if let Some(choice) = translated.tool_choice {
        body["tool_choice"] = serde_json::to_value(choice)?;
    }
    if let Some(reasoning) = translated.reasoning {
        body["reasoning"] = serde_json::to_value(reasoning)?;
    }
    if let Some(format) = translated.text.format {
        body["text"] = json!({"format":format});
    }
    Ok(Prepared {
        body,
        estimated_tokens,
        tool_names,
    })
}

fn validate_content(request: &mut MessagesRequest) -> Result<()> {
    let mut calls = HashSet::new();
    let mut unresolved = HashSet::new();
    for message in &mut request.messages {
        ensure!(
            matches!(
                message.role.as_str(),
                "user" | "assistant" | "system" | "developer"
            ),
            "Unsupported message role"
        );
        if message.role == "developer" {
            message.role = "system".into();
        } // Shared converter emits developer.
        if message.content.is_string() {
            continue;
        }
        let blocks = message
            .content
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("Message content must be text or an array"))?;
        for block in blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    ensure!(block["text"].is_string(), "Text block must contain text");
                }
                "thinking" | "redacted_thinking" if message.role == "assistant" => {
                    // Reasoning is not user-visible history. Preserve assistant text and tool calls.
                    if let Some(object) = block.as_object_mut() {
                        object.remove("signature");
                    }
                }
                "image" if message.role == "user" => validate_image(block, false)?,
                "tool_use" if message.role == "assistant" => {
                    let id = block["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("Tool call missing ID"))?;
                    ensure!(
                        block["name"].as_str().is_some_and(|n| !n.is_empty())
                            && block["input"].is_object(),
                        "Invalid tool call name or arguments"
                    );
                    ensure!(
                        calls.insert(id.to_string()),
                        "Duplicate tool call ID in history"
                    );
                    unresolved.insert(id.to_string());
                }
                "tool_result" if message.role == "user" => {
                    let id = block["tool_use_id"].as_str().unwrap_or("");
                    ensure!(
                        unresolved.remove(id),
                        "Tool result has no matching prior unresolved call ID"
                    );
                    if let Some(content) = block.get("content") {
                        if let Some(items) = content.as_array() {
                            for item in items {
                                match item["type"].as_str() {
                                    Some("text") if item["text"].is_string() => {}
                                    Some("image") => validate_image(item, true)?,
                                    _ => bail!(
                                        "Unsupported tool result content; refusing to drop it"
                                    ),
                                }
                            }
                        } else {
                            ensure!(
                                content.is_string(),
                                "Tool result must contain text or content blocks"
                            );
                        }
                    }
                }
                kind => bail!("Unsupported message content type '{kind}'; refusing to drop it"),
            }
        }
    }
    ensure!(
        unresolved.is_empty(),
        "History contains tool calls without results; complete or repair the client conversation before retrying"
    );
    if let Some(system) = request.extra.get("system")
        && !system.is_string()
    {
        ensure!(
            system.as_array().is_some_and(|blocks| blocks
                .iter()
                .all(|b| b["type"] == "text" && b["text"].is_string())),
            "System instructions must contain only text"
        );
    }
    Ok(())
}

fn validate_image(block: &Value, tool_result: bool) -> Result<()> {
    let source = &block["source"];
    match source["type"].as_str() {
        Some("base64") => {
            use base64::Engine;
            ensure!(
                matches!(
                    source["media_type"].as_str(),
                    Some("image/png" | "image/jpeg" | "image/webp" | "image/gif")
                ),
                "Unsupported image media type"
            );
            ensure!(
                source["data"].as_str().is_some_and(|s| !s.is_empty()
                    && base64::engine::general_purpose::STANDARD.decode(s).is_ok()),
                "Invalid base64 image"
            );
        }
        Some("url") if !tool_result => {
            ensure!(
                source["url"]
                    .as_str()
                    .and_then(|s| url::Url::parse(s).ok())
                    .is_some_and(|u| u.scheme() == "https"),
                "Image URL must use HTTPS"
            );
        }
        _ => bail!("Unsupported image source; tool results require base64 images"),
    }
    Ok(())
}
