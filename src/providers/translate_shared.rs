use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;

const CLAUDE_CODE_CLIENT_TOOL_SEARCH_NAME: &str = "ToolSearch";
const IMAGE_DECODE_CONCURRENCY: usize = 2;
const IMAGE_DECODE_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
static IMAGE_DECODE_SLOTS: once_cell::sync::Lazy<std::sync::Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(IMAGE_DECODE_CONCURRENCY))
    });

/// Anthropic request extensions understood by the GPT/Grok compatibility
/// layers. Keeping this allowlist shared prevents one provider from silently
/// accepting a misspelled control that the other rejects.
const ALTERNATE_PROVIDER_EXTRA_FIELDS: &[&str] = &[
    "system",
    "tools",
    "tool_choice",
    "context_management",
    "metadata",
    "output_config",
    "thinking",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "service_tier",
];

/// Validate the top-level control plane shared by alternate providers.
///
/// `context_management`, `metadata`, and adaptive `thinking` are
/// compatibility-only Claude Code fields: the proxy validates their shape but
/// does not project them verbatim. Fixed thinking budgets and disabled
/// reasoning cannot be represented exactly and fail closed. Sampling controls
/// likewise fail closed instead of becoming silent no-ops.
pub fn validate_alternate_provider_fields(
    req: &MessagesRequest,
    provider: &str,
) -> Result<(), anyhow::Error> {
    for key in req.extra.keys() {
        if !ALTERNATE_PROVIDER_EXTRA_FIELDS.contains(&key.as_str()) {
            anyhow::bail!("unsupported {provider} request field: {key}");
        }
    }

    if req.messages.is_empty() {
        anyhow::bail!("messages must contain at least one message");
    }

    for field in ["temperature", "top_p", "top_k"] {
        if req.extra.contains_key(field) {
            anyhow::bail!(
                "{provider} does not support Anthropic request field {field}; remove it instead of relying on a silent no-op"
            );
        }
    }

    if let Some(stop_sequences) = req.extra.get("stop_sequences") {
        let stop_sequences = stop_sequences
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("stop_sequences must be an array of strings"))?;
        if stop_sequences.iter().any(|value| !value.is_string()) {
            anyhow::bail!("stop_sequences must be an array of strings");
        }
        if !stop_sequences.is_empty() {
            anyhow::bail!(
                "{provider} does not support non-empty stop_sequences; the request was not sent upstream"
            );
        }
    }

    if let Some(service_tier) = req.extra.get("service_tier") {
        match service_tier.as_str() {
            Some("auto" | "standard_only") => {}
            _ => anyhow::bail!("service_tier must be auto or standard_only"),
        }
    }

    validate_compatibility_metadata(req.extra.get("metadata"))?;
    validate_compatibility_thinking(req.extra.get("thinking"), provider)?;
    validate_context_management_shape(req.extra.get("context_management"))?;

    Ok(())
}

fn validate_compatibility_metadata(metadata: Option<&Value>) -> Result<(), anyhow::Error> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    if metadata.is_null() {
        return Ok(());
    }
    let metadata = metadata
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("metadata must be an object or null"))?;
    for key in metadata.keys() {
        if key != "user_id" {
            anyhow::bail!("unsupported metadata field: {key}");
        }
    }
    if let Some(user_id) = metadata.get("user_id")
        && !user_id.is_null()
        && !user_id.is_string()
    {
        anyhow::bail!("metadata.user_id must be a string or null");
    }
    Ok(())
}

fn validate_compatibility_thinking(
    thinking: Option<&Value>,
    provider: &str,
) -> Result<(), anyhow::Error> {
    let Some(thinking) = thinking else {
        return Ok(());
    };
    if thinking.is_null() {
        return Ok(());
    }
    let thinking = thinking
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("thinking must be an object or null"))?;
    let kind = thinking
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("thinking.type must be a string"))?;
    match kind {
        // Claude Code currently sends adaptive thinking. Both alternate lanes
        // already own their reasoning policy, so this is the one compatible
        // request-level mode that can be represented without a false budget
        // promise.
        "adaptive" => {
            for key in thinking.keys() {
                if !matches!(key.as_str(), "type" | "display") {
                    anyhow::bail!("unsupported adaptive thinking field: {key}");
                }
            }
            if let Some(display) = thinking.get("display")
                && !display.is_null()
                && !display.is_string()
            {
                anyhow::bail!("thinking.display must be a string or null");
            }
        }
        "enabled" => anyhow::bail!(
            "{provider} cannot enforce Anthropic thinking.budget_tokens; use adaptive thinking or output_config.effort"
        ),
        "disabled" => anyhow::bail!(
            "{provider} cannot disable model reasoning exactly; use output_config.effort=low if reduced reasoning is acceptable"
        ),
        _ => anyhow::bail!("unsupported thinking.type: {kind}"),
    }
    Ok(())
}

