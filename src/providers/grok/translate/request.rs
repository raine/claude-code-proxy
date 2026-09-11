use std::collections::HashSet;

use serde::Serialize;
use serde_json::Value;

use crate::anthropic::schema::{Message, MessagesRequest};
use crate::config::{GrokToolImageMode, SearchConstraints};
use crate::providers::translate_shared::{
    ImageSource, image_source_to_url, parallel_tool_calls, read_effort_with_allowed,
};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<GrokReasoning>,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
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
    FunctionCallOutput {
        call_id: String,
        output: GrokToolOutput,
    },
}

/// Tool output payload: a plain string, or (in `inline` image mode) an array
/// of `input_text` + `input_image` parts. Untagged so string outputs serialize
/// byte-identically to the pre-inline shape.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GrokToolOutput {
    Text(String),
    Parts(Vec<GrokContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GrokContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_x_handles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_x_handles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_date: Option<String>,
    #[serde(skip)]
    choice_name: Option<String>,
}

impl GrokTool {
    fn hosted(kind: &str) -> Self {
        Self::hosted_for_choice(kind, None)
    }

    fn hosted_named(kind: &str, name: &str) -> Self {
        Self::hosted_for_choice(kind, Some(name))
    }

    fn hosted_for_choice(kind: &str, choice_name: Option<&str>) -> Self {
        Self {
            kind: kind.into(),
            name: None,
            description: None,
            parameters: None,
            allowed_x_handles: None,
            excluded_x_handles: None,
            from_date: None,
            to_date: None,
            choice_name: choice_name.map(str::to_string),
        }
    }

    fn function(name: &str, description: Option<String>, parameters: Value) -> Self {
        Self {
            kind: "function".into(),
            name: Some(name.into()),
            description,
            parameters: Some(parameters),
            allowed_x_handles: None,
            excluded_x_handles: None,
            from_date: None,
            to_date: None,
            choice_name: Some(name.into()),
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

#[derive(Debug, Clone, Serialize)]
pub struct GrokReasoning {
    pub effort: String,
}

pub fn translate_request(
    req: &MessagesRequest,
    model: String,
) -> anyhow::Result<GrokResponsesRequest> {
    translate_request_with_mode(req, model, crate::config::grok_tool_image_mode())
}

pub fn translate_request_with_mode(
    req: &MessagesRequest,
    model: String,
    image_mode: GrokToolImageMode,
) -> anyhow::Result<GrokResponsesRequest> {
    translate_request_with_options(
        req,
        model,
        image_mode,
        crate::config::grok_hosted_search(),
        crate::config::search_constraints(),
    )
}

/// `hosted_search` selects how xAI's hosted search tools reach the model. When
/// disabled, the translator offers `x_search` only for X-specific turns and
/// preserves every caller tool. When enabled, hosted tools replace caller
/// search tools and explicit search turns require a tool call. Tests pass the
/// policy directly so their behavior is independent of process configuration.
///
/// `constraints` selects what happens when Anthropic hosted-search options
/// (`allowed_domains`, `blocked_domains`, `user_location`) are present and
/// Grok cannot enforce them.
pub fn translate_request_with_options(
    req: &MessagesRequest,
    model: String,
    image_mode: GrokToolImageMode,
    hosted_search: bool,
    constraints: SearchConstraints,
) -> anyhow::Result<GrokResponsesRequest> {
    reject_unknown_top_level(req)?;
    let mut instructions = parse_system(req.extra.get("system"))?;
    let (mut tools, constraint_hint) =
        parse_tools(req.extra.get("tools"), hosted_search, constraints)?;
    if let Some(hint) = constraint_hint {
        append_guidance(&mut instructions, &hint);
    }
    let hosted_web_search = tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool.kind == "web_search"));
    let dedicated_x_search = tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool.kind == "x_search"));
    let mut forced_hosted_tool = false;
    if hosted_search {
        // The enabled policy favors xAI-hosted search and citations over the
        // caller's search implementation. Explicit search turns expose only
        // the selected hosted tool and require a tool call.
        let force_x_search = dedicated_x_search || requests_x_search(req);
        let force_web_search = !force_x_search && hosted_web_search && requests_web_search(req);
        if force_x_search {
            tools = Some(vec![GrokTool::hosted("x_search")]);
        } else if force_web_search {
            tools = Some(vec![GrokTool::hosted("web_search")]);
        } else {
            let tools = tools.get_or_insert_default();
            if !tools.iter().any(|tool| tool.kind == "x_search") {
                tools.push(GrokTool::hosted("x_search"));
            }
        }
        if hosted_web_search {
            append_guidance(
                &mut instructions,
                "For general web searches, use the hosted web_search tool. Do not use shell commands, HTTP clients, or local tools to search the web.",
            );
        }
        if force_x_search {
            append_guidance(
                &mut instructions,
                "For requests to search X or Twitter, use the hosted x_search tool. XSearch accepts a query and supports allowed_x_handles, excluded_x_handles, from_date, and to_date filters. Do not use Bash, curl, HTTP clients, or general web_search for X searches.",
            );
        }
        forced_hosted_tool = force_x_search || force_web_search;
    } else if requests_x_search(req) {
        // The disabled policy adds access to xAI's X index without changing
        // caller tools, instructions, or tool choice.
        let tools = tools.get_or_insert_default();
        if !tools.iter().any(|tool| tool.kind == "x_search") {
            tools.push(GrokTool::hosted("x_search"));
        }
    }
    let tool_choice = if forced_hosted_tool {
        Some(GrokToolChoice::Required("required".into()))
    } else {
        parse_tool_choice(req.extra.get("tool_choice"), &mut tools)?
    };
    let reasoning =
        read_effort_with_allowed(req, &["none", "low", "medium", "high", "xhigh", "max"])?.map(
            |effort| GrokReasoning {
                effort: map_reasoning_effort(effort, &model),
            },
        );
    let mut call_ids = HashSet::new();
    let mut input = Vec::new();
    let mut budget = ReattachBudget::new(&req.messages, image_mode);
    for message in &req.messages {
        parse_message(message, &mut input, &mut call_ids, image_mode, &mut budget)?;
    }
    Ok(GrokResponsesRequest {
        model,
        instructions,
        input,
        tools,
        tool_choice,
        parallel_tool_calls: parallel_tool_calls(req),
        reasoning,
        store: false,
        stream: true,
        max_output_tokens: req.max_tokens,
    })
}

/// Pre-scans the request to decide which gate-passing images survive the
/// request-wide "keep only the last few" cap, so the main walk can mark
/// cap-dropped images with a reason instead of silently dropping pixels.
struct ReattachBudget {
    /// Per-image-URL count of gate-passing occurrences that must be dropped
    /// (the oldest ones) before survivors begin, so the *last* few images win.
    drops: std::collections::HashMap<String, usize>,
}

impl ReattachBudget {
    fn new(messages: &[Message], image_mode: GrokToolImageMode) -> Self {
        let mut drops: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        if matches!(
            image_mode,
            GrokToolImageMode::Reattach | GrokToolImageMode::Inline
        ) {
            let passing: Vec<String> = messages
                .iter()
                .flat_map(candidate_image_blocks)
                .filter_map(|block| {
                    let source = parse_image_source(block)?;
                    gate_image(&source)
                        .ok()
                        .map(|()| image_source_to_url(&source))
                })
                .collect();
            // Drop the oldest occurrences beyond the cap, keeping the last few.
            for url in passing
                .iter()
                .take(passing.len().saturating_sub(MAX_REATTACHED_IMAGES))
            {
                *drops.entry(url.clone()).or_insert(0) += 1;
            }
        }
        Self { drops }
    }

    /// Record a gate-passing image; `true` when it survives the request cap.
    /// Oldest occurrences are dropped first, in conversation order.
    fn admit(&mut self, image_url: &str) -> bool {
        let Some(drops) = self.drops.get_mut(image_url) else {
            return true;
        };
        *drops -= 1;
        if *drops == 0 {
            self.drops.remove(image_url);
        }
        false
    }
}

