use super::{
    config::RequestMetadata,
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
        LegacyId,
    }
    thread_local! { static COUNTS: Cell<[usize;6]> = const { Cell::new([0;6]) }; }
    pub(crate) fn note(kind: Kind) {
        COUNTS.with(|cell| {
            let mut counts = cell.get();
            counts[kind as usize] += 1;
            cell.set(counts);
        });
    }
    pub(crate) fn reset() {
        COUNTS.with(|cell| cell.set([0; 6]));
    }
    pub(crate) fn snapshot() -> [usize; 6] {
        COUNTS.with(Cell::get)
    }
    pub(crate) fn count(kind: Kind) -> usize {
        snapshot()[kind as usize]
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

fn legacy_session() -> String {
    #[cfg(test)]
    effects::note(effects::Kind::LegacyId);
    replay::mint_session()
}

fn legacy_outer_id() -> String {
    #[cfg(test)]
    effects::note(effects::Kind::LegacyId);
    format!("ccp-{}", uuid::Uuid::new_v4())
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum Purpose {
    Generate,
    Estimate,
}

pub(crate) enum Metadata {
    Legacy,
    Fresh,
    Continuation {
        state: replay::v2::State,
        new_user_execution: bool,
        execution_ids: Vec<String>,
    },
    Ready(replay::v2::State),
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
            Metadata::Fresh => replay::v2::State {
                lineage: replay::v2::Lineage::new(uuid(), uuid())?,
                native_checkpoint_index: 0,
                user_execution: replay::v2::UserExecution {
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
                        return Err("V2 USER execution UUID collision");
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
            .ok_or("V2 step overflow")?;
        let outer_step = last_step.checked_add(1).ok_or("V2 step overflow")?;
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
        self.session = replay::v2::BACKEND_SESSION.to_owned();
        self.wire["request"]["sessionId"] = json!(self.session);
        self.metadata = Metadata::Ready(state);
        Ok(())
    }
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

fn sanitize_system_parts(parts: Vec<Value>, policy: RequestMetadata) -> Vec<Value> {
    let filtered: Vec<Value> = parts
        .into_iter()
        .filter_map(|part| {
            let text = part["text"].as_str()?;
            strip_claude_code_system_header(text).map(|cleaned| json!({"text": cleaned}))
        })
        .collect();
    if policy == RequestMetadata::Agy && filtered.len() > 1 {
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

fn native_checkpoint_parts(envelope: &Value) -> Result<()> {
    let contents = envelope["contents"].as_array().unwrap();
    if contents.is_empty() {
        return Err("V2 continuation requires native content");
    }
    let is_claude = envelope["native_model"]
        .as_str()
        .is_some_and(|m| m.starts_with("claude-"));
    // Preserve every raw content/signature and original codec. A completed
    // assistant checkpoint counts once regardless of native SSE fragmentation.
    for content in contents {
        keys(content, &["role", "parts"], &["role", "parts"])?;
        let parts = content["parts"].as_array().ok_or("invalid native parts")?;
        if parts.is_empty() {
            return Err("V2 continuation requires native parts");
        }
        for part in parts {
            if part.get("functionCall").is_some() {
                let required: &[&str] = if is_claude {
                    &["functionCall"]
                } else {
                    &["functionCall", "thoughtSignature"]
                };
                keys(part, &["functionCall", "thoughtSignature"], required)?;
                if let Some(sig) = part.get("thoughtSignature") {
                    nonempty(sig)?;
                }
            } else {
                keys(part, &["text", "thought", "thoughtSignature"], &["text"])?;
                if !part["text"].is_string() {
                    return Err("unsupported V2 native part");
                }
            }
            if part.get("thought").is_some_and(|v| !v.is_boolean())
                || part.get("thoughtSignature").is_some_and(|v| !v.is_string())
            {
                return Err("unsupported V2 native part");
            }
        }
    }
    Ok(())
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
    native_checkpoint_parts(envelope)?;
    match nonempty(&envelope["checkpoint_stop_reason"])? {
        "end_turn" => {
            if text_parts(&user.content)?.is_empty() {
                return Err("V2 continuation requires ordinary text");
            }
            Ok(true)
        }
        "tool_use" => {
            // Only the captured single complete, ID-bearing, successful string
            // result is qualified here. Parallel/error/ID-less/compound batches
            // must not acquire invented Go counter/grouping semantics.
            let mapping = envelope["tool_mapping"].as_array().unwrap();
            let calls = replay::calls(&envelope["contents"])?;
            let is_claude = envelope["native_model"]
                .as_str()
                .is_some_and(|m| m.starts_with("claude-"));
            if is_claude {
                if mapping.is_empty()
                    || mapping.len() != calls.len()
                    || calls.iter().any(|c| c.2.get("id").is_none())
                {
                    return Err("unqualified V2 tool continuation shape");
                }
                let blocks = user
                    .content
                    .as_array()
                    .filter(|b| b.len() >= mapping.len())
                    .ok_or("V2 tool continuation requires one result")?;
                let expected_ids: HashSet<&str> = mapping
                    .iter()
                    .filter_map(|m| m["local_id"].as_str())
                    .collect();
                let mut seen_ids = HashSet::new();
                for block in blocks.iter().take(mapping.len()) {
                    let Some(id) = block["tool_use_id"].as_str() else {
                        return Err("unqualified V2 tool result");
                    };
                    if block["type"] != "tool_result"
                        || !expected_ids.contains(id)
                        || !seen_ids.insert(id)
                        || !block["content"].is_string()
                        || block
                            .get("is_error")
                            .is_some_and(|v| v.as_bool() != Some(false))
                    {
                        return Err("unqualified V2 tool result");
                    }
                }
                Ok(false)
            } else {
                if mapping.len() != 1 || calls.len() != 1 || calls[0].2.get("id").is_none() {
                    return Err("unqualified V2 tool continuation shape");
                }
                let blocks = user
                    .content
                    .as_array()
                    .filter(|b| b.len() == 1)
                    .ok_or("V2 tool continuation requires one result")?;
                let block = &blocks[0];
                if block["type"] != "tool_result"
                    || block["tool_use_id"] != mapping[0]["local_id"]
                    || !block["content"].is_string()
                    || block
                        .get("is_error")
                        .is_some_and(|v| v.as_bool() != Some(false))
                {
                    return Err("unqualified V2 tool result");
                }
                Ok(false)
            }
        }
        _ => Err("unqualified continuation after incomplete V2 checkpoint"),
    }
}

fn observed_continuation(
    messages: &[Cow<'_, Message>],
    envelopes: &[Value],
    model: &str,
) -> Result<Metadata> {
    // The state machine is shared by the existing three source-defined tiers.
    // Genuine official runtime reference/entitlement evidence remains LOW-only.
    // Keep existing message/history limits and checked proxy-owned progression.
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
        return Err("unsupported V2 continuation role sequence");
    }
    if text_parts(&ordered[0].content)?.is_empty() {
        return Err("V2 needs an initial text user");
    }
    let mut new_user_execution = false;
    for (index, envelope) in envelopes.iter().enumerate() {
        new_user_execution = continuation_input(envelope, ordered[(index + 1) * 2])?;
    }
    let last = envelopes.last().unwrap();
    let mut state = replay::v2::State::from_envelope(last)?;
    state.native_checkpoint_index = state
        .native_checkpoint_index
        .checked_add(1)
        .filter(|n| *n < replay::v2::MAX_CHECKPOINTS)
        .ok_or("V2 checkpoint limit")?;
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

/// Local semantic estimate only, never generation admission. New-policy counts
/// have no owner/profile reads, refresh/discovery, clock/ID allocation, lineage
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
    prepare_inner(body, &binding, RequestMetadata::Agy, Purpose::Estimate).map(|p| p.estimate)
}

pub(crate) fn prepare(body: &MessagesRequest, binding: &Binding) -> Result<Prepared> {
    prepare_with_policy(body, binding, RequestMetadata::Legacy)
}

pub(crate) fn prepare_with_policy(
    body: &MessagesRequest,
    binding: &Binding,
    policy: RequestMetadata,
) -> Result<Prepared> {
    prepare_inner(body, binding, policy, Purpose::Generate)
}

fn prepare_inner(
    body: &MessagesRequest,
    binding: &Binding,
    policy: RequestMetadata,
    purpose: Purpose,
) -> Result<Prepared> {
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
    let system = sanitize_system_parts(system, policy);
    let v2_start_idx = if policy == RequestMetadata::Agy {
        messages
            .iter()
            .rposition(|m| {
                m.role == "assistant"
                    && m.content
                        .get(0)
                        .is_none_or(|block| block["type"] != "thinking")
            })
            .map(|idx| idx + 1)
            .unwrap_or(0)
    } else {
        0
    };
    let (prefix_messages, active_messages) = messages.split_at(v2_start_idx);
    let turns = active_messages
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content.clone())
        .collect::<Vec<_>>();
    // Classify ONLY the leading carrier. V1 deliberately ignores noncarrier
    // signatures, including strings that happen to look like another prefix.
    let v2_turns = turns
        .iter()
        .filter(|turn| {
            turn.get(0).is_some_and(|block| {
                block["type"] == "thinking"
                    && block["signature"]
                        .as_str()
                        .is_some_and(|s| s.starts_with(replay::v2::PREFIX))
            })
        })
        .count();
    let (envelopes, metadata) = if v2_turns != 0 {
        if v2_turns != turns.len() {
            return Err("mixed transcript carrier versions are unsupported");
        }
        if policy != RequestMetadata::Agy {
            return Err("V2 history requires the agy request metadata policy");
        }
        let envelopes = replay::v2::decode_history(&json!(turns), binding)?;
        let metadata = observed_continuation(active_messages, &envelopes, model)?;
        (envelopes, metadata)
    } else {
        let envelopes = replay::decode_history(&json!(turns), binding)?;
        let metadata = if policy == RequestMetadata::Agy && turns.is_empty() {
            let input = active_messages
                .iter()
                .filter(|m| m.role != "system")
                .collect::<Vec<_>>();
            if input.len() != 1
                || input[0].role != "user"
                || text_parts(&input[0].content)?.is_empty()
            {
                return Err("fresh agy execution requires one text user input");
            }
            Metadata::Fresh
        } else {
            Metadata::Legacy
        };
        (envelopes, metadata)
    };
    let session = if matches!(metadata, Metadata::Legacy) {
        envelopes
            .first()
            .map(|e| e["session_id"].as_str().unwrap().to_string())
            .unwrap_or_else(|| {
                if purpose == Purpose::Generate {
                    legacy_session()
                } else {
                    String::new()
                }
            })
    } else {
        // No new-policy wire execution metadata is rendered before unchanged
        // credential access and actual binding validation. Legacy stays as-is.
        String::new()
    };
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
                        let formatted = if is_error {
                            format!("[tool_result ({id}) error: {rendered}]")
                        } else {
                            format!("[tool_result ({id}): {rendered}]")
                        };
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
    for message in active_messages {
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
    let outer_id = if matches!(metadata, Metadata::Legacy) && purpose == Purpose::Generate {
        legacy_outer_id()
    } else {
        String::new()
    };
    let wire = json!({"project":binding.project_id,"model":binding.native_model,"request":request,
        "requestId":outer_id,"userAgent":"antigravity","requestType":"agent"});
    Ok(Prepared {
        wire,
        session,
        display,
        tools,
        estimate: ((estimate_bytes as u64).div_ceil(3)).max(1),
        metadata,
    })
}
