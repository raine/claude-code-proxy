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
    "antigravity/claude-opus-5-5-low",
    "antigravity/claude-opus-5-5-medium",
    "antigravity/claude-opus-5-5-high",
];

pub(crate) fn effective_tier(model: &str) -> Option<&'static str> {
    match model {
        "antigravity/gemini-4-argon-low" | "antigravity/claude-opus-5-5-low" => Some("low"),
        "antigravity/gemini-4-argon-medium" | "antigravity/claude-opus-5-5-medium" => {
            Some("medium")
        }
        "antigravity/gemini-4-argon-high" | "antigravity/claude-opus-5-5-high" => Some("high"),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod effects {
    use std::cell::Cell;
    #[derive(Clone, Copy)]
    pub(crate) enum Kind {
        CredentialLoad,
        Access,
        Factory,
        Clock,
        NativeId,
    }
    thread_local! { static COUNTS: Cell<[usize;5]> = const { Cell::new([0;5]) }; }
    pub(crate) fn note(kind: Kind) {
        COUNTS.with(|cell| {
            let mut counts = cell.get();
            counts[kind as usize] += 1;
            cell.set(counts);
        });
    }
    pub(crate) fn reset() {
        COUNTS.with(|cell| cell.set([0; 5]));
    }
    pub(crate) fn snapshot() -> [usize; 5] {
        COUNTS.with(Cell::get)
    }
}

pub(crate) fn metadata_millis() -> Result<i64> {
    #[cfg(test)]
    effects::note(effects::Kind::Clock);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "invalid metadata clock")?
        .as_millis();
    i64::try_from(millis).map_err(|_| "invalid metadata clock")
}

pub(crate) fn metadata_uuid() -> String {
    #[cfg(test)]
    effects::note(effects::Kind::NativeId);
    uuid::Uuid::new_v4().to_string()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServingScope {
    pub profile_id: String,
    pub project_id: String,
}

/// Immutable, nonsecret VALUE inputs only. No Auth, Store, client, clock or ID
/// source is available to the estimate function. A missing serving scope does
/// not authorize generation: carrier scope is then an untrusted consistency
/// claim checked against the known model/configured project, never published.
#[derive(Clone, Copy)]
pub(crate) struct CountInputs<'a> {
    pub serving_scope: Option<&'a ServingScope>,
    pub configured_project: Option<&'a str>,
}

pub(crate) enum Metadata {
    Fresh,
    Continuation {
        state: replay::State,
        new_user_execution: bool,
        execution_ids: Vec<String>,
    },
    Ready(replay::State),
}

pub(crate) struct Prepared {
    pub wire: Value,
    pub session: String,
    pub display: String,
    pub tools: ToolSet,
    pub estimate: u64,
    pub metadata: Metadata,
}

