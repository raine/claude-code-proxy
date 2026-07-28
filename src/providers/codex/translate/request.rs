use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::num::NonZeroU64;
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::anthropic::schema::MessagesRequest;
use crate::config;
use crate::providers::translate_shared::{
    ContentBlock, RequestedOutputFormat, flatten_system_text, image_source_to_url,
    is_claude_code_compaction_request, normalize_content, parse_output_format, read_effort,
    validate_alternate_provider_fields, validate_codex_message_roles,
    validate_tool_reference_provenance,
};

use super::read_rewrite::{ReadOffsetRewrite, read_offset_rewrite};
use super::reasoning_signature::decode_reasoning_signature;
use super::schema_bridge::SchemaBridge;

const PARALLEL_TOOL_GUIDANCE: &str = "Emit independent function calls together in one response, including multiple independent calls to the same tool. Parallelize read-only operations when they have no data dependencies; serialize dependent calls and calls with overlapping side effects.";
const MAX_CODEX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CODEX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_CODEX_IMAGE_DECODE_BYTES: usize = 64 * 1024 * 1024;
const INVALID_CODEX_IMAGE: &str =
    "does not match source.media_type or is not a complete decodable image";
const CODEX_IMAGE_DECODE_LIMIT: &str = "exceeds the image decode limit";
const CLAUDE_CODE_AGENT_LIST_PREFIX: &str = "Available agent types for the Agent tool:\n";
const CLAUDE_CODE_ULTRACODE_MARKER: &str =
    "Ultracode is on: optimize for the most exhaustive, correct answer";
const CLAUDE_CODE_ULTRACODE_OFF_MARKER: &str = "Ultracode is off";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effort::None => write!(f, "none"),
            Effort::Low => write!(f, "low"),
            Effort::Medium => write!(f, "medium"),
            Effort::High => write!(f, "high"),
            Effort::Xhigh => write!(f, "xhigh"),
            Effort::Max => write!(f, "max"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Priority,
    Flex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesToolChoiceMode {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(ResponsesToolChoiceMode),
    Function {
        r#type: String,
        name: String,
    },
    WebSearch {
        r#type: String,
    },
    AllowedTools {
        r#type: String,
        mode: String,
        tools: Vec<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<ResponsesInputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponsesToolChoice>,
    pub store: bool,
    pub stream: bool,
    pub parallel_tool_calls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<std::collections::HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    pub text: ResponsesText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoning>,
    /// Request-local response validator/normalizer. This must never cross the
    /// Codex wire boundary; only `text.format.schema` is serialized upstream.
    #[serde(skip)]
    pub(crate) schema_bridge: Option<Arc<SchemaBridge>>,
    /// Anthropic's hosted-search budget has no Codex wire equivalent. Retain
    /// the effective request-local limit so response validation can enforce it
    /// before accepting an upstream hosted call.
    #[serde(skip)]
    pub(crate) hosted_web_search_max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesText {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ResponsesTextFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(default)]
        strict: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesInputItem {
    #[serde(rename = "additional_tools")]
    AdditionalTools {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: String,
        tools: Vec<Value>,
    },
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<ResponsesContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        #[serde(default)]
        call_id: String,
        output: ResponsesFunctionCallOutput,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: Vec<Value>,
        encrypted_content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesFunctionCallOutput {
    Text(String),
    Content(Vec<ResponsesFunctionCallOutputContent>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesFunctionCallOutputContent {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesTool {
    Function(ResponsesFunctionTool),
    WebSearch(ResponsesWebSearchTool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesFunctionTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub external_web_access: bool,
    pub search_content_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<ResponsesWebSearchFilters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<ResponsesWebSearchUserLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponsesWebSearchUserLocation {
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Debug, Default)]
struct ToolPlan {
    initial: Option<Vec<ResponsesTool>>,
    deferred: HashMap<String, ResponsesTool>,
}

pub struct TranslateOptions {
    pub session_id: Option<String>,
    pub service_tier: Option<ServiceTier>,
    pub model: String,
    pub use_responses_lite: bool,
}

/// Runtime configuration is injected by the provider so pure translation
/// tests never depend on a developer's process environment or config file.
#[derive(Debug, Clone, Default)]
pub struct TranslationOverrides {
    pub service_tier: Option<String>,
    pub effort: Option<String>,
    pub reasoning_summary: Option<String>,
}

impl TranslationOverrides {
    pub fn configured() -> Self {
        Self {
            service_tier: config::codex_service_tier(),
            effort: config::codex_effort(),
            reasoning_summary: config::codex_reasoning_summary(),
        }
    }
}

// ---------------------------------------------------------------------------
// Translation entry point
// ---------------------------------------------------------------------------

fn to_codex_effort(effort: Option<&str>) -> Option<Effort> {
    match effort {
        Some("max") => Some(Effort::Max),
        Some("xhigh") => Some(Effort::Xhigh),
        Some("low") => Some(Effort::Low),
        Some("medium") => Some(Effort::Medium),
        Some("high") => Some(Effort::High),
        _ => None,
    }
}

fn for_each_message_text(content: &Value, mut visit: impl FnMut(&str)) {
    match content {
        Value::String(text) => visit(text),
        Value::Array(blocks) => {
            for text in blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
            {
                visit(text);
            }
        }
        _ => {}
    }
}

fn is_claude_code_ultracode_request(req: &MessagesRequest) -> bool {
    // Claude Code adds this generated note to the current agent-list reminder
    // when Ultracode starts. If the session later leaves Ultracode, the old
    // marker remains in history and a newer generated system message turns it
    // off, so replay the control messages in order instead of scanning for any
    // historical match.
    let mut active = false;
    for message in &req.messages {
        for_each_message_text(&message.content, |text| {
            if message.role == "system" && text.starts_with(CLAUDE_CODE_ULTRACODE_OFF_MARKER) {
                active = false;
            } else if text.contains(CLAUDE_CODE_ULTRACODE_MARKER)
                && (message.role == "system"
                    || message.role == "user" && text.starts_with(CLAUDE_CODE_AGENT_LIST_PREFIX))
            {
                active = true;
            }
        });
    }
    active
}

fn resolve_effort(
    effort: Option<Effort>,
    override_effort: Option<&str>,
) -> Result<Option<Effort>, anyhow::Error> {
    resolve_effort_override(effort, override_effort)
}

fn resolve_effort_override(
    effort: Option<Effort>,
    override_effort: Option<&str>,
) -> Result<Option<Effort>, anyhow::Error> {
    if let Some(val) = override_effort {
        let valid = ["none", "low", "medium", "high", "xhigh", "max"];
        if !valid.contains(&val) {
            anyhow::bail!(
                "Invalid effort override: \"{val}\". Must be one of: none, low, medium, high, xhigh, max"
            );
        }
        return Ok(Some(match val {
            "max" => Effort::Max,
            "xhigh" => Effort::Xhigh,
            "high" => Effort::High,
            "medium" => Effort::Medium,
            "low" => Effort::Low,
            _ => Effort::None,
        }));
    }
    Ok(effort)
}

fn reasoning_summary_requested(summary: Option<&str>) -> bool {
    !matches!(summary, Some("off" | "none"))
}

const VALID_SERVICE_TIERS: &[&str] = &["fast", "priority", "flex"];

fn normalize_service_tier(tier: &str) -> Result<ServiceTier, anyhow::Error> {
    if !VALID_SERVICE_TIERS.contains(&tier) {
        anyhow::bail!(
            "Invalid service tier override: \"{tier}\". Must be one of: {}",
            VALID_SERVICE_TIERS.join(", ")
        );
    }
    match tier {
        "flex" => Ok(ServiceTier::Flex),
        _ => Ok(ServiceTier::Priority),
    }
}

fn resolve_service_tier(
    model_tier: Option<ServiceTier>,
    override_tier: Option<&str>,
) -> Result<Option<ServiceTier>, anyhow::Error> {
    match override_tier {
        Some(val) => Ok(Some(normalize_service_tier(val)?)),
        None => Ok(model_tier),
    }
}

/// Hosted tools (web_search) are rejected by the Responses Lite lane. Count
/// only tools that this turn actually exposes: eager, loaded deferred, or
/// explicitly forced tools.
pub fn has_hosted_web_search(req: &MessagesRequest) -> bool {
    has_effective_direct_tool(req, is_supported_web_search_type)
}

/// Whether this turn exposes at least one function tool that the model may
/// call directly. Deferred tools count only after a validated history has
/// loaded them, or when the request explicitly forces that tool.
pub(in crate::providers::codex) fn has_parallel_callable_function(req: &MessagesRequest) -> bool {
    if !parallel_tool_calls_enabled(req) {
        return false;
    }

    has_effective_direct_tool(req, |tool_type| !is_supported_web_search_type(tool_type))
}

fn has_effective_direct_tool(req: &MessagesRequest, accepts_type: impl Fn(&str) -> bool) -> bool {
    let forced_name = forced_tool_name(req);
    let loaded_deferred = loaded_deferred_tool_names(req);
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                let tool_type = tool
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("function");
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    return false;
                };
                if name.is_empty()
                    || !accepts_type(tool_type)
                    || !tool_allows_direct_calls(tool, tool_type)
                {
                    return false;
                }

                let deferred = tool
                    .get("defer_loading")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                !deferred || forced_name == Some(name) || loaded_deferred.contains(name)
            })
        })
}

fn loaded_deferred_tool_names(req: &MessagesRequest) -> HashSet<&str> {
    req.messages
        .iter()
        .filter(|message| message.role == "user")
        .filter_map(|message| message.content.as_array())
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_reference"))
        .filter_map(|block| block.get("tool_name").and_then(Value::as_str))
        .collect()
}

fn is_supported_web_search_type(tool_type: &str) -> bool {
    matches!(
        tool_type,
        "web_search_20250305" | "web_search_20260209" | "web_search_20260318"
    )
}

pub fn translate_request(
    req: &MessagesRequest,
    opts: TranslateOptions,
) -> Result<ResponsesRequest, anyhow::Error> {
    translate_request_with_overrides(req, opts, TranslationOverrides::configured())
}

pub fn translate_request_with_overrides(
    req: &MessagesRequest,
    opts: TranslateOptions,
    overrides: TranslationOverrides,
) -> Result<ResponsesRequest, anyhow::Error> {
    validate_alternate_provider_fields(req, "Codex")?;
    validate_codex_message_roles(req)?;
    validate_codex_image_inputs(req)?;
    validate_tool_contract(req)?;
    let (output_format, schema_bridge) = read_output_format(req)?;
    let mut instructions = flatten_system_text(req.extra.get("system"));
    let tool_plan = read_tools(req)?;
    let input = build_input(req, &tool_plan.deferred);
    validate_effective_tool_choice(req, &tool_plan, &input)?;
    let parallel_function_available = tool_plan.initial.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| matches!(tool, ResponsesTool::Function(_)))
    }) || input.iter().any(|item| {
        matches!(
            item,
            ResponsesInputItem::AdditionalTools { tools, .. }
                if tools.iter().any(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        )
    });
    let hosted_web_search_available = tool_plan.initial.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
    }) || input.iter().any(|item| {
        matches!(
            item,
            ResponsesInputItem::AdditionalTools { tools, .. }
                if tools.iter().any(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search"))
        )
    });
    let hosted_web_search_max_uses = hosted_web_search_available
        .then(|| effective_web_search_max_uses(req))
        .flatten();
    if hosted_web_search_available && hosted_tool_guidance_allowed(req) {
        append_web_search_request_guidance(req, &mut instructions);
    }
    let tools = tool_plan.initial;
    let tool_choice = map_tool_choice(req)?;

    let mut text = ResponsesText {
        verbosity: Some("low".to_string()),
        format: None,
    };

    if let Some(fmt) = output_format {
        text.format = Some(fmt);
    }

    let mut out = ResponsesRequest {
        model: opts.model,
        instructions,
        input,
        store: false,
        stream: true,
        parallel_tool_calls: parallel_tool_calls_enabled(req),
        tool_choice,
        text,
        tools: None,
        include: Some(vec!["reasoning.encrypted_content".to_string()]),
        client_metadata: None,
        service_tier: None,
        prompt_cache_key: None,
        reasoning: None,
        schema_bridge,
        hosted_web_search_max_uses,
    };

    if opts.use_responses_lite {
        out.client_metadata = Some(std::collections::HashMap::from([(
            "ws_request_header_x_openai_internal_codex_responses_lite".to_string(),
            "true".to_string(),
        )]));
        out.parallel_tool_calls = false;

        let mut prefix = Vec::new();
        if let Some(ref tools) = tools
            && !tools.is_empty()
        {
            let tools = tools
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            prefix.push(ResponsesInputItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools,
            });
        }
        if let Some(instructions) = out.instructions.take()
            && !instructions.is_empty()
        {
            prefix.push(ResponsesInputItem::Message {
                role: "developer".to_string(),
                content: vec![ResponsesContentPart::InputText { text: instructions }],
            });
        }
        if !prefix.is_empty() {
            prefix.extend(out.input);
            out.input = prefix;
        }
    } else {
        if out.parallel_tool_calls
            && parallel_function_available
            && function_tool_guidance_allowed(req)
        {
            append_instruction(&mut out.instructions, PARALLEL_TOOL_GUIDANCE);
        }
        if let Some(tools) = tools
            && !tools.is_empty()
        {
            out.tools = Some(tools);
        }
    }

    if hosted_web_search_available && let Some(include) = &mut out.include {
        include.push("web_search_call.action.sources".to_string());
    }

    // Never force a web_search tool_choice the request didn't register —
    // upstream 502s instead of ignoring it.
    if matches!(
        out.tool_choice,
        Some(ResponsesToolChoice::WebSearch { .. } | ResponsesToolChoice::AllowedTools { .. })
    ) {
        let has_web_search = out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        });
        if !has_web_search {
            out.tool_choice = Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto));
        }
    }

    if let Some(sid) = opts.session_id {
        out.prompt_cache_key = Some(sid);
    }

    // Anthropic's `standard_only` is a hard caller constraint. Omitting the
    // OpenAI service_tier is the standard-lane representation and must override
    // a local `-fast` alias or priority configuration.
    let service_tier =
        if req.extra.get("service_tier").and_then(Value::as_str) == Some("standard_only") {
            None
        } else {
            resolve_service_tier(opts.service_tier, overrides.service_tier.as_deref())?
        };
    if let Some(ref tier) = service_tier {
        out.service_tier = Some(tier.clone());
    }

    let effort = read_effort(req)?;
    // Compaction is a structured summary pass, so max reasoning only adds
    // latency. Claude Code also omits effort for Haiku; default Luna to medium.
    let mut codex_effort = if is_claude_code_compaction_request(req) {
        Some(Effort::Medium)
    } else {
        to_codex_effort(effort).or_else(|| (out.model == "gpt-5.6-luna").then_some(Effort::Medium))
    };
    if matches!(codex_effort.as_ref(), Some(Effort::Xhigh)) && is_claude_code_ultracode_request(req)
    {
        codex_effort = Some(Effort::Max);
    }
    let resolved_effort = resolve_effort(codex_effort, overrides.effort.as_deref())?;
    if resolved_effort.is_some() || opts.use_responses_lite {
        let summary = if resolved_effort.is_some()
            && reasoning_summary_requested(overrides.reasoning_summary.as_deref())
        {
            Some("auto".to_string())
        } else {
            None
        };
        out.reasoning = Some(ResponsesReasoning {
            effort: resolved_effort.clone(),
            summary,
            context: opts.use_responses_lite.then_some("all_turns".to_string()),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn append_instruction(instructions: &mut Option<String>, guidance: &str) {
    *instructions = Some(match instructions.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{guidance}"),
        _ => guidance.to_string(),
    });
}

fn append_web_search_request_guidance(req: &MessagesRequest, instructions: &mut Option<String>) {
    let Some(tools) = req.extra.get("tools").and_then(Value::as_array) else {
        return;
    };
    let forced_name = forced_tool_name(req);
    let loaded_deferred = loaded_deferred_tool_names(req);
    for tool in tools {
        let Some(tool_type) = tool.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !is_supported_web_search_type(tool_type) || !tool_allows_direct_calls(tool, tool_type) {
            continue;
        }
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let deferred = tool
            .get("defer_loading")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if deferred && forced_name != Some(name) && !loaded_deferred.contains(name) {
            continue;
        }
        if let Some(max_uses) = tool.get("max_uses").and_then(Value::as_u64) {
            // Codex's hosted web search schema has no hard per-request call
            // limit. Preserve Anthropic's intent as explicit best-effort
            // model guidance without claiming protocol-level enforcement.
            append_instruction(
                instructions,
                &format!(
                    "Best-effort compatibility limit: use web_search no more than {max_uses} time(s) in this response."
                ),
            );
        }
        // response_inclusion only controls results consumed by a completed
        // Anthropic code_execution call. Codex does not expose programmatic
        // hosted-tool callers here, while direct calls are always returned in
        // full, so both `full` and `excluded` safely map to direct full output.
    }
}

fn effective_web_search_max_uses(req: &MessagesRequest) -> Option<u64> {
    if !hosted_tool_guidance_allowed(req) {
        return None;
    }
    let forced_name = forced_tool_name(req);
    let loaded_deferred = loaded_deferred_tool_names(req);
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools.iter().find(|tool| {
                let tool_type = tool
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("function");
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    return false;
                };
                if !is_supported_web_search_type(tool_type)
                    || !tool_allows_direct_calls(tool, tool_type)
                {
                    return false;
                }
                let deferred = tool
                    .get("defer_loading")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                !deferred || forced_name == Some(name) || loaded_deferred.contains(name)
            })
        })
        .and_then(|tool| tool.get("max_uses"))
        .and_then(Value::as_u64)
}

