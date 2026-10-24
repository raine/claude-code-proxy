use super::{
    replay::{self, Binding, Result, keys, nonempty},
    schema::ToolSet,
};
use crate::anthropic::schema::{Message, MessagesRequest};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

pub const MODELS: &[&str] = &[
    "antigravity/gemini-4-argon-low",
    "antigravity/gemini-4-argon-medium",
    "antigravity/gemini-4-argon-high",
];

pub(crate) fn effective_tier(model: &str) -> Option<&'static str> {
    match model {
        "antigravity/gemini-4-argon-low" => Some("low"),
        "antigravity/gemini-4-argon-medium" => Some("medium"),
        "antigravity/gemini-4-argon-high" => Some("high"),
        _ => None,
    }
}

pub(crate) struct Prepared {
    pub wire: Value,
    pub session: String,
    pub display: String,
    pub tools: ToolSet,
    pub estimate: u64,
}

pub(crate) fn cache_hint(value: Option<&Value>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    keys(value, &["type", "ttl"], &["type"])?;
    if value["type"] != "ephemeral"
        || value
            .get("ttl")
            .is_some_and(|v| !matches!(v.as_str(), Some("5m" | "1h")))
    {
        return Err("unsupported cache hint");
    }
    Ok(())
}

pub(crate) fn validate_raw(raw: &[u8]) -> Result<()> {
    let value = replay::parse_json(raw, crate::anthropic::MAX_ANTHROPIC_REQUEST_BYTES)?;
    for message in value["messages"]
        .as_array()
        .ok_or("messages must be an array")?
    {
        keys(message, &["role", "content"], &["role", "content"])?;
    }
    Ok(())
}

fn text_parts(value: &Value) -> Result<Vec<Value>> {
    if let Some(text) = value.as_str() {
        return Ok(vec![json!({"text":text})]);
    }
    let mut parts = Vec::new();
    for block in value
        .as_array()
        .ok_or("content must be text or text blocks")?
    {
        keys(block, &["type", "text", "cache_control"], &["type", "text"])?;
        if block["type"] != "text" {
            return Err("non-text system/tool-result content is unsupported");
        }
        cache_hint(block.get("cache_control"))?;
        parts.push(json!({"text":block["text"].as_str().ok_or("text must be a string")?}));
    }
    Ok(parts)
}

fn options(body: &MessagesRequest) -> Result<&'static str> {
    let allowed = [
        "system",
        "tools",
        "thinking",
        "output_config",
        "context_management",
        "metadata",
        "tool_choice",
        "cache_control",
    ];
    if body.extra.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err("unsupported Antigravity semantic request option");
    }
    let mut display = "full";
    if let Some(thinking) = body.extra.get("thinking") {
        keys(thinking, &["type", "display"], &["type"])?;
        if thinking["type"] != "adaptive" {
            return Err(
                "Antigravity supports adaptive thinking only; model tier controls reasoning",
            );
        }
        if let Some(value) = thinking.get("display") {
            if value != "omitted" {
                return Err("unsupported thinking display");
            }
            display = "omitted";
        }
    }
    if let Some(output) = body.extra.get("output_config") {
        keys(output, &["effort"], &[])?;
        if output
            .get("effort")
            .is_some_and(|v| !matches!(v.as_str(), Some("low" | "medium" | "high" | "max")))
        {
            return Err("unsupported advisory effort");
        }
    }
    if let Some(context) = body.extra.get("context_management") {
        keys(context, &["edits"], &["edits"])?;
        for edit in context["edits"]
            .as_array()
            .ok_or("context edits must be an array")?
        {
            if edit != &json!({"type":"clear_thinking_20251015","keep":"all"}) {
                return Err("only keep-all thinking context edits are supported");
            }
        }
    }
    if let Some(metadata) = body.extra.get("metadata") {
        keys(metadata, &["user_id"], &[])?;
        if metadata.get("user_id").is_some_and(|v| !v.is_string()) {
            return Err("invalid metadata");
        }
    }
    cache_hint(body.extra.get("cache_control"))?;
    Ok(display)
}