fn validate_context_management_shape(
    context_management: Option<&Value>,
) -> Result<(), anyhow::Error> {
    let Some(context_management) = context_management else {
        return Ok(());
    };
    if context_management.is_null() {
        return Ok(());
    }
    let context_management = context_management
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("context_management must be an object or null"))?;
    for key in context_management.keys() {
        if key != "edits" {
            anyhow::bail!("unsupported context_management field: {key}");
        }
    }
    let edits = context_management
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("context_management.edits must be an array"))?;
    for (index, edit) in edits.iter().enumerate() {
        let edit = edit.as_object().ok_or_else(|| {
            anyhow::anyhow!("context_management.edits[{index}] must be an object")
        })?;
        if edit
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            anyhow::bail!("context_management.edits[{index}].type must be a non-empty string");
        }
    }
    Ok(())
}

/// `/v1/messages` requires an explicit generation budget. Alternate upstreams
/// do not expose Anthropic's `max_tokens: 0` prompt-cache-only operation, so a
/// zero budget must fail before dispatch rather than accidentally generating.
pub fn validate_generation_max_tokens(
    req: &MessagesRequest,
    provider: &str,
) -> Result<u32, anyhow::Error> {
    match req.max_tokens {
        Some(0) => anyhow::bail!(
            "{provider} cannot implement Anthropic max_tokens=0 cache-only semantics; the request was not sent upstream"
        ),
        Some(max_tokens) => Ok(max_tokens),
        None => anyhow::bail!("max_tokens is required for /v1/messages"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDecodeAdmissionError {
    Closed,
    Timeout,
}

pub async fn acquire_image_decode_slot()
-> Result<tokio::sync::OwnedSemaphorePermit, ImageDecodeAdmissionError> {
    match tokio::time::timeout(
        IMAGE_DECODE_QUEUE_TIMEOUT,
        IMAGE_DECODE_SLOTS.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(ImageDecodeAdmissionError::Closed),
        Err(_) => Err(ImageDecodeAdmissionError::Timeout),
    }
}

pub fn messages_request_contains_base64_image(body: &MessagesRequest) -> bool {
    body.messages
        .iter()
        .any(|message| value_contains_base64_image(&message.content))
}

fn value_contains_base64_image(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(value_contains_base64_image),
        Value::Object(object) => {
            let is_base64_image = object.get("type").and_then(Value::as_str) == Some("image")
                && object
                    .get("source")
                    .and_then(Value::as_object)
                    .and_then(|source| source.get("type"))
                    .and_then(Value::as_str)
                    == Some("base64");
            is_base64_image || object.values().any(value_contains_base64_image)
        }
        _ => false,
    }
}

#[derive(Debug)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
}

#[derive(Debug)]
pub struct ImageSource {
    pub media_type: String,
    pub data: String,
    pub source_type: String,
}

pub fn flatten_system_text(system_val: Option<&Value>) -> Option<String> {
    let system = system_val?;
    let texts: Vec<String> = match system {
        Value::String(s) => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                let text = b.get("text").and_then(|v| v.as_str())?;
                if text.starts_with("x-anthropic-billing-header:") {
                    None
                } else {
                    Some(text.to_string())
                }
            })
            .collect(),
        _ => return None,
    };
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

pub fn read_effort(req: &MessagesRequest) -> Result<Option<&str>, anyhow::Error> {
    let output_config = match req.extra.get("output_config") {
        Some(Value::Object(m)) => m,
        None | Some(Value::Null) => return Ok(None),
        Some(_) => anyhow::bail!("output_config must be an object or null"),
    };
    match output_config.get("effort") {
        Some(Value::String(s)) => {
            let valid = ["low", "medium", "high", "xhigh", "max"];
            if valid.contains(&s.as_str()) {
                Ok(Some(s.as_str()))
            } else {
                anyhow::bail!("Invalid output_config.effort: {s}")
            }
        }
        None | Some(Value::Null) => Ok(None),
        Some(_) => {
            anyhow::bail!("output_config.effort must be one of low, medium, high, xhigh, or max")
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RequestedOutputFormat<'a> {
    Text,
    JsonObject,
    JsonSchema { name: &'a str, schema: &'a Value },
}

/// Distinguish an omitted output format from a malformed requested contract.
/// Invalid structured-output configuration must never degrade to free-form text.
pub fn parse_output_format(
    req: &MessagesRequest,
) -> Result<Option<RequestedOutputFormat<'_>>, anyhow::Error> {
    let Some(output_config) = req.extra.get("output_config") else {
        return Ok(None);
    };
    if output_config.is_null() {
        return Ok(None);
    }
    let output_config = output_config
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("output_config must be an object or null"))?;
    for key in output_config.keys() {
        if !matches!(key.as_str(), "effort" | "format") {
            anyhow::bail!("unsupported output_config field: {key}");
        }
    }
    let Some(format) = output_config.get("format") else {
        return Ok(None);
    };
    if format.is_null() {
        return Ok(None);
    }
    let format = format
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("output_config.format must be an object"))?;
    let kind = format
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| anyhow::anyhow!("output_config.format.type must be a non-empty string"))?;

    match kind {
        "text" => Ok(Some(RequestedOutputFormat::Text)),
        "json_object" => Ok(Some(RequestedOutputFormat::JsonObject)),
        "json_schema" => {
            let name = match format.get("name") {
                None => "response",
                Some(Value::String(name)) if !name.is_empty() => name,
                Some(_) => {
                    anyhow::bail!(
                        "output_config.format.name must be a non-empty string when provided"
                    )
                }
            };
            let schema = format
                .get("schema")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| anyhow::anyhow!("output_config.format.schema must be an object"))?;
            Ok(Some(RequestedOutputFormat::JsonSchema { name, schema }))
        }
        _ => anyhow::bail!("unsupported output_config.format.type: {kind}"),
    }
}

