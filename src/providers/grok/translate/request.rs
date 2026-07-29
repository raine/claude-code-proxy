use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Write,
};

use serde::Serialize;
use serde_json::Value;

use crate::anthropic::schema::{Message, MessagesRequest};
use crate::providers::translate_shared::{
    RequestedOutputFormat, flatten_system_text, is_claude_code_compaction_request,
    parse_output_format, read_effort, validate_alternate_provider_fields,
    validate_grok_message_roles, validate_tool_reference_provenance,
};

const MAX_GROK_IMAGE_BYTES: usize = 20 * 1024 * 1024;
// One maximally-sized base64 image expands to about 27 MiB. Leave room for
// its JSON envelope while preventing multiple large blocks from growing one
// function_call_output without bound.
const MAX_TOOL_RESULT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const PARALLEL_FUNCTION_TOOL_GUIDANCE: &str = "\
You can emit multiple independent function calls in one response, including repeated calls \
to the same tool. Emit independent calls together so the client can run them concurrently. \
Serialize dependent calls and calls with overlapping side effects.";
const CLAUDE_CODE_DEDICATED_TOOLS: [&str; 4] = ["Read", "Grep", "Glob", "Edit"];

#[derive(Debug, Clone, Serialize)]
pub struct GrokResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<GrokInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GrokTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<GrokToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<GrokReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<GrokText>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokReasoning {
    pub effort: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokText {
    pub format: GrokTextFormat,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GrokTextFormat {
    JsonSchema {
        name: String,
        schema: Value,
        strict: bool,
    },
    JsonObject,
    Text,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GrokInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<GrokContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GrokContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: String,
        /// Computed while validating embedded base64 so token estimation never decodes it again.
        #[serde(skip)]
        estimated_tokens: Option<u64>,
    },
    #[serde(rename = "output_text")]
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokTool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(skip)]
    pub strict: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<GrokWebSearchFilters>,
    #[serde(skip)]
    pub location: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_x_handles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_x_handles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_date: Option<String>,
    /// Anthropic limits hosted search calls per request. xAI has no
    /// equivalent request field, so this is retained for model guidance and
    /// deliberately omitted from the upstream JSON.
    #[serde(skip)]
    pub max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokWebSearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_domains: Option<Vec<String>>,
}

impl GrokTool {
    fn hosted(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            name: None,
            description: None,
            parameters: None,
            strict: None,
            filters: None,
            location: None,
            allowed_x_handles: None,
            excluded_x_handles: None,
            from_date: None,
            to_date: None,
            max_uses: None,
        }
    }

    fn function(name: &str, description: Option<String>, parameters: Value) -> Self {
        Self {
            kind: "function".into(),
            name: Some(name.into()),
            description,
            parameters: Some(parameters),
            strict: None,
            filters: None,
            location: None,
            allowed_x_handles: None,
            excluded_x_handles: None,
            from_date: None,
            to_date: None,
            max_uses: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GrokToolChoice {
    Auto(String),
    Required(String),
    None(String),
    Function { r#type: String, name: String },
}

pub fn translate_request(
    req: &MessagesRequest,
    model: String,
) -> anyhow::Result<GrokResponsesRequest> {
    validate_alternate_provider_fields(req, "Grok")?;
    validate_grok_message_roles(req)?;
    let reasoning_effort = read_effort(req)?;
    if reasoning_effort.is_some() && model != "grok-4.5" {
        anyhow::bail!(
            "Grok model {model} does not support output_config.effort; omit the field or use grok-4.5"
        );
    }
    let compaction = is_claude_code_compaction_request(req);
    let text = read_output_format(req)?.map(|format| GrokText { format });
    let internal_text_request = compaction || text.is_some();
    let mut instructions = parse_system(req.extra.get("system"))?;
    let selected_deferred = selected_deferred_tools(req)?;
    let mut tools = if compaction {
        None
    } else {
        parse_tools(req.extra.get("tools"), &selected_deferred)?
    };
    let (tool_choice, forced_hosted_kind) = if compaction {
        (None, None)
    } else {
        parse_tool_choice(req.extra.get("tool_choice"), tools.as_ref())?
    };
    if let (Some(kind), Some(tools)) = (forced_hosted_kind, tools.as_mut()) {
        tools.retain(|tool| tool.kind == kind);
    } else if matches!(&tool_choice, Some(GrokToolChoice::Required(_)))
        && let Some(tools) = tools.as_mut()
    {
        // Anthropic has no observable x_search server-tool block. A generic `any` choice must not
        // leave x_search eligible upstream while response policy intentionally refuses to count it
        // as satisfying the required tool call.
        tools.retain(|tool| tool.kind != "x_search");
    }
    let has_tools = tools.as_ref().is_some_and(|tools| !tools.is_empty());
    let tool_calls_allowed = !matches!(&tool_choice, Some(GrokToolChoice::None(_)));
    let parallel_tool_calls = if compaction || !has_tools {
        None
    } else if !tool_calls_allowed {
        // Keep the upstream request explicitly fail-closed when Anthropic's
        // tool_choice forbids all tool calls.
        Some(false)
    } else {
        let disabled = req
            .extra
            .get("tool_choice")
            .and_then(Value::as_object)
            .and_then(|choice| choice.get("disable_parallel_tool_use"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(!disabled)
    };
    let forced_function_name = match &tool_choice {
        Some(GrokToolChoice::Function { name, .. }) => Some(name.as_str()),
        _ => None,
    };
    // Keep every declared tool on the wire, but scope generated guidance to the
    // effective forced function when Anthropic selected one by name.
    let guidance_tools = tools
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|tool| {
            forced_function_name
                .is_none_or(|name| tool.kind == "function" && tool.name.as_deref() == Some(name))
        })
        .collect::<Vec<_>>();
    let function_tool_count = guidance_tools
        .iter()
        .filter(|tool| tool.kind == "function")
        .count();
    // Keep high-priority tool-calling policy ahead of the (often very large) Claude Code
    // system prompt so Grok does not bury it under session context and serialize independent
    // calls. Mention dedicated tools or Bash only when the effective tool choice permits them.
    let hosted_web_search = guidance_tools.iter().any(|tool| tool.kind == "web_search");
    let dedicated_x_search = guidance_tools.iter().any(|tool| tool.kind == "x_search");
    if !internal_text_request && tool_calls_allowed {
        if parallel_tool_calls == Some(true) && function_tool_count >= 1 {
            let guidance =
                parallel_function_tool_guidance(&guidance_tools, forced_function_name.is_some());
            prepend_guidance(&mut instructions, &guidance);
        }
        if hosted_web_search {
            append_guidance(
                &mut instructions,
                "For general web searches, use the hosted web_search tool. Do not use shell commands, HTTP clients, or local tools to search the web.",
            );
        }
        if dedicated_x_search {
            append_guidance(
                &mut instructions,
                "For requests to search X or Twitter, use the hosted x_search tool. XSearch accepts a query and supports allowed_x_handles, excluded_x_handles, from_date, and to_date filters. Do not use Bash, curl, HTTP clients, or general web_search for X searches.",
            );
        }
        for tool in guidance_tools {
            if tool.strict == Some(true)
                && let Some(name) = tool.name.as_deref()
            {
                append_guidance(
                    &mut instructions,
                    &format!(
                        "When calling {name}, emit arguments that match its JSON schema exactly and do not add undeclared properties."
                    ),
                );
            }
            if let Some(max_uses) = tool.max_uses {
                append_guidance(
                    &mut instructions,
                    &format!(
                        "Use the hosted {} tool no more than {max_uses} time(s) in this response.",
                        tool.kind
                    ),
                );
            }
            if let Some(location) = &tool.location {
                append_guidance(
                    &mut instructions,
                    &format!(
                        "Prefer hosted {} results relevant to this approximate user location: {}.",
                        tool.kind, location
                    ),
                );
            }
        }
    }
    validate_hosted_tool_history(&req.messages)?;
    let mut call_ids = HashSet::new();
    let mut input = Vec::new();
    for message in &req.messages {
        if !call_ids.is_empty() && message.role != "user" {
            anyhow::bail!("unresolved tool calls must be followed by tool results");
        }
        if !call_ids.is_empty() {
            validate_immediate_tool_results(message)?;
        }
        parse_message(message, &mut input, &mut call_ids)?;
        if message.role == "user" && !call_ids.is_empty() {
            anyhow::bail!("user message did not resolve every pending tool call");
        }
    }
    if !call_ids.is_empty() {
        anyhow::bail!("request ends with unresolved tool calls");
    }
    let reasoning = if model == "grok-4.5" {
        reasoning_effort.map(|effort| GrokReasoning {
            effort: match effort {
                "max" | "xhigh" => "high",
                value => value,
            }
            .to_string(),
        })
    } else {
        None
    };
    Ok(GrokResponsesRequest {
        model,
        instructions,
        input,
        tools,
        tool_choice,
        parallel_tool_calls,
        store: false,
        stream: true,
        max_output_tokens: req.max_tokens,
        reasoning,
        text,
    })
}

fn validate_immediate_tool_results(message: &Message) -> anyhow::Result<()> {
    let blocks = message
        .content
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("pending tool calls require a tool_result user message"))?;
    let mut saw_non_result = false;
    let mut result_count = 0usize;
    for block in blocks {
        let is_result = block.get("type").and_then(Value::as_str) == Some("tool_result");
        if is_result {
            if saw_non_result {
                anyhow::bail!("tool_result blocks must precede other user content");
            }
            result_count += 1;
        } else {
            saw_non_result = true;
        }
    }
    if result_count == 0 {
        anyhow::bail!("pending tool calls require immediate tool_result blocks");
    }
    Ok(())
}

fn validate_hosted_tool_history(messages: &[Message]) -> anyhow::Result<()> {
    let mut seen_ids = HashSet::new();
    let mut pending_custom = HashSet::new();
    let mut custom_order = Vec::new();
    let mut pending_hosted = HashMap::new();
    let mut hosted_order = Vec::new();
    let mut hosted_mixed_pause = false;

    for (message_index, message) in messages.iter().enumerate() {
        let expects_custom_results = !pending_custom.is_empty();
        if expects_custom_results && message.role != "user" {
            let id = first_pending_tool_id(&custom_order, |id| pending_custom.contains(id));
            anyhow::bail!(
                "custom tool use id {id} must be resolved in the immediately following user message"
            );
        }
        let expects_delayed_hosted =
            hosted_mixed_pause && pending_custom.is_empty() && !pending_hosted.is_empty();
        if expects_delayed_hosted && message.role != "assistant" {
            let id = first_pending_tool_id(&hosted_order, |id| pending_hosted.contains_key(id));
            anyhow::bail!(
                "delayed hosted tool use id {id} must be resolved at the start of the next assistant message"
            );
        }

        let blocks = match &message.content {
            Value::Array(blocks) => blocks,
            Value::String(_) => {
                if expects_custom_results || expects_delayed_hosted {
                    anyhow::bail!("pending tool results require structured content blocks");
                }
                continue;
            }
            _ => continue,
        };
        let mixed_pause_user_message = expects_custom_results && !pending_hosted.is_empty();
        let mut resolving_delayed_hosted = expects_delayed_hosted;
        let mut saw_non_custom_result = false;

        for (block_index, block) in blocks.iter().enumerate() {
            let Some(object) = block.as_object() else {
                continue;
            };
            let Some(kind) = object.get("type").and_then(Value::as_str) else {
                continue;
            };
            if mixed_pause_user_message && kind != "tool_result" {
                anyhow::bail!(
                    "messages[{message_index}] must contain only client tool_result blocks while hosted tools are paused"
                );
            }
            if expects_custom_results && kind == "tool_result" {
                if saw_non_custom_result {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}] tool_result blocks must precede other content"
                    );
                }
            } else if expects_custom_results {
                saw_non_custom_result = true;
            }

            if resolving_delayed_hosted {
                if !matches!(kind, "web_search_tool_result" | "x_search_tool_result") {
                    let id =
                        first_pending_tool_id(&hosted_order, |id| pending_hosted.contains_key(id));
                    anyhow::bail!(
                        "delayed hosted tool use id {id} must be resolved before other assistant content"
                    );
                }
                let (id, content) = hosted_result_identity(object, message_index, block_index)?;
                validate_hosted_result_content(content, message_index, block_index)?;
                resolve_hosted_result(&mut pending_hosted, id, kind)?;
                if pending_hosted.is_empty() {
                    resolving_delayed_hosted = false;
                    hosted_mixed_pause = false;
                }
                continue;
            }

            match kind {
                "tool_use" => {
                    if message.role != "assistant" {
                        continue;
                    }
                    let Some(id) = object
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    if !seen_ids.insert(id.to_string()) {
                        anyhow::bail!("duplicate tool use id: {id}");
                    }
                    pending_custom.insert(id.to_string());
                    custom_order.push(id.to_string());
                }
                "server_tool_use" => {
                    if message.role != "assistant" {
                        anyhow::bail!(
                            "messages[{message_index}].content[{block_index}] server_tool_use must have assistant role"
                        );
                    }
                    let id = object
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "messages[{message_index}].content[{block_index}].id must be a non-empty string"
                            )
                        })?;
                    if !seen_ids.insert(id.to_string()) {
                        anyhow::bail!("duplicate server tool use id: {id}");
                    }
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "messages[{message_index}].content[{block_index}].name must be a non-empty string"
                            )
                        })?;
                    let expected_result = match name {
                        "web_search" => "web_search_tool_result",
                        // Older cg transcripts emitted a private x_search hosted-tool pair. It is
                        // accepted only as validated history and is never generated downstream.
                        "x_search" => "x_search_tool_result",
                        _ => anyhow::bail!("unsupported server tool use name: {name}"),
                    };
                    if !object.contains_key("input") {
                        anyhow::bail!(
                            "messages[{message_index}].content[{block_index}].input is required"
                        );
                    }
                    pending_hosted.insert(id.to_string(), expected_result.to_string());
                    hosted_order.push(id.to_string());
                }
                "tool_result" => {
                    if message.role != "user" {
                        continue;
                    }
                    let Some(id) = object
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    if !pending_custom.remove(id) {
                        anyhow::bail!(
                            "tool result references an unknown or not-yet-seen tool use id: {id}"
                        );
                    }
                }
                "web_search_tool_result" | "x_search_tool_result" => {
                    if message.role != "assistant" {
                        anyhow::bail!(
                            "messages[{message_index}].content[{block_index}] {kind} must have assistant role"
                        );
                    }
                    let (id, content) = hosted_result_identity(object, message_index, block_index)?;
                    validate_hosted_result_content(content, message_index, block_index)?;
                    resolve_hosted_result(&mut pending_hosted, id, kind)?;
                }
                _ => {}
            }
        }

        if expects_custom_results && !pending_custom.is_empty() {
            let id = first_pending_tool_id(&custom_order, |id| pending_custom.contains(id));
            anyhow::bail!(
                "custom tool use id {id} must be resolved in the immediately following user message"
            );
        }
        if resolving_delayed_hosted {
            let id = first_pending_tool_id(&hosted_order, |id| pending_hosted.contains_key(id));
            anyhow::bail!(
                "delayed hosted tool use id {id} must be resolved at the start of the next assistant message"
            );
        }
        if message.role == "assistant" && !pending_hosted.is_empty() {
            if pending_custom.is_empty() {
                let id = first_pending_tool_id(&hosted_order, |id| pending_hosted.contains_key(id));
                anyhow::bail!("unresolved server tool use id: {id}");
            }
            hosted_mixed_pause = true;
        }
    }

    if let Some(id) = custom_order.iter().find(|id| pending_custom.contains(*id)) {
        anyhow::bail!("unresolved custom tool use id: {id}");
    }
    if let Some(id) = hosted_order
        .iter()
        .find(|id| pending_hosted.contains_key(*id))
    {
        anyhow::bail!("unresolved server tool use id: {id}");
    }
    Ok(())
}

