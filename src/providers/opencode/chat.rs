use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use base64::Engine;
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use super::client::{OpenCodeError, OpenCodeResponse};
use crate::anthropic::{
    schema::MessagesRequest,
    sse::{encode_sse_event, parse_sse_events},
};
use crate::monitor::{MonitorHandle, usage_from_anthropic_sse};
use crate::providers::{
    grok::translate::stream::SseDecoder,
    translate_shared::{
        ContentBlock, flatten_system_text, image_source_to_url, normalize_content, read_effort,
    },
};
use crate::traffic::{StreamTrafficCapture, TrafficCapture};

const DEFAULT_MAX_TOKENS: u32 = 32_000;

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
    pub stream_options: StreamOptions,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Translate the Anthropic Messages wire model to the OpenAI-compatible wire
/// model accepted by OpenCode Go's `/chat/completions` endpoint.
pub fn prepare_request(req: &MessagesRequest, model: &str) -> anyhow::Result<ChatRequest> {
    let messages = build_messages(req, model)?;
    let tools = read_tools(req)?;
    let (tool_choice, parallel_tool_calls) = read_tool_choice(req)?;
    if tools.is_empty()
        && tool_choice
            .as_ref()
            .is_some_and(|choice| choice.as_str() != Some("none"))
    {
        anyhow::bail!("tool_choice requires at least one tool");
    }

    Ok(ChatRequest {
        model: model.to_string(),
        messages,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        parallel_tool_calls,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        max_tokens: req
            .max_tokens
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS),
        reasoning_effort: map_reasoning_effort(req, model)?,
    })
}

fn build_messages(req: &MessagesRequest, model: &str) -> anyhow::Result<Vec<Value>> {
    let deepseek = model.to_ascii_lowercase().contains("deepseek");
    let mut system = Vec::new();
    if let Some(text) = flatten_system_text(req.extra.get("system")) {
        system.push(text);
    }

    let mut messages = Vec::new();
    for message in &req.messages {
        let blocks = normalize_content(&message.content, json!({}));
        match message.role.as_str() {
            "system" | "developer" => {
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "user" => push_user_messages(&mut messages, &blocks),
            "assistant" => {
                if let Some(message) = assistant_message(&blocks, deepseek)? {
                    messages.push(message);
                }
            }
            other => anyhow::bail!("unexpected message role: {other}"),
        }
    }

    if !system.is_empty() {
        messages.insert(
            0,
            json!({
                "role": "system",
                "content": system.join("\n\n"),
            }),
        );
    }
    Ok(messages)
}

fn push_user_messages(messages: &mut Vec<Value>, blocks: &[ContentBlock]) {
    let mut content = Vec::new();
    let mut tool_messages = Vec::new();
    let flush = |messages: &mut Vec<Value>, content: &mut Vec<Value>| {
        if content.is_empty() {
            return;
        }
        let value = if content
            .iter()
            .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        {
            Value::String(
                content
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect(),
            )
        } else {
            Value::Array(std::mem::take(content))
        };
        messages.push(json!({"role":"user", "content":value}));
        content.clear();
    };

    for block in blocks {
        match block {
            ContentBlock::Text { text } => content.push(json!({"type":"text", "text":text})),
            ContentBlock::Image { source } => content.push(json!({
                "type":"image_url",
                "image_url":{"url":image_source_to_url(source)},
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content: result,
                is_error,
            } => {
                let rendered = render_tool_result(result, is_error.unwrap_or(false));
                tool_messages.push(json!({
                    "role":"tool",
                    "tool_call_id":tool_use_id,
                    "content":rendered.text,
                }));
                content.extend(rendered.images);
            }
            ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } => {}
        }
    }
    // A parallel assistant tool-call turn requires every matching tool
    // response before any subsequent user message. Keep tool results in their
    // original order, then reattach vision parts and ordinary user content in
    // one protocol-compatible user message.
    messages.extend(tool_messages);
    flush(messages, &mut content);
}

fn assistant_message(blocks: &[ContentBlock], deepseek: bool) -> anyhow::Result<Option<Value>> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: value } => text.push_str(value),
            ContentBlock::Thinking {
                thinking,
                signature: _,
            } => {
                if !reasoning.is_empty() && !thinking.is_empty() {
                    reasoning.push_str("\n\n");
                }
                reasoning.push_str(thinking);
            }
            ContentBlock::ToolUse { id, name, input } => {
                if id.is_empty() || name.is_empty() {
                    anyhow::bail!("assistant tool_use requires non-empty id and name");
                }
                tool_calls.push(json!({
                    "id":id,
                    "type":"function",
                    "function":{
                        "name":name,
                        "arguments":serde_json::to_string(input)?,
                    },
                }));
            }
            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
        }
    }
    if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
        return Ok(None);
    }

    let mut message = serde_json::Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    message.insert("content".into(), Value::String(text));
    // DeepSeek requires the field on every replayed assistant message, even
    // when that turn did not contain visible reasoning.
    if deepseek || !reasoning.is_empty() {
        message.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Ok(Some(Value::Object(message)))
}

struct RenderedToolResult {
    text: String,
    images: Vec<Value>,
}