pub fn validate_message_roles(req: &MessagesRequest) -> Result<(), anyhow::Error> {
    validate_message_roles_for_provider(req, false)
}

/// Codex accepts the nested `system` text message emitted by Claude Code's
/// Ultracode profile and maps it to developer input.
pub fn validate_codex_message_roles(req: &MessagesRequest) -> Result<(), anyhow::Error> {
    validate_message_roles_for_provider(req, true)
}

/// Grok forwards nested `system` text messages as upstream system input. Keep
/// non-text system content and unknown roles fail-closed.
pub fn validate_grok_message_roles(req: &MessagesRequest) -> Result<(), anyhow::Error> {
    validate_message_roles_for_provider(req, true)
}

fn validate_message_roles_for_provider(
    req: &MessagesRequest,
    allow_system_text: bool,
) -> Result<(), anyhow::Error> {
    for (index, message) in req.messages.iter().enumerate() {
        let is_nested_system = message.role == "system";
        if !(matches!(message.role.as_str(), "user" | "assistant")
            || allow_system_text && is_nested_system)
        {
            anyhow::bail!(
                "messages[{index}].role must be user or assistant; system instructions belong in the top-level system field"
            );
        }
        let blocks = match &message.content {
            Value::String(_) => continue,
            Value::Array(blocks) => blocks,
            _ => {
                anyhow::bail!(
                    "messages[{index}].content must be a string or array of content blocks"
                )
            }
        };
        for (block_index, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && !block.get("text").is_some_and(Value::is_string)
            {
                anyhow::bail!("messages[{index}].content[{block_index}].text must be a string");
            }
            if is_nested_system && block.get("type").and_then(Value::as_str) != Some("text") {
                anyhow::bail!(
                    "messages[{index}].content[{block_index}] must be a text block for system role"
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ToolReferenceDeclaration {
    deferred: bool,
    custom_search_capable: bool,
    allowed_callers: HashSet<String>,
}

#[derive(Debug)]
struct ToolReferenceCall {
    name: String,
    caller: String,
}

fn declared_allowed_callers(
    tool: &serde_json::Map<String, Value>,
    declared_type: Option<&str>,
) -> Result<HashSet<String>, anyhow::Error> {
    let direct_by_default = !matches!(
        declared_type,
        Some("web_search_20260209" | "web_search_20260318")
    );
    match tool.get("allowed_callers") {
        None | Some(Value::Null) if direct_by_default => Ok(HashSet::from(["direct".to_string()])),
        None | Some(Value::Null) => Ok(HashSet::new()),
        Some(Value::Array(callers)) => callers
            .iter()
            .map(|caller| {
                caller
                    .as_str()
                    .filter(|caller| !caller.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        anyhow::anyhow!("allowed_callers entries must be non-empty strings")
                    })
            })
            .collect(),
        Some(_) => anyhow::bail!("allowed_callers must be an array or null"),
    }
}

fn historical_tool_caller(block: &serde_json::Map<String, Value>) -> Result<String, anyhow::Error> {
    match block.get("caller") {
        None | Some(Value::Null) => Ok("direct".to_string()),
        Some(Value::Object(caller)) => caller
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("tool_use caller.type must be a non-empty string")),
        Some(_) => anyhow::bail!("tool_use caller must be an object or null"),
    }
}

fn valid_tool_reference_cache_control(value: Option<&Value>) -> bool {
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

/// Validate Claude Code client ToolSearch history and return the deferred tools it selected.
///
/// A `tool_reference` is control data rather than ordinary tool output: it changes which
/// schemas a provider exposes to the model. Keep that capability bound to the exact
/// `tool_use_id` of a declared, non-deferred Claude Code client `ToolSearch` call.
/// Anthropic's generic custom-tool wire format has no search-capability marker, so this
/// proxy intentionally recognizes only Claude Code's built-in contract. Validation is
/// transactional so callers never observe a partially accepted set of references.
pub fn validate_tool_reference_provenance(
    req: &MessagesRequest,
) -> Result<HashSet<String>, anyhow::Error> {
    let has_tool_reference = req.messages.iter().any(|message| {
        message.content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|children| {
                        children.iter().any(|child| {
                            child.get("type").and_then(Value::as_str) == Some("tool_reference")
                        })
                    })
            })
        })
    });
    if !has_tool_reference {
        return Ok(HashSet::new());
    }

    let mut declarations = HashMap::new();
    for tool in req
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(tool) = tool.as_object() else {
            continue;
        };
        let declared_type = tool.get("type").and_then(Value::as_str);
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                declared_type
                    .is_some_and(|kind| kind.starts_with("web_search_"))
                    .then_some("web_search")
            });
        let Some(name) = name else {
            continue;
        };
        let custom = matches!(declared_type, None | Some("custom") | Some("function"));
        let deferred = tool.get("defer_loading").and_then(Value::as_bool) == Some(true);
        declarations.insert(
            name.to_string(),
            ToolReferenceDeclaration {
                deferred,
                custom_search_capable: custom && name == CLAUDE_CODE_CLIENT_TOOL_SEARCH_NAME,
                allowed_callers: declared_allowed_callers(tool, declared_type)?,
            },
        );
    }

    let mut calls = HashMap::<String, ToolReferenceCall>::new();
    let mut ambiguous_call_ids = HashSet::new();
    let mut resolved_call_ids = HashSet::new();
    let mut selected = HashSet::new();

    for (message_index, message) in req.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let Some(block) = block.as_object() else {
                continue;
            };
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") if message.role == "assistant" => {
                    let Some(id) = block
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    let Some(name) = block
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                    else {
                        continue;
                    };
                    let caller = historical_tool_caller(block)?;
                    if calls
                        .insert(
                            id.to_string(),
                            ToolReferenceCall {
                                name: name.to_string(),
                                caller,
                            },
                        )
                        .is_some()
                    {
                        ambiguous_call_ids.insert(id.to_string());
                    }
                }
                Some("tool_result") => {
                    let references = block
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|child| {
                            child.get("type").and_then(Value::as_str) == Some("tool_reference")
                        })
                        .collect::<Vec<_>>();

                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty());
                    let already_resolved =
                        id.is_some_and(|id| !resolved_call_ids.insert(id.to_string()));
                    if references.is_empty() {
                        continue;
                    }
                    if message.role != "user" {
                        anyhow::bail!(
                            "messages[{message_index}].content[{block_index}] tool_reference must be returned by a user tool_result"
                        );
                    }
                    let id = id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "messages[{message_index}].content[{block_index}].tool_use_id must be a non-empty string"
                        )
                    })?;
                    if already_resolved || ambiguous_call_ids.contains(id) {
                        anyhow::bail!(
                            "tool_reference result references an unknown, resolved, or ambiguous tool use id: {id}"
                        );
                    }
                    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                        anyhow::bail!(
                            "tool_reference cannot be loaded from an error tool_result: {id}"
                        );
                    }
                    let source_call = calls.get(id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "tool_reference result references an unknown or not-yet-seen tool use id: {id}"
                        )
                    })?;
                    let caller = declarations.get(&source_call.name).ok_or_else(|| {
                        anyhow::anyhow!(
                            "tool_reference source {} is not a declared Claude Code client ToolSearch tool",
                            source_call.name
                        )
                    })?;
                    if !caller.custom_search_capable || caller.deferred {
                        anyhow::bail!(
                            "tool_reference source {} is not a declared, non-deferred Claude Code client ToolSearch tool",
                            source_call.name
                        );
                    }
                    if !caller.allowed_callers.contains(&source_call.caller) {
                        anyhow::bail!(
                            "tool_reference source {} does not allow its historical {} caller",
                            source_call.name,
                            source_call.caller
                        );
                    }

                    let mut result_references = HashSet::new();
                    for reference in references {
                        let reference = reference.as_object().ok_or_else(|| {
                            anyhow::anyhow!(
                                "messages[{message_index}].content[{block_index}] tool_reference must be an object"
                            )
                        })?;
                        if reference.keys().any(|key| {
                            !matches!(key.as_str(), "type" | "tool_name" | "cache_control")
                        }) || !valid_tool_reference_cache_control(reference.get("cache_control"))
                        {
                            anyhow::bail!(
                                "messages[{message_index}].content[{block_index}] tool_reference contains unsupported fields or cache_control"
                            );
                        }
                        let name = reference
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "messages[{message_index}].content[{block_index}] tool_reference must contain a non-empty tool_name"
                                )
                            })?;
                        if !result_references.insert(name) {
                            anyhow::bail!("duplicate tool_reference in one tool_result: {name}");
                        }
                        let declaration = declarations.get(name).ok_or_else(|| {
                            anyhow::anyhow!("tool reference '{name}' not found in available tools")
                        })?;
                        if !declaration.deferred {
                            anyhow::bail!(
                                "tool reference '{name}' must refer to a tool with defer_loading=true"
                            );
                        }
                        if !declaration.allowed_callers.contains("direct") {
                            anyhow::bail!(
                                "tool reference '{name}' cannot be loaded because it does not allow direct callers"
                            );
                        }
                        selected.insert(name.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    Ok(selected)
}