fn read_output_format(
    req: &MessagesRequest,
) -> anyhow::Result<(Option<ResponsesTextFormat>, Option<Arc<SchemaBridge>>)> {
    let Some(format) = parse_output_format(req)? else {
        return Ok((None, None));
    };
    let (format, schema_bridge) = match format {
        RequestedOutputFormat::JsonSchema { name, schema } => {
            if name.len() > 64
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                anyhow::bail!(
                    "output_config.format.name must contain only ASCII letters, digits, underscores, or hyphens and be at most 64 characters"
                );
            }
            let bridge = Arc::new(SchemaBridge::build(schema)?);
            (
                ResponsesTextFormat::JsonSchema {
                    name: name.to_string(),
                    schema: bridge.upstream_schema().clone(),
                    strict: Some(true),
                },
                Some(bridge),
            )
        }
        RequestedOutputFormat::JsonObject => (ResponsesTextFormat::JsonObject, None),
        RequestedOutputFormat::Text => (ResponsesTextFormat::Text, None),
    };
    Ok((Some(format), schema_bridge))
}

fn validate_codex_image_inputs(req: &MessagesRequest) -> Result<(), anyhow::Error> {
    let mut validated_base64_images = HashSet::new();
    for (message_index, message) in req.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let Some(kind) = block.get("type").and_then(Value::as_str) else {
                continue;
            };
            let path = format!("messages[{message_index}].content[{block_index}]");
            if kind == "image" {
                if message.role != "user" {
                    anyhow::bail!("{path} image blocks must have user role");
                }
                validate_codex_image_block(block, &path, &mut validated_base64_images)?;
            } else if kind == "tool_result"
                && let Some(result_blocks) = block.get("content").and_then(Value::as_array)
            {
                for (result_index, result_block) in result_blocks.iter().enumerate() {
                    if result_block.get("type").and_then(Value::as_str) == Some("image") {
                        validate_codex_image_block(
                            result_block,
                            &format!("{path}.content[{result_index}]"),
                            &mut validated_base64_images,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_codex_image_block<'a>(
    block: &'a Value,
    path: &str,
    validated_base64_images: &mut HashSet<(&'a str, &'a str)>,
) -> Result<(), anyhow::Error> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("{path}.source must be an object"))?;
    let source_type = source
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{path}.source.type must be a string"))?;
    match source_type {
        "url" => {
            let raw = source
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| anyhow::anyhow!("{path}.source.url must be a non-empty string"))?;
            let url = Url::parse(raw)
                .map_err(|_| anyhow::anyhow!("{path}.source.url must be a fully qualified URL"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                anyhow::bail!("{path}.source.url must use http or https and include a host");
            }
            if !url.username().is_empty() || url.password().is_some() {
                anyhow::bail!("{path}.source.url must not contain credentials");
            }
        }
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("{path}.source.media_type must be a string"))?;
            if !matches!(
                media_type,
                "image/png" | "image/jpeg" | "image/webp" | "image/gif"
            ) {
                anyhow::bail!("{path}.source.media_type is not supported: {media_type}");
            }
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .filter(|data| !data.is_empty())
                .ok_or_else(|| anyhow::anyhow!("{path}.source.data must be a non-empty string"))?;
            if validated_base64_images.insert((media_type, data)) {
                validate_codex_base64_image(data, media_type, MAX_CODEX_IMAGE_BYTES)
                    .map_err(|message| anyhow::anyhow!("{path}.source.data {message}"))?;
            }
        }
        other => anyhow::bail!("{path}.source.type is not supported: {other}"),
    }
    Ok(())
}

fn validate_codex_base64_image(
    encoded: &str,
    media_type: &str,
    max_decoded_bytes: usize,
) -> Result<(), &'static str> {
    let max_encoded_bytes = max_decoded_bytes
        .saturating_add(2)
        .checked_div(3)
        .unwrap_or(0)
        .saturating_mul(4);
    if encoded.len() > max_encoded_bytes {
        return Err("exceeds the image byte limit");
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "must be valid base64")?;
    if decoded.len() > max_decoded_bytes {
        return Err("exceeds the image byte limit");
    }

    match media_type {
        "image/png" => validate_complete_png(&decoded),
        "image/jpeg" => validate_complete_jpeg(&decoded),
        "image/gif" => validate_complete_gif(&decoded),
        "image/webp" => validate_complete_webp(&decoded),
        _ => Err(INVALID_CODEX_IMAGE),
    }
}

fn validate_image_pixel_count(width: u64, height: u64) -> Result<u64, &'static str> {
    let pixels = width
        .checked_mul(height)
        .filter(|pixels| width != 0 && height != 0 && *pixels <= MAX_CODEX_IMAGE_PIXELS)
        .ok_or(CODEX_IMAGE_DECODE_LIMIT)?;
    Ok(pixels)
}

fn allocate_image_validation_buffer(size: usize) -> Result<Vec<u8>, &'static str> {
    if size > MAX_CODEX_IMAGE_DECODE_BYTES {
        return Err(CODEX_IMAGE_DECODE_LIMIT);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(size)
        .map_err(|_| CODEX_IMAGE_DECODE_LIMIT)?;
    output.resize(size, 0);
    Ok(output)
}

fn validate_complete_png(bytes: &[u8]) -> Result<(), &'static str> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(INVALID_CODEX_IMAGE);
    }

    let mut cursor = Cursor::new(bytes);
    let decoder = png::Decoder::new_with_limits(
        &mut cursor,
        png::Limits {
            bytes: MAX_CODEX_IMAGE_DECODE_BYTES,
        },
    );
    let mut reader = decoder.read_info().map_err(map_codex_png_error)?;
    validate_image_pixel_count(
        u64::from(reader.info().width),
        u64::from(reader.info().height),
    )?;
    if reader
        .output_buffer_size()
        .filter(|size| *size <= MAX_CODEX_IMAGE_DECODE_BYTES)
        .is_none()
    {
        return Err(CODEX_IMAGE_DECODE_LIMIT);
    }

    while reader.next_row().map_err(map_codex_png_error)?.is_some() {}
    reader.finish().map_err(map_codex_png_error)?;
    drop(reader);
    if cursor.position() != bytes.len() as u64 {
        return Err(INVALID_CODEX_IMAGE);
    }
    Ok(())
}

fn map_codex_png_error(error: png::DecodingError) -> &'static str {
    match error {
        png::DecodingError::LimitsExceeded => CODEX_IMAGE_DECODE_LIMIT,
        _ => INVALID_CODEX_IMAGE,
    }
}

fn validate_complete_jpeg(bytes: &[u8]) -> Result<(), &'static str> {
    validate_codex_jpeg_framing(bytes)?;

    let options = zune_core::options::DecoderOptions::new_safe()
        .set_strict_mode(true)
        .set_max_width(usize::from(u16::MAX))
        .set_max_height(usize::from(u16::MAX))
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::Luma);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(bytes), options);
    decoder.decode_headers().map_err(|_| INVALID_CODEX_IMAGE)?;
    let info = decoder.info().ok_or(INVALID_CODEX_IMAGE)?;
    validate_image_pixel_count(u64::from(info.width), u64::from(info.height))?;
    let output_size = decoder
        .output_buffer_size()
        .filter(|size| *size <= MAX_CODEX_IMAGE_DECODE_BYTES)
        .ok_or(CODEX_IMAGE_DECODE_LIMIT)?;
    let mut output = allocate_image_validation_buffer(output_size)?;
    decoder
        .decode_into(&mut output)
        .map_err(|_| INVALID_CODEX_IMAGE)?;
    Ok(())
}

fn validate_codex_jpeg_framing(bytes: &[u8]) -> Result<(), &'static str> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(INVALID_CODEX_IMAGE);
    }

    let mut offset = 2;
    let mut saw_scan = false;
    while offset < bytes.len() {
        if bytes[offset] != 0xff {
            return Err(INVALID_CODEX_IMAGE);
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset).ok_or(INVALID_CODEX_IMAGE)?;
        offset += 1;

        match marker {
            0xd9 => {
                return if saw_scan && offset == bytes.len() {
                    Ok(())
                } else {
                    Err(INVALID_CODEX_IMAGE)
                };
            }
            0xd8 | 0x00 | 0xd0..=0xd7 => return Err(INVALID_CODEX_IMAGE),
            0x01 => continue,
            _ => {}
        }

        let length_bytes = bytes
            .get(offset..offset.saturating_add(2))
            .ok_or(INVALID_CODEX_IMAGE)?;
        let segment_length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if segment_length < 2 {
            return Err(INVALID_CODEX_IMAGE);
        }
        offset = offset
            .checked_add(segment_length)
            .filter(|end| *end <= bytes.len())
            .ok_or(INVALID_CODEX_IMAGE)?;
        if marker != 0xda {
            continue;
        }

        saw_scan = true;
        let mut entropy_bytes = 0_usize;
        while offset < bytes.len() {
            if bytes[offset] != 0xff {
                entropy_bytes += 1;
                offset += 1;
                continue;
            }
            let marker_start = offset;
            while offset < bytes.len() && bytes[offset] == 0xff {
                offset += 1;
            }
            let next = *bytes.get(offset).ok_or(INVALID_CODEX_IMAGE)?;
            match next {
                0x00 => {
                    entropy_bytes += 1;
                    offset += 1;
                }
                0xd0..=0xd7 => offset += 1,
                _ => {
                    offset = marker_start;
                    break;
                }
            }
        }
        if entropy_bytes == 0 {
            return Err(INVALID_CODEX_IMAGE);
        }
    }
    Err(INVALID_CODEX_IMAGE)
}

fn validate_complete_gif(bytes: &[u8]) -> Result<(), &'static str> {
    if !bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a") {
        return Err(INVALID_CODEX_IMAGE);
    }

    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::Indexed);
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        NonZeroU64::new(MAX_CODEX_IMAGE_DECODE_BYTES as u64)
            .expect("image decode limit is non-zero"),
    ));
    options.check_frame_consistency(true);
    options.check_lzw_end_code(true);
    options.allow_unknown_blocks(false);
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|_| INVALID_CODEX_IMAGE)?;
    validate_image_pixel_count(u64::from(decoder.width()), u64::from(decoder.height()))?;

    let mut frame_count = 0_u64;
    while let Some(frame) = decoder.read_next_frame().map_err(|_| INVALID_CODEX_IMAGE)? {
        validate_image_pixel_count(u64::from(frame.width), u64::from(frame.height))?;
        frame_count += 1;
        if frame_count > 1 {
            return Err(INVALID_CODEX_IMAGE);
        }
    }
    if frame_count == 0 {
        return Err(INVALID_CODEX_IMAGE);
    }

    let reader = decoder.into_inner();
    let logical_position = reader
        .get_ref()
        .position()
        .checked_sub(reader.buffer().len() as u64)
        .ok_or(INVALID_CODEX_IMAGE)?;
    if logical_position != bytes.len() as u64 {
        return Err(INVALID_CODEX_IMAGE);
    }
    Ok(())
}

fn validate_complete_webp(bytes: &[u8]) -> Result<(), &'static str> {
    validate_webp_riff_layout(bytes)?;
    let mut decoder =
        image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|_| INVALID_CODEX_IMAGE)?;
    decoder.set_memory_limit(MAX_CODEX_IMAGE_DECODE_BYTES);
    let (width, height) = decoder.dimensions();
    validate_image_pixel_count(u64::from(width), u64::from(height))?;
    let output_size = decoder
        .output_buffer_size()
        .filter(|size| *size <= MAX_CODEX_IMAGE_DECODE_BYTES)
        .ok_or(CODEX_IMAGE_DECODE_LIMIT)?;
    let mut output = allocate_image_validation_buffer(output_size)?;

    if decoder.is_animated() {
        return Err(INVALID_CODEX_IMAGE);
    }
    decoder
        .read_image(&mut output)
        .map_err(|_| INVALID_CODEX_IMAGE)?;
    Ok(())
}

fn validate_webp_riff_layout(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() < 20 || bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return Err(INVALID_CODEX_IMAGE);
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| INVALID_CODEX_IMAGE)?);
    let total_size = usize::try_from(riff_size)
        .ok()
        .and_then(|size| size.checked_add(8))
        .ok_or(INVALID_CODEX_IMAGE)?;
    if total_size != bytes.len() {
        return Err(INVALID_CODEX_IMAGE);
    }

    let mut offset = 12_usize;
    let mut chunk_count = 0_usize;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or(INVALID_CODEX_IMAGE)?;
        let header = bytes.get(offset..header_end).ok_or(INVALID_CODEX_IMAGE)?;
        let chunk_size = usize::try_from(u32::from_le_bytes(
            header[4..8].try_into().map_err(|_| INVALID_CODEX_IMAGE)?,
        ))
        .map_err(|_| INVALID_CODEX_IMAGE)?;
        let data_end = header_end
            .checked_add(chunk_size)
            .filter(|end| *end <= bytes.len())
            .ok_or(INVALID_CODEX_IMAGE)?;
        let padded_end = data_end
            .checked_add(chunk_size & 1)
            .filter(|end| *end <= bytes.len())
            .ok_or(INVALID_CODEX_IMAGE)?;
        if chunk_size & 1 == 1 && bytes[data_end] != 0 {
            return Err(INVALID_CODEX_IMAGE);
        }
        if matches!(&header[..4], b"ANIM" | b"ANMF") {
            return Err(INVALID_CODEX_IMAGE);
        }
        offset = padded_end;
        chunk_count += 1;
    }
    if offset != bytes.len() || chunk_count == 0 {
        return Err(INVALID_CODEX_IMAGE);
    }
    Ok(())
}