fn render_tool_result(value: &Value, is_error: bool) -> RenderedToolResult {
    let mut text = String::new();
    let mut images = Vec::new();
    if is_error {
        text.push_str("[tool execution error]\n");
    }
    match value {
        Value::String(value) => text.push_str(value),
        Value::Array(parts) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    Some("image") => match normalized_tool_result_image(part) {
                        Some(image_url) => {
                            images.push(json!({
                                "type":"image_url",
                                "image_url":{"url":image_url},
                            }));
                        }
                        None => text.push_str("[unsupported tool result block omitted: image]"),
                    },
                    Some(kind) => {
                        text.push_str(&format!("[unsupported tool result block omitted: {kind}]"))
                    }
                    None => text.push_str("[unsupported tool result block omitted]"),
                }
            }
        }
        other => text.push_str(&other.to_string()),
    }
    RenderedToolResult { text, images }
}

fn normalized_tool_result_image(part: &Value) -> Option<String> {
    let block = normalize_content(&Value::Array(vec![part.clone()]), json!({}))
        .into_iter()
        .next()?;
    let ContentBlock::Image { source } = block else {
        return None;
    };
    Some(image_source_to_url(&source))
}

fn read_tools(req: &MessagesRequest) -> anyhow::Result<Vec<Value>> {
    let Some(value) = req.extra.get("tools") else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
    tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow::anyhow!("tool name must be a non-empty string"))?;
            let description = tool.get("description").cloned();
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut function = serde_json::Map::new();
            function.insert("name".into(), Value::String(name.to_string()));
            if let Some(description) = description.filter(|value| !value.is_null()) {
                function.insert("description".into(), description);
            }
            function.insert("parameters".into(), parameters);
            Ok(json!({"type":"function", "function":function}))
        })
        .collect()
}

fn read_tool_choice(req: &MessagesRequest) -> anyhow::Result<(Option<Value>, Option<bool>)> {
    let Some(value) = req.extra.get("tool_choice") else {
        return Ok((None, None));
    };
    if value.is_null() {
        return Ok((None, None));
    }
    let choice = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool_choice must be an object"))?;
    let parallel = match choice.get("disable_parallel_tool_use") {
        Some(Value::Bool(disabled)) => Some(!disabled),
        Some(_) => anyhow::bail!("tool_choice.disable_parallel_tool_use must be a boolean"),
        None => None,
    };
    let translated = match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(Value::String("auto".into())),
        Some("none") => Some(Value::String("none".into())),
        Some("any") => Some(Value::String("required".into())),
        Some("tool") => {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow::anyhow!("tool_choice.name must be a non-empty string"))?;
            Some(json!({"type":"function", "function":{"name":name}}))
        }
        Some(kind) => anyhow::bail!("unsupported tool_choice type: {kind}"),
        None => anyhow::bail!("tool_choice.type must be a string"),
    };
    Ok((translated, parallel))
}

fn map_reasoning_effort(req: &MessagesRequest, model: &str) -> anyhow::Result<Option<String>> {
    let Some(effort) = read_effort(req)? else {
        return Ok(None);
    };
    let id = model.to_ascii_lowercase();
    if ["glm-5.2", "glm-5-2", "glm-5p2"]
        .iter()
        .any(|needle| id.contains(needle))
    {
        return match effort {
            "high" => Ok(Some("high".into())),
            "xhigh" | "max" => Ok(Some("max".into())),
            other => anyhow::bail!(
                "OpenCode Go model {model} does not support reasoning effort {other}; use high, xhigh, or max"
            ),
        };
    }
    if id.contains("deepseek-v4") {
        return match effort {
            "low" | "medium" | "high" | "max" => Ok(Some(effort.into())),
            "xhigh" => Ok(Some("max".into())),
            _ => unreachable!("read_effort validates the effort"),
        };
    }
    if id.contains("mimo") {
        return match effort {
            "low" | "medium" | "high" => Ok(Some(effort.into())),
            other => anyhow::bail!(
                "OpenCode Go model {model} does not support reasoning effort {other}; use low, medium, or high"
            ),
        };
    }
    // OpenCode exposes no selectable effort variants for the remaining chat
    // models. Their native/default reasoning behavior remains in effect.
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

impl StopReason {
    fn anthropic(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::ToolUse => "tool_use",
            Self::MaxTokens => "max_tokens",
        }
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct Usage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u64>,
}

impl Usage {
    fn anthropic(&self) -> Value {
        let cached = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .or(self.cached_tokens)
            .unwrap_or(0);
        json!({
            "input_tokens":self.prompt_tokens.unwrap_or(0).saturating_sub(cached),
            "output_tokens":self.completion_tokens.unwrap_or(0),
            "cache_creation_input_tokens":0,
            "cache_read_input_tokens":cached,
        })
    }
}

#[derive(Debug, serde::Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Option<Vec<Choice>>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<UpstreamError>,
}

#[derive(Debug, serde::Deserialize)]
struct UpstreamError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, serde::Deserialize)]
struct ToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ToolFunctionDelta>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ToolFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug)]
struct Block {
    index: usize,
    kind: BlockKind,
}

#[derive(Debug)]
enum BlockKind {
    Thinking {
        text: String,
    },
    Text {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        args: String,
    },
}