fn first_pending_tool_id(order: &[String], is_pending: impl Fn(&str) -> bool) -> &str {
    order
        .iter()
        .find(|id| is_pending(id))
        .expect("pending tool ids originate from recorded history")
}

fn hosted_result_identity(
    object: &serde_json::Map<String, Value>,
    message_index: usize,
    block_index: usize,
) -> anyhow::Result<(&str, &Value)> {
    let id = object
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "messages[{message_index}].content[{block_index}].tool_use_id must be a non-empty string"
            )
        })?;
    let content = object.get("content").ok_or_else(|| {
        anyhow::anyhow!("messages[{message_index}].content[{block_index}].content is required")
    })?;
    Ok((id, content))
}

fn validate_hosted_result_content(
    content: &Value,
    message_index: usize,
    block_index: usize,
) -> anyhow::Result<()> {
    if matches!(content, Value::Array(_) | Value::Object(_)) {
        Ok(())
    } else {
        anyhow::bail!(
            "messages[{message_index}].content[{block_index}].content must be an array or object"
        )
    }
}

fn resolve_hosted_result(
    pending: &mut HashMap<String, String>,
    id: &str,
    actual_kind: &str,
) -> anyhow::Result<()> {
    let Some(expected_kind) = pending.get(id) else {
        anyhow::bail!(
            "hosted tool result references an unknown or not-yet-seen server tool use id: {id}"
        );
    };
    if expected_kind != actual_kind {
        anyhow::bail!(
            "hosted tool result kind mismatch for {id}: expected {expected_kind}, got {actual_kind}"
        );
    }
    pending.remove(id);
    Ok(())
}

fn read_output_format(req: &MessagesRequest) -> anyhow::Result<Option<GrokTextFormat>> {
    parse_output_format(req).map(|format| {
        format.map(|format| match format {
            RequestedOutputFormat::JsonSchema { name, schema } => GrokTextFormat::JsonSchema {
                name: name.to_string(),
                // xAI follows ordinary JSON Schema required/optional semantics;
                // do not apply OpenAI strict-schema nullable normalization here.
                schema: schema.clone(),
                strict: true,
            },
            RequestedOutputFormat::JsonObject => GrokTextFormat::JsonObject,
            RequestedOutputFormat::Text => GrokTextFormat::Text,
        })
    })
}

fn append_guidance(instructions: &mut Option<String>, guidance: &str) {
    if let Some(existing) = instructions.as_mut().filter(|value| !value.is_empty()) {
        existing.push_str("\n\n");
        existing.push_str(guidance);
    } else {
        *instructions = Some(guidance.to_string());
    }
}

fn prepend_guidance(instructions: &mut Option<String>, guidance: &str) {
    *instructions = Some(match instructions.take() {
        Some(existing) if !existing.is_empty() => format!("{guidance}\n\n{existing}"),
        _ => guidance.into(),
    });
}

fn parallel_function_tool_guidance(tools: &[&GrokTool], forced_function: bool) -> String {
    let declared = tools
        .iter()
        .filter(|tool| tool.kind == "function")
        .filter_map(|tool| tool.name.as_deref())
        .collect::<HashSet<_>>();
    let dedicated = CLAUDE_CODE_DEDICATED_TOOLS
        .into_iter()
        .filter(|name| declared.contains(name))
        .collect::<Vec<_>>();
    let has_bash = declared.contains("Bash");

    let mut guidance = PARALLEL_FUNCTION_TOOL_GUIDANCE.to_owned();
    if forced_function {
        guidance.push_str(
            " Because this request forces one function, issue parallel calls only to that selected function.",
        );
    }
    if has_bash && !dedicated.is_empty() {
        guidance.push_str(" Prefer the declared dedicated tools (");
        guidance.push_str(&dedicated.join(", "));
        guidance.push_str(") over one large Bash script that combines independent lookups.");
    }
    if has_bash {
        guidance.push_str(
            " Keep each Bash command focused and avoid packing unrelated work into a single shell script.",
        );
    }
    guidance
}

fn parse_system(value: Option<&Value>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    match value {
        Value::String(_) => Ok(flatten_system_text(Some(value))),
        Value::Array(blocks) => {
            for block in blocks {
                let object = block
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("system content must contain text blocks"))?;
                if object
                    .keys()
                    .any(|key| !["type", "text", "cache_control"].contains(&key.as_str()))
                    || object.get("type").and_then(Value::as_str) != Some("text")
                    || !valid_cache_control(object.get("cache_control"))
                {
                    anyhow::bail!("unsupported system block");
                }
                if object.get("text").and_then(Value::as_str).is_none() {
                    anyhow::bail!("system text is invalid");
                }
            }
            Ok(flatten_system_text(Some(value)))
        }
        _ => anyhow::bail!("system must be text"),
    }
}

fn selected_deferred_tools(req: &MessagesRequest) -> anyhow::Result<HashSet<String>> {
    let mut selected = HashSet::new();
    if let Some(name) = req
        .extra
        .get("tool_choice")
        .and_then(Value::as_object)
        .filter(|choice| choice.get("type").and_then(Value::as_str) == Some("tool"))
        .and_then(|choice| choice.get("name"))
        .and_then(Value::as_str)
    {
        selected.insert(name.to_string());
    }
    selected.extend(validate_tool_reference_provenance(req)?);
    Ok(selected)
}

fn parse_tools(
    value: Option<&Value>,
    selected_deferred: &HashSet<String>,
) -> anyhow::Result<Option<Vec<GrokTool>>> {
    let Some(value) = value else { return Ok(None) };
    let tools = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
    let mut names = HashSet::new();
    let mut out = Vec::new();
    for tool in tools {
        let obj = tool
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("tool must be an object"))?;
        for key in obj.keys() {
            if ![
                "type",
                "name",
                "description",
                "input_schema",
                "cache_control",
                // Claude Code adds these execution hints to built-in and MCP
                // tools. Grok's Responses endpoint does not consume them, but
                // rejecting the whole request turns otherwise valid Workflow
                // and deferred-tool turns into a local APIError.
                "strict",
                "defer_loading",
                "eager_input_streaming",
                "allowed_callers",
                "input_examples",
                "max_uses",
                "allowed_domains",
                "blocked_domains",
                "user_location",
                "response_inclusion",
            ]
            .contains(&key.as_str())
            {
                anyhow::bail!("unsupported tool field: {key}");
            }
        }
        let declared_type = match obj.get("type") {
            None | Some(Value::Null) => None,
            Some(Value::String(kind)) if !kind.is_empty() => Some(kind.as_str()),
            Some(_) => anyhow::bail!("tool type is invalid"),
        };
        if !valid_cache_control(obj.get("cache_control")) {
            anyhow::bail!("unsupported tool cache_control");
        }
        let supplied_name = obj
            .get("name")
            .map(|name| {
                name.as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool name is invalid"))
            })
            .transpose()?;
        let is_typed_web_search = declared_type.is_some_and(|kind| kind.starts_with("web_search_"));
        let is_typed_x_search = declared_type == Some("x_search");
        // Tool names are client-controlled and Anthropic custom functions commonly omit `type`.
        // Only an explicit hosted type may move execution from the client to the provider.
        let canonical_name = if is_typed_web_search {
            "web_search"
        } else if is_typed_x_search {
            "x_search"
        } else {
            supplied_name.ok_or_else(|| anyhow::anyhow!("tool name is invalid"))?
        };
        if !names.insert(canonical_name.to_string()) {
            anyhow::bail!("duplicate tool name");
        }
        let deferred = match obj.get("defer_loading") {
            Some(Value::Bool(deferred)) => *deferred,
            Some(Value::Null) => false,
            Some(_) => anyhow::bail!("tool defer_loading must be boolean"),
            None => false,
        };
        if let Some(eager) = obj.get("eager_input_streaming")
            && !eager.is_null()
            && !eager.is_boolean()
        {
            anyhow::bail!("tool eager_input_streaming must be boolean");
        }
        let direct_by_default = !matches!(
            declared_type,
            Some("web_search_20260209" | "web_search_20260318")
        );
        let direct_allowed = direct_caller_allowed(obj.get("allowed_callers"), direct_by_default)?;
        let selected = selected_deferred.contains(canonical_name)
            || supplied_name.is_some_and(|name| selected_deferred.contains(name));
        if !direct_allowed || (deferred && canonical_name != "ToolSearch" && !selected) {
            continue;
        }
        if is_typed_web_search {
            let mut tool = GrokTool::hosted("web_search");
            let allowed_domains = parse_string_list(obj.get("allowed_domains"), "allowed_domains")?;
            let excluded_domains =
                parse_string_list(obj.get("blocked_domains"), "blocked_domains")?;
            if allowed_domains.is_some() && excluded_domains.is_some() {
                anyhow::bail!("web_search cannot combine allowed_domains and blocked_domains");
            }
            for (field, values) in [
                ("allowed_domains", allowed_domains.as_ref()),
                ("blocked_domains", excluded_domains.as_ref()),
            ] {
                if values.is_some_and(|values| values.len() > 5) {
                    anyhow::bail!("Grok {field} supports at most 5 domains");
                }
            }
            if allowed_domains.is_some() || excluded_domains.is_some() {
                tool.filters = Some(GrokWebSearchFilters {
                    allowed_domains,
                    excluded_domains,
                });
            }
            tool.location = parse_user_location(obj.get("user_location"))?;
            tool.max_uses = parse_positive_u64(obj.get("max_uses"), "max_uses")?;
            validate_response_inclusion(obj.get("response_inclusion"), declared_type)?;
            out.push(tool);
            continue;
        }
        if is_typed_x_search {
            let mut tool = GrokTool::hosted("x_search");
            tool.max_uses = parse_positive_u64(obj.get("max_uses"), "max_uses")?;
            out.push(tool);
            continue;
        }
        let parameters = obj
            .get("input_schema")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tool input_schema must be an object"))?;
        let strict = match obj.get("strict") {
            Some(Value::Bool(strict)) => *strict,
            Some(Value::Null) => false,
            Some(_) => anyhow::bail!("tool strict must be boolean"),
            None => false,
        };
        let mut tool = GrokTool::function(
            canonical_name,
            obj.get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters,
        );
        tool.strict = strict.then_some(true);
        out.push(tool);
    }
    Ok((!out.is_empty()).then_some(out))
}