fn validate_tool_contract(req: &MessagesRequest) -> Result<(), anyhow::Error> {
    let mut registered_tools: HashMap<&str, bool> = HashMap::new();
    if let Some(tools) = req.extra.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
        for (index, tool) in tools.iter().enumerate() {
            let tool = tool
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("tools[{index}] must be an object"))?;
            let tool_type = match tool.get("type") {
                Some(Value::String(value)) => value.as_str(),
                Some(Value::Null) => "function",
                Some(_) => anyhow::bail!("tools[{index}].type must be a string"),
                None => "function",
            };
            if tool_type.starts_with("web_search_") && !is_supported_web_search_type(tool_type) {
                anyhow::bail!("unsupported Anthropic web search tool type: {tool_type}");
            }
            if tool_type.starts_with("tool_search_tool_") {
                anyhow::bail!(
                    "Anthropic hosted tool search is not supported by Codex translation; use the client ToolSearch tool"
                );
            }

            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow::anyhow!("tools[{index}].name must be a non-empty string"))?;
            if is_supported_web_search_type(tool_type) && name != "web_search" {
                anyhow::bail!("tools[{index}].name must be web_search for {tool_type}");
            }
            if registered_tools
                .insert(
                    name,
                    tool_allows_direct_calls(&Value::Object(tool.clone()), tool_type),
                )
                .is_some()
            {
                anyhow::bail!("duplicate tool name: {name}");
            }

            for field in ["defer_loading", "strict", "eager_input_streaming"] {
                if let Some(value) = tool.get(field)
                    && !value.is_null()
                    && !value.is_boolean()
                {
                    anyhow::bail!("tools[{index}].{field} must be a boolean");
                }
            }
            if let Some(value) = tool.get("description")
                && !value.is_string()
                && !value.is_null()
            {
                anyhow::bail!("tools[{index}].description must be a string");
            }
            if !is_supported_web_search_type(tool_type)
                && let Some(schema) = tool.get("input_schema")
                && !schema.is_object()
            {
                anyhow::bail!("tools[{index}].input_schema must be an object");
            }
            validate_allowed_callers(tool, index)?;
            if is_supported_web_search_type(tool_type) {
                validate_web_search_tool(tool, index, tool_type)?;
            }
        }
    }

    validate_tool_choice(req, &registered_tools)?;
    validate_message_tool_history(req)?;
    validate_tool_reference_provenance(req)?;
    Ok(())
}

fn validate_allowed_callers(
    tool: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<(), anyhow::Error> {
    let Some(callers) = tool.get("allowed_callers") else {
        return Ok(());
    };
    if callers.is_null() {
        return Ok(());
    }
    let callers = callers
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools[{index}].allowed_callers must be an array"))?;
    for caller in callers {
        let caller = caller.as_str().ok_or_else(|| {
            anyhow::anyhow!("tools[{index}].allowed_callers entries must be strings")
        })?;
        if caller.is_empty() {
            anyhow::bail!("tools[{index}].allowed_callers entries must not be empty");
        }
    }
    Ok(())
}

fn validate_web_search_tool(
    tool: &serde_json::Map<String, Value>,
    index: usize,
    tool_type: &str,
) -> Result<(), anyhow::Error> {
    for field in ["allowed_domains", "blocked_domains"] {
        if let Some(value) = tool.get(field)
            && !value.is_null()
        {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("tools[{index}].{field} must be an array"))?;
            if values.iter().any(|entry| !entry.is_string()) {
                anyhow::bail!("tools[{index}].{field} entries must be strings");
            }
        }
    }
    let has_allowed = tool
        .get("allowed_domains")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty());
    let has_blocked = tool
        .get("blocked_domains")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty());
    if has_allowed && has_blocked {
        anyhow::bail!("tools[{index}] cannot specify both allowed_domains and blocked_domains");
    }

    if let Some(value) = tool.get("max_uses")
        && !value.is_null()
        && value.as_u64().is_none_or(|value| value == 0)
    {
        anyhow::bail!("tools[{index}].max_uses must be a positive integer");
    }
    if let Some(value) = tool.get("response_inclusion")
        && !value.is_null()
    {
        if tool_type != "web_search_20260318" {
            anyhow::bail!("tools[{index}].response_inclusion requires web_search_20260318");
        }
        if !matches!(value.as_str(), Some("full" | "excluded")) {
            anyhow::bail!("tools[{index}].response_inclusion must be full or excluded");
        }
    }
    if let Some(value) = tool.get("user_location")
        && !value.is_null()
    {
        let location = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("tools[{index}].user_location must be an object"))?;
        if location.get("type").and_then(Value::as_str) != Some("approximate") {
            anyhow::bail!("tools[{index}].user_location.type must be approximate");
        }
        for field in ["city", "country", "region", "timezone"] {
            if let Some(value) = location.get(field)
                && !value.is_string()
                && !value.is_null()
            {
                anyhow::bail!("tools[{index}].user_location.{field} must be a string");
            }
        }
    }
    Ok(())
}

fn validate_tool_choice(
    req: &MessagesRequest,
    registered_tools: &HashMap<&str, bool>,
) -> Result<(), anyhow::Error> {
    let Some(choice) = req.extra.get("tool_choice") else {
        return Ok(());
    };
    if let Some(choice) = choice.as_str() {
        if !matches!(choice, "auto" | "none" | "any" | "required") {
            anyhow::bail!("unsupported tool_choice: {choice}");
        }
        return Ok(());
    }
    let choice = choice
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool_choice must be a string or object"))?;
    let choice_type = choice
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tool_choice.type must be a string"))?;
    if !matches!(choice_type, "auto" | "none" | "any" | "required" | "tool") {
        anyhow::bail!("unsupported tool_choice.type: {choice_type}");
    }
    if let Some(value) = choice.get("disable_parallel_tool_use")
        && !value.is_boolean()
    {
        anyhow::bail!("tool_choice.disable_parallel_tool_use must be a boolean");
    }
    if choice_type == "tool" {
        let name = choice
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("tool_choice.name must be a non-empty string"))?;
        let Some(allows_direct) = registered_tools.get(name) else {
            anyhow::bail!("tool_choice references unknown tool: {name}");
        };
        if !allows_direct {
            anyhow::bail!("tool_choice cannot force tool {name} because it disallows direct calls");
        }
    }
    Ok(())
}

fn validate_effective_tool_choice(
    req: &MessagesRequest,
    tool_plan: &ToolPlan,
    input: &[ResponsesInputItem],
) -> Result<(), anyhow::Error> {
    let requires_tool = match req.extra.get("tool_choice") {
        Some(Value::String(value)) => matches!(value.as_str(), "any" | "required"),
        Some(Value::Object(choice)) => choice
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|value| matches!(value, "any" | "required")),
        _ => false,
    };
    if !requires_tool {
        return Ok(());
    }
    let has_initial = tool_plan
        .initial
        .as_ref()
        .is_some_and(|tools| !tools.is_empty());
    let has_loaded_deferred = input.iter().any(|item| {
        matches!(item, ResponsesInputItem::AdditionalTools { tools, .. } if !tools.is_empty())
    });
    if !has_initial && !has_loaded_deferred {
        anyhow::bail!("tool_choice requires a tool, but no directly callable tools are available");
    }
    Ok(())
}

fn validate_message_tool_history(req: &MessagesRequest) -> Result<(), anyhow::Error> {
    let mut seen_use_ids = HashSet::new();
    let mut pending_custom = HashSet::new();
    let mut custom_use_order = Vec::new();
    let mut pending_hosted: HashMap<String, String> = HashMap::new();
    let mut hosted_use_order = Vec::new();
    let mut hosted_mixed_pause = false;

    for (message_index, message) in req.messages.iter().enumerate() {
        let expects_custom_results = !pending_custom.is_empty();
        if expects_custom_results && message.role != "user" {
            let id = first_pending_id(&custom_use_order, &pending_custom);
            anyhow::bail!(
                "custom tool use id {id} must be resolved in the immediately following user message"
            );
        }
        let expects_delayed_hosted_results =
            hosted_mixed_pause && pending_custom.is_empty() && !pending_hosted.is_empty();
        if expects_delayed_hosted_results && message.role != "assistant" {
            let id = first_pending_id(&hosted_use_order, &pending_hosted);
            anyhow::bail!(
                "delayed hosted tool use id {id} must be resolved at the start of the next assistant message"
            );
        }

        let blocks = match &message.content {
            Value::String(_) => {
                if expects_custom_results {
                    let id = first_pending_id(&custom_use_order, &pending_custom);
                    anyhow::bail!(
                        "custom tool use id {id} must be resolved in the immediately following user message"
                    );
                }
                if expects_delayed_hosted_results {
                    let id = first_pending_id(&hosted_use_order, &pending_hosted);
                    anyhow::bail!(
                        "delayed hosted tool use id {id} must be resolved at the start of the next assistant message"
                    );
                }
                continue;
            }
            Value::Array(blocks) => blocks,
            _ => anyhow::bail!("messages[{message_index}].content must be a string or array"),
        };
        let mut saw_non_tool_result = false;
        let mixed_pause_user_message = expects_custom_results && !pending_hosted.is_empty();
        let mut resolving_delayed_hosted = expects_delayed_hosted_results;

        for (block_index, block) in blocks.iter().enumerate() {
            let Some(block) = block.as_object() else {
                anyhow::bail!("messages[{message_index}].content[{block_index}] must be an object");
            };
            let Some(kind) = block.get("type").and_then(Value::as_str) else {
                anyhow::bail!(
                    "messages[{message_index}].content[{block_index}].type must be a string"
                );
            };

            if mixed_pause_user_message && kind != "tool_result" {
                anyhow::bail!(
                    "messages[{message_index}] must contain only client tool_result blocks while hosted tools are paused"
                );
            }
            if expects_custom_results && kind == "tool_result" {
                if saw_non_tool_result {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}] tool_result blocks must precede text and other content"
                    );
                }
            } else if expects_custom_results {
                saw_non_tool_result = true;
            }

            if resolving_delayed_hosted {
                if !is_hosted_tool_result_type(kind) {
                    let id = first_pending_id(&hosted_use_order, &pending_hosted);
                    anyhow::bail!(
                        "delayed hosted tool use id {id} must be resolved before other assistant content"
                    );
                }
                let id = validate_hosted_tool_result_id(block, message_index, block_index)?;
                resolve_hosted_tool_result(&mut pending_hosted, id, kind)?;
                if pending_hosted.is_empty() {
                    resolving_delayed_hosted = false;
                    hosted_mixed_pause = false;
                }
                continue;
            }

            if kind == "tool_use" {
                if message.role != "assistant" {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}] tool_use must have assistant role"
                    );
                }
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "messages[{message_index}].content[{block_index}].id must be a non-empty string"
                        )
                    })?;
                if !seen_use_ids.insert(id.to_string()) {
                    anyhow::bail!("duplicate tool use id: {id}");
                }
                if block
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}].name must be a non-empty string"
                    );
                }
                if !block.get("input").is_some_and(Value::is_object) {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}].input must be an object"
                    );
                }
                pending_custom.insert(id.to_string());
                custom_use_order.push(id.to_string());
            } else if kind == "server_tool_use" {
                if message.role != "assistant" {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}] server_tool_use must have assistant role"
                    );
                }
                let id = validate_tool_use_identity(block, message_index, block_index)?;
                if !seen_use_ids.insert(id.to_string()) {
                    anyhow::bail!("duplicate tool use id: {id}");
                }
                if !block.contains_key("input") {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}].input is required"
                    );
                }
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .expect("tool identity validation guarantees a name");
                let expected_result = hosted_tool_result_kind(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "messages[{message_index}].content[{block_index}] unsupported server tool name: {name}"
                    )
                })?;
                pending_hosted.insert(id.to_string(), expected_result.to_string());
                hosted_use_order.push(id.to_string());
            } else if kind == "tool_result" {
                if message.role != "user" {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}] tool_result must have user role"
                    );
                }
                let id = validate_tool_result_block(block, message_index, block_index)?;
                if !pending_custom.remove(id) {
                    anyhow::bail!(
                        "tool result references unknown tool use id (already resolved or not-yet-seen): {id}"
                    );
                }
            } else if is_hosted_tool_result_type(kind) {
                if message.role != "assistant" {
                    anyhow::bail!(
                        "messages[{message_index}].content[{block_index}] {kind} must have assistant role"
                    );
                }
                let id = validate_hosted_tool_result_id(block, message_index, block_index)?;
                resolve_hosted_tool_result(&mut pending_hosted, id, kind)?;
            }
        }

        if expects_custom_results && !pending_custom.is_empty() {
            let id = first_pending_id(&custom_use_order, &pending_custom);
            anyhow::bail!(
                "custom tool use id {id} must be resolved in the immediately following user message"
            );
        }

        if resolving_delayed_hosted {
            let id = first_pending_id(&hosted_use_order, &pending_hosted);
            anyhow::bail!(
                "delayed hosted tool use id {id} must be resolved at the start of the next assistant message"
            );
        }
        if message.role == "assistant" && !pending_hosted.is_empty() {
            if pending_custom.is_empty() {
                let id = first_pending_id(&hosted_use_order, &pending_hosted);
                anyhow::bail!("unresolved server tool use id: {id}");
            }
            hosted_mixed_pause = true;
        }
    }

    if let Some(id) = custom_use_order
        .iter()
        .find(|id| pending_custom.contains(id.as_str()))
    {
        anyhow::bail!("unresolved custom tool use id: {id}");
    }
    if let Some(id) = hosted_use_order
        .iter()
        .find(|id| pending_hosted.contains_key(id.as_str()))
    {
        anyhow::bail!("unresolved server tool use id: {id}");
    }
    Ok(())
}

trait PendingToolIds {
    fn contains_pending(&self, id: &str) -> bool;
}

impl PendingToolIds for HashSet<String> {
    fn contains_pending(&self, id: &str) -> bool {
        self.contains(id)
    }
}

impl PendingToolIds for HashMap<String, String> {
    fn contains_pending(&self, id: &str) -> bool {
        self.contains_key(id)
    }
}

fn first_pending_id<'a>(order: &'a [String], pending: &impl PendingToolIds) -> &'a str {
    order
        .iter()
        .find(|id| pending.contains_pending(id))
        .expect("pending tool ids originate from the recorded use order")
}

fn resolve_hosted_tool_result(
    pending: &mut HashMap<String, String>,
    id: &str,
    actual_kind: &str,
) -> Result<(), anyhow::Error> {
    let Some(expected_kind) = pending.get(id) else {
        anyhow::bail!(
            "hosted tool result references unknown server tool use id (already resolved or not-yet-seen): {id}"
        );
    };
    if actual_kind != expected_kind {
        anyhow::bail!(
            "hosted tool result kind mismatch for server tool use id {id}: expected {expected_kind}, got {actual_kind}"
        );
    }
    pending.remove(id);
    Ok(())
}

fn validate_tool_use_identity(
    block: &serde_json::Map<String, Value>,
    message_index: usize,
    block_index: usize,
) -> Result<&str, anyhow::Error> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "messages[{message_index}].content[{block_index}].id must be a non-empty string"
            )
        })?;
    if block
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        anyhow::bail!(
            "messages[{message_index}].content[{block_index}].name must be a non-empty string"
        );
    }
    Ok(id)
}

fn is_hosted_tool_result_type(kind: &str) -> bool {
    kind != "tool_result" && kind.ends_with("_tool_result")
}

fn hosted_tool_result_kind(name: &str) -> Option<&'static str> {
    match name {
        "web_search" => Some("web_search_tool_result"),
        "web_fetch" => Some("web_fetch_tool_result"),
        "code_execution" => Some("code_execution_tool_result"),
        "bash_code_execution" => Some("bash_code_execution_tool_result"),
        "text_editor_code_execution" => Some("text_editor_code_execution_tool_result"),
        "tool_search_tool_regex" | "tool_search_tool_bm25" => Some("tool_search_tool_result"),
        // Compatibility for transcripts emitted by older cg builds. This is
        // accepted as history but is not generated by the Codex translator.
        "x_search" => Some("x_search_tool_result"),
        _ => None,
    }
}

fn validate_hosted_tool_result_id(
    block: &serde_json::Map<String, Value>,
    message_index: usize,
    block_index: usize,
) -> Result<&str, anyhow::Error> {
    block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "messages[{message_index}].content[{block_index}].tool_use_id must be a non-empty string"
            )
        })
}