#[derive(Debug)]
struct ToolSlot {
    upstream_index: usize,
    block_index: usize,
    upstream_id: String,
    downstream_id: String,
    name: String,
    args: String,
    state: ToolBlockState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolBlockState {
    /// The upstream index is known, but no downstream block has been emitted.
    Pending,
    /// The downstream block has started and can still receive argument deltas.
    Open,
    /// The downstream block was emitted and stopped; it must not be reopened.
    Closed,
}

struct TranslationState {
    message_id: String,
    model: String,
    message_started: bool,
    finished: bool,
    next_block_index: usize,
    thinking_index: Option<usize>,
    text_index: Option<usize>,
    blocks: Vec<Block>,
    tools: Vec<ToolSlot>,
    pending_stop: Option<StopReason>,
    usage: Usage,
}

impl TranslationState {
    fn new(message_id: String, model: String) -> Self {
        Self {
            message_id,
            model,
            message_started: false,
            finished: false,
            next_block_index: 0,
            thinking_index: None,
            text_index: None,
            blocks: Vec::new(),
            tools: Vec::new(),
            pending_stop: None,
            usage: Usage::default(),
        }
    }

    fn apply_chunk(&mut self, chunk: StreamChunk) -> anyhow::Result<Vec<u8>> {
        if self.finished {
            anyhow::bail!("OpenCode Go event after terminal completion");
        }
        if let Some(error) = chunk.error {
            anyhow::bail!(
                "OpenCode Go upstream error: {}",
                error.message.unwrap_or_else(|| "unknown error".into())
            );
        }
        if let Some(usage) = chunk.usage {
            self.usage = usage;
        }
        let Some(choices) = chunk.choices else {
            return Ok(Vec::new());
        };
        if choices.is_empty() {
            return Ok(Vec::new());
        }
        if choices.len() != 1 {
            anyhow::bail!("OpenCode Go stream returned multiple choices");
        }
        let choice = choices.into_iter().next().expect("one choice");
        let mut out = Vec::new();
        if let Some(delta) = choice.delta {
            self.apply_delta(delta, &mut out)?;
        }
        if let Some(reason) = choice.finish_reason {
            let reason = parse_finish_reason(&reason)?;
            if self.pending_stop.is_some_and(|current| current != reason) {
                anyhow::bail!("OpenCode Go stream changed finish_reason");
            }
            self.pending_stop = Some(reason);
        }
        Ok(out)
    }

    fn apply_delta(&mut self, delta: Delta, out: &mut Vec<u8>) -> anyhow::Result<()> {
        if let Some(reasoning) = delta.reasoning_content.filter(|value| !value.is_empty()) {
            self.close_text(out);
            self.close_tools(out);
            let index = match self.thinking_index {
                Some(index) => index,
                None => {
                    let index = self.allocate_block();
                    self.ensure_message_start(out);
                    emit(
                        out,
                        "content_block_start",
                        json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                    );
                    self.blocks.push(Block {
                        index,
                        kind: BlockKind::Thinking {
                            text: String::new(),
                        },
                    });
                    self.thinking_index = Some(index);
                    index
                }
            };
            if let Some(Block {
                kind: BlockKind::Thinking { text },
                ..
            }) = self.blocks.iter_mut().find(|block| block.index == index)
            {
                text.push_str(&reasoning);
            }
            emit(
                out,
                "content_block_delta",
                json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":reasoning}}),
            );
        }

        if let Some(content) = delta.content.filter(|value| !value.is_empty()) {
            self.close_thinking(out);
            self.close_tools(out);
            let index = match self.text_index {
                Some(index) => index,
                None => {
                    let index = self.allocate_block();
                    self.ensure_message_start(out);
                    emit(
                        out,
                        "content_block_start",
                        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                    );
                    self.blocks.push(Block {
                        index,
                        kind: BlockKind::Text {
                            text: String::new(),
                        },
                    });
                    self.text_index = Some(index);
                    index
                }
            };
            if let Some(Block {
                kind: BlockKind::Text { text },
                ..
            }) = self.blocks.iter_mut().find(|block| block.index == index)
            {
                text.push_str(&content);
            }
            emit(
                out,
                "content_block_delta",
                json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":content}}),
            );
        }

        if let Some(tool_calls) = delta.tool_calls.filter(|value| !value.is_empty()) {
            self.close_thinking(out);
            self.close_text(out);
            for call in tool_calls {
                self.apply_tool_delta(call, out)?;
            }
        }
        Ok(())
    }