#[derive(Debug)]
pub enum JsonObjectError {
    Empty,
    Invalid(serde_json::Error),
    NotObject,
}

/// Parse a completed Responses API tool argument payload using one provider-independent rule.
pub fn parse_json_object(value: &str) -> Result<Value, JsonObjectError> {
    if value.is_empty() {
        return Err(JsonObjectError::Empty);
    }
    let parsed = serde_json::from_str::<Value>(value).map_err(JsonObjectError::Invalid)?;
    if !parsed.is_object() {
        return Err(JsonObjectError::NotObject);
    }
    Ok(parsed)
}

fn content_contains_text(content: &Value, needle: &str) -> bool {
    match content {
        Value::String(text) => text.contains(needle),
        Value::Array(blocks) => blocks.iter().any(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains(needle))
        }),
        _ => false,
    }
}

/// Detect Claude Code's automatic and manual compaction summary prompt.
///
/// Claude Code currently sends compaction with the selected main model, so
/// providers and the server share this classifier when applying internal
/// request policy.
pub fn is_claude_code_compaction_request(req: &MessagesRequest) -> bool {
    let Some(content) = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| &message.content)
    else {
        return false;
    };

    [
        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.",
        "Your entire response must be plain text: an <analysis> block followed by a <summary> block.",
        "Your task is to create a detailed summary",
    ]
    .iter()
    .all(|marker| content_contains_text(content, marker))
}