/// Establish logical assistant boundaries in the ORIGINAL role sequence. A
/// real system message is a boundary, even though its text is extracted later.
/// A new leading carrier starts a distinct complete turn; only carrier-less
/// adjacent fragments can extend the preceding turn and must pass its digest.
fn logical_messages(messages: &[Message]) -> Result<Vec<Cow<'_, Message>>> {
    let mut output: Vec<Cow<'_, Message>> = Vec::new();
    let mut current_bytes = 0_usize;
    for message in messages {
        if message.role != "assistant" {
            output.push(Cow::Borrowed(message));
            current_bytes = 0;
            continue;
        }
        let converted;
        let blocks = if let Some(text) = message.content.as_str() {
            converted = vec![json!({"type":"text", "text":text})];
            &converted
        } else {
            message
                .content
                .as_array()
                .ok_or("invalid assistant content")?
        };
        if blocks.is_empty() {
            continue;
        }
        let bytes = blocks.iter().try_fold(0_usize, |total, block| {
            total
                .checked_add(replay::projected_block_bytes(block)?)
                .filter(|n| *n <= replay::MAX_DECODED)
                .ok_or("assistant projection byte limit")
        })?;
        if blocks.len() > 4097 {
            return Err("assistant projection block limit");
        }
        let starts_turn = blocks
            .first()
            .is_some_and(|block| block["type"] == "thinking");
        let append = !starts_turn
            && output
                .last()
                .is_some_and(|previous| previous.role == "assistant");
        if append {
            let previous = output.last_mut().unwrap();
            let count = previous.content.as_array().map_or(1, Vec::len);
            if count.saturating_add(blocks.len()) > 4097 {
                return Err("assistant projection block limit");
            }
            current_bytes = current_bytes
                .checked_add(bytes)
                .filter(|n| *n <= replay::MAX_DECODED)
                .ok_or("assistant projection byte limit")?;
            previous
                .to_mut()
                .content
                .as_array_mut()
                .expect("normalized assistant array")
                .extend(blocks.iter().cloned());
        } else {
            current_bytes = bytes;
            output.push(if message.content.is_array() {
                Cow::Borrowed(message)
            } else {
                Cow::Owned(Message {
                    role: message.role.clone(),
                    content: json!(blocks),
                })
            });
        }
    }
    Ok(output)
}