    fn apply_tool_delta(&mut self, delta: ToolCallDelta, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let ToolCallDelta {
            index: upstream_index,
            id,
            function,
        } = delta;
        let position = match self
            .tools
            .iter()
            .position(|slot| slot.upstream_index == upstream_index)
        {
            Some(position) => position,
            None => {
                let block_index = self.allocate_block();
                self.tools.push(ToolSlot {
                    upstream_index,
                    block_index,
                    upstream_id: String::new(),
                    downstream_id: make_tool_use_id(&self.message_id, upstream_index),
                    name: String::new(),
                    args: String::new(),
                    state: ToolBlockState::Pending,
                });
                self.tools.len() - 1
            }
        };
        if self.tools[position].state == ToolBlockState::Closed {
            anyhow::bail!(
                "OpenCode Go tool call {} continued after its content block was closed",
                upstream_index
            );
        }
        let mut new_arguments = String::new();
        {
            let slot = &mut self.tools[position];
            if let Some(id) = id {
                slot.upstream_id.push_str(&id);
            }
            if let Some(function) = function {
                if let Some(name) = function.name {
                    slot.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    new_arguments = arguments;
                    slot.args.push_str(&new_arguments);
                }
            }
        }
        let should_start = {
            let slot = &self.tools[position];
            slot.state == ToolBlockState::Pending
                && !slot.upstream_id.is_empty()
                && !slot.name.is_empty()
        };
        if should_start {
            self.ensure_message_start(out);
            let (block_index, id, name, args) = {
                let slot = &mut self.tools[position];
                slot.state = ToolBlockState::Open;
                (
                    slot.block_index,
                    slot.downstream_id.clone(),
                    slot.name.clone(),
                    slot.args.clone(),
                )
            };
            emit(
                out,
                "content_block_start",
                json!({"type":"content_block_start","index":block_index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            );
            self.blocks.push(Block {
                index: block_index,
                kind: BlockKind::Tool {
                    id,
                    name,
                    args: String::new(),
                },
            });
            if !args.is_empty() {
                emit(
                    out,
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":block_index,"delta":{"type":"input_json_delta","partial_json":args}}),
                );
                if let Some(Block {
                    kind: BlockKind::Tool { args: stored, .. },
                    ..
                }) = self
                    .blocks
                    .iter_mut()
                    .find(|block| block.index == block_index)
                {
                    stored.push_str(&args);
                }
            }
        } else if self.tools[position].state == ToolBlockState::Open && !new_arguments.is_empty() {
            let block_index = self.tools[position].block_index;
            emit(
                out,
                "content_block_delta",
                json!({"type":"content_block_delta","index":block_index,"delta":{"type":"input_json_delta","partial_json":new_arguments}}),
            );
            if let Some(Block {
                kind: BlockKind::Tool { args, .. },
                ..
            }) = self
                .blocks
                .iter_mut()
                .find(|block| block.index == block_index)
            {
                args.push_str(&new_arguments);
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> anyhow::Result<Vec<u8>> {
        if self.finished {
            return Ok(Vec::new());
        }
        for slot in &self.tools {
            if slot.state == ToolBlockState::Pending {
                anyhow::bail!(
                    "OpenCode Go tool call {} ended without id or function name",
                    slot.upstream_index
                );
            }
        }
        for block in &self.blocks {
            if let BlockKind::Tool { args, .. } = &block.kind {
                let value: Value = serde_json::from_str(if args.is_empty() { "{}" } else { args })
                    .map_err(|_| anyhow::anyhow!("OpenCode Go tool arguments are invalid JSON"))?;
                if !value.is_object() {
                    anyhow::bail!("OpenCode Go tool arguments must be a JSON object");
                }
            }
        }
        let mut out = Vec::new();
        self.close_thinking(&mut out);
        self.close_text(&mut out);
        self.close_tools(&mut out);
        self.ensure_message_start(&mut out);
        let stop = self.pending_stop.unwrap_or({
            if self.tools.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            }
        });
        emit(
            &mut out,
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":stop.anthropic(),"stop_sequence":null},"usage":self.usage.anthropic()}),
        );
        emit(&mut out, "message_stop", json!({"type":"message_stop"}));
        self.finished = true;
        Ok(out)
    }

    fn response(&self) -> anyhow::Result<Value> {
        if !self.finished {
            anyhow::bail!("OpenCode Go response is not complete");
        }
        let mut content = Vec::new();
        for block in &self.blocks {
            match &block.kind {
                BlockKind::Thinking { text } => content.push(json!({
                    "type":"thinking",
                    "thinking":text,
                    "signature":make_thinking_signature(&self.message_id, block.index),
                })),
                BlockKind::Text { text } => {
                    content.push(json!({"type":"text", "text":text}));
                }
                BlockKind::Tool { id, name, args } => {
                    let input: Value =
                        serde_json::from_str(if args.is_empty() { "{}" } else { args })?;
                    content.push(json!({
                        "type":"tool_use",
                        "id":id,
                        "name":name,
                        "input":input,
                    }));
                }
            }
        }
        let stop = self.pending_stop.unwrap_or({
            if self.tools.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            }
        });
        Ok(json!({
            "id":self.message_id,
            "type":"message",
            "role":"assistant",
            "model":self.model,
            "content":content,
            "stop_reason":stop.anthropic(),
            "stop_sequence":null,
            "usage":self.usage.anthropic(),
        }))
    }

    fn ensure_message_start(&mut self, out: &mut Vec<u8>) {
        if self.message_started {
            return;
        }
        emit(
            out,
            "message_start",
            json!({"type":"message_start","message":{"id":self.message_id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}),
        );
        self.message_started = true;
    }

    fn allocate_block(&mut self) -> usize {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    fn close_thinking(&mut self, out: &mut Vec<u8>) {
        let Some(index) = self.thinking_index.take() else {
            return;
        };
        emit(
            out,
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":make_thinking_signature(&self.message_id, index)}}),
        );
        emit(
            out,
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        );
    }

    fn close_text(&mut self, out: &mut Vec<u8>) {
        let Some(index) = self.text_index.take() else {
            return;
        };
        emit(
            out,
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        );
    }

    fn close_tools(&mut self, out: &mut Vec<u8>) {
        for slot in &mut self.tools {
            if slot.state == ToolBlockState::Open {
                emit(
                    out,
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":slot.block_index}),
                );
                slot.state = ToolBlockState::Closed;
            }
        }
    }
}

fn parse_finish_reason(reason: &str) -> anyhow::Result<StopReason> {
    match reason {
        "stop" => Ok(StopReason::EndTurn),
        "tool_calls" | "function_call" => Ok(StopReason::ToolUse),
        "length" => Ok(StopReason::MaxTokens),
        "content_filter" => anyhow::bail!("OpenCode Go response was blocked by a content filter"),
        other => anyhow::bail!("unsupported OpenCode Go finish_reason: {other}"),
    }
}

fn make_thinking_signature(message_id: &str, index: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("ccp:opencode:v1:{message_id}:{index}"))
}

fn make_tool_use_id(message_id: &str, upstream_index: usize) -> String {
    // Several Chat Completions providers restart tool-call IDs (for example,
    // `Agent_0`) in every independent completion. Nested Claude Code agents
    // are flattened into one downstream stream, so forwarding those IDs can
    // make a child tool call appear to reference itself as its parent. The
    // response message ID is generated uniquely by CCP; namespacing the slot
    // with it keeps split deltas stable and independent completions distinct.
    format!("toolu_{message_id}_{upstream_index}")
}

fn emit(out: &mut Vec<u8>, event: &str, value: Value) {
    out.extend(encode_sse_event(Some(event), &value.to_string()));
}

pub struct LiveStreamTranslator {
    decoder: SseDecoder,
    state: TranslationState,
}

impl LiveStreamTranslator {
    pub fn new(message_id: String, model: String) -> Self {
        Self {
            decoder: SseDecoder::default(),
            state: TranslationState::new(message_id, model),
        }
    }

    pub fn push(&mut self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        for event in self.decoder.push(input)? {
            let data = event.data.trim();
            if data == "[DONE]" {
                out.extend(self.state.finalize()?);
                continue;
            }
            if self.state.finished {
                validate_post_done_metadata(data)?;
                continue;
            }
            let chunk: StreamChunk = serde_json::from_str(data)
                .map_err(|_| anyhow::anyhow!("malformed OpenCode Go SSE event"))?;
            out.extend(self.state.apply_chunk(chunk)?);
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> anyhow::Result<Vec<u8>> {
        self.decoder.finish()?;
        if self.state.finished {
            return Ok(Vec::new());
        }
        if self.state.pending_stop.is_none() {
            anyhow::bail!("OpenCode Go stream ended without [DONE] or finish_reason");
        }
        self.state.finalize()
    }

    pub fn is_finished(&self) -> bool {
        self.state.finished
    }
}

fn validate_post_done_metadata(data: &str) -> anyhow::Result<()> {
    let value: Value = serde_json::from_str(data)
        .map_err(|_| anyhow::anyhow!("malformed OpenCode Go SSE event after [DONE]"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("OpenCode Go event after terminal completion"))?;
    let has_empty_choices = object
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    let has_cost = object
        .get("cost")
        .is_some_and(|cost| cost.is_string() || cost.is_number());
    let known_keys = object.keys().all(|key| {
        matches!(
            key.as_str(),
            "choices" | "cost" | "x-opencode-type" | "normalizedUsage"
        )
    });
    if has_empty_choices && has_cost && known_keys {
        return Ok(());
    }
    anyhow::bail!("OpenCode Go event after terminal completion")
}

pub fn accumulate_response(input: &[u8], message_id: &str, model: &str) -> anyhow::Result<Value> {
    let mut translator = LiveStreamTranslator::new(message_id.into(), model.into());
    translator.push(input)?;
    translator.finish()?;
    translator.state.response()
}

pub fn stream_error(message: &str) -> Vec<u8> {
    encode_sse_event(
        Some("error"),
        &json!({"type":"error","error":{"type":"api_error","message":message}}).to_string(),
    )
}

pub fn stream_body(
    upstream: OpenCodeResponse,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<TrafficCapture>>,
) -> Body {
    let state = OpenCodeChatStreamState {
        upstream: upstream.into_stream(),
        translator: LiveStreamTranslator::new(message_id, model),
        capture_decoder: SseDecoder::default(),
        terminal: false,
        error_sent: false,
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
        stream_capture: traffic.as_ref().map(|traffic| traffic.stream_capture()),
        traffic,
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        state
            .next_output()
            .await
            .map(|bytes| (Ok::<Bytes, Infallible>(Bytes::from(bytes)), state))
    });
    Body::from_stream(stream)
}

struct OpenCodeChatStreamState<S> {
    upstream: S,
    translator: LiveStreamTranslator,
    capture_decoder: SseDecoder,
    terminal: bool,
    error_sent: bool,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    chunks: u64,
    stream_capture: Option<StreamTrafficCapture>,
    traffic: Option<Arc<TrafficCapture>>,
}

impl<S> OpenCodeChatStreamState<S>
where
    S: futures_util::Stream<Item = Result<Bytes, OpenCodeError>> + Unpin,
{
    async fn next_output(&mut self) -> Option<Vec<u8>> {
        if self.terminal {
            return None;
        }
        if self.error_sent {
            self.terminal = true;
            return None;
        }
        loop {
            let chunk = match self.upstream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(_)) => return Some(self.fail_at("transport", "upstream_stream")),
                None => {
                    let output = match self.translator.finish() {
                        Ok(output) => output,
                        Err(_) => return Some(self.fail_at("decoder", "incomplete_stream")),
                    };
                    if self.capture_decoder.finish().is_err() {
                        return Some(self.fail_at("capture", "incomplete_stream"));
                    }
                    self.terminal = true;
                    self.capture_downstream(&output);
                    self.finish_capture(true);
                    return (!output.is_empty()).then_some(output);
                }
            };
            if self.bytes == 0
                && let Some(monitor) = self.monitor.as_ref()
            {
                monitor.generation_started(&self.req_id);
            }
            self.bytes = self.bytes.saturating_add(chunk.len() as u64);
            self.chunks = self.chunks.saturating_add(1);
            self.capture_upstream(&chunk);

            let output = match self.translator.push(&chunk) {
                Ok(output) => output,
                Err(_) => return Some(self.fail_at("translation", "invalid_event")),
            };
            if !output.is_empty() {
                let (input_tokens, output_tokens) = usage_from_anthropic_sse(&output);
                if let Some(monitor) = self.monitor.as_ref() {
                    monitor.stream_progress(
                        &self.req_id,
                        output.len() as u64,
                        count_sse_events(&output),
                        input_tokens,
                        output_tokens,
                    );
                }
                self.capture_downstream(&output);
            }
            if self.translator.is_finished() {
                if self.translator.finish().is_err() || self.capture_decoder.finish().is_err() {
                    return Some(self.fail_at("decoder", "trailing_incomplete_frame"));
                }
                self.terminal = true;
                self.finish_capture(true);
                return (!output.is_empty()).then_some(output);
            }
            if !output.is_empty() {
                return Some(output);
            }
        }
    }

    fn capture_upstream(&mut self, bytes: &[u8]) {
        let events = match self.capture_decoder.push(bytes) {
            Ok(events) => events,
            Err(_) => {
                if let Some(capture) = self.stream_capture.as_mut() {
                    capture.malformed("decoder", "malformed_sse");
                }
                return;
            }
        };
        let Some(capture) = self.stream_capture.as_mut() else {
            return;
        };
        for event in events {
            match serde_json::from_str(&event.data) {
                Ok(value) => capture.upstream_event(event.event.as_deref(), &value),
                Err(_) if event.data.trim() == "[DONE]" => {
                    capture.upstream_event(event.event.as_deref(), &json!("[DONE]"));
                }
                Err(_) => capture.malformed("json", "malformed_event"),
            }
        }
    }

    fn capture_downstream(&mut self, bytes: &[u8]) {
        let Some(capture) = self.stream_capture.as_mut() else {
            return;
        };
        for event in parse_sse_events(bytes) {
            if let Ok(value) = serde_json::from_str(&event.data) {
                capture.downstream_event(event.event.as_deref().unwrap_or("message"), value);
            }
        }
    }

    fn fail_at(&mut self, stage: &str, kind: &str) -> Vec<u8> {
        self.error_sent = true;
        let output = stream_error("OpenCode Go stream is invalid");
        if let Some(capture) = self.stream_capture.as_mut() {
            capture.malformed(stage, kind);
        }
        self.capture_downstream(&output);
        self.finish_capture(false);
        output
    }

    fn finish_capture(&mut self, completed: bool) {
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish_named(
                traffic,
                json!({
                    "kind":if completed { "stream_completion" } else { "stream_error" },
                    "bytes":self.bytes,
                    "chunks":self.chunks,
                }),
                "061-opencode-stream-summary",
            );
        }
    }
}

impl<S> Drop for OpenCodeChatStreamState<S> {
    fn drop(&mut self) {
        if self.terminal || self.stream_capture.is_none() {
            return;
        }
        if let (Some(capture), Some(traffic)) = (self.stream_capture.take(), self.traffic.as_ref())
        {
            capture.finish_named(
                traffic,
                json!({
                    "kind":"stream_abandoned",
                    "reason":"downstream_body_dropped",
                    "bytes":self.bytes,
                    "chunks":self.chunks,
                }),
                "061-opencode-stream-summary",
            );
        }
    }
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    parse_sse_events(bytes).len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> MessagesRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn request_maps_glm_effort_tools_parallelism_and_replay() {
        let req = request(json!({
            "model":"opencode-go/glm-5.2",
            "max_tokens":123,
            "system":"system",
            "messages":[
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"reason","signature":"opaque"},
                    {"type":"tool_use","id":"call_1","name":"lookup","input":{"q":"rust"}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"ok"}]}
            ],
            "tools":[{"name":"lookup","description":"Lookup","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"lookup","disable_parallel_tool_use":true},
            "output_config":{"effort":"xhigh"}
        }));
        let wire = serde_json::to_value(prepare_request(&req, "glm-5.2").unwrap()).unwrap();
        assert_eq!(wire["model"], "glm-5.2");
        assert_eq!(wire["max_tokens"], 123);
        assert_eq!(wire["reasoning_effort"], "max");
        assert_eq!(wire["parallel_tool_calls"], false);
        assert_eq!(wire["messages"][1]["reasoning_content"], "reason");
        assert_eq!(wire["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(wire["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(
            wire["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"rust\"}"
        );
    }

    #[test]
    fn tool_result_images_follow_the_required_tool_response_in_image_order() {
        let req = request(json!({
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_image","name":"Read","input":{"file_path":"image.png"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_image","is_error":true,"content":[
                        {"type":"text","text":"before"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"YWJj"}},
                        {"type":"text","text":"between"},
                        {"type":"image","source":{"type":"url","url":"https://example.invalid/image.webp"}},
                        {"type":"text","text":"after"}
                    ]}
                ]}
            ]
        }));

        let wire = serde_json::to_value(prepare_request(&req, "glm-5.2").unwrap()).unwrap();
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_image");
        assert_eq!(
            messages[1]["content"],
            concat!("[tool execution error]\n", "before", "between", "after")
        );
        assert!(
            !messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("unsupported tool result block omitted: image")
        );

        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"],
            json!([
                {
                    "type":"image_url",
                    "image_url":{"url":"data:image/png;base64,YWJj"}
                },
                {
                    "type":"image_url",
                    "image_url":{"url":"https://example.invalid/image.webp"}
                }
            ])
        );
    }