fn parse_string_list(value: Option<&Value>, field: &str) -> anyhow::Result<Option<Vec<String>>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    let values = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{field} must be an array"))?;
    if values.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{field} entries must be non-empty strings"))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

fn parse_positive_u64(value: Option<&Value>, field: &str) -> anyhow::Result<Option<u64>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a positive integer"))
}

fn parse_user_location(value: Option<&Value>) -> anyhow::Result<Option<Value>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("user_location must be an object"))?;
    if object
        .keys()
        .any(|key| !["type", "city", "region", "country", "timezone"].contains(&key.as_str()))
        || object.get("type").and_then(Value::as_str) != Some("approximate")
    {
        anyhow::bail!("unsupported user_location");
    }
    let mut mapped = serde_json::Map::new();
    for key in ["city", "region", "country"] {
        if let Some(value) = object.get(key) {
            let value = value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("user_location {key} must be a non-empty string"))?;
            mapped.insert(key.into(), Value::String(value.into()));
        }
    }
    if let Some(timezone) = object.get("timezone")
        && timezone.as_str().is_none_or(str::is_empty)
    {
        anyhow::bail!("user_location timezone must be a non-empty string");
    }
    Ok((!mapped.is_empty()).then_some(Value::Object(mapped)))
}

fn direct_caller_allowed(value: Option<&Value>, default: bool) -> anyhow::Result<bool> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let callers = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("allowed_callers must be an array"))?;
    let callers = callers
        .iter()
        .map(|caller| {
            caller
                .as_str()
                .filter(|caller| !caller.is_empty())
                .ok_or_else(|| anyhow::anyhow!("allowed_callers entries must be strings"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(callers.contains(&"direct"))
}

fn validate_response_inclusion(
    value: Option<&Value>,
    declared_type: Option<&str>,
) -> anyhow::Result<()> {
    let Some(value) = value else { return Ok(()) };
    if value.is_null() {
        return Ok(());
    }
    if declared_type != Some("web_search_20260318") {
        anyhow::bail!("response_inclusion is supported by web_search_20260318 only");
    }
    if matches!(value.as_str(), Some("full" | "excluded")) {
        Ok(())
    } else {
        anyhow::bail!("response_inclusion must be full or excluded")
    }
}

fn parse_tool_choice(
    value: Option<&Value>,
    tools: Option<&Vec<GrokTool>>,
) -> anyhow::Result<(Option<GrokToolChoice>, Option<&'static str>)> {
    let Some(value) = value else {
        return Ok((None, None));
    };
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool_choice must be an object"))?;
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tool_choice type is invalid"))?;
    if let Some(disable_parallel) = obj.get("disable_parallel_tool_use")
        && !disable_parallel.is_boolean()
    {
        anyhow::bail!("tool_choice disable_parallel_tool_use must be boolean");
    }
    match kind {
        "auto" | "any" | "none"
            if obj
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "disable_parallel_tool_use")) =>
        {
            if kind == "any" && tools.is_none_or(|items| items.is_empty()) {
                anyhow::bail!("tool_choice {kind} requires at least one tool");
            }
            if kind == "any"
                && tools.is_some_and(|items| items.iter().all(|tool| tool.kind == "x_search"))
            {
                anyhow::bail!(
                    "tool_choice any cannot be satisfied by hosted x_search because Anthropic has no observable x_search tool block"
                );
            }
            if kind == "auto" && tools.is_none_or(|items| items.is_empty()) {
                return Ok((None, None));
            }
            Ok((
                Some(match kind {
                    "auto" => GrokToolChoice::Auto("auto".into()),
                    "any" => GrokToolChoice::Required("required".into()),
                    "none" => GrokToolChoice::None("none".into()),
                    _ => unreachable!(),
                }),
                None,
            ))
        }
        "tool"
            if obj.keys().all(|key| {
                matches!(key.as_str(), "type" | "name" | "disable_parallel_tool_use")
            }) =>
        {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("tool_choice name is invalid"))?;
            let matched = tools.and_then(|items| {
                // A custom function may intentionally use the Claude Code-style alias of a
                // separately declared hosted tool. Preserve the exact client function choice
                // regardless of declaration order; hosted aliases are only a fallback when no
                // exact function exists.
                items
                    .iter()
                    .find(|tool| tool.kind == "function" && tool.name.as_deref() == Some(name))
                    .or_else(|| {
                        items.iter().find(|tool| match tool.kind.as_str() {
                            "web_search" => matches!(name, "web_search" | "WebSearch"),
                            "x_search" => matches!(name, "x_search" | "XSearch"),
                            _ => false,
                        })
                    })
            });
            let Some(tool) = matched else {
                anyhow::bail!("tool_choice references an unknown tool");
            };
            if tool.kind == "function" {
                Ok((
                    Some(GrokToolChoice::Function {
                        r#type: "function".into(),
                        name: name.into(),
                    }),
                    None,
                ))
            } else {
                if tool.kind == "x_search" {
                    anyhow::bail!(
                        "tool_choice cannot force hosted XSearch because Anthropic has no observable x_search tool block"
                    );
                }
                let kind = match tool.kind.as_str() {
                    "web_search" => "web_search",
                    _ => unreachable!("parse_tools only emits supported hosted tool kinds"),
                };
                Ok((
                    Some(GrokToolChoice::Required("required".into())),
                    Some(kind),
                ))
            }
        }
        _ => anyhow::bail!("unsupported tool_choice"),
    }
}

fn parse_message(
    message: &Message,
    out: &mut Vec<GrokInputItem>,
    calls: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if !["system", "user", "assistant"].contains(&message.role.as_str()) {
        anyhow::bail!("unsupported message role");
    }
    let blocks: Cow<'_, [Value]> = match &message.content {
        Value::String(text) => Cow::Owned(vec![serde_json::json!({"type":"text", "text":text})]),
        Value::Array(items) => Cow::Borrowed(items),
        _ => anyhow::bail!("message content must be text or blocks"),
    };
    if message.role == "system" && blocks.is_empty() {
        anyhow::bail!("system message must contain at least one text block");
    }
    let mut content = Vec::new();
    let mut tool_result_images = Vec::new();
    for block in blocks.iter() {
        let object = block
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("content block must be an object"))?;
        let typ = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("content block type is invalid"))?;
        if message.role == "user" && typ != "tool_result" && !tool_result_images.is_empty() {
            content.append(&mut tool_result_images);
        }
        match (message.role.as_str(), typ) {
            (_, "thinking") | (_, "redacted_thinking") => {}
            (_, "text") => {
                let has_unsupported_field = object.keys().any(|key| {
                    if message.role == "system" {
                        !matches!(key.as_str(), "type" | "text" | "cache_control")
                    } else {
                        !["type", "text", "citations", "cache_control"].contains(&key.as_str())
                    }
                });
                if has_unsupported_field
                    || !valid_cache_control(object.get("cache_control"))
                    || object
                        .get("citations")
                        .is_some_and(|citations| !citations.is_null() && !citations.is_array())
                {
                    anyhow::bail!("unsupported text block field");
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("text block is invalid"))?;
                content.push(if message.role == "assistant" {
                    GrokContentPart::OutputText { text: text.into() }
                } else {
                    GrokContentPart::InputText { text: text.into() }
                });
            }
            ("user", "image") => {
                if object
                    .keys()
                    .any(|key| !["type", "source", "cache_control"].contains(&key.as_str()))
                    || !valid_cache_control(object.get("cache_control"))
                {
                    anyhow::bail!("unsupported image block field");
                }
                let (image_url, estimated_tokens) = parse_image_source(object.get("source"))?;
                content.push(GrokContentPart::InputImage {
                    image_url,
                    estimated_tokens,
                });
            }
            ("assistant", "server_tool_use") => {
                let name = object.get("name").and_then(Value::as_str);
                if !matches!(name, Some("web_search" | "x_search")) {
                    anyhow::bail!("unsupported server tool use");
                }
            }
            ("assistant", "web_search_tool_result" | "x_search_tool_result") => {}
            ("assistant", "tool_use") => {
                if object.keys().any(|key| {
                    !["type", "id", "name", "input", "caller", "cache_control"]
                        .contains(&key.as_str())
                }) || !valid_cache_control(object.get("cache_control"))
                    || !valid_tool_caller(object.get("caller"))
                {
                    anyhow::bail!("unsupported tool_use field");
                }
                flush_message(&message.role, &mut content, out);
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool call id is invalid"))?;
                let name = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool call name is invalid"))?;
                let input = object
                    .get("input")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| anyhow::anyhow!("tool call input must be an object"))?;
                if !calls.insert(id.into()) {
                    anyhow::bail!("duplicate tool call id");
                }
                out.push(GrokInputItem::FunctionCall {
                    call_id: id.into(),
                    name: name.into(),
                    arguments: serde_json::to_string(input)?,
                });
            }
            ("user", "tool_result") => {
                if object.keys().any(|key| {
                    ![
                        "type",
                        "tool_use_id",
                        "content",
                        "is_error",
                        "cache_control",
                    ]
                    .contains(&key.as_str())
                }) || !valid_cache_control(object.get("cache_control"))
                {
                    anyhow::bail!("unsupported tool_result field");
                }
                if let Some(is_error) = object.get("is_error")
                    && !is_error.is_boolean()
                {
                    anyhow::bail!("tool result is_error must be boolean");
                }
                flush_message(&message.role, &mut content, out);
                let id = object
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool result id is invalid"))?;
                if !calls.remove(id) {
                    anyhow::bail!("tool result references an unknown or resolved tool call");
                }
                let translated = match object.get("content") {
                    None | Some(Value::Null) => TranslatedToolResult::default(),
                    Some(Value::String(text)) => {
                        if text.len() > MAX_TOOL_RESULT_OUTPUT_BYTES {
                            anyhow::bail!("tool result output exceeds the size limit");
                        }
                        TranslatedToolResult {
                            output: text.clone(),
                            images: Vec::new(),
                        }
                    }
                    Some(Value::Array(parts)) => tool_result_parts_to_text(parts)?,
                    Some(_) => anyhow::bail!("tool result content must be text or an array"),
                };
                let output = if object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    if translated.output.is_empty() {
                        "[tool execution error]".to_string()
                    } else {
                        format!("[tool execution error]\n{}", translated.output)
                    }
                } else {
                    translated.output
                };
                if output.len().saturating_add(
                    translated
                        .images
                        .iter()
                        .map(grok_content_part_payload_bytes)
                        .sum::<usize>(),
                ) > MAX_TOOL_RESULT_OUTPUT_BYTES
                {
                    anyhow::bail!("tool result output exceeds the size limit");
                }
                out.push(GrokInputItem::FunctionCallOutput {
                    call_id: id.into(),
                    output,
                });
                for image in translated.images {
                    tool_result_images.push(GrokContentPart::InputText {
                        text: format!("[image from tool result call_id: {id}]"),
                    });
                    tool_result_images.push(image);
                }
            }
            _ => anyhow::bail!("unsupported content block: {typ}"),
        }
    }
    flush_message(&message.role, &mut content, out);
    flush_message(&message.role, &mut tool_result_images, out);
    Ok(())
}