fn validate_tool_result_block(
    block: &serde_json::Map<String, Value>,
    message_index: usize,
    block_index: usize,
) -> Result<&str, anyhow::Error> {
    let id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "messages[{message_index}].content[{block_index}].tool_use_id must be a non-empty string"
            )
        })?;
    if let Some(value) = block.get("is_error")
        && !value.is_boolean()
    {
        anyhow::bail!(
            "messages[{message_index}].content[{block_index}].is_error must be a boolean"
        );
    }
    let Some(content) = block.get("content") else {
        return Ok(id);
    };
    match content {
        Value::String(_) => {}
        Value::Array(children) => {
            for (child_index, child) in children.iter().enumerate() {
                validate_tool_result_child(child, message_index, block_index, child_index)?;
            }
        }
        _ => anyhow::bail!(
            "messages[{message_index}].content[{block_index}].content must be a string or array"
        ),
    }
    Ok(id)
}

fn validate_tool_result_child(
    child: &Value,
    message_index: usize,
    block_index: usize,
    child_index: usize,
) -> Result<(), anyhow::Error> {
    let child = child.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "messages[{message_index}].content[{block_index}].content[{child_index}] must be an object"
        )
    })?;
    let kind = child.get("type").and_then(Value::as_str).ok_or_else(|| {
        anyhow::anyhow!(
            "messages[{message_index}].content[{block_index}].content[{child_index}].type must be a string"
        )
    })?;
    if !matches!(
        kind,
        "text" | "image" | "search_result" | "document" | "tool_reference"
    ) {
        anyhow::bail!(
            "unsupported tool result content type at messages[{message_index}].content[{block_index}].content[{child_index}]: {kind}"
        );
    }
    if kind == "text" && !child.get("text").is_some_and(Value::is_string) {
        anyhow::bail!("tool result text block must contain a string text field");
    }
    if kind == "tool_reference"
        && child
            .get("tool_name")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        anyhow::bail!("tool_reference block must contain a non-empty tool_name");
    }
    if matches!(kind, "image" | "document") && !child.get("source").is_some_and(Value::is_object) {
        anyhow::bail!("tool result {kind} block must contain an object source field");
    }
    Ok(())
}

fn read_tools(req: &MessagesRequest) -> Result<ToolPlan, anyhow::Error> {
    let Some(tools) = req.extra.get("tools") else {
        return Ok(ToolPlan::default());
    };
    let tools_arr = tools
        .as_array()
        .expect("tool contract validation guarantees an array");
    let forced_name = forced_tool_name(req);
    let mut initial = Vec::new();
    let mut deferred = HashMap::new();

    for tool in tools_arr {
        let tool_type = tool
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("tool contract validation guarantees a name")
            .to_string();

        // Codex has no equivalent for Anthropic's programmatic callers. Never
        // expose a tool directly when the request explicitly forbids direct
        // use; silently exposing it would widen permissions.
        if !tool_allows_direct_calls(tool, tool_type) {
            continue;
        }

        let translated = if is_supported_web_search_type(tool_type) {
            let mut filters = ResponsesWebSearchFilters {
                allowed_domains: None,
                blocked_domains: None,
            };
            if let Some(allowed) = read_string_array(tool, "allowed_domains")
                && !allowed.is_empty()
            {
                filters.allowed_domains = Some(allowed);
            }
            if let Some(blocked) = read_string_array(tool, "blocked_domains")
                && !blocked.is_empty()
            {
                filters.blocked_domains = Some(blocked);
            }
            let has_filters =
                filters.allowed_domains.is_some() || filters.blocked_domains.is_some();
            ResponsesTool::WebSearch(ResponsesWebSearchTool {
                kind: "web_search".to_string(),
                external_web_access: true,
                search_content_types: vec!["text".to_string(), "image".to_string()],
                filters: if has_filters { Some(filters) } else { None },
                user_location: read_web_search_user_location(tool),
            })
        } else {
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let description = codex_tool_description(&name, description);
            let parameters = codex_tool_parameters(&name, parameters);
            let strict_requested = tool.get("strict").and_then(Value::as_bool) == Some(true);
            if strict_requested && !strict_schema_is_supported(&parameters) {
                anyhow::bail!(
                    "tool {name} requested strict=true, but its input_schema cannot be represented by the Codex strict function schema; make every declared property required and set additionalProperties=false, or remove strict=true"
                );
            }
            let strict = strict_requested;
            ResponsesTool::Function(ResponsesFunctionTool {
                kind: "function".to_string(),
                name: name.clone(),
                description,
                parameters,
                strict,
            })
        };

        let wants_deferred = tool
            .get("defer_loading")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let forced = forced_name.is_some_and(|forced| forced == name);
        if wants_deferred && !forced {
            deferred.insert(name, translated);
        } else {
            initial.push(translated);
        }
    }

    Ok(ToolPlan {
        initial: (!initial.is_empty()).then_some(initial),
        deferred,
    })
}

fn forced_tool_name(req: &MessagesRequest) -> Option<&str> {
    let choice = req.extra.get("tool_choice")?.as_object()?;
    (choice.get("type")?.as_str()? == "tool")
        .then(|| choice.get("name").and_then(Value::as_str))
        .flatten()
}

fn tool_choice_forbids_tools(req: &MessagesRequest) -> bool {
    match req.extra.get("tool_choice") {
        Some(Value::String(mode)) => mode == "none",
        Some(Value::Object(choice)) => choice.get("type").and_then(Value::as_str) == Some("none"),
        _ => false,
    }
}

fn forced_tool_type(req: &MessagesRequest) -> Option<&str> {
    let forced = forced_tool_name(req)?;
    req.extra
        .get("tools")?
        .as_array()?
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(forced))
        .map(|tool| {
            tool.get("type")
                .and_then(Value::as_str)
                .unwrap_or("function")
        })
}

fn hosted_tool_guidance_allowed(req: &MessagesRequest) -> bool {
    !tool_choice_forbids_tools(req)
        && forced_tool_type(req).is_none_or(is_supported_web_search_type)
}

fn function_tool_guidance_allowed(req: &MessagesRequest) -> bool {
    !tool_choice_forbids_tools(req)
        && forced_tool_type(req).is_none_or(|tool_type| !is_supported_web_search_type(tool_type))
}

fn tool_allows_direct_calls(tool: &Value, tool_type: &str) -> bool {
    let direct_by_default = !matches!(tool_type, "web_search_20260209" | "web_search_20260318");
    match tool.get("allowed_callers") {
        Some(Value::Array(callers)) => callers
            .iter()
            .any(|caller| caller.as_str() == Some("direct")),
        Some(Value::Null) | None => direct_by_default,
        Some(_) => false,
    }
}

fn read_string_array(tool: &Value, field: &str) -> Option<Vec<String>> {
    tool.get(field).and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn read_web_search_user_location(tool: &Value) -> Option<ResponsesWebSearchUserLocation> {
    let location = tool.get("user_location")?.as_object()?;
    Some(ResponsesWebSearchUserLocation {
        r#type: "approximate".to_string(),
        city: location
            .get("city")
            .and_then(Value::as_str)
            .map(str::to_string),
        country: location
            .get("country")
            .and_then(Value::as_str)
            .map(str::to_string),
        region: location
            .get("region")
            .and_then(Value::as_str)
            .map(str::to_string),
        timezone: location
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn strict_schema_is_supported(schema: &Value) -> bool {
    match schema {
        Value::Array(values) => values.iter().all(strict_schema_is_supported),
        Value::Object(object) => {
            let is_object_schema = object.get("type").and_then(Value::as_str) == Some("object")
                || object.contains_key("properties");
            if is_object_schema {
                let Some(properties) = object.get("properties").and_then(Value::as_object) else {
                    return false;
                };
                if object.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
                    return false;
                }
                let Some(required) = object.get("required").and_then(Value::as_array) else {
                    return properties.is_empty();
                };
                let required: HashSet<&str> = required.iter().filter_map(Value::as_str).collect();
                if properties
                    .keys()
                    .any(|key| !required.contains(key.as_str()))
                {
                    return false;
                }
            }
            object.values().all(strict_schema_is_supported)
        }
        _ => true,
    }
}

fn codex_tool_description(name: &str, description: Option<String>) -> Option<String> {
    if name != "Read" {
        return description;
    }

    let base = description.unwrap_or_else(|| "Reads a file from the local filesystem.".to_string());
    Some(format!("{base}\n\n{}", read_offset_guidance()))
}

fn codex_tool_parameters(name: &str, mut parameters: Value) -> Value {
    if name != "Read" {
        return parameters;
    }

    let Some(props) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return parameters;
    };

    if let Some(offset) = props.get_mut("offset").and_then(Value::as_object_mut) {
        offset.insert(
            "description".to_string(),
            Value::String(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure.".to_string(),
            ),
        );
    }

    if let Some(limit) = props.get_mut("limit").and_then(Value::as_object_mut) {
        limit.insert(
            "description".to_string(),
            Value::String(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
                    .to_string(),
            ),
        );
    }

    parameters
}

fn map_tool_choice(req: &MessagesRequest) -> Result<Option<ResponsesToolChoice>, anyhow::Error> {
    let choice = match req.extra.get("tool_choice") {
        Some(Value::Object(m)) => m,
        Some(Value::String(s)) => {
            return Ok(Some(match s.as_str() {
                "auto" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
                "none" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None),
                "any" | "required" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
                _ => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
            }));
        }
        _ => return Ok(None),
    };

    let choice_type = choice
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    match choice_type {
        "auto" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Auto,
        ))),
        "none" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::None,
        ))),
        "any" | "required" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Required,
        ))),
        "tool" => {
            let name = choice.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let tools = req.extra.get("tools").and_then(|v| v.as_array());
            let is_web_search = tools.is_some_and(|t| {
                t.iter().any(|tool| {
                    tool.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(is_supported_web_search_type)
                        && tool.get("name").and_then(|v| v.as_str()) == Some(name)
                })
            });
            if is_web_search {
                Ok(Some(ResponsesToolChoice::AllowedTools {
                    r#type: "allowed_tools".to_string(),
                    mode: "required".to_string(),
                    tools: vec![serde_json::json!({"type": "web_search"})],
                }))
            } else {
                Ok(Some(ResponsesToolChoice::Function {
                    r#type: "function".to_string(),
                    name: name.to_string(),
                }))
            }
        }
        _ => Ok(None),
    }
}

fn parallel_tool_calls_enabled(req: &MessagesRequest) -> bool {
    match req.extra.get("tool_choice") {
        Some(Value::String(mode)) => mode != "none",
        Some(Value::Object(choice)) => {
            choice.get("type").and_then(Value::as_str) != Some("none")
                && !choice
                    .get("disable_parallel_tool_use")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
        _ => true,
    }
}

enum CodexInputBlock {
    Native(ContentBlock),
    PreservedText(String),
}

fn build_input(
    req: &MessagesRequest,
    deferred_tools: &HashMap<String, ResponsesTool>,
) -> Vec<ResponsesInputItem> {
    let mut out: Vec<ResponsesInputItem> = Vec::new();
    let mut read_tool_uses_with_offset = HashSet::new();
    let mut loaded_deferred_tools = HashSet::new();

    for msg in &req.messages {
        let blocks = codex_input_blocks(&msg.content);
        match msg.role.as_str() {
            "user" => {
                let mut parts: Vec<ResponsesContentPart> = Vec::new();
                for block in &blocks {
                    match block {
                        CodexInputBlock::Native(ContentBlock::Text { text })
                        | CodexInputBlock::PreservedText(text) => {
                            parts.push(ResponsesContentPart::InputText { text: text.clone() });
                        }
                        CodexInputBlock::Native(ContentBlock::Image { source }) => {
                            parts.push(ResponsesContentPart::InputImage {
                                image_url: image_source_to_url(source),
                                detail: None,
                            });
                        }
                        CodexInputBlock::Native(ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        }) => {
                            if !parts.is_empty() {
                                out.push(ResponsesInputItem::Message {
                                    role: "user".to_string(),
                                    content: std::mem::take(&mut parts),
                                });
                            }
                            let mut output = tool_result_to_output(content);
                            if is_error.unwrap_or(false) {
                                prepend_tool_result_text(&mut output, "[tool execution error]");
                            }
                            let output = annotate_tool_result_output(
                                output,
                                tool_use_id,
                                read_tool_uses_with_offset.contains(tool_use_id),
                                is_error.unwrap_or(false),
                            );
                            out.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: tool_use_id.clone(),
                                output,
                            });
                            let additions = deferred_tools_for_result(
                                content,
                                deferred_tools,
                                &mut loaded_deferred_tools,
                            );
                            if !additions.is_empty() {
                                out.push(ResponsesInputItem::AdditionalTools {
                                    id: None,
                                    role: "developer".to_string(),
                                    tools: additions,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: parts,
                    });
                }
            }
            "system" => {
                let parts: Vec<ResponsesContentPart> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        CodexInputBlock::Native(ContentBlock::Text { text })
                        | CodexInputBlock::PreservedText(text) => {
                            Some(ResponsesContentPart::InputText { text: text.clone() })
                        }
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "developer".to_string(),
                        content: parts,
                    });
                }
            }
            _ => {
                let mut text_parts: Vec<ResponsesContentPart> = Vec::new();
                let flush_text =
                    |out: &mut Vec<ResponsesInputItem>,
                     text_parts: &mut Vec<ResponsesContentPart>| {
                        if !text_parts.is_empty() {
                            out.push(ResponsesInputItem::Message {
                                role: "assistant".to_string(),
                                content: std::mem::take(text_parts),
                            });
                        }
                    };
                for block in &blocks {
                    match block {
                        CodexInputBlock::Native(ContentBlock::Text { text })
                        | CodexInputBlock::PreservedText(text) => {
                            text_parts
                                .push(ResponsesContentPart::OutputText { text: text.clone() });
                        }
                        CodexInputBlock::Native(ContentBlock::ToolUse { id, name, input }) => {
                            flush_text(&mut out, &mut text_parts);
                            if is_read_tool_use_with_offset(name, input) {
                                read_tool_uses_with_offset.insert(id.clone());
                            }
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            out.push(ResponsesInputItem::FunctionCall {
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: args,
                            });
                        }
                        CodexInputBlock::Native(ContentBlock::Thinking { signature, .. }) => {
                            let Some(replay) =
                                signature.as_deref().and_then(decode_reasoning_signature)
                            else {
                                continue;
                            };
                            flush_text(&mut out, &mut text_parts);
                            out.push(ResponsesInputItem::Reasoning {
                                id: replay.id,
                                summary: Vec::new(),
                                encrypted_content: replay.encrypted_content,
                            });
                        }
                        _ => {}
                    }
                }
                flush_text(&mut out, &mut text_parts);
            }
        }
    }

    out
}