pub(crate) fn prepare(body: &MessagesRequest, binding: &Binding) -> Result<Prepared> {
    let model = body.model.as_deref().ok_or("missing model")?;
    if !MODELS.contains(&model) || model.strip_prefix("antigravity/") != Some(&binding.native_model)
    {
        return Err("unsupported Antigravity model or replay scope");
    }
    let tier = match model {
        "antigravity/gemini-4-argon-low" => "LOW",
        "antigravity/gemini-4-argon-medium" => "MEDIUM",
        _ => "HIGH",
    };
    let cap = body.max_tokens.unwrap_or(8192);
    if cap == 0 || cap > 65536 {
        return Err("max_tokens must be between 1 and 65536");
    }
    let display = options(body)?.to_owned();
    if body.messages.is_empty() || body.messages.len() > 512 {
        return Err("message count limit");
    }
    let messages = logical_messages(&body.messages)?;
    let mut tools = ToolSet::compile(body.extra.get("tools").unwrap_or(&json!([])))?;
    if let Some(choice) = body.extra.get("tool_choice") {
        keys(choice, &["type"], &["type"])?;
        match choice["type"].as_str() {
            Some("auto") => {}
            Some("none") => tools = ToolSet::compile(&json!([]))?,
            _ => return Err("forced tool choice/parallel-disable semantics are unsupported"),
        }
    }
    let mut system = body
        .extra
        .get("system")
        .map(text_parts)
        .transpose()?
        .unwrap_or_default();
    for message in &messages {
        if message.role == "system" {
            system.extend(text_parts(&message.content)?);
        }
    }
    let turns = messages
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content.clone())
        .collect::<Vec<_>>();
    let envelopes = replay::decode_history(&json!(turns), binding)?;
    let session = envelopes
        .first()
        .map(|e| e["session_id"].as_str().unwrap().to_string())
        .unwrap_or_else(replay::mint_session);
    let mut history = envelopes.into_iter();
    let mut pending: Vec<(Value, Value)> = Vec::new();
    let mut contents = Vec::new();
    let mut last_role = "";
    let mut estimate_bytes = serde_json::to_vec(&system)
        .map_err(|_| "invalid system")?
        .len();
    estimate_bytes += serde_json::to_vec(&tools.declarations)
        .map_err(|_| "invalid tools")?
        .len();
    for message in &messages {
        if message.role == "system" {
            continue;
        }
        if message.role == "assistant" {
            if !pending.is_empty() {
                return Err("missing tool results before next assistant turn");
            }
            let envelope = history.next().ok_or("missing replay envelope")?;
            for entry in envelope["tool_mapping"].as_array().unwrap() {
                let ci = entry["content_index"].as_u64().unwrap() as usize;
                let pi = entry["part_index"].as_u64().unwrap() as usize;
                pending.push((
                    entry.clone(),
                    envelope["contents"][ci]["parts"][pi]["functionCall"].clone(),
                ));
            }
            // Count the validated native semantic history in a canonical FULL
            // projection, not the client's display or opaque carrier. This also
            // applies recorded caller-name and JSON codecs without current schemas.
            let semantic = replay::projection_values(&replay::projection(
                &envelope["contents"],
                "full",
                &envelope["tool_mapping"],
            )?)?;
            for block in semantic.as_array().unwrap() {
                for key in ["text", "thinking", "input", "name"] {
                    if let Some(value) = block.get(key) {
                        estimate_bytes += serde_json::to_vec(value)
                            .map_err(|_| "invalid history")?
                            .len();
                    }
                }
            }
            let native = envelope["contents"].as_array().unwrap();
            if contents.len().saturating_add(native.len()) > 1024 {
                return Err("native history content limit");
            }
            contents.extend(native.iter().cloned());
        } else if message.role == "user" {
            let blocks = if let Some(text) = message.content.as_str() {
                vec![json!({"type":"text","text":text})]
            } else {
                message
                    .content
                    .as_array()
                    .ok_or("invalid user content")?
                    .clone()
            };
            let mut results = BTreeMap::new();
            let mut text = Vec::new();
            let mut saw_text = false;
            let known: HashSet<&str> = pending
                .iter()
                .map(|(e, _)| e["local_id"].as_str().unwrap())
                .collect();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        keys(
                            &block,
                            &["type", "text", "cache_control"],
                            &["type", "text"],
                        )?;
                        cache_hint(block.get("cache_control"))?;
                        if results.len() != pending.len() {
                            return Err("tool results must form a complete prefix before text");
                        }
                        saw_text = true;
                        let value = block["text"].as_str().ok_or("invalid user text")?;
                        estimate_bytes += value.len();
                        text.push(json!({"text":value}));
                    }
                    Some("tool_result") => {
                        keys(
                            &block,
                            &[
                                "type",
                                "tool_use_id",
                                "content",
                                "is_error",
                                "cache_control",
                            ],
                            &["type", "tool_use_id"],
                        )?;
                        cache_hint(block.get("cache_control"))?;
                        let id = nonempty(&block["tool_use_id"])?;
                        if saw_text || !known.contains(id) || results.contains_key(id) {
                            return Err("ambiguous/unknown/interleaved tool result");
                        }
                        let value = match block.get("content") {
                            None => json!(""),
                            Some(Value::String(s)) => json!(s),
                            Some(value) => json!(
                                text_parts(value)?
                                    .iter()
                                    .map(|p| p["text"].clone())
                                    .collect::<Vec<_>>()
                            ),
                        };
                        estimate_bytes += serde_json::to_vec(&value)
                            .map_err(|_| "invalid tool result")?
                            .len();
                        let is_error = block
                            .get("is_error")
                            .map(|v| v.as_bool().ok_or("invalid is_error flag"))
                            .transpose()?
                            .unwrap_or(false);
                        results.insert(
                            id.to_string(),
                            if is_error {
                                json!({"error":value,"is_error":true})
                            } else {
                                json!({"output":value})
                            },
                        );
                    }
                    _ => return Err("unsupported user content/media"),
                }
            }
            if results.len() != pending.len() {
                return Err("incomplete tool result batch");
            }
            if !pending.is_empty() {
                if contents.len() >= 1024 {
                    return Err("native history content limit");
                }
                let mut parts = Vec::new();
                for (entry, call) in pending.drain(..) {
                    let mut response = json!({"name":call["name"],"response":results.remove(entry["local_id"].as_str().unwrap()).unwrap()});
                    if let Some(id) = call.get("id") {
                        response["id"] = id.clone();
                    }
                    parts.push(json!({"functionResponse":response}));
                }
                contents.push(json!({"role":"model","parts":parts}));
            }
            if !text.is_empty() {
                if contents.len() >= 1024 {
                    return Err("native history content limit");
                }
                contents.push(json!({"role":"user","parts":text}));
            }
        } else {
            return Err("unsupported message role");
        }
        last_role = &message.role;
    }
    if contents.is_empty() || last_role != "user" || !pending.is_empty() {
        return Err("request needs a final user turn with complete tool results");
    }
    if contents.len() > 1024 {
        return Err("native history content limit");
    }
    let mut request = json!({"contents":contents,"sessionId":session,
        "generationConfig":{"maxOutputTokens":cap,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":tier}}});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"parts":system});
    }
    if !tools.declarations.as_array().unwrap().is_empty() {
        request["tools"] = json!([{"functionDeclarations":tools.declarations}]);
    }
    let wire = json!({"project":binding.project_id,"model":binding.native_model,"request":request,
        "requestId":format!("ccp-{}",uuid::Uuid::new_v4()),"userAgent":"antigravity","requestType":"agent"});
    Ok(Prepared {
        wire,
        session,
        display,
        tools,
        estimate: ((estimate_bytes as u64).div_ceil(3)).max(1),
    })
}