/// Normalize Anthropic JSON schemas for strict Responses API validation.
pub fn normalize_strict_json_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let originally_required: std::collections::HashSet<&str> = map
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let mut out = map.clone();

            for keyword in [
                "properties",
                "patternProperties",
                "$defs",
                "definitions",
                "dependentSchemas",
            ] {
                if let Some(Value::Object(schemas)) = out.get_mut(keyword) {
                    for schema in schemas.values_mut() {
                        *schema = normalize_strict_json_schema(schema);
                    }
                }
            }
            if let Some(Value::Object(dependencies)) = out.get_mut("dependencies") {
                for dependency in dependencies.values_mut() {
                    if dependency.is_object() || dependency.is_boolean() {
                        *dependency = normalize_strict_json_schema(dependency);
                    }
                }
            }
            for keyword in ["prefixItems", "allOf", "anyOf", "oneOf"] {
                if let Some(Value::Array(schemas)) = out.get_mut(keyword) {
                    for schema in schemas {
                        *schema = normalize_strict_json_schema(schema);
                    }
                }
            }
            for keyword in [
                "items",
                "additionalItems",
                "additionalProperties",
                "unevaluatedItems",
                "unevaluatedProperties",
                "propertyNames",
                "contains",
                "contentSchema",
                "not",
                "if",
                "then",
                "else",
            ] {
                let Some(value) = out.get_mut(keyword) else {
                    continue;
                };
                match value {
                    Value::Array(schemas) => {
                        for schema in schemas {
                            *schema = normalize_strict_json_schema(schema);
                        }
                    }
                    Value::Object(_) | Value::Bool(_) => {
                        *value = normalize_strict_json_schema(value);
                    }
                    _ => {}
                }
            }

            let is_bare_object = !out.contains_key("properties")
                && out.get("type").and_then(Value::as_str) == Some("object");
            let property_names = match out.get_mut("properties") {
                Some(Value::Object(properties)) => {
                    for (name, property_schema) in properties.iter_mut() {
                        if !originally_required.contains(name.as_str()) {
                            *property_schema = nullable_schema(std::mem::take(property_schema));
                        }
                    }
                    Some(properties.keys().cloned().collect())
                }
                None if is_bare_object => {
                    out.insert("properties".into(), Value::Object(serde_json::Map::new()));
                    Some(Vec::new())
                }
                _ => None,
            };
            if let Some(property_names) = property_names {
                out.insert(
                    "required".into(),
                    Value::Array(property_names.into_iter().map(Value::String).collect()),
                );
                out.insert("additionalProperties".into(), Value::Bool(false));
            }
            Value::Object(out)
        }
        _ => schema.clone(),
    }
}

fn nullable_schema(schema: Value) -> Value {
    if schema_accepts_null(&schema) {
        schema
    } else {
        serde_json::json!({"anyOf": [schema, {"type": "null"}]})
    }
}

fn schema_accepts_null(schema: &Value) -> bool {
    let Value::Object(schema) = schema else {
        return schema.as_bool().unwrap_or(false);
    };

    // JSON Schema keywords at one schema location are conjunctive. Seeing
    // null in `type`, `const`, or `enum` is not sufficient when another one
    // rejects it.
    let mut accepts = match schema.get("type") {
        None => true,
        Some(Value::String(kind)) => kind == "null",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("null")),
        Some(_) => false,
    } && schema.get("const").is_none_or(Value::is_null)
        && schema
            .get("enum")
            .and_then(Value::as_array)
            .is_none_or(|values| values.iter().any(Value::is_null));

    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        accepts &= branches.iter().all(schema_accepts_null);
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        accepts &= branches.iter().any(schema_accepts_null);
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        accepts &= branches
            .iter()
            .filter(|branch| schema_accepts_null(branch))
            .count()
            == 1;
    }
    if let Some(negated) = schema.get("not") {
        accepts &= !schema_accepts_null(negated);
    }
    accepts
}

pub fn normalize_content(content: &Value, missing_tool_input: Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => {
            vec![ContentBlock::Text { text: s.clone() }]
        }
        Value::Array(arr) => {
            let mut blocks = Vec::new();
            for item in arr {
                if let Some(block) = parse_content_block(item, missing_tool_input.clone()) {
                    blocks.push(block);
                }
            }
            blocks
        }
        _ => Vec::new(),
    }
}