fn codex_input_blocks(content: &Value) -> Vec<CodexInputBlock> {
    match content {
        Value::String(_) => normalize_content(content, Value::Null)
            .into_iter()
            .map(CodexInputBlock::Native)
            .collect(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                let normalized = normalize_content(&Value::Array(vec![block.clone()]), Value::Null);
                if let Some(block) = normalized.into_iter().next() {
                    return CodexInputBlock::Native(block);
                }
                // Future or provider-specific Anthropic blocks must remain visible
                // to the model instead of disappearing through filter_map.
                CodexInputBlock::PreservedText(preserved_anthropic_block_text(block))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn preserved_anthropic_block_text(block: &Value) -> String {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let encoded = serde_json::to_string(block).unwrap_or_else(|_| "null".to_string());
    format!("[Anthropic {kind} block]\n{encoded}")
}

fn deferred_tools_for_result(
    content: &Value,
    deferred_tools: &HashMap<String, ResponsesTool>,
    loaded: &mut HashSet<String>,
) -> Vec<Value> {
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_reference"))
        .filter_map(|block| block.get("tool_name").and_then(Value::as_str))
        .filter_map(|name| {
            if loaded.contains(name) {
                return None;
            }
            let tool = deferred_tools.get(name)?;
            let encoded = serde_json::to_value(tool).ok()?;
            loaded.insert(name.to_string());
            Some(encoded)
        })
        .collect()
}

fn is_read_tool_use_with_offset(name: &str, input: &Value) -> bool {
    name == "Read" && input.get("offset").is_some()
}

fn maybe_append_rewritten_read_offset_note(output: String, tool_use_id: &str) -> String {
    if output.contains("Proxy Read offset note:") {
        return output;
    }
    let Some(rewrite) = read_offset_rewrite(tool_use_id) else {
        return output;
    };
    format!("{output}\n\n{}", read_offset_rewrite_note(&rewrite))
}

fn read_offset_rewrite_note(rewrite: &ReadOffsetRewrite) -> String {
    let file = rewrite
        .file_path
        .as_deref()
        .map(|path| format!(" for {path}"))
        .unwrap_or_default();
    format!(
        "Proxy Read offset note:\n\
         - Requested Read offset {}{} exceeds the proxy rewrite threshold of 1000000.\n\
         - This Read starts at the beginning of the file.\n\
         - For continuation reads, use offset after a prior Read of the same file returned content and more lines are needed.\n\
         - Compute offset as prior offset plus the number of lines returned by that prior Read.",
        rewrite.offset, file
    )
}

fn maybe_append_read_offset_guidance(
    output: String,
    read_call_had_offset: bool,
    is_error: bool,
) -> String {
    if !read_call_had_offset
        || output.contains("Codex Read guidance:")
        || !looks_like_read_offset_result(&output)
        || (!is_error && !looks_like_read_offset_warning(&output))
    {
        return output;
    }
    format!("{output}\n\n{}", read_offset_guidance())
}

fn looks_like_read_offset_result(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("offset")
        && (lower.contains("file has")
            || lower.contains("out of range")
            || (lower.contains("line") && lower.contains("requested")))
}

fn looks_like_read_offset_warning(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("warning") || lower.contains("system-reminder")
}

fn read_offset_guidance() -> &'static str {
    "Codex Read guidance:\n\
     - offset is an optional zero based continuation index, not a line number lookup.\n\
     - Use offset only after a prior Read of the same file returned content and more lines are needed.\n\
     - Compute offset as prior offset plus the number of lines returned by that prior Read.\n\
     - Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets.\n\
     - Omit offset and limit when opening a file or when unsure."
}

// ---------------------------------------------------------------------------
// Tool result rendering
// ---------------------------------------------------------------------------

fn tool_result_to_output(content: &Value) -> ResponsesFunctionCallOutput {
    match content {
        Value::String(s) => ResponsesFunctionCallOutput::Text(s.clone()),
        Value::Array(arr) => {
            let mut text_parts = Vec::new();
            let mut content_parts = Vec::new();
            let mut has_image = false;
            for b in arr {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("text") => match b.get("text").and_then(|v| v.as_str()) {
                        Some(text) => {
                            text_parts.push(text.to_string());
                            content_parts.push(ResponsesFunctionCallOutputContent::InputText {
                                text: text.to_string(),
                            });
                        }
                        None => push_unsupported_tool_result_block(
                            b,
                            &mut text_parts,
                            &mut content_parts,
                        ),
                    },
                    Some("image") => {
                        if let Some(image_url) = tool_result_image_url(b) {
                            has_image = true;
                            content_parts.push(ResponsesFunctionCallOutputContent::InputImage {
                                image_url,
                                detail: None,
                            });
                        } else {
                            push_unsupported_tool_result_block(
                                b,
                                &mut text_parts,
                                &mut content_parts,
                            );
                        }
                    }
                    Some("tool_reference") => {
                        let text = b
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .map(|name| format!("[tool reference: {name}]"))
                            .unwrap_or_else(|| unsupported_tool_result_block_to_string(b));
                        text_parts.push(text.clone());
                        content_parts.push(ResponsesFunctionCallOutputContent::InputText { text });
                    }
                    Some("document" | "search_result") => {
                        push_preserved_tool_result_block(b, &mut text_parts, &mut content_parts)
                    }
                    Some(_) => {
                        push_preserved_tool_result_block(b, &mut text_parts, &mut content_parts)
                    }
                    None => {
                        push_unsupported_tool_result_block(b, &mut text_parts, &mut content_parts)
                    }
                }
            }
            if has_image {
                ResponsesFunctionCallOutput::Content(content_parts)
            } else {
                ResponsesFunctionCallOutput::Text(text_parts.join("\n"))
            }
        }
        _ => ResponsesFunctionCallOutput::Text(String::new()),
    }
}

fn tool_result_image_url(block: &Value) -> Option<String> {
    let source = block.get("source")?.as_object()?;
    match source.get("type")?.as_str()? {
        "url" => source
            .get("url")?
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        "base64" => {
            let media_type = source
                .get("media_type")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            let data = source
                .get("data")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        _ => None,
    }
}

fn push_unsupported_tool_result_block(
    block: &Value,
    text_parts: &mut Vec<String>,
    content_parts: &mut Vec<ResponsesFunctionCallOutputContent>,
) {
    push_preserved_tool_result_block(block, text_parts, content_parts);
}

fn push_preserved_tool_result_block(
    block: &Value,
    text_parts: &mut Vec<String>,
    content_parts: &mut Vec<ResponsesFunctionCallOutputContent>,
) {
    let text = unsupported_tool_result_block_to_string(block);
    text_parts.push(text.clone());
    content_parts.push(ResponsesFunctionCallOutputContent::InputText { text });
}

fn prepend_tool_result_text(output: &mut ResponsesFunctionCallOutput, prefix: &str) {
    match output {
        ResponsesFunctionCallOutput::Text(text) => {
            if text.is_empty() {
                *text = prefix.to_string();
            } else {
                text.insert(0, '\n');
                text.insert_str(0, prefix);
            }
        }
        ResponsesFunctionCallOutput::Content(parts) => {
            parts.insert(
                0,
                ResponsesFunctionCallOutputContent::InputText {
                    text: prefix.to_string(),
                },
            );
        }
    }
}

fn annotate_tool_result_output(
    mut output: ResponsesFunctionCallOutput,
    tool_use_id: &str,
    read_call_had_offset: bool,
    is_error: bool,
) -> ResponsesFunctionCallOutput {
    let text = tool_result_text(&output);
    let annotated = maybe_append_rewritten_read_offset_note(text.clone(), tool_use_id);
    let annotated = maybe_append_read_offset_guidance(annotated, read_call_had_offset, is_error);
    if annotated == text {
        return output;
    }

    match &mut output {
        ResponsesFunctionCallOutput::Text(value) => *value = annotated,
        ResponsesFunctionCallOutput::Content(parts) => {
            let annotation = annotated
                .strip_prefix(&text)
                .unwrap_or(&annotated)
                .trim_start_matches('\n');
            if !annotation.is_empty() {
                parts.push(ResponsesFunctionCallOutputContent::InputText {
                    text: annotation.to_string(),
                });
            }
        }
    }
    output
}

fn tool_result_text(output: &ResponsesFunctionCallOutput) -> String {
    match output {
        ResponsesFunctionCallOutput::Text(text) => text.clone(),
        ResponsesFunctionCallOutput::Content(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ResponsesFunctionCallOutputContent::InputText { text } => Some(text.as_str()),
                ResponsesFunctionCallOutputContent::InputImage { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn unsupported_tool_result_block_to_string(block: &Value) -> String {
    let kind = block
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let encoded = serde_json::to_string(block).unwrap_or_else(|_| "null".to_string());
    format!("[Anthropic {kind} tool result block]\n{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VALID_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAGklEQVR4nO3BAQEAAACCIP+vbkhAAQAAAO8GECAAARlDNO4AAAAASUVORK5CYII=";

    fn valid_png_bytes() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(VALID_PNG_BASE64)
            .unwrap()
    }

    fn valid_jpeg_bytes() -> Vec<u8> {
        let mut image = Vec::new();
        jpeg_encoder::Encoder::new(&mut image, 90)
            .encode(&vec![0; 32 * 32], 32, 32, jpeg_encoder::ColorType::Luma)
            .unwrap();
        image
    }

    fn valid_gif_bytes(frame_count: usize) -> Vec<u8> {
        let mut image = Vec::new();
        {
            let mut encoder =
                gif::Encoder::new(&mut image, 32, 32, &[0, 0, 0, 255, 255, 255]).unwrap();
            for index in 0..frame_count {
                let frame =
                    gif::Frame::from_indexed_pixels(32, 32, vec![(index & 1) as u8; 32 * 32], None);
                encoder.write_frame(&frame).unwrap();
            }
        }
        image
    }

    fn valid_webp_bytes() -> Vec<u8> {
        let mut image = Vec::new();
        image_webp::WebPEncoder::new(&mut image)
            .encode(&vec![0; 32 * 32 * 3], 32, 32, image_webp::ColorType::Rgb8)
            .unwrap();
        image
    }

    fn base64_image(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn webp_with_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let padded_size = data.len() + (data.len() & 1);
        let mut image = Vec::with_capacity(20 + padded_size);
        image.extend_from_slice(b"RIFF");
        image.extend_from_slice(&u32::try_from(12 + padded_size).unwrap().to_le_bytes());
        image.extend_from_slice(b"WEBP");
        image.extend_from_slice(chunk_type);
        image.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        image.extend_from_slice(data);
        if data.len() & 1 == 1 {
            image.push(0);
        }
        image
    }

    // Translation tests exercise protocol semantics only. Runtime configuration
    // is injected explicitly by the provider and covered by dedicated tests.
    fn translate_request(
        req: &MessagesRequest,
        opts: TranslateOptions,
    ) -> Result<ResponsesRequest, anyhow::Error> {
        translate_request_with_overrides(req, opts, TranslationOverrides::default())
    }

    fn opts() -> TranslateOptions {
        TranslateOptions {
            session_id: None,
            service_tier: None,
            model: "gpt-5.5".to_string(),
            use_responses_lite: false,
        }
    }

    #[test]
    fn translate_web_search_tool_to_codex_tool() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: Some("s".into()),
                service_tier: None,
                model: "gpt-5.5".to_string(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert_eq!(out.prompt_cache_key.as_deref(), Some("s"));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
        let tool_choice = serde_json::to_value(out.tool_choice.as_ref().unwrap()).unwrap();
        assert_eq!(tool_choice["type"], "allowed_tools");
        assert_eq!(tool_choice["mode"], "required");
        assert_eq!(tool_choice["tools"], json!([{"type":"web_search"}]));
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        assert_eq!(
            tool.filters.as_ref().unwrap().allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert!(out.instructions.is_none());
        assert_eq!(
            out.include.as_ref().unwrap(),
            &vec![
                "reasoning.encrypted_content".to_string(),
                "web_search_call.action.sources".to_string()
            ]
        );
    }

    #[test]
    fn all_current_web_search_versions_use_hosted_tool_and_location() {
        for version in [
            "web_search_20250305",
            "web_search_20260209",
            "web_search_20260318",
        ] {
            let mut body = json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"find it"}],
                "tools":[{
                    "type":version,
                    "name":"web_search",
                    "allowed_callers":["direct"],
                    "max_uses":3,
                    "user_location":{
                        "type":"approximate",
                        "city":"Nanjing",
                        "country":"CN",
                        "timezone":"Asia/Shanghai"
                    }
                }]
            });
            if version == "web_search_20260318" {
                body["tools"][0]["response_inclusion"] = json!("excluded");
            }
            let req: MessagesRequest = serde_json::from_value(body).unwrap();
            let out = translate_request(&req, opts()).unwrap();
            assert_eq!(out.hosted_web_search_max_uses, Some(3));
            assert!(
                !serde_json::to_value(&out)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("hosted_web_search_max_uses")
            );
            let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
                panic!("expected hosted web search for {version}");
            };
            assert_eq!(
                tool.user_location,
                Some(ResponsesWebSearchUserLocation {
                    r#type: "approximate".to_string(),
                    city: Some("Nanjing".to_string()),
                    country: Some("CN".to_string()),
                    region: None,
                    timezone: Some("Asia/Shanghai".to_string()),
                })
            );
            assert!(has_hosted_web_search(&req));
            assert!(
                out.instructions
                    .as_deref()
                    .unwrap()
                    .contains("Best-effort compatibility limit")
            );
            assert!(
                !serde_json::to_value(tool)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("response_inclusion")
            );
        }
    }

    #[test]
    fn web_search_default_callers_follow_version_semantics() {
        let legacy: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"find it"}],
            "tools":[{"type":"web_search_20250305", "name":"web_search"}]
        }))
        .unwrap();
        let legacy_out = translate_request(&legacy, opts()).unwrap();
        assert!(legacy_out.tools.as_ref().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        }));

        for version in ["web_search_20260209", "web_search_20260318"] {
            let implicit_code_only: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"find it"}],
                "tools":[{"type":version, "name":"web_search"}]
            }))
            .unwrap();
            let out = translate_request(&implicit_code_only, opts()).unwrap();
            assert!(out.tools.is_none(), "{version} must not default to direct");
            assert!(!has_hosted_web_search(&implicit_code_only));

            let explicit_direct: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"find it"}],
                "tools":[{
                    "type":version,
                    "name":"web_search",
                    "allowed_callers":["direct"]
                }]
            }))
            .unwrap();
            let out = translate_request(&explicit_direct, opts()).unwrap();
            assert!(out.tools.as_ref().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
            }));
            assert!(has_hosted_web_search(&explicit_direct));
        }
    }

    #[test]
    fn nullable_anthropic_tool_fields_follow_omitted_field_defaults() {
        let custom: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[{
                "type":null,
                "name":"NullableCustom",
                "input_schema":{"type":"object"},
                "defer_loading":null,
                "strict":null,
                "eager_input_streaming":null,
                "allowed_callers":null
            }]
        }))
        .unwrap();
        let custom_out = translate_request(&custom, opts()).unwrap();
        let Some(ResponsesTool::Function(tool)) = custom_out.tools.as_ref().unwrap().first() else {
            panic!("nullable custom tool type must default to a function tool");
        };
        assert_eq!(tool.name, "NullableCustom");
        assert!(!tool.strict);

        for (version, expected_direct) in [
            ("web_search_20250305", true),
            ("web_search_20260209", false),
            ("web_search_20260318", false),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"find it"}],
                "tools":[{
                    "type":version,
                    "name":"web_search",
                    "allowed_callers":null,
                    "allowed_domains":null,
                    "blocked_domains":null,
                    "max_uses":null,
                    "user_location":null,
                    "response_inclusion":null
                }]
            }))
            .unwrap();
            let out = translate_request(&req, opts()).unwrap();
            assert_eq!(out.tools.is_some(), expected_direct, "{version}");
            assert_eq!(has_hosted_web_search(&req), expected_direct, "{version}");
            if expected_direct {
                let Some(ResponsesTool::WebSearch(tool)) = out.tools.as_ref().unwrap().first()
                else {
                    panic!("expected hosted web search for {version}");
                };
                assert!(tool.filters.is_none());
                assert!(tool.user_location.is_none());
            }
        }

        let current_direct: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"find it"}],
            "tools":[{
                "type":"web_search_20260318",
                "name":"web_search",
                "allowed_callers":["direct"],
                "allowed_domains":null,
                "blocked_domains":null,
                "max_uses":null,
                "user_location":null,
                "response_inclusion":null
            }]
        }))
        .unwrap();
        assert!(translate_request(&current_direct, opts()).is_ok());
    }

    #[test]
    fn rejects_unknown_web_search_version_and_conflicting_domains() {
        let unknown: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"find it"}],
            "tools":[{"type":"web_search_20990101", "name":"web_search"}]
        }))
        .unwrap();
        assert!(
            translate_request(&unknown, opts())
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );

        let conflicting: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"find it"}],
            "tools":[{
                "type":"web_search_20260318",
                "name":"web_search",
                "allowed_domains":["example.com"],
                "blocked_domains":["spam.example"]
            }]
        }))
        .unwrap();
        assert!(
            translate_request(&conflicting, opts())
                .unwrap_err()
                .to_string()
                .contains("cannot specify both")
        );

        for (version, inclusion, expected) in [
            (
                "web_search_20260209",
                "full",
                "requires web_search_20260318",
            ),
            (
                "web_search_20260318",
                "citations",
                "must be full or excluded",
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user", "content":"find it"}],
                "tools":[{
                    "type":version,
                    "name":"web_search",
                    "allowed_callers":["direct"],
                    "response_inclusion":inclusion
                }]
            }))
            .unwrap();
            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn automatic_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"auto"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        let filters = tool.filters.as_ref().unwrap();
        assert!(filters.allowed_domains.is_none());
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn simple_tool_choices_serialize_as_responses_modes() {
        for (anthropic, expected) in [("auto", "auto"), ("none", "none"), ("any", "required")] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"use a tool"}],
                "tools": [{"name":"test", "input_schema":{"type":"object"}}],
                "tool_choice": {"type": anthropic}
            }))
            .unwrap();
            let out = translate_request(&req, opts()).unwrap();
            let value = serde_json::to_value(out).unwrap();
            assert_eq!(value["tool_choice"], expected);
            assert_eq!(
                value["parallel_tool_calls"],
                Value::Bool(anthropic != "none")
            );
        }
    }

    #[test]
    fn disable_parallel_tool_use_is_forwarded_on_full_lane() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"use a tool"}],
            "tools": [{"name":"test", "input_schema":{"type":"object"}}],
            "tool_choice": {"type":"auto", "disable_parallel_tool_use":true}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(!out.parallel_tool_calls);
        assert!(out.instructions.is_none());
    }

    #[test]
    fn full_lane_guides_single_tool_parallel_function_calls() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "system": "Follow repository rules.",
            "messages": [{"role":"user", "content":"inspect both"}],
            "tools": [{"name":"Read", "input_schema":{"type":"object"}}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert!(out.parallel_tool_calls);
        let instructions = out.instructions.unwrap();
        assert!(instructions.starts_with("Follow repository rules."));
        assert!(instructions.contains("multiple independent calls to the same tool"));
        assert!(instructions.contains("overlapping side effects"));
    }

    #[test]
    fn forced_function_keeps_same_tool_parallel_guidance_unless_disabled() {
        for (disabled, expect_parallel) in [(false, true), (true, false)] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"read both"}],
                "tools": [{"name":"Read", "input_schema":{"type":"object"}}],
                "tool_choice": {
                    "type":"tool",
                    "name":"Read",
                    "disable_parallel_tool_use":disabled
                }
            }))
            .unwrap();

            let out = translate_request(&req, opts()).unwrap();
            assert_eq!(out.parallel_tool_calls, expect_parallel);
            assert_eq!(out.instructions.is_some(), expect_parallel);
        }
    }

    #[test]
    fn tool_choice_none_suppresses_parallel_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "system": "Answer directly.",
            "messages": [{"role":"user", "content":"do not use tools"}],
            "tools": [{"name":"Read", "input_schema":{"type":"object"}}],
            "tool_choice": {"type":"none"}
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert!(!out.parallel_tool_calls);
        assert_eq!(out.instructions.as_deref(), Some("Answer directly."));
    }

    #[test]
    fn generated_tool_guidance_is_scoped_to_the_forced_choice() {
        for (choice, expect_parallel, expect_hosted) in [
            (json!({"type":"none"}), false, false),
            (json!({"type":"tool", "name":"web_search"}), false, true),
            (json!({"type":"tool", "name":"Read"}), true, false),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.5",
                "system":"Follow repository rules.",
                "messages":[{"role":"user", "content":"inspect"}],
                "tools":[
                    {"name":"Read", "input_schema":{"type":"object"}},
                    {
                        "type":"web_search_20250305",
                        "name":"web_search",
                        "max_uses":2
                    }
                ],
                "tool_choice":choice
            }))
            .unwrap();

            let out = translate_request(&req, opts()).unwrap();
            let instructions = out.instructions.as_deref().unwrap();
            assert_eq!(
                instructions.contains("multiple independent calls to the same tool"),
                expect_parallel,
                "{instructions}"
            );
            assert_eq!(
                instructions.contains("Best-effort compatibility limit"),
                expect_hosted,
                "{instructions}"
            );
        }
    }

    #[test]
    fn forced_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "system": "Be brief.",
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["a.example", "b.example"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["a.example".to_string(), "b.example".to_string()][..])
        );
        assert!(filters.blocked_domains.is_none());
        assert_eq!(out.instructions.as_deref(), Some("Be brief."));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn unfiltered_web_search_adds_no_domain_instructions() {
        for tool_choice in [None, Some(json!({"type":"tool", "name":"web_search"}))] {
            let mut body = json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"find it"}],
                "tools": [{"type":"web_search_20250305", "name":"web_search"}]
            });
            if let Some(tool_choice) = tool_choice {
                body["tool_choice"] = tool_choice;
            }
            let req: MessagesRequest = serde_json::from_value(body).unwrap();
            let out = translate_request(&req, opts()).unwrap();
            let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
                panic!("expected web_search tool");
            };
            assert!(tool.external_web_access);
            assert!(tool.filters.is_none());
            assert!(out.instructions.is_none());
        }
    }

    #[test]
    fn has_hosted_web_search_detects_web_search_tool() {
        let with: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [
                {"name":"Bash", "input_schema":{}},
                {"type":"web_search_20250305", "name":"web_search"}
            ]
        }))
        .unwrap();
        assert!(has_hosted_web_search(&with));

        let without: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"run it"}],
            "tools": [{"name":"Bash", "input_schema":{}}]
        }))
        .unwrap();
        assert!(!has_hosted_web_search(&without));
    }

    #[test]
    fn deferred_hosted_web_search_is_effective_only_when_forced_or_loaded() {
        let tools = json!([
            {"name":"ToolSearch", "input_schema":{"type":"object"}},
            {
                "type":"web_search_20260318",
                "name":"web_search",
                "defer_loading":true,
                "allowed_callers":["direct"]
            }
        ]);
        let unreferenced: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"find it"}],
            "tools":tools
        }))
        .unwrap();
        assert!(!has_hosted_web_search(&unreferenced));

        let forced: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"find it"}],
            "tools":tools,
            "tool_choice":{"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        assert!(has_hosted_web_search(&forced));

        let loaded: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"search_1", "name":"ToolSearch", "input":{}
                }]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"search_1", "content":[{
                        "type":"tool_reference", "tool_name":"web_search"
                    }]
                }]}
            ],
            "tools":tools
        }))
        .unwrap();
        assert!(has_hosted_web_search(&loaded));
    }

    #[test]
    fn responses_lite_downgrades_unregistered_web_search_tool_choice() {
        // On the lite lane tools travel in the AdditionalTools developer
        // prefix, so a top-level web_search tool_choice would reference a
        // tool upstream doesn't know about and 502.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
            },
        )
        .unwrap();
        assert!(out.tools.is_none());
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto))
        ));
    }

    #[test]
    fn full_lane_keeps_web_search_tool_choice_registered() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert!(out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        }));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn deferred_tools_load_only_after_historical_tool_reference() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"search_1", "name":"ToolSearch",
                    "input":{"query":"deferred"}
                }]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"search_1",
                    "content":[
                        {"type":"text", "text":"one match"},
                        {"type":"tool_reference", "tool_name":"DeferredTool"}
                    ]
                }]}
            ],
            "tools":[
                {"name":"ToolSearch", "input_schema":{"type":"object"}},
                {"name":"EagerTool", "input_schema":{"type":"object"}},
                {"name":"DeferredTool", "defer_loading":true,
                 "input_schema":{"type":"object", "properties":{"value":{"type":"string"}}}}
            ]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        let initial_names: Vec<&str> = out
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|tool| match tool {
                ResponsesTool::Function(tool) => Some(tool.name.as_str()),
                ResponsesTool::WebSearch(_) => None,
            })
            .collect();
        assert_eq!(initial_names, ["ToolSearch", "EagerTool"]);

        let additions: Vec<&Vec<Value>> = out
            .input
            .iter()
            .filter_map(|item| match item {
                ResponsesInputItem::AdditionalTools { tools, .. } => Some(tools),
                _ => None,
            })
            .collect();
        assert_eq!(additions.len(), 1);
        assert_eq!(additions[0][0]["name"], "DeferredTool");
    }

    #[test]
    fn rejects_tool_references_without_valid_search_provenance() {
        let cases = [
            (
                "non-search source",
                "Read",
                json!([
                    {"name":"Read", "input_schema":{"type":"object"}},
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                json!([{"type":"tool_reference", "tool_name":"Bash"}]),
                "not a declared, non-deferred Claude Code client ToolSearch tool",
            ),
            (
                "undeclared source",
                "ToolSearch",
                json!([
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                json!([{"type":"tool_reference", "tool_name":"Bash"}]),
                "not a declared Claude Code client ToolSearch tool",
            ),
            (
                "unknown reference",
                "ToolSearch",
                json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}}
                ]),
                json!([{"type":"tool_reference", "tool_name":"Missing"}]),
                "not found in available tools",
            ),
            (
                "eager reference",
                "ToolSearch",
                json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}},
                    {"name":"Bash", "input_schema":{"type":"object"}}
                ]),
                json!([{"type":"tool_reference", "tool_name":"Bash"}]),
                "defer_loading=true",
            ),
            (
                "duplicate reference",
                "ToolSearch",
                json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}},
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                json!([
                    {"type":"tool_reference", "tool_name":"Bash"},
                    {"type":"tool_reference", "tool_name":"Bash"}
                ]),
                "duplicate tool_reference",
            ),
            (
                "mixed valid and invalid references",
                "ToolSearch",
                json!([
                    {"name":"ToolSearch", "input_schema":{"type":"object"}},
                    {"name":"Bash", "defer_loading":true, "input_schema":{"type":"object"}}
                ]),
                json!([
                    {"type":"tool_reference", "tool_name":"Bash"},
                    {"type":"tool_reference", "tool_name":"Missing"}
                ]),
                "not found in available tools",
            ),
        ];

        for (case, caller, tools, references, expected) in cases {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
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

            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{case}: expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn referenced_deferred_web_search_requests_per_call_sources() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"search_1", "name":"ToolSearch", "input":{"query":"web"}
                }]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"search_1", "content":[{
                        "type":"tool_reference", "tool_name":"web_search"
                    }]
                }]}
            ],
            "tools":[
                {"name":"ToolSearch", "input_schema":{"type":"object"}},
                {"type":"web_search_20260318", "name":"web_search", "defer_loading":true,
                 "allowed_callers":["direct"]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(
            out.include
                .as_ref()
                .unwrap()
                .iter()
                .any(|value| { value == "web_search_call.action.sources" })
        );
        assert!(out.input.iter().any(|item| {
            matches!(
                item,
                ResponsesInputItem::AdditionalTools { tools, .. }
                    if tools.iter().any(|tool| tool["type"] == "web_search")
            )
        }));
    }

    #[test]
    fn deferred_tool_is_kept_only_when_forced_without_a_reference() {
        let forced: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[
                {"name":"ToolSearch", "input_schema":{"type":"object"}},
                {"name":"DeferredTool", "defer_loading":true, "input_schema":{"type":"object"}}
            ],
            "tool_choice":{"type":"tool", "name":"DeferredTool"}
        }))
        .unwrap();
        let out = translate_request(&forced, opts()).unwrap();
        assert!(out.tools.as_ref().unwrap().iter().any(|tool| {
            matches!(tool, ResponsesTool::Function(function) if function.name == "DeferredTool")
        }));

        let no_search: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[{
                "name":"DeferredTool", "defer_loading":true, "input_schema":{"type":"object"}
            }]
        }))
        .unwrap();
        let out = translate_request(&no_search, opts()).unwrap();
        assert!(out.tools.is_none());
        assert!(
            !serde_json::to_string(&out)
                .unwrap()
                .contains("DeferredTool")
        );
    }

    #[test]
    fn translate_read_tool_adds_codex_offset_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"read it"}],
            "tools": [{
                "name": "Read",
                "description": "Reads a file from the local filesystem.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "offset": {"type": "integer", "description": "old offset"},
                        "limit": {"type": "integer", "description": "old limit"}
                    },
                    "required": ["file_path"]
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        let description = tool.description.as_deref().unwrap();
        assert!(description.contains("Codex Read guidance"));
        assert!(description.contains("zero based continuation index"));
        assert!(description.contains("guessed positions are invalid offsets"));

        let props = tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            props
                .get("offset")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure."
            )
        );
        assert_eq!(
            props
                .get("limit")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
            )
        );
    }

    #[test]
    fn translate_non_read_tool_preserves_tool_metadata() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"search"}],
            "tools": [{
                "name": "Search",
                "description": "Find matching records.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "offset": {"type": "integer", "description": "record offset"}
                    }
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        assert_eq!(tool.description.as_deref(), Some("Find matching records."));
        assert!(!tool.strict);
        assert_eq!(
            serde_json::to_value(tool).unwrap()["strict"],
            Value::Bool(false)
        );
        assert_eq!(
            tool.parameters
                .get("properties")
                .and_then(|v| v.get("offset"))
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some("record offset")
        );
    }

    #[test]
    fn explicit_strict_rejects_incompatible_schema_instead_of_downgrading() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[
                {
                    "name":"OptionalField",
                    "strict":true,
                    "input_schema":{
                        "type":"object",
                        "properties":{"value":{"type":"string"}},
                        "additionalProperties":false
                    }
                }
            ]
        }))
        .unwrap();
        let error = translate_request(&req, opts()).unwrap_err().to_string();
        assert!(error.contains("tool OptionalField requested strict=true"));
        assert!(error.contains("make every declared property required"));
    }

    #[test]
    fn explicit_strict_is_preserved_only_for_compatible_schema() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[
                {
                    "name":"Compatible",
                    "strict":true,
                    "input_schema":{
                        "type":"object",
                        "properties":{"value":{"type":"string"}},
                        "required":["value"],
                        "additionalProperties":false
                    }
                },
                {
                    "name":"NotRequested",
                    "strict":false,
                    "input_schema":{
                        "type":"object",
                        "properties":{"value":{"type":"string"}},
                        "additionalProperties":false
                    }
                }
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let strict_values: Vec<bool> = out
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|tool| match tool {
                ResponsesTool::Function(tool) => tool.strict,
                ResponsesTool::WebSearch(_) => false,
            })
            .collect();
        assert_eq!(strict_values, [true, false]);
        let ResponsesTool::Function(optional) = &out.tools.as_ref().unwrap()[1] else {
            panic!("expected function tool");
        };
        assert!(optional.parameters.get("required").is_none());
        assert_eq!(optional.parameters["additionalProperties"], false);
    }

    #[test]
    fn translate_requests_encrypted_reasoning_without_forcing_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(out.reasoning.is_none());
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_includes_reasoning_when_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(matches!(reasoning.effort, Some(Effort::Medium)));
        assert_eq!(reasoning.summary.as_deref(), Some("auto"));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_effort_max_maps_to_max() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "max"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Max)));
    }

    #[test]
    fn translate_effort_ultra_is_rejected() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "ultra"}
        }))
        .unwrap();
        assert_eq!(
            translate_request(&req, opts()).unwrap_err().to_string(),
            "Invalid output_config.effort: ultra"
        );
    }

    #[test]
    fn claude_code_compaction_caps_effort_at_medium() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role":"user", "content":"Earlier task"},
                {"role":"assistant", "content":"Earlier response"},
                {"role":"user", "content":[{
                    "type":"text",
                    "text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\nYour entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far."
                }]}
            ],
            "output_config": {"effort": "max"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(
            out.reasoning.unwrap().effort,
            Some(Effort::Medium)
        ));
    }

    #[test]
    fn translate_effort_override_max_maps_to_max() {
        let effort = resolve_effort_override(Some(Effort::Low), Some("max")).unwrap();
        assert!(matches!(effort, Some(Effort::Max)));
    }

    #[test]
    fn translate_effort_override_rejects_ultra() {
        let error = resolve_effort_override(Some(Effort::Low), Some("ultra"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Invalid effort override: \"ultra\". Must be one of: none, low, medium, high, xhigh, max"
        );
    }

    #[test]
    fn runtime_overrides_are_explicit_translation_inputs() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "low"}
        }))
        .unwrap();

        let baseline = translate_request(&req, opts()).unwrap();
        assert!(matches!(
            baseline.reasoning.unwrap().effort,
            Some(Effort::Low)
        ));

        let overridden = translate_request_with_overrides(
            &req,
            opts(),
            TranslationOverrides {
                service_tier: Some("priority".to_string()),
                effort: Some("max".to_string()),
                reasoning_summary: Some("off".to_string()),
            },
        )
        .unwrap();
        assert!(matches!(
            overridden.reasoning.as_ref().and_then(|r| r.effort.clone()),
            Some(Effort::Max)
        ));
        assert_eq!(overridden.reasoning.unwrap().summary, None);
        assert_eq!(overridden.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn max_tokens_is_not_serialized_for_codex() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "max_tokens": 4096,
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let value = serde_json::to_value(out).unwrap();
        assert!(value.get("max_output_tokens").is_none());
    }

    #[test]
    fn request_standard_only_overrides_local_priority_tier() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "max_tokens":4096,
            "messages":[{"role":"user", "content":"hello"}],
            "service_tier":"standard_only"
        }))
        .unwrap();
        let mut options = opts();
        options.service_tier = Some(ServiceTier::Priority);
        let translated = translate_request_with_overrides(
            &req,
            options,
            TranslationOverrides {
                service_tier: Some("priority".to_string()),
                effort: None,
                reasoning_summary: None,
            },
        )
        .unwrap();
        assert_eq!(translated.service_tier, None);
    }

    #[test]
    fn translate_effort_xhigh_maps_to_xhigh() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_ultracode_xhigh_maps_to_max() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "Available agent types for the Agent tool:\n- claude: general purpose\n\nUltracode is on: optimize for the most exhaustive, correct answer — not the fastest or cheapest."
                }]
            }],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Max)));
    }

    #[test]
    fn ordinary_user_ultracode_text_does_not_promote_xhigh() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": "Ultracode is on: optimize for the most exhaustive, correct answer"
            }],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
    }

    #[test]
    fn later_ultracode_off_marker_restores_xhigh() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {
                    "role": "system",
                    "content": "Available agent types for the Agent tool:\n- claude: general purpose\n\nUltracode is on: optimize for the most exhaustive, correct answer — not the fastest or cheapest."
                },
                {"role": "assistant", "content": "Earlier answer"},
                {
                    "role": "system",
                    "content": "Ultracode is off — the Workflow tool's standard opt-in rule applies again."
                },
                {"role": "user", "content": "Continue normally"}
            ],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();

        assert!(!is_claude_code_ultracode_request(&req));
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
    }

    #[test]
    fn later_ultracode_on_marker_reactivates_max() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {
                    "role": "system",
                    "content": "Available agent types for the Agent tool:\n- claude: general purpose\n\nUltracode is on: optimize for the most exhaustive, correct answer — not the fastest or cheapest."
                },
                {
                    "role": "system",
                    "content": "Ultracode is off — the Workflow tool's standard opt-in rule applies again."
                },
                {
                    "role": "system",
                    "content": "Ultracode is on: optimize for the most exhaustive, correct answer — not the fastest or cheapest."
                },
                {"role": "user", "content": "Use Ultracode again"}
            ],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();

        assert!(is_claude_code_ultracode_request(&req));
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Max)));
    }

    #[test]
    fn explicit_effort_override_wins_over_ultracode_promotion() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "system",
                "content": "Ultracode is on: optimize for the most exhaustive, correct answer"
            }],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();

        assert!(is_claude_code_ultracode_request(&req));
        let out = translate_request_with_overrides(
            &req,
            opts(),
            TranslationOverrides {
                service_tier: None,
                effort: Some("xhigh".to_string()),
                reasoning_summary: None,
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
    }

    #[test]
    fn reasoning_summary_override_values() {
        assert!(reasoning_summary_requested(None));
        assert!(reasoning_summary_requested(Some("auto")));
        assert!(reasoning_summary_requested(Some("detailed")));
        assert!(!reasoning_summary_requested(Some("off")));
        assert!(!reasoning_summary_requested(Some("none")));
    }

    #[test]
    fn translate_user_text_and_image() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [
                {"type":"text", "text":"describe"},
                {"type":"image", "source": {"type":"base64", "media_type":"image/png", "data":VALID_PNG_BASE64}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::Message { role, content } = &out.input[0] {
            assert_eq!(role, "user");
            assert_eq!(content.len(), 2);
        } else {
            panic!("expected Message");
        }
    }

    #[test]
    fn codex_image_validation_accepts_supported_base64_and_https_urls() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [
                {"type":"image", "source": {
                    "type":"base64", "media_type":"image/png", "data":VALID_PNG_BASE64
                }},
                {"type":"image", "source": {
                    "type":"url", "url":"https://example.invalid/image.png"
                }}
            ]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        let ResponsesInputItem::Message { content, .. } = &out.input[0] else {
            panic!("expected image message");
        };
        assert_eq!(content.len(), 2);
    }

    #[test]
    fn codex_image_validation_fully_decodes_each_supported_base64_format() {
        for (media_type, image) in [
            ("image/png", valid_png_bytes()),
            ("image/jpeg", valid_jpeg_bytes()),
            ("image/gif", valid_gif_bytes(1)),
            ("image/webp", valid_webp_bytes()),
        ] {
            let encoded = base64_image(&image);
            assert_eq!(
                validate_codex_base64_image(&encoded, media_type, image.len()),
                Ok(()),
                "{media_type}"
            );
        }
    }

    #[test]
    fn codex_image_validation_rejects_magic_only_truncated_and_trailing_payloads() {
        let fake_webp = webp_with_chunk(b"VP8L", &[0x2f, 0, 0, 0, 0]);
        for (media_type, fake) in [
            ("image/png", b"\x89PNG\r\n\x1a\n".to_vec()),
            ("image/jpeg", vec![0xff, 0xd8, 0xff, 0xd9]),
            ("image/gif", b"GIF89a".to_vec()),
            ("image/webp", fake_webp),
        ] {
            let encoded = base64_image(&fake);
            assert_eq!(
                validate_codex_base64_image(&encoded, media_type, usize::MAX),
                Err(INVALID_CODEX_IMAGE),
                "header-only {media_type}"
            );
        }

        for (media_type, valid) in [
            ("image/png", valid_png_bytes()),
            ("image/jpeg", valid_jpeg_bytes()),
            ("image/gif", valid_gif_bytes(1)),
            ("image/webp", valid_webp_bytes()),
        ] {
            let mut truncated = valid.clone();
            truncated.pop();
            assert_eq!(
                validate_codex_base64_image(&base64_image(&truncated), media_type, usize::MAX,),
                Err(INVALID_CODEX_IMAGE),
                "truncated {media_type}"
            );

            let mut trailing = valid;
            trailing.push(0);
            assert_eq!(
                validate_codex_base64_image(&base64_image(&trailing), media_type, usize::MAX),
                Err(INVALID_CODEX_IMAGE),
                "trailing bytes in {media_type}"
            );
        }
    }

    #[test]
    fn codex_image_validation_rejects_corrupt_complete_containers() {
        let mut corrupt_png = valid_png_bytes();
        let idat = corrupt_png
            .windows(4)
            .position(|window| window == b"IDAT")
            .unwrap();
        corrupt_png[idat + 4] ^= 1;

        let incomplete_lzw_gif = vec![
            b'G', b'I', b'F', b'8', b'9', b'a', 1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 255, 255, 255,
            0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 1, 0, 0, 0x3b,
        ];

        for (media_type, image) in [
            ("image/png", corrupt_png),
            ("image/gif", incomplete_lzw_gif),
        ] {
            assert_eq!(
                validate_codex_base64_image(&base64_image(&image), media_type, usize::MAX,),
                Err(INVALID_CODEX_IMAGE),
                "{media_type}"
            );
        }
    }

    #[test]
    fn codex_image_validation_rejects_animated_gif_and_webp() {
        let animated_gif = valid_gif_bytes(2);
        assert_eq!(
            validate_codex_base64_image(&base64_image(&animated_gif), "image/gif", usize::MAX,),
            Err(INVALID_CODEX_IMAGE)
        );

        for chunk_type in [b"ANIM", b"ANMF"] {
            let animated_webp = webp_with_chunk(chunk_type, &[0; 24]);
            assert_eq!(
                validate_codex_base64_image(
                    &base64_image(&animated_webp),
                    "image/webp",
                    usize::MAX,
                ),
                Err(INVALID_CODEX_IMAGE),
                "{}",
                String::from_utf8_lossy(chunk_type)
            );
        }
    }

    #[test]
    fn codex_image_validation_rejects_malformed_sources_before_dispatch() {
        let invalid_sources = [
            json!(null),
            json!({"type":"base64", "media_type":"image/png", "data":""}),
            json!({"type":"base64", "media_type":"image/svg+xml", "data":VALID_PNG_BASE64}),
            json!({"type":"base64", "media_type":"image/png", "data":"%%%"}),
            json!({"type":"base64", "media_type":"image/png", "data":"aGVsbG8="}),
            json!({"type":"url", "url":"file:///tmp/image.png"}),
            json!({"type":"url", "url":"https://user:secret@example.invalid/image.png"}),
            json!({"type":"url", "url":"/relative/image.png"}),
            json!({"type":"future", "url":"https://example.invalid/image.png"}),
        ];

        for source in invalid_sources {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content": [{"type":"image", "source":source}]}]
            }))
            .unwrap();
            assert!(translate_request(&req, opts()).is_err());
        }
    }

    #[test]
    fn codex_image_validation_rejects_mime_mismatch_and_oversize_data() {
        assert_eq!(
            validate_codex_base64_image(VALID_PNG_BASE64, "image/jpeg", usize::MAX),
            Err(INVALID_CODEX_IMAGE)
        );
        assert_eq!(
            validate_codex_base64_image(VALID_PNG_BASE64, "image/png", 7),
            Err("exceeds the image byte limit")
        );
    }

    #[test]
    fn codex_image_validation_covers_images_nested_in_tool_results() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"tool_1", "name":"Read", "input":{}
                }]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"tool_1", "content":[{
                        "type":"image", "source":{
                            "type":"base64", "media_type":"image/png", "data":"aGVsbG8="
                        }
                    }]
                }]}
            ]
        }))
        .unwrap();

        let error = translate_request(&req, opts()).unwrap_err().to_string();
        assert!(error.contains("messages[1].content[0].content[0]"));
    }

    #[test]
    fn codex_unknown_content_blocks_are_preserved_as_visible_text() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":[{
                "type":"future_widget", "payload":{"answer":42}
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        let ResponsesInputItem::Message { content, .. } = &out.input[0] else {
            panic!("expected user message");
        };
        let ResponsesContentPart::InputText { text } = &content[0] else {
            panic!("expected preserved text");
        };
        assert!(text.contains("[Anthropic future_widget block]"));
        assert!(text.contains("\"answer\":42"));
    }

    #[test]
    fn codex_rejects_assistant_image_blocks_instead_of_dropping_them() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"assistant", "content":[{
                "type":"image", "source":{
                    "type":"base64", "media_type":"image/png", "data":VALID_PNG_BASE64
                }
            }]}]
        }))
        .unwrap();

        assert!(
            translate_request(&req, opts())
                .unwrap_err()
                .to_string()
                .contains("image blocks must have user role")
        );
    }

    #[test]
    fn translate_assistant_with_text_and_tool_use() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [
                    {"type":"text", "text":"answer"},
                    {"type":"tool_use", "id":"tu_1", "name":"search", "input": {"q":"rust"}}
                ]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"tu_1", "content":"done"
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 3);
    }

    #[test]
    fn translate_strict_json_schema_normalization() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hi"}],
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}, "reason": {"type": "string"}},
                    "required": ["ok"]
                }
            }}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        if let Some(ResponsesTextFormat::JsonSchema { schema, .. }) = &out.text.format {
            let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
            assert!(required.iter().any(|v| v == "ok"));
            assert!(required.iter().any(|v| v == "reason"));
        } else {
            panic!("expected JsonSchema format");
        }
        assert!(out.schema_bridge.is_some());
        let serialized = serde_json::to_value(&out).unwrap();
        assert!(serialized.get("schema_bridge").is_none());
    }

    #[test]
    fn translate_rejects_malformed_output_formats() {
        for (case, output_config) in [
            ("non-object format", json!({"format":[]})),
            ("missing type", json!({"format":{}})),
            ("unknown type", json!({"format":{"type":"json_schmea"}})),
            ("missing schema", json!({"format":{"type":"json_schema"}})),
            (
                "non-object schema",
                json!({"format":{"type":"json_schema","schema":[]}}),
            ),
            (
                "empty name",
                json!({"format":{"type":"json_schema","name":"","schema":{}}}),
            ),
            (
                "non-string name",
                json!({"format":{"type":"json_schema","name":1,"schema":{}}}),
            ),
            (
                "invalid name characters",
                json!({"format":{"type":"json_schema","name":"bad name","schema":{}}}),
            ),
            (
                "overlong name",
                json!({"format":{"type":"json_schema","name":"a".repeat(65),"schema":{}}}),
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user","content":"hello"}],
                "output_config":output_config
            }))
            .unwrap();
            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(!error.is_empty(), "{case}");
        }

        let effort_only: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user","content":"hello"}],
            "output_config":{"effort":"high"}
        }))
        .unwrap();
        assert!(translate_request(&effort_only, opts()).is_ok());
    }

    #[test]
    fn null_output_config_is_treated_as_omitted() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user","content":"hello"}],
            "output_config":null
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(out.text.format.is_none());
        assert!(out.schema_bridge.is_none());
    }

    #[test]
    fn translate_accepts_nested_system_text_as_developer_input() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"system","content":"ultracode string instructions"},
                {"role":"system","content":[{
                    "type":"text",
                    "text":"ultracode block instructions",
                    "cache_control":{"type":"ephemeral"}
                }]},
                {"role":"user","content":"hello"}
            ]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 3);
        for (item, expected) in out.input[..2].iter().zip([
            "ultracode string instructions",
            "ultracode block instructions",
        ]) {
            let ResponsesInputItem::Message { role, content } = item else {
                panic!("expected nested system message to become a developer message");
            };
            assert_eq!(role, "developer");
            let [ResponsesContentPart::InputText { text }] = content.as_slice() else {
                panic!("expected exactly one input_text part");
            };
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn translate_rejects_unknown_message_roles() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"foo","content":"do not reinterpret me"}]
        }))
        .unwrap();
        let error = translate_request(&req, opts()).unwrap_err().to_string();
        assert!(error.contains("role must be user or assistant"), "{error}");
    }

    #[test]
    fn translate_rejects_non_text_nested_system_blocks() {
        for block in [
            json!({"type":"image","source":{"type":"url","url":"https://example.com/a.png"}}),
            json!({"type":"tool_use","id":"tool_1","name":"Read","input":{}}),
            json!({"type":"tool_result","tool_use_id":"tool_1","content":"result"}),
            json!({"type":"thinking","thinking":"private"}),
            json!({"type":"future_widget","payload":42}),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"system","content":[block]}]
            }))
            .unwrap();
            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(
                error.contains("must be a text block for system role"),
                "{error}"
            );
        }
    }

    #[test]
    fn translate_rejects_invalid_message_content_and_text_shapes() {
        for (content, expected) in [
            (
                json!(42),
                "content must be a string or array of content blocks",
            ),
            (
                json!({"type":"text","text":"not wrapped in an array"}),
                "content must be a string or array of content blocks",
            ),
            (json!([{"type":"text"}]), "content[0].text must be a string"),
            (
                json!([{"type":"text","text":42}]),
                "content[0].text must be a string",
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"user","content":content}]
            }))
            .unwrap();
            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn translate_tool_result_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type":"tool_use", "id":"tu_1", "name":"test", "input":{}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": [{"type":"text", "text":"result"}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { call_id, .. } = &out.input[1] {
            assert_eq!(call_id, "tu_1");
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn omitted_and_structured_tool_results_are_preserved() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[
                    {"type":"tool_use", "id":"empty_1", "name":"Empty", "input":{}},
                    {"type":"tool_use", "id":"rich_1", "name":"Rich", "input":{}}
                ]},
                {"role":"user", "content":[
                    {"type":"tool_result", "tool_use_id":"empty_1"},
                    {"type":"tool_result", "tool_use_id":"rich_1", "content":[
                        {"type":"document", "source":{"type":"text", "media_type":"text/plain", "data":"document body"}},
                        {"type":"search_result", "source":"https://example.test", "title":"Example", "content":[{"type":"text", "text":"search body"}]}
                    ]}
                ]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let outputs: Vec<&ResponsesFunctionCallOutput> = out
            .input
            .iter()
            .filter_map(|item| match item {
                ResponsesInputItem::FunctionCallOutput { output, .. } => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 2);
        assert_eq!(tool_result_text(outputs[0]), "");
        let rich = tool_result_text(outputs[1]);
        assert!(rich.contains("[Anthropic document tool result block]"));
        assert!(rich.contains("document body"));
        assert!(rich.contains("[Anthropic search_result tool result block]"));
        assert!(rich.contains("https://example.test"));
        assert!(rich.contains("search body"));
    }

    #[test]
    fn rejects_duplicate_tools_unknown_choices_and_invalid_results() {
        let duplicate: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[
                {"name":"Same", "input_schema":{"type":"object"}},
                {"name":"Same", "input_schema":{"type":"object"}}
            ]
        }))
        .unwrap();
        assert!(
            translate_request(&duplicate, opts())
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let missing_choice: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[{"name":"Known", "input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool", "name":"Missing"}
        }))
        .unwrap();
        assert!(
            translate_request(&missing_choice, opts())
                .unwrap_err()
                .to_string()
                .contains("unknown tool")
        );

        let unknown_result: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":[{
                "type":"tool_result", "tool_use_id":"missing"
            }]}]
        }))
        .unwrap();
        assert!(
            translate_request(&unknown_result, opts())
                .unwrap_err()
                .to_string()
                .contains("unknown tool use id")
        );

        let invalid_error: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"call_1", "name":"Known", "input":{}
                }]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"call_1", "is_error":"yes"
                }]}
            ]
        }))
        .unwrap();
        assert!(
            translate_request(&invalid_error, opts())
                .unwrap_err()
                .to_string()
                .contains("is_error must be a boolean")
        );
    }

    #[test]
    fn tool_history_validation_enforces_roles_order_and_resolution() {
        let cases = [
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"user", "content":[{
                        "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                    }]}]
                }),
                "tool_use must have assistant role",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"assistant", "content":[{
                        "type":"tool_result", "tool_use_id":"call_1"
                    }]}]
                }),
                "tool_result must have user role",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"user", "content":[{
                            "type":"tool_result", "tool_use_id":"call_1"
                        }]},
                        {"role":"assistant", "content":[{
                            "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                        }]}
                    ]
                }),
                "not-yet-seen",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"assistant", "content":[{
                            "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                        }]},
                        {"role":"user", "content":[
                            {"type":"tool_result", "tool_use_id":"call_1"},
                            {"type":"tool_result", "tool_use_id":"call_1"}
                        ]}
                    ]
                }),
                "already resolved",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"assistant", "content":[{
                        "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                    }]}]
                }),
                "unresolved custom tool use",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"assistant", "content":[
                        {"type":"web_search_tool_result", "tool_use_id":"srv_1", "content":[]},
                        {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{}}
                    ]}]
                }),
                "not-yet-seen",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"user", "content":[{
                        "type":"web_search_tool_result", "tool_use_id":"srv_1", "content":[]
                    }]}]
                }),
                "must have assistant role",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"assistant", "content":[{
                            "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                        }]},
                        {"role":"user", "content":"not a tool result"},
                        {"role":"user", "content":[{
                            "type":"tool_result", "tool_use_id":"call_1"
                        }]}
                    ]
                }),
                "immediately following user message",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"assistant", "content":[{
                            "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                        }]},
                        {"role":"user", "content":[
                            {"type":"text", "text":"before"},
                            {"type":"tool_result", "tool_use_id":"call_1"}
                        ]}
                    ]
                }),
                "must precede text and other content",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"assistant", "content":[
                        {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{}},
                        {"type":"x_search_tool_result", "tool_use_id":"srv_1", "content":[]}
                    ]}]
                }),
                "kind mismatch",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[{"role":"assistant", "content":[
                        {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{}},
                        {"type":"web_search_tool_result", "tool_use_id":"srv_2", "content":[]}
                    ]}]
                }),
                "unknown server tool use id",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"assistant", "content":[
                            {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{}},
                            {"type":"tool_use", "id":"call_1", "name":"Read", "input":{}}
                        ]},
                        {"role":"user", "content":[
                            {"type":"tool_result", "tool_use_id":"call_1"},
                            {"type":"text", "text":"not allowed during the pause"}
                        ]}
                    ]
                }),
                "only client tool_result blocks",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"assistant", "content":[
                            {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{}},
                            {"type":"tool_use", "id":"call_1", "name":"Read", "input":{}}
                        ]},
                        {"role":"user", "content":[{
                            "type":"tool_result", "tool_use_id":"call_1"
                        }]},
                        {"role":"assistant", "content":[
                            {"type":"text", "text":"too early"},
                            {"type":"web_search_tool_result", "tool_use_id":"srv_1", "content":[]}
                        ]}
                    ]
                }),
                "before other assistant content",
            ),
            (
                json!({
                    "model":"gpt-5.6-sol",
                    "messages":[
                        {"role":"assistant", "content":[{
                            "type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{}
                        }]},
                        {"role":"user", "content":"delayed"},
                        {"role":"assistant", "content":[{
                            "type":"web_search_tool_result", "tool_use_id":"srv_1", "content":[]
                        }]}
                    ]
                }),
                "unresolved server tool use",
            ),
        ];
        for (body, expected) in cases {
            let req: MessagesRequest = serde_json::from_value(body).unwrap();
            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn valid_tool_histories_preserve_result_before_following_text() {
        let custom: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[{
                    "type":"tool_use", "id":"call_1", "name":"Read", "input":{}
                }]},
                {"role":"user", "content":[
                    {"type":"tool_result", "tool_use_id":"call_1", "content":"done"},
                    {"type":"text", "text":"continue"}
                ]}
            ]
        }))
        .unwrap();
        let out = translate_request(&custom, opts()).unwrap();
        assert!(matches!(
            &out.input[0],
            ResponsesInputItem::FunctionCall { call_id, .. } if call_id == "call_1"
        ));
        assert!(matches!(
            &out.input[1],
            ResponsesInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        ));
        assert!(matches!(
            &out.input[2],
            ResponsesInputItem::Message { role, .. } if role == "user"
        ));

        let mixed_pause: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[
                    {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{"query":"rust"}},
                    {"type":"tool_use", "id":"call_1", "name":"Read", "input":{}}
                ]},
                {"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"call_1", "content":"done"
                }]},
                {"role":"assistant", "content":[
                    {"type":"web_search_tool_result", "tool_use_id":"srv_1", "content":[]},
                    {"type":"text", "text":"complete"}
                ]}
            ]
        }))
        .unwrap();
        assert!(translate_request(&mixed_pause, opts()).is_ok());
    }

    #[test]
    fn hosted_tool_search_names_share_the_official_result_kind() {
        for name in ["tool_search_tool_regex", "tool_search_tool_bm25"] {
            let request: MessagesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol",
                "messages":[{"role":"assistant", "content":[
                    {"type":"server_tool_use", "id":"srv_search", "name":name, "input":{"query":"Read"}},
                    {"type":"tool_search_tool_result", "tool_use_id":"srv_search", "content":[]}
                ]}]
            }))
            .unwrap();
            assert!(translate_request(&request, opts()).is_ok(), "name={name}");
        }

        let unknown: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"assistant", "content":[
                {"type":"server_tool_use", "id":"srv_unknown", "name":"future_tool", "input":{}},
                {"type":"future_tool_result", "tool_use_id":"srv_unknown", "content":[]}
            ]}]
        }))
        .unwrap();
        assert!(
            translate_request(&unknown, opts())
                .unwrap_err()
                .to_string()
                .contains("unsupported server tool name")
        );
    }

    #[test]
    fn required_choice_rejects_when_all_tools_disallow_direct_calls() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"user", "content":"run"}],
            "tools":[{
                "name":"SandboxOnly",
                "allowed_callers":["future_code_execution_version"],
                "input_schema":{"type":"object"}
            }],
            "tool_choice":{"type":"any"}
        }))
        .unwrap();
        assert!(
            translate_request(&req, opts())
                .unwrap_err()
                .to_string()
                .contains("no directly callable tools")
        );
    }

    #[test]
    fn preserves_hosted_search_history_and_text_citations_as_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[
                    {"type":"server_tool_use", "id":"srv_1", "name":"web_search", "input":{"query":"rust 2024"}},
                    {"type":"web_search_tool_result", "tool_use_id":"srv_1", "content":[{
                        "type":"web_search_result", "url":"https://example.test", "title":"Example", "encrypted_content":"opaque"
                    }]},
                    {"type":"text", "text":"Found it", "citations":[{"type":"web_search_result_location", "url":"https://example.test"}]}
                ]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let encoded = serde_json::to_string(&out.input).unwrap();
        assert!(encoded.contains("Anthropic server_tool_use block"));
        assert!(encoded.contains("rust 2024"));
        assert!(encoded.contains("Found it"));
        assert!(encoded.contains("Anthropic web_search_tool_result block"));
        assert!(encoded.contains("https://example.test"));
        assert!(encoded.contains("opaque"));
    }

    #[test]
    fn preserves_server_tool_history_with_non_object_input() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[
                {"role":"assistant", "content":[
                    {
                        "type":"server_tool_use",
                        "id":"srv_1",
                        "name":"x_search",
                        "input":"rust"
                    },
                    {
                        "type":"x_search_tool_result",
                        "tool_use_id":"srv_1",
                        "content":[]
                    }
                ]},
                {"role":"user", "content":"continue"}
            ]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        let encoded = serde_json::to_string(&out.input).unwrap();
        assert!(encoded.contains("Anthropic server_tool_use block"));
        assert!(encoded.contains("rust"));
        assert!(encoded.contains("Anthropic x_search_tool_result block"));
    }

    #[test]
    fn translate_read_offset_error_adds_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 2952, "limit": 200}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File has 331 lines, but offset 2952 was requested."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let text = tool_result_text(output);
            assert!(text.contains("[tool execution error]"));
            assert!(text.contains("File has 331 lines"));
            assert!(text.contains("Codex Read guidance:"));
            assert!(text.contains("zero based continuation index"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_unrelated_error_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File does not exist."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                tool_result_text(output),
                "[tool execution error]\nFile does not exist."
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_rewritten_read_result_adds_proxy_note() {
        crate::providers::codex::translate::read_rewrite::sanitize_read_args(
            "Read",
            r#"{"file_path":"/tmp/a","offset":1300000,"limit":20}"#,
            Some("tu_rewritten_read"),
        );
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_rewritten_read",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_rewritten_read",
                    "content": [{"type":"text", "text":"1\tcontent"}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let text = tool_result_text(output);
            assert!(text.contains("1\tcontent"));
            assert!(text.contains("Proxy Read offset note:"));
            assert!(text.contains("1300000"));
            assert!(text.contains("/tmp/a"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_success_with_offset_words_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": [{"type":"text", "text":"File has 331 lines, and the requested offset is shown in this fixture."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                tool_result_text(output),
                "File has 331 lines, and the requested offset is shown in this fixture."
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn tool_result_preserves_images_and_marks_malformed_blocks() {
        let png_url = format!("data:image/png;base64,{VALID_PNG_BASE64}");
        let output = tool_result_to_output(&json!([
            {"type": "text", "text": "caption"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": VALID_PNG_BASE64}},
            {"type": "image", "source": {"type": "url", "url": "https://example.invalid/a.png"}},
            {"type": "text"},
            {"type": "image"},
            {}
        ]));
        let rendered = serde_json::to_value(output).unwrap();
        assert_eq!(
            rendered,
            json!([
                {"type":"input_text", "text":"caption"},
                {"type":"input_image", "image_url":png_url},
                {"type":"input_image", "image_url":"https://example.invalid/a.png"},
                {"type":"input_text", "text":"[Anthropic text tool result block]\n{\"type\":\"text\"}"},
                {"type":"input_text", "text":"[Anthropic image tool result block]\n{\"type\":\"image\"}"},
                {"type":"input_text", "text":"[Anthropic unknown tool result block]\n{}"}
            ])
        );
    }

    #[test]
    fn tool_result_preserves_tool_reference_names() {
        let output = tool_result_to_output(&json!([
            {"type":"tool_reference","tool_name":"mcp__plugin_context7_context7__resolve-library-id"},
            {"type":"tool_reference","tool_name":"WebFetch"}
        ]));

        match output {
            ResponsesFunctionCallOutput::Text(text) => assert_eq!(
                text,
                "[tool reference: mcp__plugin_context7_context7__resolve-library-id]\n[tool reference: WebFetch]"
            ),
            ResponsesFunctionCallOutput::Content(_) => {
                panic!("tool references should remain text-only output")
            }
        }
    }

    #[test]
    fn luna_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn sol_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn responses_lite_moves_instructions_and_tools_into_input() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(out.instructions.is_none());
        assert!(out.tools.is_none());
        assert!(!out.parallel_tool_calls);
        assert!(out.client_metadata.is_some());
        assert_eq!(out.input.len(), 3);
        assert!(matches!(
            out.input[0],
            ResponsesInputItem::AdditionalTools { .. }
        ));
        if let ResponsesInputItem::Message { role, content } = &out.input[1] {
            assert_eq!(role, "developer");
            assert!(matches!(content[0], ResponsesContentPart::InputText { .. }));
        } else {
            panic!("expected developer message");
        }
    }

    #[test]
    fn luna_without_effort_defaults_to_medium_and_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(matches!(reasoning.effort, Some(Effort::Medium)));
        assert_eq!(reasoning.context.as_deref(), Some("all_turns"));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn responses_lite_reasoning_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.reasoning.unwrap().context.as_deref(), Some("all_turns"));
    }

    #[test]
    fn translate_returns_only_expected_top_level_fields() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool", "name":"test"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.4".to_string(),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.model, "gpt-5.4");
        let out_value = serde_json::to_value(&out).unwrap();
        let keys: std::collections::BTreeSet<String> =
            out_value.as_object().unwrap().keys().cloned().collect();
        for key in &[
            "model",
            "input",
            "store",
            "stream",
            "parallel_tool_calls",
            "text",
        ] {
            assert!(keys.contains(*key), "missing key: {key}");
        }
    }

    #[test]
    fn assistant_thinking_signature_replays_codex_reasoning_item() {
        let replay = super::super::reasoning_signature::ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "opaque".to_string(),
        };
        let signature =
            super::super::reasoning_signature::encode_reasoning_signature(&replay).unwrap();
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"user","content":"start"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"visible summary","signature":signature},
                    {"type":"text","text":"done"}
                ]},
                {"role":"user","content":"continue"}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning_index = out
            .input
            .iter()
            .position(|item| matches!(item, ResponsesInputItem::Reasoning { .. }))
            .unwrap();
        let ResponsesInputItem::Reasoning {
            id,
            summary,
            encrypted_content,
        } = &out.input[reasoning_index]
        else {
            unreachable!();
        };
        assert_eq!(id, "rs_1");
        assert!(summary.is_empty());
        assert_eq!(encrypted_content, "opaque");
        assert!(matches!(
            out.input.get(reasoning_index + 1),
            Some(ResponsesInputItem::Message { role, .. }) if role == "assistant"
        ));
    }
}