    #[test]
    fn tool_result_image_reattachment_preserves_surrounding_user_message_order() {
        let req = request(json!({
            "messages":[{"role":"user","content":[
                {"type":"text","text":"before tool"},
                {"type":"tool_result","tool_use_id":"call_image","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"YWJj"}}
                ]},
                {"type":"text","text":"after tool"}
            ]}]
        }));

        let wire = serde_json::to_value(prepare_request(&req, "glm-5.2").unwrap()).unwrap();
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_image");
        assert_eq!(messages[0]["content"], "");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            json!([
                {"type":"text","text":"before tool"},
                {
                    "type":"image_url",
                    "image_url":{"url":"data:image/jpeg;base64,YWJj"}
                },
                {"type":"text","text":"after tool"}
            ])
        );
    }

    #[test]
    fn parallel_tool_results_stay_contiguous_before_reattached_images() {
        let req = request(json!({
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_input","name":"Read","input":{"file_path":"input.txt"}},
                    {"type":"tool_use","id":"call_image","name":"Read","input":{"file_path":"image.png"}},
                    {"type":"tool_use","id":"call_bash","name":"Bash","input":{"command":"pwd"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_input","content":"input ok"},
                    {"type":"tool_result","tool_use_id":"call_image","content":[
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"YWJj"}}
                    ]},
                    {"type":"tool_result","tool_use_id":"call_bash","content":"bash ok"}
                ]}
            ]
        }));

        let wire = serde_json::to_value(prepare_request(&req, "deepseek-v4-pro").unwrap()).unwrap();
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["assistant", "tool", "tool", "tool", "user"]
        );
        assert_eq!(messages[1]["tool_call_id"], "call_input");
        assert_eq!(messages[2]["tool_call_id"], "call_image");
        assert_eq!(messages[2]["content"], "");
        assert_eq!(messages[3]["tool_call_id"], "call_bash");
        assert_eq!(
            messages[4]["content"],
            json!([{
                "type":"image_url",
                "image_url":{"url":"data:image/png;base64,YWJj"}
            }])
        );
    }

    #[test]
    fn deepseek_replay_always_has_reasoning_content() {
        let req = request(json!({
            "messages":[
                {"role":"assistant","content":[{"type":"text","text":"answer"}]},
                {"role":"user","content":"next"}
            ]
        }));
        let wire = serde_json::to_value(prepare_request(&req, "deepseek-v4-pro").unwrap()).unwrap();
        assert_eq!(wire["messages"][0]["reasoning_content"], "");
    }

    #[test]
    fn live_stream_preserves_fragmented_reasoning_text_tools_usage_and_indices() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":7,\"id\":\"call_1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":7,\"function\":{\"arguments\":\":\\\"rust\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
            "data: [DONE]\n\n"
        );
        for split in 0..=upstream.len() {
            let mut translator = LiveStreamTranslator::new("msg_1".into(), "glm-5.2".into());
            let mut output = translator.push(&upstream.as_bytes()[..split]).unwrap();
            output.extend(translator.push(&upstream.as_bytes()[split..]).unwrap());
            output.extend(translator.finish().unwrap());
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("thinking_delta"), "split {split}");
            assert!(rendered.contains("signature_delta"), "split {split}");
            assert!(rendered.contains("text_delta"), "split {split}");
            assert!(rendered.contains("input_json_delta"), "split {split}");
            assert!(rendered.contains("tool_use"), "split {split}");
            assert_eq!(
                rendered.matches("\"id\":\"toolu_msg_1_7\"").count(),
                1,
                "split {split}"
            );
            assert!(
                rendered.contains("cache_read_input_tokens"),
                "split {split}"
            );
            assert!(rendered.contains("message_stop"), "split {split}");
        }
    }

    #[test]
    fn live_stream_accepts_text_and_reasoning_after_a_tool_call() {
        for (upstream, expected_type, expected_value) in [
            (
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"search\",\"arguments\":\"{}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"after tool\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n"
                ),
                "text",
                "after tool",
            ),
            (
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"search\",\"arguments\":\"{}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"after tool\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n"
                ),
                "thinking",
                "after tool",
            ),
        ] {
            for split in 0..=upstream.len() {
                let mut translator =
                    LiveStreamTranslator::new("msg_after_tool".into(), "glm-5.2".into());
                let mut output = translator.push(&upstream.as_bytes()[..split]).unwrap();
                output.extend(translator.push(&upstream.as_bytes()[split..]).unwrap());
                output.extend(translator.finish().unwrap());
                let rendered = String::from_utf8(output).unwrap();
                let events = crate::anthropic::sse::parse_sse_events(rendered.as_bytes())
                    .into_iter()
                    .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
                    .collect::<Vec<_>>();
                let tool_starts = events
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| {
                        event["type"] == "content_block_start"
                            && event["content_block"]["type"] == "tool_use"
                    })
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                let tool_stops = events
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| {
                        event["type"] == "content_block_stop" && event["index"] == 0
                    })
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                let following_start = events
                    .iter()
                    .position(|event| {
                        event["type"] == "content_block_start"
                            && event["content_block"]["type"] == expected_type
                    })
                    .unwrap();
                assert_eq!(tool_starts.len(), 1, "split {split}");
                assert_eq!(tool_stops.len(), 1, "split {split}");
                assert!(tool_starts[0] < tool_stops[0], "split {split}");
                assert!(tool_stops[0] < following_start, "split {split}");
                assert_eq!(
                    rendered
                        .matches("\"id\":\"toolu_msg_after_tool_0\"")
                        .count(),
                    1,
                    "split {split}"
                );
                assert!(rendered.contains("message_stop"), "split {split}");
            }

            let response =
                accumulate_response(upstream.as_bytes(), "msg_after_tool_buffered", "glm-5.2")
                    .unwrap();
            assert_eq!(response["content"][0]["type"], "tool_use");
            assert_eq!(response["content"][1]["type"], expected_type);
            let value_field = if expected_type == "text" {
                "text"
            } else {
                "thinking"
            };
            assert_eq!(response["content"][1][value_field], expected_value);
            assert_eq!(response["stop_reason"], "tool_use");
        }
    }

    #[test]
    fn tool_call_without_identity_still_fails_at_finalize() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let error = accumulate_response(upstream.as_bytes(), "msg_missing", "glm-5.2")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ended without id or function name"));
    }

    #[test]
    fn tool_call_cannot_resume_after_its_content_block_closed() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"search\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"after tool\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\" \"}}]}}]}\n\n"
        );
        let error = accumulate_response(upstream.as_bytes(), "msg_resumed", "glm-5.2")
            .unwrap_err()
            .to_string();
        assert!(error.contains("continued after its content block was closed"));
    }

    #[test]
    fn new_tool_index_can_start_after_text() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"first\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"between tools\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"second\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_new_tool", "glm-5.2").unwrap();
        assert_eq!(response["content"][0]["type"], "tool_use");
        assert_eq!(response["content"][1]["type"], "text");
        assert_eq!(response["content"][2]["type"], "tool_use");
        assert_eq!(response["content"][2]["name"], "second");
    }

    #[test]
    fn chat_tool_ids_are_namespaced_per_response() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"Agent_0\",\"function\":{\"name\":\"Agent\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let main = accumulate_response(upstream.as_bytes(), "msg_main", "kimi-k3").unwrap();
        let child = accumulate_response(upstream.as_bytes(), "msg_child", "kimi-k3").unwrap();
        let main_id = main["content"][0]["id"].as_str().unwrap();
        let child_id = child["content"][0]["id"].as_str().unwrap();

        assert_eq!(main_id, "toolu_msg_main_0");
        assert_eq!(child_id, "toolu_msg_child_0");
        assert_ne!(main_id, child_id);
        assert_ne!(main_id, "Agent_0");
        assert_ne!(child_id, "Agent_0");
    }

    #[test]
    fn buffered_response_is_strict_and_complete() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n"
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_2", "glm-5.2").unwrap();
        assert_eq!(response["content"][0]["text"], "hello");
        assert_eq!(response["stop_reason"], "end_turn");
        assert_eq!(response["usage"]["output_tokens"], 1);
    }

    #[test]
    fn malformed_incomplete_and_unrecognized_terminal_streams_fail() {
        let malformed = b"data: not-json\n\n";
        let incomplete = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        let partial_frame = b"data: {\"choices\":[]}";
        let filtered = b"data: {\"choices\":[{\"finish_reason\":\"content_filter\"}]}\n\n";
        let unknown = b"data: {\"choices\":[{\"finish_reason\":\"mystery\"}]}\n\n";

        assert!(accumulate_response(malformed, "m", "model").is_err());
        assert!(accumulate_response(incomplete, "m", "model").is_err());
        assert!(accumulate_response(partial_frame, "m", "model").is_err());
        assert!(accumulate_response(filtered, "m", "model").is_err());
        assert!(accumulate_response(unknown, "m", "model").is_err());
    }

    #[test]
    fn done_is_a_real_terminal_even_without_finish_reason() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_3", "model").unwrap();
        assert_eq!(response["stop_reason"], "end_turn");
    }

    #[test]
    fn accepts_opencode_cost_metadata_after_done() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[],\"cost\":\"0\"}\n\n"
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_4", "glm-5.2").unwrap();
        assert_eq!(response["content"][0]["text"], "hello");
        assert_eq!(response["usage"]["output_tokens"], 1);
    }

    #[test]
    fn rejects_content_after_done() {
        let upstream = concat!(
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n"
        );
        assert!(accumulate_response(upstream.as_bytes(), "msg_5", "glm-5.2").is_err());
    }

    #[tokio::test]
    async fn live_stream_rejects_an_incomplete_frame_after_done() {
        let upstream = futures_util::stream::iter([Ok::<Bytes, OpenCodeError>(
            Bytes::from_static(b"data: [DONE]\n\ndata: {"),
        )]);
        let mut state = OpenCodeChatStreamState {
            upstream,
            translator: LiveStreamTranslator::new("msg_4".into(), "model".into()),
            capture_decoder: SseDecoder::default(),
            terminal: false,
            error_sent: false,
            monitor: None,
            req_id: "req".into(),
            bytes: 0,
            chunks: 0,
            stream_capture: None,
            traffic: None,
        };
        let output = state.next_output().await.expect("error event");
        assert!(String::from_utf8_lossy(&output).contains("OpenCode Go stream is invalid"));
    }
}