pub fn image_source_to_url(source: &ImageSource) -> String {
    if source.source_type == "url" {
        source.data.clone()
    } else {
        format!("data:{};base64,{}", source.media_type, source.data)
    }
}

pub fn image_block_to_url(block: &Value) -> String {
    let source_type = block
        .get("source")
        .and_then(|s| s.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("base64");
    if source_type == "url" {
        block
            .get("source")
            .and_then(|s| s.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        let media_type = block
            .get("source")
            .and_then(|s| s.get("media_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("image/png");
        let data = block
            .get("source")
            .and_then(|s| s.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        format!("data:{media_type};base64,{data}")
    }
}

fn parse_content_block(value: &Value, missing_tool_input: Value) -> Option<ContentBlock> {
    let kind = value.get("type").and_then(|v| v.as_str())?;
    match kind {
        "text" => {
            let text = value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(ContentBlock::Text { text })
        }
        "image" => {
            let source = value.get("source")?;
            let media_type = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let source_type = source
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("base64")
                .to_string();
            let data = if source_type == "url" {
                source.get("url").and_then(|v| v.as_str()).unwrap_or("")
            } else {
                source.get("data").and_then(|v| v.as_str()).unwrap_or("")
            }
            .to_string();
            Some(ContentBlock::Image {
                source: ImageSource {
                    media_type,
                    data,
                    source_type,
                },
            })
        }
        "tool_use" => {
            let id = value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = value.get("input").cloned().unwrap_or(missing_tool_input);
            Some(ContentBlock::ToolUse { id, name, input })
        }
        "tool_result" => {
            let tool_use_id = value
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = value
                .get("content")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let is_error = value.get("is_error").and_then(|v| v.as_bool());
            Some(ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            })
        }
        "thinking" => {
            let thinking = value
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let signature = value
                .get("signature")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(ContentBlock::Thinking {
                thinking,
                signature,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_provider_fields_reject_silent_generation_controls() {
        for (field, value) in [
            ("temperature", serde_json::json!(0)),
            ("top_p", serde_json::json!(0.9)),
            ("top_k", serde_json::json!(40)),
            ("stop_sequences", serde_json::json!(["END"])),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"hello"}],
                field:value
            }))
            .unwrap();
            let error = validate_alternate_provider_fields(&request, "Codex")
                .unwrap_err()
                .to_string();
            assert!(error.contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn alternate_provider_fields_accept_claude_code_compatibility_metadata() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user", "content":"hello"}],
            "context_management":{"edits":[]},
            "metadata":{"user_id":"user"},
            "thinking":{"type":"adaptive", "display":"summarized"},
            "output_config":{"effort":"high"},
            "stop_sequences":[],
            "service_tier":"auto"
        }))
        .unwrap();

        validate_alternate_provider_fields(&request, "Grok").unwrap();
    }

    #[test]
    fn alternate_provider_fields_validate_compatibility_only_shapes() {
        for (field, value, expected) in [
            (
                "metadata",
                serde_json::json!({"user_id":1}),
                "metadata.user_id",
            ),
            (
                "metadata",
                serde_json::json!({"user_id":"user", "tenant":"ignored"}),
                "unsupported metadata field",
            ),
            (
                "thinking",
                serde_json::json!({"type":"enabled", "budget_tokens":1024}),
                "cannot enforce Anthropic thinking.budget_tokens",
            ),
            (
                "thinking",
                serde_json::json!({"type":"adaptive", "display":1}),
                "thinking.display",
            ),
            (
                "context_management",
                serde_json::json!({"edits":"clear"}),
                "context_management.edits must be an array",
            ),
            (
                "context_management",
                serde_json::json!({"edits":[{}]}),
                "type must be a non-empty string",
            ),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"hello"}],
                field:value
            }))
            .unwrap();
            let error = validate_alternate_provider_fields(&request, "Codex")
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{field}: {error}");
        }
    }

    #[test]
    fn alternate_provider_fields_reject_unknown_and_malformed_controls() {
        for (field, value, expected) in [
            (
                "max_tokenz",
                serde_json::json!(1),
                "unsupported Codex request field",
            ),
            (
                "stop_sequences",
                serde_json::json!("END"),
                "array of strings",
            ),
            ("stop_sequences", serde_json::json!([1]), "array of strings"),
            (
                "service_tier",
                serde_json::json!("priority"),
                "auto or standard_only",
            ),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"hello"}],
                field:value
            }))
            .unwrap();
            let error = validate_alternate_provider_fields(&request, "Codex")
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{field}: {error}");
        }
    }

    #[test]
    fn alternate_provider_fields_require_a_message() {
        for request in [
            serde_json::json!({"model":"gpt-5.6-sol"}),
            serde_json::json!({"model":"gpt-5.6-sol", "messages":[]}),
        ] {
            let request: MessagesRequest = serde_json::from_value(request).unwrap();
            let error = validate_alternate_provider_fields(&request, "Codex")
                .unwrap_err()
                .to_string();
            assert!(error.contains("at least one message"), "{error}");
        }
    }

    #[test]
    fn generation_max_tokens_requires_positive_budget_without_affecting_count_tokens() {
        let missing: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        assert!(
            validate_generation_max_tokens(&missing, "Codex")
                .unwrap_err()
                .to_string()
                .contains("required for /v1/messages")
        );

        let zero: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"gpt-5.6-sol",
            "max_tokens":0,
            "messages":[{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        assert!(
            validate_generation_max_tokens(&zero, "Codex")
                .unwrap_err()
                .to_string()
                .contains("cache-only")
        );

        let positive: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"gpt-5.6-sol",
            "max_tokens":32,
            "messages":[{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        assert_eq!(
            validate_generation_max_tokens(&positive, "Codex").unwrap(),
            32
        );
    }

    #[test]
    fn message_content_shape_and_known_text_blocks_are_validated() {
        for content in [
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!({"type":"text","text":"not wrapped in an array"}),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "messages":[{"role":"user", "content":content}]
            }))
            .unwrap();
            let error = validate_message_roles(&request).unwrap_err().to_string();
            assert!(
                error.contains("content must be a string or array of content blocks"),
                "{error}"
            );
        }

        for text in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!(42)),
        ] {
            let mut block = serde_json::json!({"type":"text"});
            if let Some(text) = text {
                block["text"] = text;
            }
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "messages":[{"role":"assistant", "content":[block]}]
            }))
            .unwrap();
            let error = validate_message_roles(&request).unwrap_err().to_string();
            assert!(
                error.contains("content[0].text must be a string"),
                "{error}"
            );
        }

        let future_block: MessagesRequest = serde_json::from_value(serde_json::json!({
            "messages":[{"role":"user", "content":[{"type":"future_block","payload":42}]}]
        }))
        .unwrap();
        assert!(validate_message_roles(&future_block).is_ok());
    }

    #[test]
    fn compaction_detector_requires_all_markers_in_latest_user_message() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "fable",
            "messages": [
                {"role": "user", "content": "Earlier task"},
                {"role": "assistant", "content": "Earlier response"},
                {"role": "user", "content": [{
                    "type": "text",
                    "text": "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far."
                }]}
            ]
        }))
        .unwrap();

        assert!(is_claude_code_compaction_request(&request));

        let ordinary: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "fable",
            "messages": [{"role": "user", "content": "Summarize this conversation"}]
        }))
        .unwrap();
        assert!(!is_claude_code_compaction_request(&ordinary));
    }

    #[test]
    fn effort_reader_rejects_ultra() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {"effort": "ultra"}
        }))
        .unwrap();

        assert_eq!(
            read_effort(&request).unwrap_err().to_string(),
            "Invalid output_config.effort: ultra"
        );
    }

    #[test]
    fn effort_reader_rejects_non_string_values() {
        for effort in [serde_json::json!(1), serde_json::json!({})] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model": "gpt-5.6-sol",
                "messages": [{"role": "user", "content": "hello"}],
                "output_config": {"effort": effort}
            }))
            .unwrap();

            let error = read_effort(&request).unwrap_err().to_string();
            assert!(error.contains("output_config.effort must be one of"));
        }
    }

    #[test]
    fn null_output_config_and_members_are_treated_as_omitted() {
        for output_config in [
            Value::Null,
            serde_json::json!({"effort":null}),
            serde_json::json!({"format":null}),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"hello"}],
                "output_config":output_config
            }))
            .unwrap();
            assert_eq!(read_effort(&request).unwrap(), None);
            assert!(parse_output_format(&request).unwrap().is_none());
        }
    }

    #[test]
    fn output_config_rejects_unknown_subfields() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"hello"}],
            "output_config":{"effrot":"high"}
        }))
        .unwrap();
        let error = parse_output_format(&request).unwrap_err().to_string();
        assert!(error.contains("unsupported output_config field: effrot"));
    }

    #[test]
    fn strict_schema_requires_every_declared_property_recursively() {
        let normalized = normalize_strict_json_schema(&serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "metadata": {
                    "type": "object",
                    "properties": {"short": {"type": "boolean"}}
                }
            },
            "required": ["title"]
        }));

        assert_eq!(
            normalized["required"],
            serde_json::json!(["metadata", "title"])
        );
        assert_eq!(
            normalized["properties"]["metadata"]["required"],
            Value::Null
        );
        assert_eq!(normalized["additionalProperties"], false);
        assert_eq!(
            normalized["properties"]["metadata"]["anyOf"][0]["required"],
            serde_json::json!(["short"])
        );
        assert_eq!(
            normalized["properties"]["metadata"]["anyOf"][0]["additionalProperties"],
            false
        );
        assert_eq!(
            normalized["properties"]["metadata"]["anyOf"][1],
            serde_json::json!({"type":"null"})
        );
    }

    #[test]
    fn strict_empty_object_schema_gets_explicit_closed_shape() {
        let normalized = normalize_strict_json_schema(&serde_json::json!({"type": "object"}));

        assert_eq!(normalized["properties"], serde_json::json!({}));
        assert_eq!(normalized["required"], serde_json::json!([]));
        assert_eq!(normalized["additionalProperties"], false);
    }

    #[test]
    fn strict_schema_normalization_preserves_literal_object_annotations() {
        let literal = serde_json::json!({
            "type":"object",
            "properties":{"this_is_data":{"type":"string"}}
        });
        let normalized = normalize_strict_json_schema(&serde_json::json!({
            "type":"object",
            "properties":{
                "payload":{
                    "type":"object",
                    "const":literal,
                    "default":literal,
                    "examples":[literal],
                    "enum":[literal]
                }
            },
            "required":["payload"],
            "$defs":{
                "nested":{
                    "type":"object",
                    "properties":{"optional":{"type":"string"}}
                }
            }
        }));

        for keyword in ["const", "default"] {
            assert_eq!(normalized["properties"]["payload"][keyword], literal);
        }
        assert_eq!(normalized["properties"]["payload"]["examples"][0], literal);
        assert_eq!(normalized["properties"]["payload"]["enum"][0], literal);
        assert_eq!(
            normalized["$defs"]["nested"]["required"],
            serde_json::json!(["optional"])
        );
    }

    #[test]
    fn completed_tool_arguments_must_be_a_json_object() {
        assert!(matches!(parse_json_object(""), Err(JsonObjectError::Empty)));
        assert!(matches!(
            parse_json_object("{\"path\":"),
            Err(JsonObjectError::Invalid(_))
        ));
        assert!(matches!(
            parse_json_object("[]"),
            Err(JsonObjectError::NotObject)
        ));
        assert_eq!(
            parse_json_object("{\"path\":\"/tmp/a\"}").unwrap(),
            serde_json::json!({"path":"/tmp/a"})
        );
    }

    #[test]
    fn tool_reference_provenance_enforces_source_and_target_callers() {
        let direct_source_forbidden: MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "tools":[
                    {"name":"ToolSearch","allowed_callers":["workflow"],"input_schema":{"type":"object"}},
                    {"name":"Bash","defer_loading":true,"allowed_callers":["direct"],"input_schema":{"type":"object"}}
                ],
                "messages":[
                    {"role":"assistant","content":[{"type":"tool_use","id":"search_1","name":"ToolSearch","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"search_1","content":[{"type":"tool_reference","tool_name":"Bash"}]}]}
                ]
            }))
            .unwrap();
        let error = validate_tool_reference_provenance(&direct_source_forbidden)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not allow its historical direct caller"));

        let matching_workflow_source: MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "tools":[
                    {"name":"ToolSearch","allowed_callers":["workflow"],"input_schema":{"type":"object"}},
                    {"name":"Bash","defer_loading":true,"allowed_callers":["direct"],"input_schema":{"type":"object"}}
                ],
                "messages":[
                    {"role":"assistant","content":[{"type":"tool_use","id":"search_1","name":"ToolSearch","caller":{"type":"workflow"},"input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"search_1","content":[{"type":"tool_reference","tool_name":"Bash"}]}]}
                ]
            }))
            .unwrap();
        assert_eq!(
            validate_tool_reference_provenance(&matching_workflow_source).unwrap(),
            HashSet::from(["Bash".to_string()])
        );

        let workflow_only_target: MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "tools":[
                    {"name":"ToolSearch","input_schema":{"type":"object"}},
                    {"name":"Bash","defer_loading":true,"allowed_callers":["workflow"],"input_schema":{"type":"object"}}
                ],
                "messages":[
                    {"role":"assistant","content":[{"type":"tool_use","id":"search_1","name":"ToolSearch","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"search_1","content":[{"type":"tool_reference","tool_name":"Bash"}]}]}
                ]
            }))
            .unwrap();
        let error = validate_tool_reference_provenance(&workflow_only_target)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not allow direct callers"));
    }

    #[test]
    fn error_tool_result_cannot_activate_deferred_tools() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"gpt-5.6-sol",
            "tools":[
                {"name":"ToolSearch","input_schema":{"type":"object"}},
                {"name":"Bash","defer_loading":true,"input_schema":{"type":"object"}}
            ],
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"search_1","name":"ToolSearch","input":{}}]},
                {"role":"user","content":[{
                    "type":"tool_result",
                    "tool_use_id":"search_1",
                    "is_error":true,
                    "content":[{"type":"tool_reference","tool_name":"Bash"}]
                }]}
            ]
        }))
        .unwrap();

        let error = validate_tool_reference_provenance(&request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be loaded from an error tool_result"));
    }

    #[test]
    fn tool_reference_rejects_extra_fields_and_invalid_cache_control() {
        for reference in [
            serde_json::json!({
                "type":"tool_reference",
                "tool_name":"Bash",
                "unknown":true
            }),
            serde_json::json!({
                "type":"tool_reference",
                "tool_name":"Bash",
                "cache_control":{"type":"persistent"}
            }),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"gpt-5.6-sol",
                "tools":[
                    {"name":"ToolSearch","input_schema":{"type":"object"}},
                    {"name":"Bash","defer_loading":true,"input_schema":{"type":"object"}}
                ],
                "messages":[
                    {"role":"assistant","content":[{"type":"tool_use","id":"search_1","name":"ToolSearch","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"search_1","content":[reference]}]}
                ]
            }))
            .unwrap();

            let error = validate_tool_reference_provenance(&request)
                .unwrap_err()
                .to_string();
            assert!(error.contains("unsupported fields or cache_control"));
        }
    }
}