enum ValidatedImageSource<'a> {
    Base64 {
        media_type: &'a str,
        data: &'a str,
        estimated_tokens: u64,
    },
    Url(&'a str),
}

fn validate_image_source(value: Option<&Value>) -> anyhow::Result<ValidatedImageSource<'_>> {
    let source = value
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("image source must be an object"))?;
    let source_type = source
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("image source type is invalid"))?;
    match source_type {
        "base64" => {
            if !source
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "media_type" | "data"))
            {
                anyhow::bail!("unsupported base64 image source field");
            }
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|value| matches!(*value, "image/jpeg" | "image/png"))
                .ok_or_else(|| anyhow::anyhow!("unsupported image media type"))?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("image base64 data is empty"))?;
            let estimated_tokens =
                match super::super::count_tokens::validate_and_estimate_base64_image(
                    data,
                    MAX_GROK_IMAGE_BYTES,
                    media_type,
                ) {
                    Ok(estimate) => estimate,
                    Err(super::super::count_tokens::Base64ImageError::Invalid) => {
                        anyhow::bail!("image base64 data is invalid")
                    }
                    Err(super::super::count_tokens::Base64ImageError::TooSmall) => {
                        anyhow::bail!(
                            "Grok images must be at least 8 pixels on each side and 512 total pixels"
                        )
                    }
                    Err(super::super::count_tokens::Base64ImageError::TooLarge) => {
                        anyhow::bail!(
                            "image exceeds the 20 MiB input limit or safe decoded image limits"
                        )
                    }
                };

            Ok(ValidatedImageSource::Base64 {
                media_type,
                data,
                estimated_tokens,
            })
        }
        "url" => {
            if !source
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "url"))
            {
                anyhow::bail!("unsupported URL image source field");
            }
            let raw = source
                .get("url")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("image URL is empty"))?;
            let parsed =
                url::Url::parse(raw).map_err(|_| anyhow::anyhow!("image URL is invalid"))?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
            {
                anyhow::bail!("image URL must be an HTTP(S) URL without credentials");
            }
            Ok(ValidatedImageSource::Url(raw))
        }
        _ => anyhow::bail!("unsupported image source type"),
    }
}

fn parse_image_source(value: Option<&Value>) -> anyhow::Result<(String, Option<u64>)> {
    match validate_image_source(value)? {
        ValidatedImageSource::Base64 {
            media_type,
            data,
            estimated_tokens,
        } => {
            // Build the final request value directly from the borrowed Anthropic
            // payload. Avoid an intermediate owned base64 String at peak size.
            let mut image_url = String::with_capacity(
                "data:".len() + media_type.len() + ";base64,".len() + data.len(),
            );
            image_url.push_str("data:");
            image_url.push_str(media_type);
            image_url.push_str(";base64,");
            image_url.push_str(data);
            Ok((image_url, Some(estimated_tokens)))
        }
        ValidatedImageSource::Url(url) => Ok((url.to_string(), None)),
    }
}

fn valid_cache_control(value: Option<&Value>) -> bool {
    let Some(value) = value else { return true };
    if value.is_null() {
        return true;
    }
    let Some(object) = value.as_object() else {
        return false;
    };
    object.keys().all(|key| key == "type" || key == "ttl")
        && object.get("type").and_then(Value::as_str) == Some("ephemeral")
        && object
            .get("ttl")
            .is_none_or(|ttl| matches!(ttl.as_str(), Some("5m") | Some("1h")))
}

fn valid_tool_caller(value: Option<&Value>) -> bool {
    let Some(value) = value else { return true };
    value
        .as_object()
        .and_then(|caller| caller.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| !kind.is_empty())
}

#[derive(Default)]
struct TranslatedToolResult {
    output: String,
    images: Vec<GrokContentPart>,
}

fn tool_result_parts_to_text(parts: &[Value]) -> anyhow::Result<TranslatedToolResult> {
    let mut rendered = Vec::new();
    let mut images = Vec::new();
    let mut image_payload_bytes = 0usize;

    for part in parts {
        let part = part
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("tool result child must be an object"))?;
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if part.keys().any(|key| {
                    !["type", "text", "citations", "cache_control"].contains(&key.as_str())
                }) || !valid_cache_control(part.get("cache_control"))
                    || part
                        .get("citations")
                        .is_some_and(|citations| !citations.is_null() && !citations.is_array())
                {
                    anyhow::bail!("tool result text child is invalid");
                }
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("tool result text is invalid"))?;
                append_tool_result_segment(&mut rendered, image_payload_bytes, text)?;
            }
            Some("tool_reference") => {
                if part
                    .keys()
                    .any(|key| !["type", "tool_name", "cache_control"].contains(&key.as_str()))
                    || !valid_cache_control(part.get("cache_control"))
                {
                    anyhow::bail!("tool result tool_reference child is invalid");
                }
                let tool_name = part
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool result tool_name is invalid"))?;
                append_tool_result_segment(
                    &mut rendered,
                    image_payload_bytes,
                    &format!("[tool reference: {tool_name}]"),
                )?;
            }
            Some("image") => {
                if part
                    .keys()
                    .any(|key| !["type", "source", "cache_control"].contains(&key.as_str()))
                    || !valid_cache_control(part.get("cache_control"))
                {
                    anyhow::bail!("tool result image child is invalid");
                }
                let (image_url, estimated_tokens) = parse_image_source(part.get("source"))?;
                image_payload_bytes = image_payload_bytes.saturating_add(image_url.len());
                append_tool_result_segment(
                    &mut rendered,
                    image_payload_bytes,
                    "[image tool result attached as the following user image]",
                )?;
                images.push(GrokContentPart::InputImage {
                    image_url,
                    estimated_tokens,
                });
            }
            Some("document" | "search_result") => {
                validate_structured_tool_result_part(part)?;
                // xAI function outputs are textual. Preserve non-image
                // structured blocks as compact JSON directly in the final
                // buffer, without creating a second large serialized copy.
                append_tool_result_json(&mut rendered, image_payload_bytes, part)?;
            }
            _ => anyhow::bail!(
                "tool result supports text, image, document, search_result, and tool_reference children"
            ),
        }
    }

    Ok(TranslatedToolResult {
        output: String::from_utf8(rendered).expect("text and JSON serialization are UTF-8"),
        images,
    })
}

fn append_tool_result_segment(
    rendered: &mut Vec<u8>,
    image_payload_bytes: usize,
    segment: &str,
) -> anyhow::Result<()> {
    if segment.is_empty() {
        return Ok(());
    }
    let separator = !rendered.is_empty()
        && !std::str::from_utf8(rendered)
            .expect("tool result buffer is UTF-8")
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
        && !segment.chars().next().is_some_and(char::is_whitespace);
    let additional = segment.len() + usize::from(separator);
    if rendered
        .len()
        .saturating_add(image_payload_bytes)
        .saturating_add(additional)
        > MAX_TOOL_RESULT_OUTPUT_BYTES
    {
        anyhow::bail!("tool result output exceeds the size limit");
    }
    if separator {
        rendered.push(b'\n');
    }
    rendered.extend_from_slice(segment.as_bytes());
    Ok(())
}

struct BoundedToolResultWriter<'a> {
    rendered: &'a mut Vec<u8>,
    max_rendered_bytes: usize,
}

impl Write for BoundedToolResultWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.rendered.len().saturating_add(bytes.len()) > self.max_rendered_bytes {
            return Err(std::io::Error::other(
                "tool result output exceeds the size limit",
            ));
        }
        self.rendered.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn append_tool_result_json(
    rendered: &mut Vec<u8>,
    image_payload_bytes: usize,
    value: &serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    let separator = !rendered.is_empty()
        && !std::str::from_utf8(rendered)
            .expect("tool result buffer is UTF-8")
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
    if separator {
        if rendered
            .len()
            .saturating_add(image_payload_bytes)
            .saturating_add(1)
            > MAX_TOOL_RESULT_OUTPUT_BYTES
        {
            anyhow::bail!("tool result output exceeds the size limit");
        }
        rendered.push(b'\n');
    }
    let max_rendered_bytes = MAX_TOOL_RESULT_OUTPUT_BYTES
        .checked_sub(image_payload_bytes)
        .ok_or_else(|| anyhow::anyhow!("tool result output exceeds the size limit"))?;
    serde_json::to_writer(
        BoundedToolResultWriter {
            rendered,
            max_rendered_bytes,
        },
        value,
    )
    .map_err(|_| anyhow::anyhow!("tool result output exceeds the size limit"))?;
    Ok(())
}