/// Every image block in one message: top-level user images and tool_result
/// image children, in conversation order.
fn candidate_image_blocks(message: &Message) -> Vec<&serde_json::Map<String, Value>> {
    let mut blocks = Vec::new();
    if let Value::Array(items) = &message.content {
        for item in items {
            let Some(object) = item.as_object() else {
                continue;
            };
            match object.get("type").and_then(Value::as_str) {
                Some("image") => blocks.push(object),
                Some("tool_result") => {
                    if let Some(Value::Array(parts)) = object.get("content") {
                        for part in parts {
                            if let Some(part) = part.as_object()
                                && part.get("type").and_then(Value::as_str) == Some("image")
                            {
                                blocks.push(part);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    blocks
}

fn append_guidance(instructions: &mut Option<String>, guidance: &str) {
    *instructions = Some(match instructions.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{guidance}"),
        _ => guidance.into(),
    });
}

const HOSTED_SEARCH_CONSTRAINT_FIELDS: [&str; 3] =
    ["allowed_domains", "blocked_domains", "user_location"];

fn unsupported_hosted_search_message(fields: &[&str]) -> String {
    format!(
        "Grok hosted web search does not support {}",
        fields.join(", ")
    )
}

fn non_null_constraint_fields(obj: &serde_json::Map<String, Value>) -> Vec<&'static str> {
    HOSTED_SEARCH_CONSTRAINT_FIELDS
        .into_iter()
        .filter(|field| obj.get(*field).is_some_and(|value| !value.is_null()))
        .collect()
}

/// Render the caller's value verbatim. A plain string carries no brackets of
/// its own, so it is wrapped in braces to separate the value from the sentence
/// period. Arrays and objects already delimit themselves.
fn format_constraint_value(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{{{text}}}"),
        other => other.to_string(),
    }
}

/// The directive each constraint becomes. Grok cannot enforce these fields, so
/// the instruction states the rule rather than describing the proxy's own
/// limitation.
fn constraint_directive(field: &str) -> &'static str {
    match field {
        "blocked_domains" => "You are not allowed to search",
        "user_location" => "You must search as",
        _ => "You are only allowed to search",
    }
}

fn constraint_hint_line(obj: &serde_json::Map<String, Value>) -> Option<String> {
    let mut parts = Vec::new();
    for field in HOSTED_SEARCH_CONSTRAINT_FIELDS {
        if let Some(value) = obj.get(field).filter(|value| !value.is_null()) {
            parts.push(format!(
                "{} {field}={}.",
                constraint_directive(field),
                format_constraint_value(value)
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn latest_user_text(req: &MessagesRequest) -> Option<String> {
    let message = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")?;
    match &message.content {
        Value::String(text) => Some(text.to_ascii_lowercase()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase(),
        ),
        _ => None,
    }
}

fn contains_phrase(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(start, _)| {
        let end = start + phrase.len();
        let starts_at_boundary = text[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_');
        let ends_at_boundary = text[end..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_');
        starts_at_boundary && ends_at_boundary
    })
}

fn requests_x_search(req: &MessagesRequest) -> bool {
    let Some(text) = latest_user_text(req) else {
        return false;
    };
    [
        "search x for",
        "search on x",
        "search twitter",
        "search tweets",
        "x search",
        "posts on x",
        "posts from x",
        "tweets about",
        "twitter posts",
    ]
    .iter()
    .any(|phrase| contains_phrase(&text, phrase))
}

fn requests_web_search(req: &MessagesRequest) -> bool {
    let Some(text) = latest_user_text(req) else {
        return false;
    };
    [
        "search online",
        "search the web",
        "web search",
        "look up online",
        "look up on the web",
    ]
    .iter()
    .any(|phrase| contains_phrase(&text, phrase))
}

fn map_reasoning_effort(effort: &str, model: &str) -> String {
    match effort {
        "none" | "low" | "medium" | "high" => effort.to_string(),
        "xhigh" | "max" if model == "grok-4.6" => "xhigh".to_string(),
        "xhigh" | "max" => "high".to_string(),
        _ => unreachable!("read_effort_with_allowed validates the effort"),
    }
}

fn reject_unknown_top_level(req: &MessagesRequest) -> anyhow::Result<()> {
    for key in req.extra.keys() {
        if ![
            "system",
            "tools",
            "tool_choice",
            "context_management",
            "diagnostics",
            "metadata",
            "output_config",
            "thinking",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "service_tier",
        ]
        .contains(&key.as_str())
        {
            anyhow::bail!("unsupported Grok request field: {key}");
        }
    }
    if !valid_diagnostics(req.extra.get("diagnostics")) {
        anyhow::bail!("unsupported diagnostics");
    }
    Ok(())
}

fn valid_diagnostics(value: Option<&Value>) -> bool {
    let Some(value) = value else { return true };
    let Some(object) = value.as_object() else {
        return value.is_null();
    };
    object.keys().all(|key| key == "previous_message_id")
        && object.get("previous_message_id").is_none_or(|id| {
            id.is_null()
                || id
                    .as_str()
                    .is_some_and(|previous_message_id| !previous_message_id.is_empty())
        })
}

fn parse_system(value: Option<&Value>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    match value {
        Value::String(text) => Ok(Some(text.clone())),
        Value::Array(blocks) => {
            let mut text = String::new();
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
                let part = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("system text is invalid"))?;
                text.push_str(part);
            }
            Ok(Some(text))
        }
        _ => anyhow::bail!("system must be text"),
    }
}

fn parse_tools(
    value: Option<&Value>,
    hosted_search: bool,
    constraints: SearchConstraints,
) -> anyhow::Result<(Option<Vec<GrokTool>>, Option<String>)> {
    let Some(value) = value else {
        return Ok((None, None));
    };
    let tools = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
    let mut names = HashSet::new();
    let mut out = Vec::new();
    let mut constraint_hint = None;
    for tool in tools {
        let obj = tool
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("tool must be an object"))?;
        for key in obj.keys() {
            if ![
                "name",
                "description",
                "input_schema",
                "cache_control",
                "eager_input_streaming",
                "max_uses",
                "type",
                "allowed_domains",
                "blocked_domains",
                "user_location",
            ]
            .contains(&key.as_str())
            {
                anyhow::bail!("unsupported tool field: {key}");
            }
        }
        if !valid_cache_control(obj.get("cache_control")) {
            anyhow::bail!("unsupported tool cache_control");
        }
        if obj
            .get("eager_input_streaming")
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            anyhow::bail!("tool eager_input_streaming must be boolean");
        }
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("tool name is invalid"))?;
        if !names.insert(name.to_string()) {
            anyhow::bail!("duplicate tool name");
        }
        let kind = match obj.get("type") {
            None => None,
            Some(Value::String(kind)) if !kind.is_empty() => Some(kind.as_str()),
            Some(_) => anyhow::bail!("tool type is invalid"),
        };
        if let Some(max_uses) = obj.get("max_uses")
            && !max_uses.is_null()
            && max_uses.as_u64().is_none_or(|value| value == 0)
        {
            anyhow::bail!("tool max_uses must be a positive integer or null");
        }
        if let Some(kind) = kind {
            if kind != "web_search_20250305" || name != "web_search" {
                anyhow::bail!("unsupported tool type: {kind}");
            }
            let dropped = non_null_constraint_fields(obj);
            if !dropped.is_empty() {
                match constraints {
                    SearchConstraints::Hard => {
                        anyhow::bail!("{}", unsupported_hosted_search_message(&dropped));
                    }
                    SearchConstraints::Warning => {
                        crate::logging::create_logger("grok")
                            .warn(&unsupported_hosted_search_message(&dropped), None);
                    }
                    SearchConstraints::Soft => {
                        constraint_hint = constraint_hint_line(obj);
                    }
                }
            }
            out.push(GrokTool::hosted_named("web_search", name));
            continue;
        }
        for field in HOSTED_SEARCH_CONSTRAINT_FIELDS {
            if obj.contains_key(field) {
                anyhow::bail!("unsupported tool field: {field}");
            }
        }
        if obj.contains_key("max_uses") && name != "WebSearch" {
            anyhow::bail!("unsupported tool field: max_uses");
        }
        if hosted_search && name == "WebSearch" {
            out.push(GrokTool::hosted_named("web_search", name));
            continue;
        }
        if hosted_search && name == "XSearch" {
            out.push(GrokTool::hosted_named("x_search", name));
            continue;
        }
        let parameters = obj
            .get("input_schema")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tool input_schema must be an object"))?;
        out.push(GrokTool::function(
            name,
            obj.get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters,
        ));
    }
    Ok((Some(out), constraint_hint))
}

fn parse_tool_choice(
    value: Option<&Value>,
    tools: &mut Option<Vec<GrokTool>>,
) -> anyhow::Result<Option<GrokToolChoice>> {
    let Some(value) = value else { return Ok(None) };
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool_choice must be an object"))?;
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tool_choice type is invalid"))?;
    let valid_policy = obj
        .get("disable_parallel_tool_use")
        .is_none_or(Value::is_boolean);
    match kind {
        "auto" | "any" | "none"
            if valid_policy
                && obj
                    .keys()
                    .all(|key| ["type", "disable_parallel_tool_use"].contains(&key.as_str())) =>
        {
            Ok(Some(match kind {
                "auto" => GrokToolChoice::Auto("auto".into()),
                "any" => GrokToolChoice::Required("required".into()),
                "none" => GrokToolChoice::None("none".into()),
                _ => unreachable!(),
            }))
        }
        "tool"
            if valid_policy
                && obj.keys().all(|key| {
                    ["type", "name", "disable_parallel_tool_use"].contains(&key.as_str())
                }) =>
        {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("tool_choice name is invalid"))?;
            let selected = tools
                .as_ref()
                .and_then(|items| {
                    items
                        .iter()
                        .find(|tool| tool.choice_name.as_deref() == Some(name))
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("tool_choice references an unknown tool"))?;
            if selected.kind == "function" {
                return Ok(Some(GrokToolChoice::Function {
                    r#type: "function".into(),
                    name: name.into(),
                }));
            }
            *tools = Some(vec![selected]);
            Ok(Some(GrokToolChoice::Required("required".into())))
        }
        _ => anyhow::bail!("unsupported tool_choice"),
    }
}

fn parse_message(
    message: &Message,
    out: &mut Vec<GrokInputItem>,
    calls: &mut HashSet<String>,
    image_mode: GrokToolImageMode,
    budget: &mut ReattachBudget,
) -> anyhow::Result<()> {
    if !["system", "user", "assistant"].contains(&message.role.as_str()) {
        anyhow::bail!("unsupported message role");
    }
    let blocks: Vec<Value> = match &message.content {
        Value::String(text) => vec![serde_json::json!({"type":"text", "text":text})],
        Value::Array(items) => items.clone(),
        _ => anyhow::bail!("message content must be text or blocks"),
    };
    let mut content = Vec::new();
    for block in blocks {
        let object = block
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("content block must be an object"))?;
        let typ = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("content block type is invalid"))?;
        match (message.role.as_str(), typ) {
            (_, "thinking") | (_, "redacted_thinking") => {}
            (_, "text") => {
                if object
                    .keys()
                    .any(|key| !["type", "text", "cache_control"].contains(&key.as_str()))
                    || !valid_cache_control(object.get("cache_control"))
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
            ("assistant", "server_tool_use") => {
                let name = object.get("name").and_then(Value::as_str);
                if !matches!(name, Some("web_search" | "x_search")) {
                    anyhow::bail!("unsupported server tool use");
                }
            }
            ("assistant", "web_search_tool_result" | "x_search_tool_result")
            | ("user", "web_search_tool_result" | "x_search_tool_result") => {}
            ("user", "image") => {
                if object
                    .keys()
                    .any(|key| !["type", "source", "cache_control"].contains(&key.as_str()))
                    || !valid_cache_control(object.get("cache_control"))
                {
                    anyhow::bail!("unsupported image block field");
                }
                match image_mode {
                    GrokToolImageMode::Reject => {
                        anyhow::bail!("unsupported content block: image");
                    }
                    GrokToolImageMode::Omit => {
                        let placeholder = image_placeholder(object)
                            .ok_or_else(|| anyhow::anyhow!("image source is invalid"))?;
                        content.push(GrokContentPart::InputText { text: placeholder });
                    }
                    GrokToolImageMode::Reattach | GrokToolImageMode::Inline => {
                        let source = parse_image_source(object)
                            .ok_or_else(|| anyhow::anyhow!("image source is invalid"))?;
                        match gate_image(&source) {
                            Ok(()) => {
                                let image_url = image_source_to_url(&source);
                                if budget.admit(&image_url) {
                                    content.push(GrokContentPart::InputImage { image_url });
                                } else {
                                    content.push(GrokContentPart::InputText {
                                        text: omit_with_reason(&source, &cap_reason()),
                                    });
                                }
                            }
                            Err(reason) => {
                                content.push(GrokContentPart::InputText {
                                    text: omit_with_reason(&source, &reason),
                                });
                            }
                        }
                    }
                }
            }
            ("assistant", "tool_use") => {
                if object.keys().any(|key| {
                    !["type", "id", "name", "input", "cache_control"].contains(&key.as_str())
                }) || !valid_cache_control(object.get("cache_control"))
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
                }) {
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
                let value = object
                    .get("content")
                    .ok_or_else(|| anyhow::anyhow!("tool result content is required"))?;
                let mut tool_result_images: Vec<String> = Vec::new();
                let output = match value {
                    Value::String(text) => GrokToolOutput::Text(text.clone()),
                    Value::Array(parts) => {
                        let mut texts = Vec::new();
                        let mut inline_parts: Vec<GrokContentPart> = Vec::new();
                        for part in parts {
                            let part = part.as_object().ok_or_else(|| {
                                anyhow::anyhow!("tool result child must be an object")
                            })?;
                            if part.get("type").and_then(Value::as_str) == Some("tool_reference") {
                                if part.keys().any(|key| {
                                    !["type", "tool_name", "cache_control"].contains(&key.as_str())
                                }) || part
                                    .get("tool_name")
                                    .and_then(Value::as_str)
                                    .is_none_or(str::is_empty)
                                    || !valid_cache_control(part.get("cache_control"))
                                {
                                    anyhow::bail!("unsupported tool_reference child");
                                }
                                continue;
                            }
                            if part.get("type").and_then(Value::as_str) == Some("image") {
                                if part.keys().any(|key| {
                                    !["type", "source", "cache_control"].contains(&key.as_str())
                                }) || !valid_cache_control(part.get("cache_control"))
                                {
                                    anyhow::bail!("unsupported image child");
                                }
                                match image_mode {
                                    GrokToolImageMode::Reject => {
                                        anyhow::bail!("tool result supports text children only");
                                    }
                                    GrokToolImageMode::Omit => {
                                        let placeholder =
                                            image_placeholder(part).ok_or_else(|| {
                                                anyhow::anyhow!(
                                                    "tool result image source is invalid"
                                                )
                                            })?;
                                        texts.push(placeholder);
                                    }
                                    GrokToolImageMode::Reattach => {
                                        let source = parse_image_source(part).ok_or_else(|| {
                                            anyhow::anyhow!("tool result image source is invalid")
                                        })?;
                                        match gate_image(&source) {
                                            Ok(()) => {
                                                let image_url = image_source_to_url(&source);
                                                if budget.admit(&image_url) {
                                                    texts.push(image_placeholder(part).ok_or_else(
                                                        || {
                                                            anyhow::anyhow!(
                                                                "tool result image source is invalid"
                                                            )
                                                        },
                                                    )?);
                                                    tool_result_images.push(image_url);
                                                } else {
                                                    texts.push(omit_with_reason(
                                                        &source,
                                                        &cap_reason(),
                                                    ));
                                                }
                                            }
                                            Err(reason) => {
                                                texts.push(omit_with_reason(&source, &reason));
                                            }
                                        }
                                    }
                                    GrokToolImageMode::Inline => {
                                        let source = parse_image_source(part).ok_or_else(|| {
                                            anyhow::anyhow!("tool result image source is invalid")
                                        })?;
                                        match gate_image(&source) {
                                            Ok(()) => {
                                                let image_url = image_source_to_url(&source);
                                                if budget.admit(&image_url) {
                                                    inline_parts.push(
                                                        GrokContentPart::InputImage { image_url },
                                                    );
                                                } else {
                                                    inline_parts.push(GrokContentPart::InputText {
                                                        text: omit_with_reason(
                                                            &source,
                                                            &cap_reason(),
                                                        ),
                                                    });
                                                }
                                            }
                                            Err(reason) => {
                                                inline_parts.push(GrokContentPart::InputText {
                                                    text: omit_with_reason(&source, &reason),
                                                });
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                            if part.get("type").and_then(Value::as_str) != Some("text")
                                || part.keys().any(|key| {
                                    !["type", "text", "cache_control"].contains(&key.as_str())
                                })
                                || !valid_cache_control(part.get("cache_control"))
                            {
                                anyhow::bail!("tool result supports text children only");
                            }
                            let text = part
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| anyhow::anyhow!("tool result text is invalid"))?
                                .to_string();
                            if image_mode == GrokToolImageMode::Inline {
                                inline_parts.push(GrokContentPart::InputText { text });
                            } else {
                                texts.push(text);
                            }
                        }
                        if image_mode == GrokToolImageMode::Inline {
                            // Text-only results keep the plain-string shape so
                            // inline mode is invisible unless an image is present.
                            let mut joined: Option<String> = Some(String::new());
                            for part in &inline_parts {
                                match part {
                                    GrokContentPart::InputText { text } => {
                                        if let Some(acc) = joined.as_mut() {
                                            if !acc.is_empty() {
                                                acc.push('\n');
                                            }
                                            acc.push_str(text);
                                        }
                                    }
                                    _ => joined = None,
                                }
                            }
                            match joined {
                                Some(text) => GrokToolOutput::Text(text),
                                None => GrokToolOutput::Parts(inline_parts),
                            }
                        } else {
                            GrokToolOutput::Text(texts.join("\n"))
                        }
                    }
                    _ => anyhow::bail!("tool result supports text only"),
                };
                out.push(GrokInputItem::FunctionCallOutput {
                    call_id: id.into(),
                    output,
                });
                if image_mode == GrokToolImageMode::Reattach && !tool_result_images.is_empty() {
                    out.push(GrokInputItem::Message {
                        role: "user".into(),
                        content: tool_result_images
                            .into_iter()
                            .map(|image_url| GrokContentPart::InputImage { image_url })
                            .collect(),
                    });
                }
            }
            _ => anyhow::bail!("unsupported content block: {typ}"),
        }
    }
    flush_message(&message.role, &mut content, out);
    Ok(())
}

fn image_placeholder(object: &serde_json::Map<String, Value>) -> Option<String> {
    let source = object.get("source")?.as_object()?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64")
            if source
                .get("media_type")
                .and_then(Value::as_str)
                .is_some_and(|media_type| !media_type.is_empty())
                && source.get("data").and_then(Value::as_str).is_some() =>
        {
            let media_type = source.get("media_type").and_then(Value::as_str)?;
            Some(format!("[image omitted: {media_type}]"))
        }
        Some("url")
            if source
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty()) =>
        {
            Some("[image omitted: url]".into())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// L2a reattach gates (limits verified against cli-chat-proxy.grok.com on
// 2026-07-20: 1x1 and 8x8 rejected, 32x32 accepted; production-scale
// screenshots well above the minimums; both `reattach` and `inline` wire
// shapes returned 200 with correct visual answers)
// ---------------------------------------------------------------------------

/// Upstream rejects images whose smallest side is under this many pixels.
const MIN_IMAGE_SIDE_PX: u32 = 8;
/// Upstream rejects images whose area is under this many square pixels.
const MIN_IMAGE_AREA_PX: u64 = 512;
/// Decoded RGB(A) payload cap; larger images are degraded to the omit marker.
const MAX_IMAGE_DECODED_BYTES: u64 = 5 * 1024 * 1024;
/// Only the last few images across the whole request are attached.
const MAX_REATTACHED_IMAGES: usize = 4;

/// Parse an Anthropic image block into a shared `ImageSource`. Returns `None`
/// for structurally invalid blocks (missing fields, unknown source type).
fn parse_image_source(object: &serde_json::Map<String, Value>) -> Option<ImageSource> {
    let source = object.get("source")?.as_object()?;
    let source_type = source.get("type").and_then(Value::as_str)?;
    match source_type {
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|media_type| !media_type.is_empty())?;
            let data = source.get("data").and_then(Value::as_str)?;
            Some(ImageSource {
                media_type: media_type.to_string(),
                data: data.to_string(),
                source_type: source_type.to_string(),
            })
        }
        "url" => {
            let url = source
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())?;
            Some(ImageSource {
                media_type: "image/*".to_string(),
                data: url.to_string(),
                source_type: source_type.to_string(),
            })
        }
        _ => None,
    }
}

/// Gate a candidate image against the upstream-verified limits. Returns the
/// omit reason on failure so the caller can degrade just this one image.
fn gate_image(source: &ImageSource) -> Result<(), String> {
    if source.source_type == "url" {
        // The proxy cannot gate a remote image without fetching it (dimensions
        // and decoded size are unknown), and an unverifiable image can 400 the
        // whole turn upstream — so URL sources never reattach.
        return Err("url source cannot be gated".to_string());
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.data.as_bytes())
        .map_err(|_| "undecodable base64".to_string())?;
    let raster = image_raster(&bytes, &source.media_type)
        .ok_or_else(|| format!("unreadable dimensions for {}", source.media_type))?;
    let width = raster.width;
    let height = raster.height;
    let min_side = width.min(height);
    if min_side < MIN_IMAGE_SIDE_PX {
        return Err(format!(
            "{width}x{height} below minimum side {MIN_IMAGE_SIDE_PX}px"
        ));
    }
    let area = width as u64 * height as u64;
    if area < MIN_IMAGE_AREA_PX {
        return Err(format!(
            "{width}x{height} below minimum area {MIN_IMAGE_AREA_PX}px"
        ));
    }
    let decoded = area.saturating_mul(raster.bytes_per_pixel);
    if decoded > MAX_IMAGE_DECODED_BYTES {
        return Err(format!(
            "{width}x{height} too large (decoded ~{}MB > {}MB cap)",
            decoded / (1024 * 1024),
            MAX_IMAGE_DECODED_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// Render the L1 omit marker with a gate-failure reason appended.
fn omit_with_reason(source: &ImageSource, reason: &str) -> String {
    let base = if source.source_type == "url" {
        "[image omitted: url]".to_string()
    } else {
        format!("[image omitted: {}]", source.media_type)
    };
    format!("{base} ({reason})")
}

/// Reason attached to images that passed every per-image gate but lost the
/// request-wide "keep only the last few" cap.
fn cap_reason() -> String {
    format!("only the last {MAX_REATTACHED_IMAGES} images are attached per request")
}

/// Extract raster dimensions and a conservative decoded byte width from the
/// encoded image header. PNG accounting reserves alpha when the format may
/// carry transparency. GIF accounting reserves an RGBA output pixel.
#[derive(Clone, Copy)]
struct ImageRaster {
    width: u32,
    height: u32,
    bytes_per_pixel: u64,
}

fn image_raster(bytes: &[u8], media_type: &str) -> Option<ImageRaster> {
    match media_type {
        "image/png" => png_raster(bytes),
        "image/jpeg" => jpeg_raster(bytes),
        "image/gif" => gif_raster(bytes),
        _ => sniff_raster(bytes),
    }
}

fn sniff_raster(bytes: &[u8]) -> Option<ImageRaster> {
    png_raster(bytes)
        .or_else(|| jpeg_raster(bytes))
        .or_else(|| gif_raster(bytes))
}

fn png_raster(bytes: &[u8]) -> Option<ImageRaster> {
    // Signature (8) + IHDR length (4) + "IHDR" (4) + IHDR fields.
    if bytes.len() < 26
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || u32::from_be_bytes(bytes[8..12].try_into().ok()?) != 13
        || &bytes[12..16] != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    let bit_depth = bytes[24];
    let color_type = bytes[25];
    let bytes_per_channel = match bit_depth {
        1 | 2 | 4 | 8 => 1,
        16 => 2,
        _ => return None,
    };
    let channels = match color_type {
        // Grayscale and RGB may carry transparency in a tRNS chunk.
        0 | 4 => 2,
        2 | 3 | 6 => 4,
        _ => return None,
    };
    Some(ImageRaster {
        width,
        height,
        bytes_per_pixel: bytes_per_channel * channels,
    })
}

fn gif_raster(bytes: &[u8]) -> Option<ImageRaster> {
    // "GIF87a"/"GIF89a" (6) + width (2 LE) + height (2 LE).
    if bytes.len() < 10 || !matches!(&bytes[..6], b"GIF87a" | b"GIF89a") {
        return None;
    }
    let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
    let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
    Some(ImageRaster {
        width,
        height,
        bytes_per_pixel: 4,
    })
}

fn jpeg_raster(bytes: &[u8]) -> Option<ImageRaster> {
    // Walk SOI-delimited segments to the first SOF0..SOF15 frame header.
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut cursor = 2usize;
    while cursor + 4 <= bytes.len() {
        if bytes[cursor] != 0xff {
            return None;
        }
        let marker = bytes[cursor + 1];
        // Standalone markers without a length field.
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            cursor += 2;
            continue;
        }
        let segment_len =
            u16::from_be_bytes(bytes[cursor + 2..cursor + 4].try_into().ok()?) as usize;
        if segment_len < 2 || cursor + 2 + segment_len > bytes.len() {
            return None;
        }
        let is_sof = matches!(
            marker,
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
        );
        if is_sof {
            // Segment: length (2), precision (1), dimensions (4), components (1).
            if segment_len < 8 {
                return None;
            }
            let precision = bytes[cursor + 4];
            let height = u16::from_be_bytes(bytes[cursor + 5..cursor + 7].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[cursor + 7..cursor + 9].try_into().ok()?) as u32;
            let components = bytes[cursor + 9] as usize;
            if components == 0 || segment_len < 8 + 3 * components {
                return None;
            }
            let bytes_per_channel = u64::from(precision).div_ceil(8);
            return Some(ImageRaster {
                width,
                height,
                bytes_per_pixel: bytes_per_channel.saturating_mul(components as u64),
            });
        }
        cursor += 2 + segment_len;
    }
    None
}

fn valid_cache_control(value: Option<&Value>) -> bool {
    let Some(value) = value else { return true };
    let Some(object) = value.as_object() else {
        return false;
    };
    object
        .keys()
        .all(|key| key == "type" || key == "ttl" || key == "scope")
        && object.get("type").and_then(Value::as_str) == Some("ephemeral")
        && object
            .get("ttl")
            .is_none_or(|ttl| matches!(ttl.as_str(), Some("5m") | Some("1h")))
        && object
            .get("scope")
            .is_none_or(|scope| matches!(scope.as_str(), Some("global")))
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

    /// Translate with an explicit hosted-search setting. Reading
    /// `CCP_GROK_HOSTED_SEARCH` from the environment would make these tests
    /// depend on the shell that ran them, so they pass the flag directly.
    fn translate_search(
        req: &MessagesRequest,
        model: &str,
        hosted_search: bool,
    ) -> serde_json::Value {
        translate_search_with_constraints(req, model, hosted_search, SearchConstraints::Soft)
    }

    fn translate_search_with_constraints(
        req: &MessagesRequest,
        model: &str,
        hosted_search: bool,
        constraints: SearchConstraints,
    ) -> serde_json::Value {
        serde_json::to_value(
            translate_request_with_options(
                req,
                model.into(),
                crate::config::GrokToolImageMode::Omit,
                hosted_search,
                constraints,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn translate_options(
        req: &MessagesRequest,
        hosted_search: bool,
        constraints: SearchConstraints,
    ) -> anyhow::Result<GrokResponsesRequest> {
        translate_request_with_options(
            req,
            "grok-4.5".into(),
            crate::config::GrokToolImageMode::Omit,
            hosted_search,
            constraints,
        )
    }

    fn translate_hosted(req: &MessagesRequest, model: &str) -> serde_json::Value {
        translate_search(req, model, true)
    }

    fn translate_client_search(req: &MessagesRequest, model: &str) -> serde_json::Value {
        translate_search(req, model, false)
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
        assert!(value["instructions"].as_str().unwrap().starts_with("rules"));
        assert_eq!(value["input"][1]["type"], "function_call");
        assert_eq!(value["input"][2]["type"], "function_call_output");
        assert_eq!(value["tool_choice"]["type"], "function");
    }
    #[test]
    fn grok_translation_forwards_reasoning_effort() {
        let cases = [
            ("grok-4.5", "none", "none"),
            ("grok-4.5", "low", "low"),
            ("grok-4.5", "high", "high"),
            ("grok-4.5", "xhigh", "high"),
            ("grok-4.5", "max", "high"),
            ("grok-4.6", "xhigh", "xhigh"),
            ("grok-4.6", "max", "xhigh"),
        ];
        for (model, requested, expected) in cases {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model": model,
                "messages": [{"role":"user","content":"hello"}],
                "output_config": {"effort": requested}
            }))
            .unwrap();
            let value =
                serde_json::to_value(translate_request(&request, model.into()).unwrap()).unwrap();
            assert_eq!(value["reasoning"]["effort"], expected);
        }
    }

    #[test]
    fn grok_translation_omits_reasoning_without_effort() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let value =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(value.get("reasoning").is_none());
    }

    #[test]
    fn grok_translation_rejects_unknown_reasoning_effort() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "output_config": {"effort": "invalid"}
        }))
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_maps_claude_web_search_to_hosted_web_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"search online for the project"}],
            "tools":[{
                "name":"WebSearch",
                "description":"Search the web",
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            }]
        }))
        .unwrap();
        let translated = translate_hosted(&request, "grok-4.5");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"web_search"}])
        );
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .contains("use the hosted web_search tool")
        );
        assert_eq!(translated["tool_choice"], "required");
    }

    #[test]
    fn grok_translation_keeps_client_web_search_tool_by_default() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"search online for the project"}],
            "tools":[{
                "name":"WebSearch",
                "description":"Search the web",
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            }]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        // Caller-managed search remains a function tool.
        assert_eq!(translated["tools"][0]["type"], "function");
        assert_eq!(translated["tools"][0]["name"], "WebSearch");
        assert_eq!(translated["tools"].as_array().unwrap().len(), 1);
        assert!(!translated.to_string().contains("\"web_search\""));
        assert!(!translated.to_string().contains("\"x_search\""));
    }

    #[test]
    fn grok_translation_maps_x_intent_to_required_hosted_x_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"Search X for recent posts mentioning claude-code-proxy"}],
            "tools":[
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}},
                {"name":"WebSearch","description":"Search the web","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}}
            ]
        }))
        .unwrap();
        let translated = translate_hosted(&request, "grok-4.5");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"x_search"}])
        );
        assert_eq!(translated["tool_choice"], "required");
        assert!(!translated.to_string().contains("\"name\":\"Bash\""));
    }

    #[test]
    fn grok_translation_offers_x_search_without_forcing_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"Search X for recent posts mentioning claude-code-proxy"}],
            "tools":[
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}},
                {"name":"WebSearch","description":"Search the web","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}}
            ]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        // X-specific turns receive one additional hosted capability.
        let tools = translated["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert!(tools.iter().any(|t| t["name"] == "Bash"));
        assert!(tools.iter().any(|t| t["name"] == "WebSearch"));
        assert!(tools.iter().any(|t| t["type"] == "x_search"));
        assert!(translated["tool_choice"].is_null());
        assert!(!translated.to_string().contains("use the hosted x_search"));
    }

    #[test]
    fn grok_translation_offers_no_x_search_without_x_intent() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"read the config file"}],
            "tools":[{"name":"Bash","description":"Run a command","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        assert_eq!(translated["tools"].as_array().unwrap().len(), 1);
        assert!(!translated.to_string().contains("x_search"));
    }

    #[test]
    fn grok_translation_requires_phrase_boundaries_for_x_search() {
        for text in ["use unix search for files", "run a linux search command"] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":text}],
                "tools":[{"name":"Bash","description":"Run a command","input_schema":{"type":"object"}}]
            }))
            .unwrap();
            let translated = translate_client_search(&request, "grok-4.5");
            assert_eq!(translated["tools"].as_array().unwrap().len(), 1);
            assert!(!translated.to_string().contains("x_search"));
        }
    }

    #[test]
    fn grok_translation_maps_dedicated_xsearch_with_domain_schema() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find relevant posts"}],
            "tools":[{
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
                }
            }]
        }))
        .unwrap();
        let translated = translate_hosted(&request, "grok-4.5");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"x_search"}])
        );
        assert_eq!(translated["tool_choice"], "required");
    }

    #[test]
    fn grok_translation_omits_x_search_guidance_without_x_intent() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"hi"}]
        }))
        .unwrap();
        let translated = translate_hosted(&request, "grok-4.5");
        assert_eq!(translated["instructions"], "rules");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"x_search"}])
        );
        assert!(translated["tool_choice"].is_null());
    }

    #[test]
    fn grok_translation_attaches_no_search_tool_to_a_greeting_by_default() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"hi"}]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        assert_eq!(translated["instructions"], "rules");
        assert!(translated["tools"].as_array().is_none_or(|t| t.is_empty()));
        assert!(translated["tool_choice"].is_null());
    }

    #[test]
    fn grok_translation_appends_x_search_guidance_on_x_intent() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"search twitter for the outage"}]
        }))
        .unwrap();
        let translated = translate_hosted(&request, "grok-4.5");
        let instructions = translated["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("rules"));
        assert!(instructions.contains("use the hosted x_search tool"));
    }

    #[test]
    fn grok_translation_appends_x_search_guidance_for_dedicated_x_tool() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{
                "name":"XSearch",
                "description":"Search X posts",
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            }]
        }))
        .unwrap();
        let translated = translate_hosted(&request, "grok-4.5");
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .contains("use the hosted x_search tool")
        );
    }

    #[test]
    fn grok_translation_keeps_dedicated_xsearch_as_a_function_by_default() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{
                "name":"XSearch",
                "description":"Search X posts",
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            }]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        assert_eq!(translated["tools"][0]["type"], "function");
        assert_eq!(translated["tools"][0]["name"], "XSearch");
        assert_eq!(translated["instructions"], "rules");
    }

    #[test]
    fn grok_translation_accepts_claude_code_context_management() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-composer-2.5-fast",
            "messages":[{"role":"user","content":"hello"}],
            "context_management":{"edits":[{"type":"clear_tool_uses_20250919","trigger":{"type":"input_tokens","value":100000}}]}
        }))
        .unwrap();
        let translated = translate_request(&request, "grok-composer-2.5-fast".into()).unwrap();
        assert_eq!(translated.input.len(), 1);
    }

    #[test]
    fn grok_translation_accepts_cache_diagnostics_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "diagnostics":{"previous_message_id":"msg_previous"}
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(!translated.to_string().contains("diagnostics"));
    }

    #[test]
    fn grok_translation_rejects_malformed_cache_diagnostics() {
        for diagnostics in [
            serde_json::json!(true),
            serde_json::json!({"previous_message_id": 1}),
            serde_json::json!({"previous_message_id": ""}),
            serde_json::json!({"previous_message_id": null, "unknown": true}),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"hello"}],
                "diagnostics": diagnostics
            }))
            .unwrap();
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_accepts_cache_control_scope_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":[{"type":"text","text":"rules","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}}],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .starts_with("rules")
        );
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_rejects_unknown_cache_control_scope() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral","scope":"session"}}]}]
        }))
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_accepts_claude_code_eager_input_streaming_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{
                "name":"lookup",
                "description":"d",
                "input_schema":{"type":"object"},
                "eager_input_streaming":true
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
        assert!(!translated.to_string().contains("eager_input_streaming"));
    }

    #[test]
    fn grok_translation_rejects_malformed_eager_input_streaming() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{
                "name":"lookup",
                "input_schema":{"type":"object"},
                "eager_input_streaming":"true"
            }]
        }))
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_drops_tool_reference_children_in_tool_results() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"ok"},
                {"type":"tool_reference","tool_name":"lookup"}
            ]}
        ]));
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        let rendered = translated.to_string();
        assert!(!rendered.contains("tool_reference"));
        assert!(rendered.contains("ok"));
    }

    #[test]
    fn grok_translation_rejects_malformed_tool_reference_children() {
        for child in [
            serde_json::json!({"type":"tool_reference","name":"lookup"}),
            serde_json::json!({"type":"tool_reference","tool_name":""}),
            serde_json::json!({"type":"tool_reference","tool_name":"lookup","unknown":true}),
            serde_json::json!({"type":"tool_reference","tool_name":"lookup","cache_control":{"type":"persistent"}}),
        ] {
            let request = request_with_blocks(serde_json::json!([
                {"type":"tool_result","tool_use_id":"call_1","content":[child]}
            ]));
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_accepts_web_search_max_uses_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{
                "name":"WebSearch",
                "description":"Search the web",
                "max_uses":6,
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            }]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        assert_eq!(translated["tools"][0]["name"], "WebSearch");
        assert!(!translated.to_string().contains("max_uses"));
    }

    #[test]
    fn grok_translation_maps_the_anthropic_server_web_search_declaration() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}},
                {"type":"web_search_20250305","name":"web_search","max_uses":8}
            ]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        let tools = translated["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|t| t["name"] == "Bash"));
        assert!(tools.iter().any(|t| t["type"] == "web_search"));
        assert!(!translated.to_string().contains("web_search_20250305"));
        assert!(!translated.to_string().contains("max_uses"));
    }

    #[test]
    fn grok_translation_forces_a_named_hosted_web_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[
                {"name":"Bash","description":"Run a command","input_schema":{"type":"object"}},
                {"type":"web_search_20250305","name":"web_search","max_uses":8}
            ],
            "tool_choice":{"type":"tool","name":"web_search"}
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"web_search"}])
        );
        assert_eq!(translated["tool_choice"], "required");
    }

    #[test]
    fn grok_translation_rejects_unsupported_hosted_web_search_options() {
        for (field, value) in [
            ("allowed_domains", serde_json::json!(["example.com"])),
            ("blocked_domains", serde_json::json!(["example.com"])),
            (
                "user_location",
                serde_json::json!({"type":"approximate","country":"GB"}),
            ),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"find it"}],
                "tools":[{"type":"web_search_20250305","name":"web_search",(field):value}]
            }))
            .unwrap();
            let error = translate_options(&request, false, SearchConstraints::Hard)
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                format!("Grok hosted web search does not support {field}")
            );
        }
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"],
                "user_location":{"type":"approximate","country":"GB"}
            }]
        }))
        .unwrap();
        let error = translate_options(&request, false, SearchConstraints::Hard)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Grok hosted web search does not support allowed_domains, user_location"
        );
    }

    #[test]
    fn grok_translation_softens_hosted_web_search_constraints_into_a_prompt_hint() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com","docs.example.com"],
                "blocked_domains":["spam.example"],
                "user_location":{"type":"approximate","country":"GB"}
            }]
        }))
        .unwrap();
        let translated =
            translate_search_with_constraints(&request, "grok-4.5", false, SearchConstraints::Soft);
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"web_search"}])
        );
        let instructions = translated["instructions"].as_str().unwrap();
        assert_eq!(
            instructions,
            concat!(
                "rules\n\n",
                r#"You are only allowed to search allowed_domains=["example.com","docs.example.com"]. "#,
                r#"You are not allowed to search blocked_domains=["spam.example"]. "#,
                r#"You must search as user_location={"country":"GB","type":"approximate"}."#,
            )
        );
        assert!(!translated["tools"].to_string().contains("allowed_domains"));
    }

    #[test]
    fn grok_translation_wraps_a_plain_string_constraint_value_in_braces() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search",
                "user_location":"London, GB"
            }]
        }))
        .unwrap();
        let translated =
            translate_search_with_constraints(&request, "grok-4.5", false, SearchConstraints::Soft);
        assert_eq!(
            translated["instructions"],
            "You must search as user_location={London, GB}."
        );
    }

    #[test]
    fn grok_translation_warns_and_drops_hosted_web_search_constraints() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":"rules",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"]
            }]
        }))
        .unwrap();
        let _stderr = crate::logging::suppress_stderr();
        let translated = translate_search_with_constraints(
            &request,
            "grok-4.5",
            false,
            SearchConstraints::Warning,
        );
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"web_search"}])
        );
        assert_eq!(translated["instructions"], "rules");
        assert!(!translated.to_string().contains("allowed_domains"));
    }

    #[test]
    fn search_constraints_parse_flag_values() {
        use crate::config::parse_search_constraints;
        assert_eq!(parse_search_constraints(None), SearchConstraints::Soft);
        assert_eq!(
            parse_search_constraints(Some("soft")),
            SearchConstraints::Soft
        );
        assert_eq!(
            parse_search_constraints(Some("warning")),
            SearchConstraints::Warning
        );
        assert_eq!(
            parse_search_constraints(Some("hard")),
            SearchConstraints::Hard
        );
        assert_eq!(
            parse_search_constraints(Some("bogus")),
            SearchConstraints::Soft
        );
    }

    #[test]
    fn grok_translation_accepts_null_hosted_web_search_options() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":null,
                "blocked_domains":null,
                "user_location":null
            }]
        }))
        .unwrap();
        let translated = translate_client_search(&request, "grok-4.5");
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"web_search"}])
        );
    }

    #[test]
    fn grok_translation_rejects_an_unknown_server_tool_type() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"run it"}],
            "tools":[{"type":"code_execution_20260120","name":"code_execution"}]
        }))
        .unwrap();
        let error = translate_options(&request, false, SearchConstraints::Soft)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "unsupported tool type: code_execution_20260120");
    }

    #[test]
    fn grok_translation_rejects_a_server_tool_type_it_does_not_know_exactly() {
        // The mapping requires both the supported version and canonical name.
        for (kind, name) in [
            ("web_search_20991231", "web_search"),
            ("web_search_20250305", "MyOwnSearch"),
        ] {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"find it"}],
                "tools":[{"type":kind,"name":name}]
            }))
            .unwrap();
            let error = translate_options(&request, false, SearchConstraints::Hard)
                .unwrap_err()
                .to_string();
            assert_eq!(error, format!("unsupported tool type: {kind}"));
        }
    }

    #[test]
    fn grok_translation_still_rejects_an_unknown_tool_field() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"find it"}],
            "tools":[{"name":"WebSearch","input_schema":{"type":"object"},"invented":1}]
        }))
        .unwrap();
        let error = translate_options(&request, false, SearchConstraints::Soft)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "unsupported tool field: invented");
    }

    #[test]
    fn grok_translation_rejects_invalid_or_misplaced_search_fields() {
        let invalid = [
            serde_json::json!({"type":null,"name":"Bash","input_schema":{"type":"object"}}),
            serde_json::json!({"type":1,"name":"Bash","input_schema":{"type":"object"}}),
            serde_json::json!({"name":"WebSearch","max_uses":0,"input_schema":{"type":"object"}}),
            serde_json::json!({"name":"WebSearch","max_uses":"many","input_schema":{"type":"object"}}),
            serde_json::json!({"name":"Bash","max_uses":2,"input_schema":{"type":"object"}}),
        ];
        for tool in invalid {
            let request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"run it"}],
                "tools":[tool]
            }))
            .unwrap();
            assert!(translate_options(&request, false, SearchConstraints::Soft).is_err());
        }
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

    fn translated_with_mode(
        request: &MessagesRequest,
        image_mode: crate::config::GrokToolImageMode,
    ) -> Value {
        serde_json::to_value(
            translate_request_with_mode(request, "grok-4.5".into(), image_mode).unwrap(),
        )
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
    fn grok_translation_omits_image_only_tool_result_children() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
            ]}
        ]));
        let translated = translated_with_mode(&request, crate::config::GrokToolImageMode::Omit);
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(
            translated["input"][1]["output"],
            "[image omitted: image/png]"
        );
    }

    #[test]
    fn grok_translation_omits_url_image_tool_result_children() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}}
            ]}
        ]));
        let translated = translated_with_mode(&request, crate::config::GrokToolImageMode::Omit);
        assert_eq!(translated["input"][1]["output"], "[image omitted: url]");
    }

    #[test]
    fn grok_translation_joins_text_and_image_tool_result_children() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"caption"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}},
                {"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}}
            ]}
        ]));
        let translated = translated_with_mode(&request, crate::config::GrokToolImageMode::Omit);
        assert_eq!(
            translated["input"][1]["output"],
            "caption\n[image omitted: image/png]\n[image omitted: url]"
        );
    }

    #[test]
    fn grok_translation_joins_multiple_text_tool_result_children_with_newlines() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"first"},
                {"type":"text","text":"second"}
            ]}
        ]));
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["input"][1]["output"], "first\nsecond");
    }

    #[test]
    fn grok_translation_omits_top_level_user_image_blocks() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
            ]}]
        }))
        .unwrap();
        let translated = translated_with_mode(&request, crate::config::GrokToolImageMode::Omit);
        assert_eq!(
            translated["input"][0]["content"],
            serde_json::json!([
                {"type":"input_text","text":"what is this?"},
                {"type":"input_text","text":"[image omitted: image/png]"}
            ])
        );
    }

    #[test]
    fn grok_translation_rejects_malformed_tool_result_children() {
        for child in [
            serde_json::json!("text"),
            serde_json::json!({"text":"ok"}),
            serde_json::json!({"type":"text","text":1}),
            serde_json::json!({"type":"text","text":"ok","unknown":true}),
        ] {
            let request = request_with_blocks(serde_json::json!([
                {"type":"tool_result","tool_use_id":"call_1","content":[child]}
            ]));
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
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
                    {"type":"tool_result","tool_use_id":"call_1","content":[
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

    // ---------------------------------------------------------------------
    // L2a: CCP_GROK_TOOL_IMAGE flag + reattach vision tests
    // ---------------------------------------------------------------------

    // 32x32 solid red PNG (valid dimensions, passes all gates).
    const PNG_32_RED: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAKElEQVR4nO3NsQ0AAAzCMP5/un0CNkuZ41wybXsHAAAAAAAAAAAAxR4yw/wuPL6QkAAAAABJRU5ErkJggg==";
    // 1x1 red PNG (min-side + area gate failure).
    const PNG_1_RED: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    // 8x8 red PNG (area gate failure: 64 < 512).
    const PNG_8_RED: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP4z8CAFWEXHbQSACj/P8Fu7N9hAAAAAElFTkSuQmCC";

    #[test]
    fn grok_tool_image_mode_parses_flag_values() {
        use crate::config::{GrokToolImageMode, parse_grok_tool_image_mode};
        assert_eq!(parse_grok_tool_image_mode(None), GrokToolImageMode::Omit);
        assert_eq!(
            parse_grok_tool_image_mode(Some("omit")),
            GrokToolImageMode::Omit
        );
        assert_eq!(
            parse_grok_tool_image_mode(Some("reattach")),
            GrokToolImageMode::Reattach
        );
        assert_eq!(
            parse_grok_tool_image_mode(Some("reject")),
            GrokToolImageMode::Reject
        );
        assert_eq!(
            parse_grok_tool_image_mode(Some("inline")),
            GrokToolImageMode::Inline
        );
        // Unknown values are the safe default.
        assert_eq!(
            parse_grok_tool_image_mode(Some("bogus")),
            GrokToolImageMode::Omit
        );
        assert_eq!(
            parse_grok_tool_image_mode(Some("")),
            GrokToolImageMode::Omit
        );
    }

    #[test]
    fn reattach_tool_result_image_emits_following_user_message_with_input_image() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"screenshot"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        // function_call_output keeps the L1 omit marker (text stays verbatim).
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        assert_eq!(
            translated["input"][1]["output"],
            "screenshot\n[image omitted: image/png]"
        );
        // A user message follows carrying the image as an input_image data URL.
        assert_eq!(translated["input"][2]["type"], "message");
        assert_eq!(translated["input"][2]["role"], "user");
        let parts = translated["input"][2]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "input_image");
        let url = parts[0]["image_url"].as_str().unwrap();
        assert!(
            url.starts_with("data:image/png;base64,"),
            "unexpected image_url prefix: {}",
            &url[..url.len().min(40)]
        );
        assert!(url.contains(PNG_32_RED));
    }

    #[test]
    fn reattach_top_level_user_image_becomes_input_image_part() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"what color is this?"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}]
        }))
        .unwrap();
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        let parts = translated["input"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "what color is this?");
        assert_eq!(parts[1]["type"], "input_image");
        assert!(
            parts[1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn reattach_degrades_tiny_image_to_omit_with_reason_never_error() {
        for (data, dims) in [(PNG_1_RED, "1x1"), (PNG_8_RED, "8x8")] {
            let request = request_with_blocks(serde_json::json!([
                {"type":"tool_result","tool_use_id":"call_1","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}
                ]}
            ]));
            let translated = serde_json::to_value(
                translate_request_with_mode(
                    &request,
                    "grok-4.5".into(),
                    crate::config::GrokToolImageMode::Reattach,
                )
                .unwrap(),
            )
            .unwrap();
            let output = translated["input"][1]["output"].as_str().unwrap();
            assert!(
                output.starts_with("[image omitted: image/png"),
                "unexpected output: {output}"
            );
            assert!(output.contains(dims), "reason must cite dims: {output}");
            // No reattached message: gated-out images never produce input_image.
            assert_eq!(translated["input"].as_array().unwrap().len(), 2);
        }
    }

    #[test]
    fn reattach_accepts_rgb_image_under_decoded_size_cap() {
        // The 8-bit RGB raster is 2.1MB. A format-independent 8-byte estimate
        // would incorrectly reject this common screenshot size.
        let image = solid_rgb_png_base64(1000, 700);
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":image}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            translated["input"][1]["output"],
            "[image omitted: image/png]"
        );
        assert_eq!(translated["input"][2]["content"][0]["type"], "input_image");
    }

    #[test]
    fn reattach_degrades_oversized_image_to_omit_with_reason() {
        // 6000x6000 solid PNG: decoded raw size is at least 108MB. Deflate of a
        // solid scanline is tiny, so the base64 fixture stays small.
        let big = solid_rgb_png_base64(6000, 6000);
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":big}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        let output = translated["input"][1]["output"].as_str().unwrap();
        assert!(
            output.starts_with("[image omitted: image/png"),
            "unexpected output: {output}"
        );
        assert!(
            output.contains("too large"),
            "reason must cite size: {output}"
        );
        assert_eq!(translated["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn reattach_keeps_only_last_four_images_per_request() {
        let mut result_parts = Vec::new();
        for _ in 0..6 {
            result_parts.push(serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}));
        }
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":result_parts}]}
            ]
        }))
        .unwrap();
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        let input = translated["input"].as_array().unwrap();
        // function_call + function_call_output + one reattached user message.
        assert_eq!(input.len(), 3);
        let parts = input[2]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 4, "only the last 4 images survive");
        assert!(parts.iter().all(|part| part["type"] == "input_image"));
        // The two cap-dropped images degrade with a reason in the tool output.
        let output = input[1]["output"].as_str().unwrap();
        let cap_drops = output.matches("only the last 4 images").count();
        assert_eq!(
            cap_drops, 2,
            "cap-dropped images must carry a reason: {output}"
        );
        let attached = output.matches("[image omitted: image/png]\n").count()
            + if output.ends_with("[image omitted: image/png]") {
                1
            } else {
                0
            };
        assert_eq!(attached, 4, "surviving images keep the plain L1 marker");
    }

    #[test]
    fn reattach_cap_applies_across_tool_results_in_one_request() {
        // Two tool results with 3 images each: 6 gate-passing images, so only
        // the last 4 across the request reattach (the whole second result's 3
        // plus the first result's last 1).
        let image = || serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}});
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_1","name":"lookup","input":{}},
                    {"type":"tool_use","id":"call_2","name":"lookup","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":[image(),image(),image()]},
                    {"type":"tool_result","tool_use_id":"call_2","content":[image(),image(),image()]}
                ]}
            ]
        }))
        .unwrap();
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        let input = translated["input"].as_array().unwrap();
        let total_attached: usize = input
            .iter()
            .map(|item| {
                item["content"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|part| part["type"] == "input_image")
                            .count()
                    })
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(
            total_attached, 4,
            "cap is request-wide, not per tool result"
        );
    }

    #[test]
    fn reattach_degrades_url_images_with_reason_instead_of_reattaching() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Reattach,
            )
            .unwrap(),
        )
        .unwrap();
        let output = translated["input"][1]["output"].as_str().unwrap();
        assert!(
            output.starts_with("[image omitted: url]"),
            "unexpected output: {output}"
        );
        assert!(
            output.contains("cannot be gated"),
            "reason must explain the skip: {output}"
        );
        // No input_image part anywhere.
        assert!(!translated.to_string().contains("input_image"));
    }

    #[test]
    fn reject_mode_restores_old_bail_on_tool_result_images() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}
        ]));
        let result = translate_request_with_mode(
            &request,
            "grok-4.5".into(),
            crate::config::GrokToolImageMode::Reject,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("tool result supports text children only")
        );
    }

    #[test]
    fn reject_mode_restores_old_bail_on_top_level_user_images() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}]
        }))
        .unwrap();
        let result = translate_request_with_mode(
            &request,
            "grok-4.5".into(),
            crate::config::GrokToolImageMode::Reject,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported content block: image")
        );
    }

    /// Build a solid 8-bit RGB PNG of the given size, returned as base64.
    fn solid_rgb_png_base64(width: u32, height: u32) -> String {
        use base64::Engine;
        use std::io::Write as _;
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
        write_chunk(&mut png, b"IHDR", &ihdr);
        // One scanline: filter byte + width * 3 zero bytes, repeated.
        let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
        for _ in 0..height {
            raw.push(0u8);
            raw.extend(std::iter::repeat_n(0u8, width as usize * 3));
        }
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&raw).unwrap();
        let idat = encoder.finish().unwrap();
        write_chunk(&mut png, b"IDAT", &idat);
        write_chunk(&mut png, b"IEND", &[]);
        base64::engine::general_purpose::STANDARD.encode(png)
    }

    fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_data = Vec::with_capacity(4 + data.len());
        crc_data.extend_from_slice(kind);
        crc_data.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xffff_ffff;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    // ---------------------------------------------------------------------
    // L2b: CCP_GROK_TOOL_IMAGE=inline — tool output as a content-part array
    // ---------------------------------------------------------------------

    #[test]
    fn inline_tool_result_image_emits_array_output_with_input_image_part() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"screenshot"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(translated["input"][1]["type"], "function_call_output");
        // output is an untagged array of content parts, not a string.
        let parts = translated["input"][1]["output"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "screenshot");
        assert_eq!(parts[1]["type"], "input_image");
        let url = parts[1]["image_url"].as_str().unwrap();
        assert!(
            url.starts_with("data:image/png;base64,"),
            "unexpected image_url prefix: {}",
            &url[..url.len().min(40)]
        );
        assert!(url.contains(PNG_32_RED));
        // No reattached user message: the image rides inside the tool output.
        assert_eq!(translated["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn inline_image_only_tool_result_emits_array_with_single_image_part() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        let parts = translated["input"][1]["output"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "input_image");
    }

    #[test]
    fn inline_text_only_tool_result_serializes_byte_identically_to_omit() {
        // String-only outputs (and text-only array results) must keep the
        // plain-string shape in every mode — inline included.
        let shapes = [
            serde_json::json!({"type":"tool_result","tool_use_id":"call_1","content":"plain string"}),
            serde_json::json!({"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"first"},
                {"type":"text","text":"second"}
            ]}),
        ];
        for shape in shapes {
            let request = request_with_blocks(serde_json::json!([shape]));
            let omit = serde_json::to_string(
                &translate_request_with_mode(
                    &request,
                    "grok-4.5".into(),
                    crate::config::GrokToolImageMode::Omit,
                )
                .unwrap(),
            )
            .unwrap();
            let inline = serde_json::to_string(
                &translate_request_with_mode(
                    &request,
                    "grok-4.5".into(),
                    crate::config::GrokToolImageMode::Inline,
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(omit, inline, "text-only outputs must be byte-identical");
            // And the output field really is a bare JSON string, not an array.
            let value: Value = serde_json::from_str(&inline).unwrap();
            assert!(value["input"][1]["output"].is_string());
        }
    }

    #[test]
    fn inline_string_output_matches_pre_inline_serialization_exactly() {
        // Regression: the whole upstream-bound request body for a string tool
        // result must serialize exactly as before the GrokToolOutput widening.
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"result"}
        ]));
        let body = serde_json::to_string(
            &translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            body.contains(r#""output":"result""#),
            "output must serialize as a bare string: {body}"
        );
        assert!(!body.contains(r#""output":["#));
    }

    #[test]
    fn inline_all_images_gated_out_collapse_to_string_with_reasons() {
        for (data, dims) in [(PNG_1_RED, "1x1"), (PNG_8_RED, "8x8")] {
            let request = request_with_blocks(serde_json::json!([
                {"type":"tool_result","tool_use_id":"call_1","content":[
                    {"type":"text","text":"shot"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}
                ]}
            ]));
            // Gate failures degrade per-image, never a 400 for the whole turn.
            let translated = serde_json::to_value(
                translate_request_with_mode(
                    &request,
                    "grok-4.5".into(),
                    crate::config::GrokToolImageMode::Inline,
                )
                .unwrap(),
            )
            .unwrap();
            // All parts ended up as text → collapses back to a plain string.
            let output = translated["input"][1]["output"].as_str().unwrap();
            assert!(
                output.contains("[image omitted: image/png"),
                "unexpected output: {output}"
            );
            assert!(output.contains(dims), "reason must cite dims: {output}");
            assert!(output.starts_with("shot\n"));
        }
    }

    #[test]
    fn inline_mixed_passing_and_failing_images_keep_array_shape() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_1_RED}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        let parts = translated["input"][1]["output"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        // The gated-out image becomes an in-band omit fragment inside the array.
        assert_eq!(parts[0]["type"], "input_text");
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("[image omitted: image/png")
        );
        assert!(parts[0]["text"].as_str().unwrap().contains("1x1"));
        assert_eq!(parts[1]["type"], "input_image");
    }

    #[test]
    fn inline_degrades_url_images_to_omit_fragment_inside_array() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}
        ]));
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        let parts = translated["input"][1]["output"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "input_text");
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("cannot be gated")
        );
        assert_eq!(parts[1]["type"], "input_image");
    }

    #[test]
    fn inline_keeps_only_last_four_images_per_request() {
        let mut result_parts = Vec::new();
        for _ in 0..6 {
            result_parts.push(serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}));
        }
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":result_parts}]}
            ]
        }))
        .unwrap();
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        let input = translated["input"].as_array().unwrap();
        // function_call + function_call_output only — no reattached message.
        assert_eq!(input.len(), 2);
        let parts = input[1]["output"].as_array().unwrap();
        assert_eq!(parts.len(), 6);
        let images = parts
            .iter()
            .filter(|part| part["type"] == "input_image")
            .count();
        assert_eq!(images, 4, "only the last 4 images survive");
        let cap_drops = parts
            .iter()
            .filter(|part| {
                part["type"] == "input_text"
                    && part["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("only the last 4 images"))
            })
            .count();
        assert_eq!(cap_drops, 2, "cap-dropped images carry a reason");
    }

    #[test]
    fn inline_top_level_user_image_becomes_input_image_part() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"what color is this?"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_32_RED}}
            ]}]
        }))
        .unwrap();
        let translated = serde_json::to_value(
            translate_request_with_mode(
                &request,
                "grok-4.5".into(),
                crate::config::GrokToolImageMode::Inline,
            )
            .unwrap(),
        )
        .unwrap();
        let parts = translated["input"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert!(
            parts[1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }
}