impl Prepared {
    /// Only called after unchanged access and actual binding validation. The ID
    /// source is used at fresh/new-USER execution boundaries, NEVER to fabricate
    /// last_execution_id at render time. Tool generations retain active/prior.
    pub(crate) fn finalize_metadata(
        &mut self,
        unix_millis: i64,
        mut uuid: impl FnMut() -> String,
    ) -> Result<()> {
        if unix_millis < 0 {
            return Err("invalid metadata timestamp");
        }
        // LOW is runtime-observed. MEDIUM/HIGH enum values are compiled public
        // ModelSource facts, not a claim that all tiers were runtime-observed.
        let model_enum = match self.wire["model"].as_str() {
            Some("gemini-4-argon-low") => "MODEL_PLACEHOLDER_M419",
            Some("gemini-4-argon-medium") => "MODEL_PLACEHOLDER_M418",
            Some("gemini-4-argon-high") => "MODEL_PLACEHOLDER_M417",
            Some("claude-opus-5-5-low") => "MODEL_PLACEHOLDER_M400",
            Some("claude-opus-5-5-medium") => "MODEL_PLACEHOLDER_M401",
            Some("claude-opus-5-5-high") => "MODEL_PLACEHOLDER_M402",
            _ => return Err("unsupported metadata model"),
        };
        let state = match &self.metadata {
            Metadata::Fresh => replay::State {
                lineage: replay::Lineage::new(uuid(), uuid())?,
                native_checkpoint_index: 0,
                user_execution: replay::UserExecution {
                    active_id: uuid(),
                    prior_recorded_id: None,
                },
            },
            Metadata::Continuation {
                state,
                new_user_execution,
                execution_ids,
            } => {
                let mut state = state.clone();
                if *new_user_execution {
                    let active = uuid();
                    if execution_ids.contains(&active) {
                        return Err("USER execution UUID collision");
                    }
                    state.user_execution.active_id = active;
                }
                state
            }
            _ => return Err("metadata requires a fresh or validated continuation"),
        };
        state.validate()?;
        let native_checkpoint_index = state.native_checkpoint_index;
        let last_step = native_checkpoint_index
            .checked_mul(2)
            .ok_or("step overflow")?;
        let outer_step = last_step.checked_add(1).ok_or("step overflow")?;
        let lineage = &state.lineage;
        self.wire["request"]["labels"] = json!({
            "model_enum":model_enum, "used_claude":"false", "used_claude_conservative":"false",
            "used_non_gemini_model":"false", "last_step_index":last_step.to_string(),
            "cascade_id":lineage.conversation_id, "root_cascade_id":lineage.conversation_id,
            "trajectory_id":lineage.trajectory_id, "request_id":format!("{}-{native_checkpoint_index}", lineage.trajectory_id)
        });
        if let Some(prior) = &state.user_execution.prior_recorded_id {
            self.wire["request"]["labels"]["last_execution_id"] = json!(prior);
        }
        self.wire["requestId"] = json!(format!(
            "agent/{}/{unix_millis}/{}/{outer_step}",
            lineage.conversation_id, lineage.trajectory_id
        ));
        self.session = replay::BACKEND_SESSION.to_owned();
        self.wire["request"]["sessionId"] = json!(self.session);
        self.metadata = Metadata::Ready(state);
        Ok(())
    }
}