fn validate_structured_tool_result_part(
    part: &serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    if !valid_cache_control(part.get("cache_control")) {
        anyhow::bail!("tool result structured child cache_control is invalid");
    }
    match part.get("type").and_then(Value::as_str) {
        Some("document") => {
            if part.keys().any(|key| {
                ![
                    "type",
                    "source",
                    "citations",
                    "context",
                    "title",
                    "cache_control",
                ]
                .contains(&key.as_str())
            }) || !part.get("source").is_some_and(Value::is_object)
            {
                anyhow::bail!("tool result document child is invalid");
            }
        }
        Some("search_result") => {
            if part.keys().any(|key| {
                ![
                    "type",
                    "source",
                    "title",
                    "content",
                    "citations",
                    "cache_control",
                ]
                .contains(&key.as_str())
            }) || part
                .get("source")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || part
                    .get("title")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                || !part.get("content").is_some_and(Value::is_array)
            {
                anyhow::bail!("tool result search_result child is invalid");
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn grok_content_part_payload_bytes(part: &GrokContentPart) -> usize {
    match part {
        GrokContentPart::InputImage { image_url, .. } => image_url.len(),
        GrokContentPart::InputText { text } | GrokContentPart::OutputText { text } => text.len(),
    }
}

fn flush_message(role: &str, content: &mut Vec<GrokContentPart>, out: &mut Vec<GrokInputItem>) {
    if !content.is_empty() {
        out.push(GrokInputItem::Message {
            role: role.into(),
            content: std::mem::take(content),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_translation_filters_billing_header_and_separates_system_blocks() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":[
                {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.212"},
                {"type":"text","text":"Follow repository rules."},
                {"type":"text","text":"Keep answers concise.","cache_control":{"type":"ephemeral"}}
            ],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();

        let translated = translate_request(&request, "grok-4.5".into()).unwrap();
        let instructions = translated.instructions.unwrap();
        assert!(instructions.starts_with("Follow repository rules.\n\nKeep answers concise."));
        assert!(!instructions.contains("x-anthropic-billing-header:"));
    }

    #[test]
    fn grok_translation_accepts_tool_type_and_disable_parallel_tool_use() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{
                "type":"custom",
                "name":"lookup",
                "description":"Look things up",
                "input_schema":{"type":"object"}
            }],
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(
            translated["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "lookup" })
        );
        assert_eq!(translated["tool_choice"], "auto");
        assert_eq!(translated["parallel_tool_calls"], false);
    }

    #[test]
    fn grok_translation_enables_parallel_custom_tools_by_default() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"name":"read_first","input_schema":{"type":"object"}},
                {"name":"read_second","input_schema":{"type":"object"}}
            ],
            "messages":[{"role":"user","content":"inspect both files"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["parallel_tool_calls"], true);
        let instructions = translated["instructions"].as_str().unwrap();
        assert!(instructions.starts_with(PARALLEL_FUNCTION_TOOL_GUIDANCE));
        assert!(instructions.contains("including repeated calls to the same tool"));
        for undeclared in ["Read", "Grep", "Glob", "Edit", "Bash"] {
            assert!(
                !instructions.contains(undeclared),
                "parallel guidance mentioned undeclared tool {undeclared}: {instructions}"
            );
        }
    }

    #[test]
    fn grok_translation_guides_parallel_calls_to_one_function_tool() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{"name":"Read","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"Read"},
            "messages":[{"role":"user","content":"read both files"}]
        }))
        .unwrap();

        let translated = translate_request(&request, "grok-4.5".into()).unwrap();
        assert_eq!(translated.parallel_tool_calls, Some(true));
        let instructions = translated.instructions.unwrap();
        assert!(instructions.starts_with(PARALLEL_FUNCTION_TOOL_GUIDANCE));
        assert!(instructions.contains("repeated calls to the same tool"));
    }

    #[test]
    fn grok_forced_function_guidance_excludes_other_declared_tools() {
        for forced in ["Read", "Bash"] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "tools":[{
                    "name":"Read",
                    "strict":true,
                    "input_schema":{"type":"object"}
                },{
                    "name":"Grep",
                    "strict":true,
                    "input_schema":{"type":"object"}
                },{
                    "name":"Bash",
                    "strict":true,
                    "input_schema":{"type":"object"}
                },{
                    "type":"web_search_20260318",
                    "name":"web_search",
                    "max_uses":2,
                    "user_location":{"type":"approximate","city":"Nanjing"},
                    "allowed_callers":["direct"]
                },{
                    "name":"XSearch",
                    "input_schema":{"type":"object"}
                }],
                "tool_choice":{"type":"tool","name":forced},
                "messages":[{"role":"user","content":"run independent calls"}]
            }))
            .unwrap();

            let translated =
                serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                    .unwrap();
            assert_eq!(translated["tools"].as_array().unwrap().len(), 5);
            assert_eq!(translated["tool_choice"]["type"], "function");
            assert_eq!(translated["tool_choice"]["name"], forced);
            assert_eq!(translated["parallel_tool_calls"], true);

            let instructions = translated["instructions"].as_str().unwrap();
            assert!(instructions.starts_with(PARALLEL_FUNCTION_TOOL_GUIDANCE));
            assert!(instructions.contains("only to that selected function"));
            assert!(instructions.contains(&format!("When calling {forced}")));
            for other in ["Read", "Grep", "Bash"]
                .into_iter()
                .filter(|name| *name != forced)
            {
                assert!(
                    !instructions.contains(other),
                    "forced {forced} guidance mentioned {other}: {instructions}"
                );
            }
            for forbidden in [
                "web_search",
                "x_search",
                "XSearch",
                "Nanjing",
                "no more than",
            ] {
                assert!(
                    !instructions.contains(forbidden),
                    "forced {forced} guidance mentioned {forbidden}: {instructions}"
                );
            }
        }
    }

    #[test]
    fn grok_parallel_tool_guidance_is_prepended_before_large_system_prompt() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system": format!("SESSION CONTEXT {}", "x ".repeat(200)),
            "tools":[
                {"name":"Read","input_schema":{"type":"object"}},
                {"name":"Bash","input_schema":{"type":"object"}},
                {"name":"Grep","input_schema":{"type":"object"}}
            ],
            "messages":[{"role":"user","content":"inspect the repo"}]
        }))
        .unwrap();

        let translated = translate_request(&request, "grok-4.5".into()).unwrap();
        let instructions = translated.instructions.unwrap();
        assert!(instructions.starts_with(PARALLEL_FUNCTION_TOOL_GUIDANCE));
        assert!(instructions.contains("SESSION CONTEXT"));
        assert!(instructions.contains("declared dedicated tools (Read, Grep)"));
        assert!(instructions.contains("one large Bash script"));
        assert!(instructions.contains("Keep each Bash command focused"));
        assert!(!instructions.contains("Glob"));
        assert!(!instructions.contains("Edit"));
        let guidance_end = PARALLEL_FUNCTION_TOOL_GUIDANCE.len();
        assert!(
            instructions[guidance_end..].contains("SESSION CONTEXT"),
            "session context should remain after the parallel-tool policy"
        );
    }

    #[test]
    fn grok_parallel_tool_guidance_uses_only_final_non_deferred_tools() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"name":"Read","input_schema":{"type":"object"}},
                {"name":"Grep","input_schema":{"type":"object"}},
                {"name":"Bash","defer_loading":true,"input_schema":{"type":"object"}}
            ],
            "messages":[{"role":"user","content":"inspect the repo"}]
        }))
        .unwrap();

        let translated = translate_request(&request, "grok-4.5".into()).unwrap();
        assert!(
            translated
                .tools
                .as_ref()
                .unwrap()
                .iter()
                .all(|tool| tool.name.as_deref() != Some("Bash"))
        );
        let instructions = translated.instructions.unwrap();
        assert!(instructions.starts_with(PARALLEL_FUNCTION_TOOL_GUIDANCE));
        assert!(!instructions.contains("Bash"));
        assert!(!instructions.contains("Glob"));
        assert!(!instructions.contains("Edit"));
    }

    #[test]
    fn grok_parallel_tool_guidance_respects_explicit_disable() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"name":"Read","input_schema":{"type":"object"}},
                {"name":"Grep","input_schema":{"type":"object"}}
            ],
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true},
            "messages":[{"role":"user","content":"inspect the repo"}]
        }))
        .unwrap();

        let translated = translate_request(&request, "grok-4.5".into()).unwrap();
        assert_eq!(translated.parallel_tool_calls, Some(false));
        assert!(translated.instructions.is_none());
    }

    #[test]
    fn grok_translation_omits_parallel_flag_without_tools() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(translated.get("tools").is_none());
        assert!(translated.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn grok_translation_maps_typed_anthropic_web_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "messages":[{"role":"user","content":"search online for the project"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"web_search"}])
        );
        assert!(translated.get("tool_choice").is_none());
    }

    #[test]
    fn grok_translation_rejects_invalid_tool_type_and_parallel_flag() {
        for request in [
            serde_json::json!({
                "model":"grok-4.5",
                "tools":[{"type":1,"name":"lookup","input_schema":{"type":"object"}}],
                "messages":[{"role":"user","content":"hello"}]
            }),
            serde_json::json!({
                "model":"grok-4.5",
                "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
                "tool_choice":{"type":"auto","disable_parallel_tool_use":"yes"},
                "messages":[{"role":"user","content":"hello"}]
            }),
        ] {
            let request: MessagesRequest = serde_json::from_value(request).unwrap();
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_preserves_tool_result_error_semantics() {
        let request = request_with_blocks(serde_json::json!([{
            "type":"tool_result",
            "tool_use_id":"call_1",
            "content":"permission denied",
            "is_error":true
        }]));

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(
            translated["input"][1]["output"],
            "[tool execution error]\npermission denied"
        );
    }

    #[test]
    fn grok_translation_maps_base64_and_url_images() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"compare these"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAGklEQVR4nO3BAQEAAACCIP+vbkhAAQAAAO8GECAAARlDNO4AAAAASUVORK5CYII="},"cache_control":{"type":"ephemeral"}},
                {"type":"image","source":{"type":"url","url":"https://example.com/image.png?size=large"}}
            ]}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let content = translated["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(
            content[1]["image_url"],
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAGklEQVR4nO3BAQEAAACCIP+vbkhAAQAAAO8GECAAARlDNO4AAAAASUVORK5CYII="
        );
        assert_eq!(content[2]["type"], "input_image");
        assert_eq!(
            content[2]["image_url"],
            "https://example.com/image.png?size=large"
        );
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_rejects_unsafe_or_malformed_images() {
        for source in [
            serde_json::json!({"type":"base64","media_type":"image/svg+xml","data":"aGVsbG8="}),
            serde_json::json!({"type":"base64","media_type":"image/gif","data":"R0lGODlhAQABAAAAACw="}),
            serde_json::json!({"type":"base64","media_type":"image/webp","data":"UklGRgAAAABXRUJQ"}),
            serde_json::json!({"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}),
            serde_json::json!({"type":"base64","media_type":"image/png","data":"not base64"}),
            serde_json::json!({"type":"url","url":"file:///tmp/image.png"}),
            serde_json::json!({"type":"url","url":"https://user:pass@example.com/image.png"}),
            serde_json::json!({"type":"file","file_id":"file_1"}),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":[{"type":"image","source":source}]}]
            }))
            .unwrap();
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_replays_hosted_search_history() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"user","content":"search X for the project"},
                {"role":"assistant","content":[
                    {"type":"server_tool_use","id":"srvtoolu_1","name":"x_search","input":{"query":"project"}},
                    {"type":"x_search_tool_result","tool_use_id":"srvtoolu_1","content":[]},
                    {"type":"text","text":"Found it"}
                ]},
                {"role":"user","content":"summarize it"}
            ]
        }))
        .unwrap();
        let translated = translate_request(&request, "grok-4.5".into()).unwrap();
        let value = serde_json::to_value(translated).unwrap();
        assert!(value["input"].as_array().unwrap().iter().any(|item| {
            item["role"] == "assistant" && item["content"][0]["text"] == "Found it"
        }));
        assert!(!value.to_string().contains("srvtoolu_1"));
    }

    #[test]
    fn grok_translation_validates_hosted_history_identity_kind_and_order() {
        let cases = [
            (
                serde_json::json!([
                    {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}},
                    {"type":"x_search_tool_result","tool_use_id":"srv_1","content":[]}
                ]),
                "kind mismatch",
            ),
            (
                serde_json::json!([
                    {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}},
                    {"type":"web_search_tool_result","tool_use_id":"srv_other","content":[]}
                ]),
                "unknown",
            ),
            (
                serde_json::json!([
                    {"type":"web_search_tool_result","tool_use_id":"srv_1","content":[]},
                    {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}
                ]),
                "not-yet-seen",
            ),
            (
                serde_json::json!([
                    {"type":"server_tool_use","id":"srv_1","name":"web_search"},
                    {"type":"web_search_tool_result","tool_use_id":"srv_1","content":[]}
                ]),
                ".input is required",
            ),
            (
                serde_json::json!([
                    {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}},
                    {"type":"web_search_tool_result","tool_use_id":"srv_1"}
                ]),
                ".content is required",
            ),
        ];
        for (content, expected) in cases {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"assistant","content":content}]
            }))
            .unwrap();
            let error = translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn grok_translation_accepts_mixed_client_and_server_tool_history() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"rust"}},
                    {"type":"web_search_tool_result","tool_use_id":"srv_1","content":[]},
                    {"type":"tool_use","id":"call_1","name":"Read","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"done"},
                    {"type":"text","text":"continue"}
                ]}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][0]["type"], "function_call");
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert!(!translated.to_string().contains("srv_1"));
    }

    #[test]
    fn grok_translation_replays_delayed_hosted_result_after_client_tool_pause() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"rust"}},
                    {"type":"tool_use","id":"call_1","name":"Read","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"done"}
                ]},
                {"role":"assistant","content":[
                    {"type":"web_search_tool_result","tool_use_id":"srv_1","content":[]},
                    {"type":"text","text":"combined result"}
                ]},
                {"role":"user","content":"continue"}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][0]["type"], "function_call");
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(translated["input"][2]["role"], "assistant");
        assert_eq!(
            translated["input"][2]["content"][0]["text"],
            "combined result"
        );
        assert!(!translated.to_string().contains("srv_1"));
    }

    #[test]
    fn grok_translation_maps_text_and_function_round_trip() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5", "max_tokens":12, "system":"rules",
            "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"lookup"},
            "messages":[
              {"role":"user","content":"hello"},
              {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{"q":"a"}}]},
              {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"result"}]}
            ]
        })).unwrap();
        let value =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let instructions = value["instructions"].as_str().unwrap();
        assert!(instructions.starts_with(PARALLEL_FUNCTION_TOOL_GUIDANCE));
        assert!(instructions.ends_with("rules"));
        assert_eq!(value["input"][1]["type"], "function_call");
        assert_eq!(value["input"][2]["type"], "function_call_output");
        assert_eq!(value["tool_choice"]["type"], "function");
    }

    #[test]
    fn grok_translation_forwards_supported_reasoning_effort() {
        for (requested, expected) in [
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "high"),
            ("max", "high"),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"hello"}],
                "output_config":{"effort":requested}
            }))
            .unwrap();
            let translated =
                serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                    .unwrap();
            assert_eq!(translated["reasoning"]["effort"], expected);
        }
    }

    #[test]
    fn grok_translation_rejects_ultra_reasoning_effort() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "output_config":{"effort":"ultra"}
        }))
        .unwrap();
        assert_eq!(
            translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string(),
            "Invalid output_config.effort: ultra"
        );
    }

    #[test]
    fn grok_translation_forwards_title_schema_without_search_tools() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"Generate a concise session title"}],
            "output_config": {"format": {
                "type": "json_schema",
                "name": "session_title",
                "schema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "short": {"type": "boolean"}
                    },
                    "required": ["title"]
                }
            }}
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["text"]["format"]["type"], "json_schema");
        assert_eq!(translated["text"]["format"]["name"], "session_title");
        assert_eq!(translated["text"]["format"]["strict"], true);
        let required = translated["text"]["format"]["schema"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|value| value == "title"));
        assert!(!required.iter().any(|value| value == "short"));
        assert!(
            translated["text"]["format"]["schema"]["properties"]["short"]
                .get("anyOf")
                .is_none()
        );
        assert!(translated.get("tools").is_none());
        assert!(!translated.to_string().contains("x_search"));
    }

    #[test]
    fn grok_translation_rejects_malformed_output_formats() {
        for (case, output_config) in [
            ("non-object format", serde_json::json!({"format":[]})),
            ("missing type", serde_json::json!({"format":{}})),
            (
                "unknown type",
                serde_json::json!({"format":{"type":"json_schmea"}}),
            ),
            (
                "missing schema",
                serde_json::json!({"format":{"type":"json_schema"}}),
            ),
            (
                "non-object schema",
                serde_json::json!({"format":{"type":"json_schema","schema":[]}}),
            ),
            (
                "empty name",
                serde_json::json!({"format":{"type":"json_schema","name":"","schema":{}}}),
            ),
            (
                "non-string name",
                serde_json::json!({"format":{"type":"json_schema","name":1,"schema":{}}}),
            ),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"hello"}],
                "output_config":output_config
            }))
            .unwrap();
            let error = translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string();
            assert!(!error.is_empty(), "{case}");
        }

        let effort_only: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "output_config":{"effort":"high"}
        }))
        .unwrap();
        assert!(translate_request(&effort_only, "grok-4.5".into()).is_ok());
    }

    #[test]
    fn grok_translation_preserves_nested_system_text_messages_in_order() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"top-level rules",
            "messages":[
                {"role":"system","content":"first nested rules"},
                {"role":"user","content":"hello"},
                {"role":"system","content":[
                    {"type":"text","text":"second nested rules","cache_control":{"type":"ephemeral"}},
                    {"type":"text","text":"third nested rules"}
                ]},
                {"role":"assistant","content":"ack"}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["instructions"], "top-level rules");
        assert_eq!(translated["input"].as_array().unwrap().len(), 4);
        assert_eq!(translated["input"][0]["role"], "system");
        assert_eq!(
            translated["input"][0]["content"][0]["text"],
            "first nested rules"
        );
        assert_eq!(translated["input"][1]["role"], "user");
        assert_eq!(translated["input"][2]["role"], "system");
        assert_eq!(
            translated["input"][2]["content"][0]["text"],
            "second nested rules"
        );
        assert_eq!(
            translated["input"][2]["content"][1]["text"],
            "third nested rules"
        );
        assert_eq!(translated["input"][3]["role"], "assistant");
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_preserves_empty_system_string_and_rejects_empty_block_list() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"system","content":""},
                {"role":"user","content":"hello"}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"].as_array().unwrap().len(), 2);
        assert_eq!(translated["input"][0]["role"], "system");
        assert_eq!(translated["input"][0]["content"][0]["text"], "");

        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"system","content":[]}]
        }))
        .unwrap();
        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must contain at least one text block"),
            "{error}"
        );
    }

    #[test]
    fn grok_translation_rejects_unknown_message_roles() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"foo","content":"do not reinterpret me"}]
        }))
        .unwrap();
        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(error.contains("role must be user or assistant"), "{error}");
    }

    #[test]
    fn grok_translation_rejects_non_text_nested_system_blocks() {
        for block in [
            serde_json::json!({"type":"image","source":{"type":"url","url":"https://example.com/a.png"}}),
            serde_json::json!({"type":"tool_use","id":"tool_1","name":"Read","input":{}}),
            serde_json::json!({"type":"tool_result","tool_use_id":"tool_1","content":"result"}),
            serde_json::json!({"type":"thinking","thinking":"private"}),
            serde_json::json!({"type":"future_widget","payload":42}),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"system","content":[block]}]
            }))
            .unwrap();
            let error = translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("must be a text block for system role"),
                "{error}"
            );
        }
    }

    #[test]
    fn grok_translation_rejects_invalid_nested_system_text_fields() {
        for block in [
            serde_json::json!({
                "type":"text",
                "text":"rules",
                "cache_control":{"type":"persistent"}
            }),
            serde_json::json!({"type":"text","text":"rules","citations":[]}),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"system","content":[block]}]
            }))
            .unwrap();
            let error = translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string();
            assert!(error.contains("unsupported text block field"), "{error}");
        }
    }

    #[test]
    fn grok_translation_keeps_tool_result_adjacency_fail_closed() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{
                    "type":"tool_use","id":"call_1","name":"lookup","input":{}
                }]},
                {"role":"system","content":"interposed rules"},
                {"role":"user","content":[{
                    "type":"tool_result","tool_use_id":"call_1","content":"ok"
                }]}
            ]
        }))
        .unwrap();

        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must be resolved in the immediately following user message"),
            "{error}"
        );
    }

    #[test]
    fn grok_translation_allows_system_after_tool_result_is_resolved() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{
                    "type":"tool_use","id":"call_1","name":"lookup","input":{}
                }]},
                {"role":"user","content":[{
                    "type":"tool_result","tool_use_id":"call_1","content":"ok"
                }]},
                {"role":"system","content":"rules for the next turn"},
                {"role":"user","content":"continue"}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"].as_array().unwrap().len(), 4);
        assert_eq!(translated["input"][0]["type"], "function_call");
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(translated["input"][2]["role"], "system");
        assert_eq!(
            translated["input"][2]["content"][0]["text"],
            "rules for the next turn"
        );
        assert_eq!(translated["input"][3]["role"], "user");
    }

    #[test]
    fn grok_translation_rejects_invalid_message_content_and_text_shapes() {
        for (content, expected) in [
            (
                serde_json::json!(42),
                "content must be a string or array of content blocks",
            ),
            (
                serde_json::json!({"type":"text","text":"not wrapped in an array"}),
                "content must be a string or array of content blocks",
            ),
            (
                serde_json::json!([{"type":"text"}]),
                "content[0].text must be a string",
            ),
            (
                serde_json::json!([{"type":"text","text":42}]),
                "content[0].text must be a string",
            ),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":content}]
            }))
            .unwrap();
            let error = translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn grok_compaction_disables_all_tools_and_keeps_high_effort() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5-high",
            "messages":[{"role":"user","content":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far."}],
            "tools":[{"name":"Bash","input_schema":{"type":"object"}}],
            "output_config":{"effort":"high"}
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["reasoning"]["effort"], "high");
        assert!(translated.get("tools").is_none());
        assert!(translated.get("tool_choice").is_none());
        assert!(translated.get("parallel_tool_calls").is_none());
        assert!(!translated.to_string().contains("x_search"));
    }

    #[test]
    fn grok_translation_rejects_effort_for_composer() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-composer-2.5-fast",
            "messages":[{"role":"user","content":"hello"}],
            "output_config":{"effort":"high"}
        }))
        .unwrap();
        let error = translate_request(&request, "grok-composer-2.5-fast".into())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not support output_config.effort"),
            "{error}"
        );
    }

    #[test]
    fn grok_translation_keeps_composer_without_effort_valid() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-composer-2.5-fast",
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let translated = translate_request(&request, "grok-composer-2.5-fast".into()).unwrap();
        assert_eq!(translated.model, "grok-composer-2.5-fast");
        assert!(translated.reasoning.is_none());
    }

    #[test]
    fn explicitly_typed_search_names_remain_exact_functions() {
        for name in ["WebSearch", "XSearch"] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "tools":[{
                    "type":"function",
                    "name":name,
                    "description":"client-owned search",
                    "input_schema":{
                        "type":"object",
                        "properties":{"query":{"type":"string"}},
                        "required":["query"]
                    }
                }],
                "tool_choice":{"type":"tool","name":name},
                "messages":[{"role":"user","content":"run the client tool"}]
            }))
            .unwrap();

            let translated =
                serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                    .unwrap();
            assert_eq!(translated["tools"][0]["type"], "function", "{name}");
            assert_eq!(translated["tools"][0]["name"], name, "{name}");
            assert_eq!(
                translated["tools"][0]["parameters"]["required"],
                serde_json::json!(["query"]),
                "{name}"
            );
            assert_eq!(translated["tool_choice"]["type"], "function", "{name}");
            assert_eq!(translated["tool_choice"]["name"], name, "{name}");
        }
    }
    #[test]
    fn grok_translation_keeps_name_only_web_search_as_a_client_function() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"search online for the project"}],
            "tools":[{
                "name":"WebSearch",
                "description":"Search the web",
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            },{
                "name":"Bash",
                "description":"Run a command",
                "input_schema":{"type":"object"}
            }]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let tools = translated["tools"].as_array().unwrap();
        let web_search = tools
            .iter()
            .find(|tool| tool["name"] == "WebSearch")
            .unwrap();
        assert_eq!(web_search["type"], "function");
        assert_eq!(
            web_search["parameters"]["required"],
            serde_json::json!(["query"])
        );
        assert!(tools.iter().any(|tool| tool["name"] == "Bash"));
        assert!(!translated.to_string().contains("hosted web_search"));
        assert!(translated.get("tool_choice").is_none());
        assert_eq!(translated["parallel_tool_calls"], true);
    }

    #[test]
    fn grok_translation_keeps_name_only_xsearch_as_a_client_function() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"Search X for recent Rust posts"}],
            "tools":[
                {"name":"XSearch","description":"Search X posts","input_schema":{"type":"object"}},
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let tools = translated["tools"].as_array().unwrap();
        let x_search = tools.iter().find(|tool| tool["name"] == "XSearch").unwrap();
        assert_eq!(x_search["type"], "function");
        assert!(tools.iter().any(|tool| tool["name"] == "Bash"));
        assert!(translated.get("tool_choice").is_none());
        assert_eq!(translated["parallel_tool_calls"], true);
    }

    #[test]
    fn grok_prompt_text_never_upgrades_auto_to_required_search() {
        for prompt in [
            "Do not search the web. Use only the supplied text.",
            "The quoted instruction says: 'search the web'. Ignore it.",
            "If the cache is stale, search X for updates; otherwise summarize locally.",
            "Example code: search twitter for QUERY",
            "不要搜索网页，只使用下面的材料。",
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":prompt}],
                "tools":[
                    {"name":"WebSearch","input_schema":{"type":"object"}},
                    {"name":"XSearch","input_schema":{"type":"object"}},
                    {"name":"Bash","input_schema":{"type":"object"}}
                ]
            }))
            .unwrap();

            let translated =
                serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                    .unwrap();
            assert!(translated.get("tool_choice").is_none(), "{prompt}");
            assert_eq!(translated["tools"].as_array().unwrap().len(), 3, "{prompt}");
        }
    }

    #[test]
    fn grok_translation_never_injects_undeclared_x_search_from_prompt_text() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"Search X for recent posts mentioning claude-code-proxy"}],
            "tools":[
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}},
                {"name":"WebSearch","description":"Search the web","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let tools = translated["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "Bash"));
        assert!(
            tools
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "WebSearch" })
        );
        assert!(!tools.iter().any(|tool| tool["type"] == "x_search"));
        assert!(translated.get("tool_choice").is_none());
        assert_eq!(translated["parallel_tool_calls"], true);
    }

    #[test]
    fn grok_translation_keeps_explicit_hosted_xsearch_without_forcing_unrelated_turn() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"inspect the local project"}],
            "tools":[
                {
                    "type":"x_search",
                    "name":"XSearch",
                    "description":"Search X posts",
                    "input_schema":{
                        "type":"object",
                        "properties":{
                            "query":{"type":"string"},
                            "allowed_x_handles":{"type":"array","items":{"type":"string"}},
                            "excluded_x_handles":{"type":"array","items":{"type":"string"}},
                            "from_date":{"type":"string","format":"date"},
                            "to_date":{"type":"string","format":"date"}
                        },
                        "required":["query"]
                    },
                },
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let tools = translated["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["type"] == "x_search"));
        assert!(tools.iter().any(|tool| tool["name"] == "Bash"));
        assert!(translated.get("tool_choice").is_none());
        assert_eq!(translated["parallel_tool_calls"], true);
    }

    #[test]
    fn grok_translation_rejects_non_empty_context_management() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-composer-2.5-fast",
            "messages":[{"role":"user","content":"hello"}],
            "context_management":{"edits":[{"type":"clear_tool_uses_20250919","trigger":{"type":"input_tokens","value":100000}}]}
        }))
        .unwrap();
        let error = translate_request(&request, "grok-composer-2.5-fast".into())
            .unwrap_err()
            .to_string();
        assert!(error.contains("modify conversation history"), "{error}");
    }

    #[test]
    fn grok_translation_rejects_standard_only_service_tier() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "service_tier":"standard_only"
        }))
        .unwrap();
        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(error.contains("service_tier=standard_only"), "{error}");
    }

    #[test]
    fn grok_translation_rejects_unknown_fields() {
        let request: MessagesRequest = serde_json::from_value(
            serde_json::json!({"model":"grok-4.5","messages":[],"unknown_field":true}),
        )
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_accepts_verified_cache_control_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":[{"type":"text","text":"rules","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"5m"}}]}]
        })).unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .starts_with("rules")
        );
        assert_eq!(translated["input"][0]["content"][0]["text"], "hello");
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_rejects_invalid_cache_control() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5", "messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"persistent"}}]}]
        })).unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());

        let request = request_with_blocks(serde_json::json!([{
            "type":"tool_result",
            "tool_use_id":"call_1",
            "content":"ok",
            "cache_control":{"type":"persistent"}
        }]));
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    fn request_with_blocks(blocks: Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":blocks}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn grok_translation_rejects_unknown_tool_block_fields() {
        let mut request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"ok"}
        ]));
        request.messages[0].content[0]["unknown"] = Value::Bool(true);
        assert!(translate_request(&request, "grok-4.5".into()).is_err());

        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"ok","unknown":true}
        ]));
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_rejects_malformed_tool_result_children() {
        for child in [
            serde_json::json!("text"),
            serde_json::json!({"text":"ok"}),
            serde_json::json!({"type":"image","text":"ok"}),
            serde_json::json!({"type":"text","text":1}),
            serde_json::json!({"type":"text","text":"ok","unknown":true}),
            serde_json::json!({"type":"tool_reference"}),
            serde_json::json!({"type":"tool_reference","tool_name":""}),
            serde_json::json!({"type":"tool_reference","tool_name":1}),
            serde_json::json!({"type":"tool_reference","tool_name":"WebFetch","unknown":true}),
            serde_json::json!({"type":"tool_reference","tool_name":"WebFetch","cache_control":{"type":"persistent"}}),
        ] {
            let request = request_with_blocks(serde_json::json!([
                {"type":"tool_result","tool_use_id":"call_1","content":[child]}
            ]));
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_maps_tool_reference_results_to_text() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5-high",
            "tools":[
                {"type":"custom", "name":"ToolSearch", "input_schema":{"type":"object"}},
                {"name":"mcp__plugin_context7_context7__resolve-library-id", "defer_loading":true, "input_schema":{"type":"object"}},
                {"name":"WebFetch", "defer_loading":true, "input_schema":{"type":"object"}}
            ],
            "messages":[
                {"role":"assistant","content":[{
                    "type":"tool_use",
                    "id":"call_tool_search_1",
                    "name":"ToolSearch",
                    "input":{"query":"select:WebFetch"}
                }]},
                {"role":"user","content":[{
                    "type":"tool_result",
                    "tool_use_id":"call_tool_search_1",
                    "content":[
                        {"type":"tool_reference","tool_name":"mcp__plugin_context7_context7__resolve-library-id"},
                        {"type":"tool_reference","tool_name":"WebFetch","cache_control":{"type":"ephemeral"}}
                    ]
                }]}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][0]["call_id"], "call_tool_search_1");
        assert_eq!(translated["input"][1]["call_id"], "call_tool_search_1");
        assert_eq!(
            translated["input"][1]["output"],
            "[tool reference: mcp__plugin_context7_context7__resolve-library-id]\n[tool reference: WebFetch]"
        );
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_preserves_text_concatenation_around_tool_references() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"name":"ToolSearch", "input_schema":{"type":"object"}},
                {"name":"WebFetch", "defer_loading":true, "input_schema":{"type":"object"}}
            ],
            "messages":[
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"call_1", "name":"ToolSearch", "input":{}
                }]},
                {"role":"user", "content":[{
                    "type":"tool_result",
                    "tool_use_id":"call_1",
                    "content":[
                        {"type":"text","text":"loaded "},
                        {"type":"text","text":"tools"},
                        {"type":"tool_reference","tool_name":"WebFetch"},
                        {"type":"text","text":"\nready"}
                    ]
                }]}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(
            translated["input"][1]["output"],
            "loaded tools\n[tool reference: WebFetch]\nready"
        );
    }

    #[test]
    fn grok_rejects_tool_references_without_valid_search_provenance() {
        let cases = [
            (
                "non-search source",
                "Read",
                serde_json::json!([
                    {"name":"Read", "input_schema":{"type":"object"}},
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                serde_json::json!([{"type":"tool_reference", "tool_name":"Bash"}]),
                "not a declared, non-deferred Claude Code client ToolSearch tool",
            ),
            (
                "unknown reference",
                "ToolSearch",
                serde_json::json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}}
                ]),
                serde_json::json!([{"type":"tool_reference", "tool_name":"Missing"}]),
                "not found in available tools",
            ),
            (
                "eager reference",
                "ToolSearch",
                serde_json::json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}},
                    {"name":"Bash", "input_schema":{"type":"object"}}
                ]),
                serde_json::json!([{"type":"tool_reference", "tool_name":"Bash"}]),
                "defer_loading=true",
            ),
            (
                "duplicate reference",
                "ToolSearch",
                serde_json::json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}},
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                serde_json::json!([
                    {"type":"tool_reference", "tool_name":"Bash"},
                    {"type":"tool_reference", "tool_name":"Bash"}
                ]),
                "duplicate tool_reference",
            ),
            (
                "mixed valid and invalid references",
                "ToolSearch",
                serde_json::json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}},
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                serde_json::json!([
                    {"type":"tool_reference", "tool_name":"Bash"},
                    {"type":"tool_reference", "tool_name":"Missing"}
                ]),
                "not found in available tools",
            ),
        ];

        for (case, caller, tools, references, expected) in cases {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "tools":tools,
                "messages":[
                    {"role":"assistant", "content":[{
                        "type":"tool_use", "id":"call_1", "name":caller, "input":{}
                    }]},
                    {"role":"user", "content":[{
                        "type":"tool_result", "tool_use_id":"call_1", "content":references
                    }]}
                ]
            }))
            .unwrap();

            let error = translate_request(&request, "grok-4.5".into())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected),
                "{case}: expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn grok_translation_rejects_duplicate_tool_results() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"first"},
            {"type":"tool_result","tool_use_id":"call_1","content":"second"}
        ]));
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_accepts_tool_cache_control_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{
                "name":"lookup",
                "description":"Look things up",
                "input_schema":{"type":"object"},
                "cache_control":{"type":"ephemeral"}
            }]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(
            translated["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "lookup" })
        );
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_defers_unselected_tools_and_keeps_tool_search_eager() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"inspect in parallel"}],
            "tools":[
                {
                    "name":"ToolSearch",
                    "input_schema":{"type":"object","properties":{"query":{"type":"string"}}}
                },
                {
                    "name":"Workflow",
                    "description":"Run a dynamic workflow",
                    "input_schema":{"type":"object","properties":{"script":{"type":"string"}}},
                    "strict":true,
                    "defer_loading":true,
                    "allowed_callers":["direct", "workflow"],
                    "input_examples":[{"script":"await agent('inspect')"}]
                }
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let encoded = translated.to_string();
        assert!(encoded.contains("ToolSearch"));
        assert!(!encoded.contains("Workflow"));
        assert!(!encoded.contains("defer_loading"));
        assert!(!encoded.contains("allowed_callers"));
        assert!(!encoded.contains("input_examples"));
        assert!(!encoded.contains("\"strict\""));
    }

    #[test]
    fn grok_translation_rejects_invalid_tool_cache_control() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{
                "name":"lookup",
                "input_schema":{"type":"object"},
                "cache_control":{"type":"persistent"}
            }]
        }))
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_accepts_cache_control_on_tool_use_and_tool_result() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_1","name":"lookup","input":{"q":"a"},"cache_control":{"type":"ephemeral","ttl":"1h"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","cache_control":{"type":"ephemeral"},"content":[
                        {"type":"text","text":"result","cache_control":{"type":"ephemeral"}}
                    ]}
                ]}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][0]["type"], "function_call");
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(translated["input"][1]["output"], "result");
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_accepts_empty_and_structured_tool_results_losslessly() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_1","name":"Read","input":{}},
                    {"type":"tool_use","id":"call_2","name":"Read","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1"},
                    {"type":"tool_result","tool_use_id":"call_2","content":[
                        {"type":"text","text":"first block"},
                        {"type":"text","text":"agentId: abc"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAGklEQVR4nO3BAQEAAACCIP+vbkhAAQAAAO8GECAAARlDNO4AAAAASUVORK5CYII="}},
                        {"type":"document","source":{"type":"text","media_type":"text/plain","data":"doc"},"title":"note"},
                        {"type":"search_result","source":"https://example.com","title":"Example","content":[{"type":"text","text":"result"}]}
                    ]}
                ]}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][2]["output"], "");
        let output = translated["input"][3]["output"].as_str().unwrap();
        assert!(output.starts_with("first block\nagentId: abc\n"));
        assert!(output.contains("image tool result attached"));
        assert!(output.contains("\"type\":\"document\""));
        assert!(output.contains("\"type\":\"search_result\""));
        assert!(!output.contains("iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0"));
        assert!(output.contains("https://example.com"));
        assert_eq!(translated["input"][4]["type"], "message");
        assert_eq!(translated["input"][4]["role"], "user");
        assert_eq!(
            translated["input"][4]["content"][0]["text"],
            "[image from tool result call_id: call_2]"
        );
        assert_eq!(
            translated["input"][4]["content"][1]["image_url"],
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAGklEQVR4nO3BAQEAAACCIP+vbkhAAQAAAO8GECAAARlDNO4AAAAASUVORK5CYII="
        );
    }

    #[test]
    fn grok_translation_accepts_caller_citations_and_eager_streaming_hints() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{
                "name":"lookup",
                "input_schema":{"type":"object"},
                "eager_input_streaming":true
            }],
            "messages":[
                {"role":"assistant","content":[
                    {"type":"text","text":"cited","citations":[{"type":"char_location","start_char_index":0,"end_char_index":5}]},
                    {"type":"tool_use","id":"call_1","name":"lookup","input":{},"caller":{"type":"direct"}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1"}]}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(!translated.to_string().contains("eager_input_streaming"));
        assert!(!translated.to_string().contains("citations"));
        assert!(!translated.to_string().contains("caller"));
    }

    #[test]
    fn grok_translation_loads_referenced_deferred_tool_with_strict_schema() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"name":"ToolSearch","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}},
                {
                    "name":"Workflow",
                    "defer_loading":true,
                    "strict":true,
                    "allowed_callers":["direct","workflow"],
                    "input_schema":{"type":"object","properties":{"script":{"type":"string"},"timeout":{"type":"integer"}},"required":["script"]}
                }
            ],
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_search","name":"ToolSearch","input":{"query":"select:Workflow"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_search","content":[{"type":"tool_reference","tool_name":"Workflow"}]}]},
                {"role":"user","content":"run it"}
            ]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let workflow = translated["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "Workflow")
            .unwrap();
        assert!(workflow.get("strict").is_none());
        assert!(workflow["parameters"].get("additionalProperties").is_none());
        assert_eq!(
            workflow["parameters"]["required"],
            serde_json::json!(["script"])
        );
        assert!(!translated.to_string().contains("defer_loading"));
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .contains("match its JSON schema exactly")
        );
    }

    #[test]
    fn grok_translation_omits_tools_that_disallow_direct_callers() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{
                "name":"WorkflowOnly",
                "allowed_callers":["workflow"],
                "input_schema":{"type":"object"}
            }],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(translated.get("tools").is_none());
        assert!(translated.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn grok_translation_maps_current_web_search_configuration() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{
                "type":"web_search_20260318",
                "name":"web_search",
                "max_uses":2,
                "allowed_domains":["docs.rs","rust-lang.org"],
                "user_location":{"type":"approximate","city":"Nanjing","region":"Jiangsu","country":"CN","timezone":"Asia/Shanghai"},
                "allowed_callers":["direct"],
                "response_inclusion":"full"
            },{
                "name":"Bash",
                "input_schema":{"type":"object"}
            }],
            "tool_choice":{"type":"tool","name":"web_search"},
            "messages":[{"role":"user","content":"search the web"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(
            translated["tools"],
            serde_json::json!([{
                "type":"web_search",
                "filters":{"allowed_domains":["docs.rs","rust-lang.org"]}
            }])
        );
        assert_eq!(translated["tool_choice"], "required");
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .contains("no more than 2 time(s)")
        );
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .contains("Nanjing")
        );
    }

    #[test]
    fn grok_translation_respects_none_for_declared_x_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{"type":"x_search","name":"XSearch","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"none"},
            "messages":[{"role":"user","content":"Search X for recent posts"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["tool_choice"], "none");
        assert_eq!(translated["tools"][0]["type"], "x_search");
    }

    #[test]
    fn grok_translation_none_disables_parallel_and_generated_tool_guidance() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"Keep the final answer concise.",
            "tools":[{
                "type":"web_search_20260318",
                "name":"web_search",
                "max_uses":2,
                "user_location":{"type":"approximate","city":"Nanjing"},
                "allowed_callers":["direct"]
            },{
                "name":"XSearch",
                "input_schema":{"type":"object"}
            },{
                "name":"Read",
                "strict":true,
                "input_schema":{"type":"object"}
            },{
                "name":"Grep",
                "strict":true,
                "input_schema":{"type":"object"}
            }],
            "tool_choice":{"type":"none"},
            "messages":[{"role":"user","content":"Summarize without tools."}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["tool_choice"], "none");
        assert_eq!(translated["parallel_tool_calls"], false);
        assert_eq!(translated["instructions"], "Keep the final answer concise.");
        assert_eq!(translated["tools"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn grok_translation_forced_function_matches_only_its_declared_name() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"function"},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();

        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(error.contains("tool_choice references an unknown tool"));

        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{"name":"function","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"function"},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["tool_choice"]["type"], "function");
        assert_eq!(translated["tool_choice"]["name"], "function");
    }

    #[test]
    fn grok_translation_named_choice_prefers_exact_function_over_hosted_alias() {
        for (name, hosted_kind, hosted) in [
            (
                "XSearch",
                "x_search",
                serde_json::json!({
                    "type":"x_search",
                    "name":"XSearch",
                    "input_schema":{"type":"object"}
                }),
            ),
            (
                "WebSearch",
                "web_search",
                serde_json::json!({
                    "type":"web_search_20260318",
                    "name":"web_search",
                    "allowed_callers":["direct"]
                }),
            ),
        ] {
            let function = serde_json::json!({
                "name":name,
                "input_schema":{"type":"object"}
            });
            for hosted_first in [false, true] {
                let tools = if hosted_first {
                    vec![hosted.clone(), function.clone()]
                } else {
                    vec![function.clone(), hosted.clone()]
                };
                let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                    "model":"grok-4.5",
                    "tools":tools,
                    "tool_choice":{"type":"tool","name":name},
                    "messages":[{"role":"user","content":"run the selected client function"}]
                }))
                .unwrap();

                let translated =
                    serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                        .unwrap();
                assert_eq!(translated["tool_choice"]["type"], "function");
                assert_eq!(translated["tool_choice"]["name"], name);
                let tools = translated["tools"].as_array().unwrap();
                assert!(
                    tools
                        .iter()
                        .any(|tool| { tool["type"] == "function" && tool["name"] == name })
                );
                assert!(tools.iter().any(|tool| tool["type"] == hosted_kind));
            }
        }
    }

    #[test]
    fn grok_translation_rejects_forced_explicit_hosted_x_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"type":"x_search","name":"XSearch","input_schema":{"type":"object"}},
                {"name":"Bash","input_schema":{"type":"object"}}
            ],
            "tool_choice":{"type":"tool","name":"XSearch"},
            "messages":[{"role":"user","content":"Search X for recent posts"}]
        }))
        .unwrap();

        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cannot force hosted XSearch"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn grok_translation_rejects_required_choice_with_only_hidden_x_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{"type":"x_search","name":"XSearch","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"any"},
            "messages":[{"role":"user","content":"Search X"}]
        }))
        .unwrap();

        let error = translate_request(&request, "grok-4.5".into())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cannot be satisfied by hosted x_search"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn grok_translation_filters_hidden_x_search_from_mixed_required_choice() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[
                {"type":"x_search","name":"XSearch","input_schema":{"type":"object"}},
                {"name":"Read","input_schema":{"type":"object"}}
            ],
            "tool_choice":{"type":"any"},
            "messages":[{"role":"user","content":"Use a tool"}]
        }))
        .unwrap();

        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["tool_choice"], "required");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{
                "type":"function",
                "name":"Read",
                "parameters":{"type":"object"}
            }])
        );
        assert!(!translated.to_string().contains("x_search"));
    }

    #[test]
    fn grok_translation_validates_any_and_maps_blocked_domains() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tool_choice":{"type":"any"},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());

        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{
                "type":"web_search_20260209",
                "allowed_callers":["direct"],
                "blocked_domains":["example.com"]
            }],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(
            translated["tools"][0]["filters"]["excluded_domains"],
            serde_json::json!(["example.com"])
        );
    }

    #[test]
    fn grok_translation_applies_versioned_web_search_caller_defaults() {
        for version in ["web_search_20260209", "web_search_20260318"] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "tools":[{"type":version,"name":"web_search"}],
                "messages":[{"role":"user","content":"hello"}]
            }))
            .unwrap();
            let translated =
                serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                    .unwrap();
            assert!(translated.get("tools").is_none(), "{version}");
        }

        for (version, callers) in [
            ("web_search_20250305", None),
            ("web_search_20260209", Some(serde_json::json!(["direct"]))),
            ("web_search_20260318", Some(serde_json::json!(["direct"]))),
        ] {
            let mut tool = serde_json::json!({"type":version,"name":"web_search"});
            if let Some(callers) = callers {
                tool["allowed_callers"] = callers;
            }
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "tools":[tool],
                "messages":[{"role":"user","content":"hello"}]
            }))
            .unwrap();
            let translated =
                serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap())
                    .unwrap();
            assert_eq!(translated["tools"][0]["type"], "web_search", "{version}");
        }
    }

    #[test]
    fn grok_translation_accepts_nullable_anthropic_tool_fields() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":[{"type":"text","text":"rules","cache_control":null}],
            "tools":[
                {
                    "type":null,
                    "name":"lookup",
                    "input_schema":{"type":"object"},
                    "cache_control":null,
                    "eager_input_streaming":null
                },
                {
                    "type":"web_search_20250305",
                    "name":"web_search",
                    "allowed_domains":null,
                    "blocked_domains":null,
                    "max_uses":null,
                    "user_location":null,
                    "response_inclusion":null,
                    "cache_control":null
                }
            ],
            "messages":[
                {"role":"user","content":[{"type":"text","text":"hello","citations":null,"cache_control":null}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{},"cache_control":null}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","cache_control":null,"content":[{"type":"text","text":"ok","citations":null,"cache_control":null}]}]}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["tools"].as_array().unwrap().len(), 2);
        assert!(!translated.to_string().contains("cache_control"));
        assert!(!translated.to_string().contains("citations"));
        assert!(!translated.to_string().contains("eager_input_streaming"));
    }

    #[test]
    fn grok_translation_rejects_misplaced_eager_streaming_and_response_inclusion() {
        let top_level: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "eager_input_streaming":true,
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(translate_request(&top_level, "grok-4.5".into()).is_err());

        let invalid_eager: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "tools":[{"name":"lookup","input_schema":{"type":"object"},"eager_input_streaming":"yes"}],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(translate_request(&invalid_eager, "grok-4.5".into()).is_err());

        for (version, inclusion) in [
            ("web_search_20250305", "full"),
            ("web_search_20260209", "excluded"),
            ("web_search_20260318", "citations"),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "tools":[{
                    "type":version,
                    "name":"web_search",
                    "allowed_callers":["direct"],
                    "response_inclusion":inclusion
                }],
                "messages":[{"role":"user","content":"hello"}]
            }))
            .unwrap();
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_enforces_immediate_prefix_tool_results() {
        let trailing: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"user","content":"run it"},
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]}
            ]
        }))
        .unwrap();
        assert!(translate_request(&trailing, "grok-4.5".into()).is_err());

        let skipped: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":"continue without a result"},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"late"}]}
            ]
        }))
        .unwrap();
        assert!(translate_request(&skipped, "grok-4.5".into()).is_err());

        let text_first: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":[
                    {"type":"text","text":"before"},
                    {"type":"tool_result","tool_use_id":"call_1","content":"result"}
                ]}
            ]
        }))
        .unwrap();
        assert!(translate_request(&text_first, "grok-4.5".into()).is_err());

        let valid: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"result"},
                    {"type":"text","text":"after"}
                ]}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&valid, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][0]["type"], "function_call");
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(translated["input"][2]["type"], "message");
        assert_eq!(translated["input"][2]["content"][0]["text"], "after");
    }

    #[test]
    fn grok_translation_validates_tool_result_images_and_bounds_text_output() {
        let invalid_image = request_with_blocks(serde_json::json!([{
            "type":"tool_result",
            "tool_use_id":"call_1",
            "content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"not base64"}}]
        }]));
        assert!(translate_request(&invalid_image, "grok-4.5".into()).is_err());

        let oversized = request_with_blocks(serde_json::json!([{
            "type":"tool_result",
            "tool_use_id":"call_1",
            "content":"x".repeat(MAX_TOOL_RESULT_OUTPUT_BYTES + 1)
        }]));
        let error = translate_request(&oversized, "grok-4.5".into()).unwrap_err();
        assert!(error.to_string().contains("size limit"));
    }

    #[test]
    fn grok_translation_labels_parallel_tool_result_images_by_call_id() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_a","name":"Read","input":{}},
                    {"type":"tool_use","id":"call_b","name":"Read","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_a","content":[{"type":"image","source":{"type":"url","url":"https://example.com/a.png"}}]},
                    {"type":"tool_result","tool_use_id":"call_b","content":[{"type":"image","source":{"type":"url","url":"https://example.com/b.png"}}]}
                ]}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][2]["call_id"], "call_a");
        assert_eq!(translated["input"][3]["call_id"], "call_b");
        let content = translated["input"][4]["content"].as_array().unwrap();
        assert_eq!(
            content[0]["text"],
            "[image from tool result call_id: call_a]"
        );
        assert_eq!(content[1]["image_url"], "https://example.com/a.png");
        assert_eq!(
            content[2]["text"],
            "[image from tool result call_id: call_b]"
        );
        assert_eq!(content[3]["image_url"], "https://example.com/b.png");
    }

    #[test]
    fn grok_translation_keeps_tool_result_images_before_trailing_user_text() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_1","name":"Read","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":[
                        {"type":"image","source":{"type":"url","url":"https://example.com/result.png"}}
                    ]},
                    {"type":"text","text":"describe this image"}
                ]}
            ]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        let content = translated["input"][2]["content"].as_array().unwrap();
        assert_eq!(
            content[0]["text"],
            "[image from tool result call_id: call_1]"
        );
        assert_eq!(content[1]["image_url"], "https://example.com/result.png");
        assert_eq!(content[2]["text"], "describe this image");
    }

    #[test]
    fn grok_translation_rejects_user_role_hosted_search_results() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[{
                "type":"web_search_tool_result",
                "tool_use_id":"srvtoolu_1",
                "content":[]
            }]}]
        }))
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }
}