pub(crate) fn cache_hint(value: Option<&Value>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    let Some(obj) = value.as_object() else {
        return Err("unsupported cache hint");
    };
    if obj.get("type").and_then(Value::as_str) != Some("ephemeral") {
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

const CLAUDE_CODE_SYSTEM_HEADERS: [&str; 3] = [
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];

fn strip_claude_code_system_header(mut text: &str) -> Option<&str> {
    let mut stripped_any = false;
    loop {
        let trimmed = text.trim_start_matches([' ', '\t', '\r', '\n']);
        if let Some(rest) = trimmed.strip_prefix("x-anthropic-billing-header:") {
            stripped_any = true;
            let after_line = rest.split_once('\n').map_or("", |(_, tail)| tail);
            text = after_line
                .strip_prefix("\r\n")
                .or_else(|| after_line.strip_prefix('\n'))
                .unwrap_or(after_line);
            continue;
        }
        if let Some(after_header) = CLAUDE_CODE_SYSTEM_HEADERS
            .iter()
            .find_map(|header| trimmed.strip_prefix(header))
        {
            let after_ws = after_header.trim_start_matches([' ', '\t']);
            if after_ws.is_empty() || after_ws.starts_with('\n') || after_ws.starts_with("\r\n") {
                stripped_any = true;
                let after_line = after_ws
                    .strip_prefix("\r\n")
                    .or_else(|| after_ws.strip_prefix('\n'))
                    .unwrap_or(after_ws);
                text = after_line
                    .strip_prefix("\r\n")
                    .or_else(|| after_line.strip_prefix('\n'))
                    .unwrap_or(after_line);
                continue;
            }
        }
        break;
    }
    if stripped_any && text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn sanitize_system_parts(parts: Vec<Value>) -> Vec<Value> {
    let filtered: Vec<Value> = parts
        .into_iter()
        .filter_map(|part| {
            let text = part["text"].as_str()?;
            strip_claude_code_system_header(text).map(|cleaned| json!({"text": cleaned}))
        })
        .collect();
    if filtered.len() > 1 {
        let joined = filtered
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        vec![json!({"text": joined})]
    } else {
        filtered
    }
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

fn format_prefix_tool_result(block: &Value) -> Result<String> {
    keys(
        block,
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
    let rendered = match block.get("content") {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(value) => text_parts(value)?
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let is_error = block
        .get("is_error")
        .map(|v| v.as_bool().ok_or("invalid is_error flag"))
        .transpose()?
        .unwrap_or(false);
    Ok(if is_error {
        format!("[tool_result ({id}) error: {rendered}]")
    } else {
        format!("[tool_result ({id}): {rendered}]")
    })
}

fn user_parts(value: &Value, allow_prefix_tool_result: bool) -> Result<Vec<Value>> {
    if !allow_prefix_tool_result {
        return text_parts(value);
    }
    if let Some(text) = value.as_str() {
        return Ok(vec![json!({"text":text})]);
    }
    let mut parts = Vec::new();
    for block in value
        .as_array()
        .ok_or("content must be text or text blocks")?
    {
        match block["type"].as_str() {
            Some("text") => {
                keys(block, &["type", "text", "cache_control"], &["type", "text"])?;
                cache_hint(block.get("cache_control"))?;
                parts.push(json!({"text":block["text"].as_str().ok_or("text must be a string")?}));
            }
            Some("tool_result") => {
                let formatted = format_prefix_tool_result(block)?;
                parts.push(json!({"text":formatted}));
            }
            _ => return Err("non-text system/tool-result content is unsupported"),
        }
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
        "service_tier",
        "betas",
        "anthropic_beta",
    ];
    if body.extra.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err("unsupported Antigravity semantic request option");
    }
    let mut display = "full";
    if let Some(thinking) = body.extra.get("thinking") {
        keys(thinking, &["type", "display", "budget_tokens"], &["type"])?;
        match thinking["type"].as_str() {
            Some("adaptive") => {
                if thinking.get("budget_tokens").is_some() {
                    return Err("adaptive thinking does not take budget_tokens");
                }
            }
            Some("enabled") => {
                if thinking
                    .get("budget_tokens")
                    .and_then(Value::as_u64)
                    .is_none_or(|n| n < 1024)
                {
                    return Err(
                        "Antigravity supports adaptive thinking or enabled thinking with budget_tokens >= 1024",
                    );
                }
            }
            _ => {
                return Err(
                    "Antigravity supports adaptive thinking only; model tier controls reasoning",
                );
            }
        }
        if let Some(value) = thinking.get("display") {
            match value.as_str() {
                Some("omitted") => display = "omitted",
                Some("full") => display = "full",
                _ => return Err("unsupported thinking display"),
            }
        }
    }
    if let Some(output) = body.extra.get("output_config") {
        keys(output, &["effort"], &[])?;
        if output.get("effort").is_some_and(|v| {
            !matches!(
                v.as_str(),
                Some("low" | "medium" | "high" | "xhigh" | "max" | "auto")
            )
        }) {
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
    if let Some(metadata) = body.extra.get("metadata")
        && (!metadata.is_object() || metadata.get("user_id").is_some_and(|v| !v.is_string()))
    {
        return Err("invalid metadata");
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

fn coalesce_claude_native_turn(native: &[Value]) -> Vec<Value> {
    let mut merged_parts: Vec<Value> = Vec::new();
    for content in native {
        let Some(parts) = content["parts"].as_array() else {
            continue;
        };
        for part in parts {
            if part["thought"] == true {
                let chunk = part["text"].as_str().unwrap_or("");
                let sig = part.get("thoughtSignature").cloned();
                if let Some(last) = merged_parts.last_mut().filter(|p| p["thought"] == true) {
                    if let Some(Value::String(prev)) = last.get_mut("text") {
                        prev.push_str(chunk);
                    }
                    if let Some(sig) = sig {
                        last["thoughtSignature"] = sig;
                    }
                } else {
                    let mut item = json!({"thought": true, "text": chunk});
                    if let Some(sig) = sig {
                        item["thoughtSignature"] = sig;
                    }
                    merged_parts.push(item);
                }
            } else if part.get("functionCall").is_some() {
                merged_parts.push(part.clone());
            } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                if text.is_empty() && part.get("thoughtSignature").is_none() {
                    continue;
                }
                if part.get("thoughtSignature").is_none()
                    && let Some(last) = merged_parts.last_mut().filter(|p| {
                        p.get("text").is_some()
                            && p.get("thought").is_none()
                            && p.get("thoughtSignature").is_none()
                    })
                {
                    if let Some(Value::String(prev)) = last.get_mut("text") {
                        prev.push_str(text);
                    }
                } else {
                    merged_parts.push(part.clone());
                }
            } else {
                merged_parts.push(part.clone());
            }
        }
    }
    merged_parts.retain(|p| p["thought"] != true || p.get("thoughtSignature").is_some());
    if merged_parts.is_empty() {
        Vec::new()
    } else {
        vec![json!({"role": "model", "parts": merged_parts})]
    }
}

fn continuation_input(envelope: &Value, user: &Message) -> Result<bool> {
    match nonempty(&envelope["checkpoint_stop_reason"])? {
        "end_turn" => {
            if text_parts(&user.content)?.is_empty() {
                return Err("continuation requires ordinary text");
            }
            Ok(true)
        }
        "tool_use" => Ok(false),
        _ => Err("unqualified continuation after incomplete checkpoint"),
    }
}

fn is_prior_or_unsigned_assistant(content: &Value, native_model: &str) -> bool {
    content
        .get(0)
        .is_none_or(|block| block["type"] != "thinking")
        || replay::is_prior_carrier_turn(content, native_model)
}

fn observed_continuation(
    messages: &[Cow<'_, Message>],
    envelopes: &[Value],
    model: &str,
    has_prefix: bool,
) -> Result<Metadata> {
    let ordered = messages
        .iter()
        .filter(|m| m.role != "system")
        .collect::<Vec<_>>();
    if !MODELS.contains(&model)
        || envelopes.is_empty()
        || ordered.len() != envelopes.len().saturating_mul(2).saturating_add(1)
        || ordered
            .iter()
            .enumerate()
            .any(|(i, m)| m.role != if i % 2 == 0 { "user" } else { "assistant" })
    {
        return Err("unsupported continuation role sequence");
    }
    if user_parts(&ordered[0].content, has_prefix)?.is_empty() {
        return Err("request needs an initial text user");
    }
    let mut new_user_execution = false;
    for (index, envelope) in envelopes.iter().enumerate() {
        new_user_execution = continuation_input(envelope, ordered[(index + 1) * 2])?;
    }
    let last = envelopes.last().unwrap();
    let mut state = replay::State::from_envelope(last)?;
    state.native_checkpoint_index = state
        .native_checkpoint_index
        .checked_add(1)
        .filter(|n| *n < replay::MAX_CHECKPOINTS)
        .ok_or("checkpoint limit")?;
    if new_user_execution {
        state.user_execution.prior_recorded_id = state
            .recorded_after(nonempty(&last["checkpoint_stop_reason"])?)?
            .map(str::to_owned);
        // Active is replaced only at the post-access USER execution boundary.
    }
    let execution_ids = envelopes
        .iter()
        .map(|e| {
            e["user_execution"]["active_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    Ok(Metadata::Continuation {
        state,
        new_user_execution,
        execution_ids,
    })
}

/// Local semantic estimate only, never generation admission. Counts have no
/// owner/profile reads, refresh/discovery, clock/ID allocation, lineage
/// mutation or scope publication. Failed/unknown scope is not filled from a
/// caller claim; successful serving is the sole publisher of trusted inputs.
pub(crate) fn estimate(body: &MessagesRequest, inputs: CountInputs<'_>) -> Result<u64> {
    let model = body.model.as_deref().ok_or("missing model")?;
    if !MODELS.contains(&model) {
        return Err("unsupported Antigravity model");
    }
    let native_model = model.strip_prefix("antigravity/").unwrap();
    let binding = if let Some(scope) = inputs.serving_scope {
        if scope.profile_id.is_empty()
            || scope.project_id.is_empty()
            || inputs
                .configured_project
                .is_some_and(|p| p != scope.project_id.as_str())
        {
            return Err("inconsistent published Antigravity scope");
        }
        Binding {
            profile_id: scope.profile_id.clone(),
            project_id: scope.project_id.clone(),
            native_model: native_model.to_owned(),
        }
    } else {
        let messages = logical_messages(&body.messages)?;
        if let Some(turn) = messages.iter().rev().find(|m| {
            m.role == "assistant"
                && m.content
                    .get(0)
                    .is_some_and(|block| block["type"] == "thinking")
        }) {
            replay::claimed_binding(&turn.content, native_model, inputs.configured_project)?
        } else {
            Binding {
                profile_id: "unconfigured".into(),
                project_id: inputs
                    .configured_project
                    .unwrap_or("unconfigured")
                    .to_owned(),
                native_model: native_model.to_owned(),
            }
        }
    };
    prepare(body, &binding).map(|p| p.estimate)
}

pub(crate) fn prepare(body: &MessagesRequest, binding: &Binding) -> Result<Prepared> {
    let model = body.model.as_deref().ok_or("missing model")?;
    if !MODELS.contains(&model) || model.strip_prefix("antigravity/") != Some(&binding.native_model)
    {
        return Err("unsupported Antigravity model or replay scope");
    }
    let tier = match model {
        "antigravity/gemini-4-argon-low" | "antigravity/claude-opus-5-5-low" => "LOW",
        "antigravity/gemini-4-argon-medium" | "antigravity/claude-opus-5-5-medium" => "MEDIUM",
        _ => "HIGH",
    };
    let cap = body.max_tokens.unwrap_or(65536);
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
    let system = sanitize_system_parts(system);
    let start_idx = messages
        .iter()
        .rposition(|m| {
            m.role == "assistant"
                && is_prior_or_unsigned_assistant(&m.content, &binding.native_model)
        })
        .filter(|&last_unsigned| {
            let prefix = &messages[..=last_unsigned];
            let no_current_thinking = prefix.iter().all(|m| {
                m.role != "assistant"
                    || is_prior_or_unsigned_assistant(&m.content, &binding.native_model)
            });
            let non_system: Vec<_> = prefix.iter().filter(|m| m.role != "system").collect();
            let valid_alternation = !non_system.is_empty()
                && non_system.len() % 2 == 0
                && non_system
                    .iter()
                    .enumerate()
                    .all(|(i, m)| m.role == if i % 2 == 0 { "user" } else { "assistant" });
            no_current_thinking && valid_alternation && last_unsigned + 1 < messages.len()
        })
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let (prefix_messages, active_messages) = messages.split_at(start_idx);
    let has_prefix = !prefix_messages.is_empty();
    let mut turns = active_messages
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content.clone())
        .collect::<Vec<_>>();
    for turn in &mut turns {
        replay::canonicalize_tool_inputs(turn);
    }
    let envelopes = replay::decode_history(&json!(turns), binding)?;
    let metadata = if turns.is_empty() {
        let input = active_messages
            .iter()
            .filter(|m| m.role != "system")
            .collect::<Vec<_>>();
        if input.len() != 1
            || input[0].role != "user"
            || user_parts(&input[0].content, has_prefix)?.is_empty()
        {
            return Err("fresh execution requires one text user input");
        }
        Metadata::Fresh
    } else {
        observed_continuation(active_messages, &envelopes, model, has_prefix)?
    };
    let session = String::new();
    let mut history = envelopes.into_iter();
    let mut pending: Vec<(Value, Value)> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut last_role = "";
    let mut estimate_bytes = serde_json::to_vec(&system)
        .map_err(|_| "invalid system")?
        .len();
    estimate_bytes += serde_json::to_vec(&tools.declarations)
        .map_err(|_| "invalid tools")?
        .len();
    for message in prefix_messages {
        if message.role == "system" {
            continue;
        }
        if message.role == "user" {
            let blocks = if let Some(text) = message.content.as_str() {
                vec![json!({"type":"text","text":text})]
            } else {
                message
                    .content
                    .as_array()
                    .ok_or("invalid user content")?
                    .clone()
            };
            let mut parts = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        keys(
                            &block,
                            &["type", "text", "cache_control"],
                            &["type", "text"],
                        )?;
                        cache_hint(block.get("cache_control"))?;
                        let value = block["text"].as_str().ok_or("invalid user text")?;
                        estimate_bytes += value.len();
                        parts.push(json!({"text":value}));
                    }
                    Some("tool_result") => {
                        let formatted = format_prefix_tool_result(&block)?;
                        estimate_bytes += formatted.len();
                        parts.push(json!({"text":formatted}));
                    }
                    _ => return Err("unsupported user content/media"),
                }
            }
            if !parts.is_empty() {
                if contents.last().is_some_and(|c| c["role"] == "user") {
                    contents
                        .last_mut()
                        .unwrap()
                        .get_mut("parts")
                        .and_then(Value::as_array_mut)
                        .unwrap()
                        .extend(parts);
                } else {
                    if contents.len() >= 1024 {
                        return Err("native history content limit");
                    }
                    contents.push(json!({"role":"user","parts":parts}));
                }
                last_role = "user";
            }
        } else if message.role == "assistant" {
            let blocks = message
                .content
                .as_array()
                .ok_or("invalid assistant content")?;
            let mut parts = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("thinking") => {}
                    Some("text") => {
                        let text = block["text"].as_str().ok_or("invalid display text")?;
                        if !text.is_empty() {
                            estimate_bytes += text.len();
                            parts.push(json!({"text":text}));
                        }
                    }
                    Some("tool_use") => {
                        let id = nonempty(&block["id"])?;
                        let name = nonempty(&block["name"])?;
                        let input = serde_json::to_string(&block["input"])
                            .map_err(|_| "invalid tool input")?;
                        let formatted = format!("[tool_use {name}(id={id}): {input}]");
                        estimate_bytes += formatted.len();
                        parts.push(json!({"text":formatted}));
                    }
                    _ => return Err("unsupported projection block"),
                }
            }
            if parts.is_empty() {
                parts.push(json!({"text":"..."}));
            }
            if contents.last().is_some_and(|c| c["role"] == "model") {
                contents
                    .last_mut()
                    .unwrap()
                    .get_mut("parts")
                    .and_then(Value::as_array_mut)
                    .unwrap()
                    .extend(parts);
            } else {
                if contents.len() >= 1024 {
                    return Err("native history content limit");
                }
                contents.push(json!({"role":"model","parts":parts}));
            }
            last_role = "assistant";
        } else {
            return Err("unsupported message role");
        }
    }
    let mut allow_prefix_tool_result = has_prefix;
    for message in active_messages {
        if message.role == "system" {
            continue;
        }
        if message.role == "assistant" {
            allow_prefix_tool_result = false;
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
            if binding.native_model.starts_with("claude-") {
                let coalesced = coalesce_claude_native_turn(native);
                if contents.len().saturating_add(coalesced.len()) > 1024 {
                    return Err("native history content limit");
                }
                contents.extend(coalesced);
            } else {
                if contents.len().saturating_add(native.len()) > 1024 {
                    return Err("native history content limit");
                }
                contents.extend(native.iter().cloned());
            }
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
                        if allow_prefix_tool_result {
                            let formatted = format_prefix_tool_result(&block)?;
                            estimate_bytes += formatted.len();
                            text.push(json!({"text":formatted}));
                            continue;
                        }
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
            allow_prefix_tool_result = false;
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
        request["systemInstruction"] = json!({"role":"user","parts":system});
    }
    if !tools.declarations.as_array().unwrap().is_empty() {
        request["tools"] = json!(
            tools
                .declarations
                .as_array()
                .unwrap()
                .iter()
                .map(|declaration| json!({"functionDeclarations":[declaration]}))
                .collect::<Vec<_>>()
        );
        if body
            .extra
            .get("tool_choice")
            .is_some_and(|choice| choice["type"] == "auto")
        {
            request["toolConfig"] = json!({"functionCallingConfig":{"mode":"VALIDATED"}});
        }
    }
    let wire = json!({"project":binding.project_id,"model":binding.native_model,"request":request,
        "requestId":"","userAgent":"antigravity","requestType":"agent"});
    Ok(Prepared {
        wire,
        session,
        display,
        tools,
        estimate: ((estimate_bytes as u64).div_ceil(3)).max(1),
        metadata,
    })
}
