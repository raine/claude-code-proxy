//! Cursor tool bridge: state machine, result builders, pending tool tracking,
//! stream re-entry, and SSE pause/resume.
//!
//! The bridge coordinates the pause-and-continue lifecycle when the Cursor
//! upstream emits a `<tool_use>` text block. The bridge pauses the SSE stream,
//! stores the pending tool, and waits for Claude's `tool_result` in the next
//! client request. On resume it builds Cursor protocol result messages and
//! continues producing SSE output from stored upstream events.

use std::collections::BTreeSet;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::anthropic::schema::MessagesRequest;
use crate::providers::cursor::response::CursorStreamEvent;
use crate::providers::cursor::sse::CursorSseFramer;
use crate::providers::cursor::tool_use_xml::{CursorToolUseXmlParser, RecoveredCursorEvent};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Execution context for a Cursor tool.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorExec {
    pub id: Option<u64>,
    pub exec_id: Option<String>,
    pub args: serde_json::Value,
}

/// A tool result produced by Claude.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorNativeToolResult {
    pub content: String,
    pub is_error: bool,
}

/// A pending Cursor tool that Claude must fulfill.
#[derive(Debug, Clone)]
pub enum PendingCursorTool {
    Read {
        tool_use_id: String,
        path: String,
    },
    Write {
        tool_use_id: String,
        path: String,
        content: String,
    },
    Bash {
        tool_use_id: String,
        command: String,
        working_directory: String,
        timeout_ms: u64,
    },
}

impl PendingCursorTool {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Read { .. } => "Read",
            Self::Write { .. } => "Write",
            Self::Bash { .. } => "Bash",
        }
    }

    pub fn tool_use_id(&self) -> &str {
        match self {
            Self::Read { tool_use_id, .. }
            | Self::Write { tool_use_id, .. }
            | Self::Bash { tool_use_id, .. } => tool_use_id,
        }
    }

    /// Build the JSON input that matches the Claude tool_use block.
    pub fn input_json(&self) -> serde_json::Value {
        match self {
            Self::Read { path, .. } => {
                serde_json::json!({ "file_path": path })
            }
            Self::Write { path, content, .. } => {
                serde_json::json!({ "file_path": path, "content": content })
            }
            Self::Bash {
                command,
                working_directory,
                timeout_ms,
                ..
            } => {
                let cmd = if working_directory.is_empty() {
                    command.clone()
                } else {
                    format!("cd '{}' && {command}", working_directory)
                };
                serde_json::json!({
                    "command": cmd,
                    "timeout": timeout_ms,
                    "description": "Run Cursor-requested shell command",
                    "run_in_background": false,
                    "dangerouslyDisableSandbox": false
                })
            }
        }
    }
}

/// Bridge state stored per session.
#[derive(Debug)]
pub struct CursorBridgeState {
    pub session_id: String,
    pub message_id: String,
    pub model: String,
    pub pending_tool: Option<PendingCursorTool>,
    pub remaining_events: Vec<CursorStreamEvent>,
    pub event_cursor: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub allowed_tool_names: Option<BTreeSet<String>>,
    pub xml_parser: CursorToolUseXmlParser,
}

impl CursorBridgeState {
    fn new(
        session_id: String,
        message_id: String,
        model: String,
        allowed_tool_names: Option<BTreeSet<String>>,
        id_factory: Box<dyn FnMut() -> String + Send>,
    ) -> Self {
        Self {
            session_id,
            message_id,
            model,
            pending_tool: None,
            remaining_events: Vec::new(),
            event_cursor: 0,
            input_tokens: 0,
            output_tokens: 0,
            allowed_tool_names: allowed_tool_names.clone(),
            xml_parser: CursorToolUseXmlParser::new_with_id_factory(allowed_tool_names, id_factory),
        }
    }
}

// ---------------------------------------------------------------------------
// Global bridge registry
// ---------------------------------------------------------------------------

static BRIDGE_REGISTRY: Lazy<Mutex<BridgeRegistryInner>> =
    Lazy::new(|| Mutex::new(BridgeRegistryInner::new()));

struct BridgeRegistryInner {
    sessions: Vec<CursorBridgeState>,
}

impl BridgeRegistryInner {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
        }
    }
}

/// Global registry of active bridge sessions.
pub struct BridgeRegistry;

impl BridgeRegistry {
    /// Insert a new bridge state for a session.
    pub fn insert(state: CursorBridgeState) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.push(state);
    }

    /// Get the bridge state for a session.
    pub fn get(session_id: &str) -> Option<usize> {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.iter().position(|s| s.session_id == session_id)
    }

    /// Get the pending tool for a session (if any).
    pub fn pending_tool(session_id: &str) -> Option<PendingCursorTool> {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .and_then(|s| s.pending_tool.clone())
    }

    /// Take the bridge state for a session (removes it).
    pub fn take(session_id: &str) -> Option<CursorBridgeState> {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        let pos = reg
            .sessions
            .iter()
            .position(|s| s.session_id == session_id)?;
        Some(reg.sessions.swap_remove(pos))
    }

    /// Remove a bridge state for a session.
    pub fn remove(session_id: &str) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.retain(|s| s.session_id != session_id);
    }

    /// Insert or update the pending tool for a session.
    pub fn set_pending_tool(session_id: &str, tool: PendingCursorTool) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        if let Some(state) = reg.sessions.iter_mut().find(|s| s.session_id == session_id) {
            state.pending_tool = Some(tool);
        }
    }

    /// Update usage for a session.
    pub fn record_usage(session_id: &str, input_tokens: u64, output_tokens: u64) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        if let Some(state) = reg.sessions.iter_mut().find(|s| s.session_id == session_id) {
            state.input_tokens = input_tokens.max(state.input_tokens);
            state.output_tokens = output_tokens.max(state.output_tokens);
        }
    }

    /// Clear all bridge state.
    pub fn clear() {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.clear();
    }

    /// Number of active sessions.
    pub fn active_count() -> usize {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.len()
    }
}

// ---------------------------------------------------------------------------
// Tool detection helpers
// ---------------------------------------------------------------------------

/// Extract advertised tool names from a MessagesRequest.
pub fn advertised_tool_names(body: &MessagesRequest) -> Option<BTreeSet<String>> {
    let tools = body.extra.get("tools")?.as_array()?;
    if tools.is_empty() {
        return None;
    }
    let names: BTreeSet<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .map(|n| n.to_string())
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

/// Whether the request can use the Cursor native tool bridge.
///
/// Returns `true` when the request is streaming, has a session id, and
/// advertises at least one of Read, Write, or Bash.
pub fn can_bridge_cursor_native_tools(body: &MessagesRequest, session_id: Option<&str>) -> bool {
    let _sid = match session_id {
        Some(id) if !id.is_empty() => id,
        _ => return false,
    };
    if !body.stream {
        return false;
    }
    let names = match advertised_tool_names(body) {
        Some(n) => n,
        None => return false,
    };
    names.contains("Read") || names.contains("Write") || names.contains("Bash")
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// Find the last `tool_result` block matching `tool_use_id` in the request.
pub fn find_tool_result<'a>(
    body: &'a MessagesRequest,
    tool_use_id: &str,
) -> Option<&'a serde_json::Value> {
    for message in body.messages.iter().rev() {
        if message.role != "user" {
            continue;
        }
        let blocks = match &message.content {
            serde_json::Value::Array(arr) => arr,
            _ => continue,
        };
        for block in blocks.iter().rev() {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                && block.get("tool_use_id").and_then(|t| t.as_str()) == Some(tool_use_id)
            {
                return Some(block);
            }
        }
    }
    None
}

/// Render the content of a `tool_result` block into a string.
pub fn render_tool_result_content(result: &serde_json::Value) -> String {
    let content = match result.get("content") {
        Some(serde_json::Value::String(s)) => return s.clone(),
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        _ => return String::new(),
    };
    let parts: Vec<String> = content
        .iter()
        .map(|block| match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => block
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            Some("image") => "[image result omitted]".to_string(),
            Some("thinking") => block
                .get("thinking")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            _ => serde_json::to_string(block).unwrap_or_default(),
        })
        .collect();
    parts.join("\n")
}

/// Whether a `tool_result` block indicates an error.
pub fn tool_result_is_error(result: &serde_json::Value) -> bool {
    result
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false)
}

/// Build the partial JSON string for a pending tool's input (for the
/// input_json_delta in the tool_use content block).
pub fn build_tool_use_input_json(tool: &PendingCursorTool) -> String {
    serde_json::to_string(&tool.input_json()).unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// Cursor protocol message builders
// ---------------------------------------------------------------------------

/// Inject `id` and `execId` fields into a JSON payload.
pub fn with_exec_ids(
    exec: &CursorExec,
    mut payload: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    if let Some(id) = exec.id {
        payload.insert("id".into(), id.into());
    }
    if let Some(ref exec_id) = exec.exec_id {
        payload.insert("execId".into(), exec_id.clone().into());
    }
    serde_json::Value::Object(payload)
}

/// Build the Cursor `readResult` message from a Claude `tool_result`.
pub fn build_read_result_from_native(
    exec: &CursorExec,
    result: &CursorNativeToolResult,
) -> serde_json::Value {
    let path = exec
        .args
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = &result.content;
    let lines = if content.is_empty() {
        0
    } else {
        content.lines().count()
    };
    let file_size = content.len().to_string();

    let read_result = serde_json::json!({
        "success": {
            "path": path,
            "content": content,
            "totalLines": lines,
            "fileSize": file_size
        }
    });

    let mut map = serde_json::Map::new();
    map.insert("readResult".into(), read_result);
    with_exec_ids(exec, map)
}

/// Build the Cursor `writeResult` message from a Claude `tool_result`.
pub fn build_write_result_from_native(
    exec: &CursorExec,
    result: &CursorNativeToolResult,
) -> serde_json::Value {
    let path = exec
        .args
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let write_result = if result.is_error {
        serde_json::json!({
            "error": {
                "path": path,
                "error": result.content
            }
        })
    } else {
        let lines = if result.content.is_empty() {
            0
        } else {
            result.content.lines().count()
        };
        serde_json::json!({
            "success": {
                "path": path,
                "linesCreated": lines,
                "fileSize": result.content.len()
            }
        })
    };

    let mut map = serde_json::Map::new();
    map.insert("writeResult".into(), write_result);
    with_exec_ids(exec, map)
}

/// Build the collection of Cursor `shellStream` messages from a Claude
/// `tool_result`.
///
/// Returns: start, stdout/stderr, exit, streamClose.
pub fn build_shell_stream_result(
    exec: &CursorExec,
    result: &CursorNativeToolResult,
    local_execution_time: std::time::Duration,
    cwd: &str,
) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    // Start
    let start_msg = with_exec_ids(
        exec,
        serde_json::json!({ "shellStream": { "start": {} } })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    messages.push(start_msg);

    // Content (stdout or stderr)
    if !result.content.is_empty() {
        let stream_key = if result.is_error { "stderr" } else { "stdout" };
        let content_msg = with_exec_ids(
            exec,
            serde_json::json!({ "shellStream": { stream_key: { "data": result.content } } })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        messages.push(content_msg);
    }

    // Exit
    let exit_code: u32 = if result.is_error { 1 } else { 0 };
    let exit_msg = with_exec_ids(
        exec,
        serde_json::json!({
            "shellStream": {
                "exit": {
                    "code": exit_code,
                    "cwd": cwd,
                    "localExecutionTimeMs": local_execution_time.as_millis() as u64,
                }
            }
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    messages.push(exit_msg);

    // Stream close
    if let Some(id) = exec.id {
        let close_msg = serde_json::json!({
            "execClientControlMessage": {
                "streamClose": {
                    "id": id
                }
            }
        });
        messages.push(close_msg);
    } else {
        let close_msg = serde_json::json!({
            "execClientControlMessage": {
                "streamClose": {}
            }
        });
        messages.push(close_msg);
    }

    messages
}

// ---------------------------------------------------------------------------
// Bridge start and resume
// ---------------------------------------------------------------------------

/// Start a new tool bridge session.
///
/// Processes upstream events through XML recovery. When a `<tool_use>` is
/// recovered, emits the SSE pause (tool_use content block + message_stop with
/// stop_reason="tool_use") and stores the bridge state for resume.
///
/// Returns the SSE bytes and whether a tool_use pause was emitted.
pub fn start_cursor_tool_bridge(
    message_id: &str,
    model: &str,
    session_id: &str,
    events: &[CursorStreamEvent],
    allowed_tool_names: Option<BTreeSet<String>>,
    id_factory: Box<dyn FnMut() -> String + Send>,
) -> (Vec<u8>, bool) {
    let mut sse = Vec::new();
    let mut framer = CursorSseFramer::new(&mut sse, message_id, model);

    let mut state = CursorBridgeState::new(
        session_id.to_string(),
        message_id.to_string(),
        model.to_string(),
        allowed_tool_names,
        id_factory,
    );

    let mut paused = false;

    for event in events {
        if paused {
            state.remaining_events.push(event.clone());
            continue;
        }

        match event {
            CursorStreamEvent::ThinkingDelta { text } => {
                framer.emit_thinking_delta(text);
            }
            CursorStreamEvent::TextDelta { text } => {
                let recovered = state.xml_parser.push(text);
                for recovered_event in &recovered {
                    if paused {
                        if let RecoveredCursorEvent::Text(t) = recovered_event {
                            state
                                .remaining_events
                                .push(CursorStreamEvent::TextDelta { text: t.clone() });
                        }
                        continue;
                    }
                    match recovered_event {
                        RecoveredCursorEvent::Text(t) => {
                            framer.emit_text_delta(t);
                        }
                        RecoveredCursorEvent::ToolUse(tool_use) => {
                            let input_json = serde_json::to_string(&tool_use.input)
                                .unwrap_or_else(|_| "{}".to_string());
                            framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);

                            if let Some(pending) = pending_from_recovered_tool(tool_use) {
                                state.pending_tool = Some(pending);
                            }

                            paused = true;
                        }
                    }
                }
            }
            CursorStreamEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                framer.record_usage(*input_tokens, *output_tokens, 0, 0);
                state.input_tokens = *input_tokens;
                state.output_tokens = *output_tokens;
            }
            CursorStreamEvent::Session { .. } => {
                // Session info is not mapped to SSE events
            }
            CursorStreamEvent::End => {
                // If we haven't paused, finalize normally
                if !paused {
                    // Process any remaining XML before finalizing
                    let flushed = state.xml_parser.flush();
                    for evt in &flushed {
                        if let RecoveredCursorEvent::ToolUse(tool_use) = evt {
                            let input_json = serde_json::to_string(&tool_use.input)
                                .unwrap_or_else(|_| "{}".to_string());
                            framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                            if let Some(pending) = pending_from_recovered_tool(tool_use) {
                                state.pending_tool = Some(pending);
                            }
                            paused = true;
                        }
                    }
                    if !paused {
                        framer.finalize();
                    }
                }
            }
        }
    }

    if paused {
        let remaining = state.remaining_events.clone();
        let mut stored_state = CursorBridgeState::new(
            session_id.to_string(),
            message_id.to_string(),
            model.to_string(),
            state.allowed_tool_names.clone(),
            Box::new(|| {
                format!(
                    "call_cursor_{}",
                    uuid::Uuid::new_v4().to_string().replace('-', "")
                )
            }),
        );
        stored_state.pending_tool = state.pending_tool.clone();
        stored_state.remaining_events = remaining;
        stored_state.event_cursor = 0;
        stored_state.input_tokens = state.input_tokens;
        stored_state.output_tokens = state.output_tokens;
        BridgeRegistry::insert(stored_state);
    }

    if !paused {
        // Flush any remaining text from XML parser
        let flushed = state.xml_parser.flush();
        for evt in &flushed {
            if let RecoveredCursorEvent::ToolUse(tool_use) = evt {
                let input_json =
                    serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                if let Some(pending) = pending_from_recovered_tool(tool_use) {
                    state.pending_tool = Some(pending);
                }
                paused = true;
            }
        }
        if !paused {
            framer.finalize();
        }
    }

    (sse, paused)
}

/// Resume a paused tool bridge session.
///
/// Finds the stored state by session_id, resolves the pending tool with
/// Claude's `tool_result`, and continues producing SSE from remaining events.
pub fn resume_cursor_tool_bridge(
    session_id: &str,
    new_message_id: &str,
    new_model: &str,
    result: &serde_json::Value,
    pending_tool: &PendingCursorTool,
) -> (Vec<serde_json::Value>, Vec<u8>) {
    let native_result = CursorNativeToolResult {
        content: render_tool_result_content(result),
        is_error: tool_result_is_error(result),
    };

    // Build Cursor protocol messages for the resolved tool
    let exec = CursorExec {
        id: None,
        exec_id: None,
        args: pending_tool.input_json(),
    };
    let result_messages = match pending_tool {
        PendingCursorTool::Read { .. } => {
            let msg = build_read_result_from_native(&exec, &native_result);
            vec![msg]
        }
        PendingCursorTool::Write { .. } => {
            let msg = build_write_result_from_native(&exec, &native_result);
            vec![msg]
        }
        PendingCursorTool::Bash {
            working_directory, ..
        } => build_shell_stream_result(
            &exec,
            &native_result,
            std::time::Duration::from_millis(0),
            working_directory,
        ),
    };

    // Generate SSE continuation from remaining events
    let mut sse = Vec::new();
    let mut framer = CursorSseFramer::new(&mut sse, new_message_id, new_model);

    // Retrieve stored state for remaining events
    let remaining: Vec<CursorStreamEvent> = BridgeRegistry::pending_tool(session_id)
        .and_then(|_| BridgeRegistry::take(session_id))
        .map(|state| state.remaining_events)
        .unwrap_or_default();

    if remaining.is_empty() {
        // No remaining events: just finalize
        framer.finalize();
    } else {
        let mut xml_parser = CursorToolUseXmlParser::new(None);
        let mut paused_again = false;

        for event in &remaining {
            match event {
                CursorStreamEvent::ThinkingDelta { text } => {
                    if !paused_again {
                        framer.emit_thinking_delta(text);
                    }
                }
                CursorStreamEvent::TextDelta { text } => {
                    if paused_again {
                        continue;
                    }
                    let recovered = xml_parser.push(text);
                    for evt in &recovered {
                        match evt {
                            RecoveredCursorEvent::Text(t) => {
                                framer.emit_text_delta(t);
                            }
                            RecoveredCursorEvent::ToolUse(tool_use) => {
                                let input_json = serde_json::to_string(&tool_use.input)
                                    .unwrap_or_else(|_| "{}".to_string());
                                framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                                paused_again = true;
                            }
                        }
                    }
                }
                CursorStreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                    ..
                } => {
                    if !paused_again {
                        framer.record_usage(*input_tokens, *output_tokens, 0, 0);
                    }
                }
                CursorStreamEvent::Session { .. } => {}
                CursorStreamEvent::End => {
                    if !paused_again {
                        // Flush before finalizing
                        let flushed = xml_parser.flush();
                        for evt in &flushed {
                            if let RecoveredCursorEvent::ToolUse(tool_use) = evt {
                                let input_json = serde_json::to_string(&tool_use.input)
                                    .unwrap_or_else(|_| "{}".to_string());
                                framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                                paused_again = true;
                            }
                        }
                        if !paused_again {
                            framer.finalize();
                        }
                    }
                }
            }
        }

        if !paused_again {
            let flushed = xml_parser.flush();
            for evt in &flushed {
                if let RecoveredCursorEvent::ToolUse(tool_use) = evt {
                    let input_json =
                        serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                    framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                    paused_again = true;
                }
            }
            if !paused_again {
                framer.finalize();
            }
        }

        if paused_again && !remaining.is_empty() {
            let state = CursorBridgeState::new(
                session_id.to_string(),
                new_message_id.to_string(),
                new_model.to_string(),
                None,
                Box::new(|| {
                    format!(
                        "call_cursor_{}",
                        uuid::Uuid::new_v4().to_string().replace('-', "")
                    )
                }),
            );
            BridgeRegistry::insert(state);
        }
    }

    (result_messages, sse)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Create a `PendingCursorTool` from a recovered XML tool_use event.
fn pending_from_recovered_tool(
    tool_use: &crate::providers::cursor::tool_use_xml::RecoveredCursorToolUse,
) -> Option<PendingCursorTool> {
    match tool_use.name.as_str() {
        "Read" => {
            let file_path = tool_use
                .input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(PendingCursorTool::Read {
                tool_use_id: tool_use.id.clone(),
                path: file_path,
            })
        }
        "Write" => {
            let file_path = tool_use
                .input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = tool_use
                .input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(PendingCursorTool::Write {
                tool_use_id: tool_use.id.clone(),
                path: file_path,
                content,
            })
        }
        "Bash" => {
            let command = tool_use
                .input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let working_directory = String::new();
            let timeout_ms = tool_use
                .input
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);
            Some(PendingCursorTool::Bash {
                tool_use_id: tool_use.id.clone(),
                command,
                working_directory,
                timeout_ms,
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::MessagesRequest;
    use std::sync::Mutex;

    /// Serialize tests that share the global bridge registry.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    // -----------------------------------------------------------------------
    // PendingCursorTool tests
    // -----------------------------------------------------------------------

    #[test]
    fn pending_read_input_matches_claude_read_tool() {
        let tool = PendingCursorTool::Read {
            tool_use_id: "call_cursor_1".into(),
            path: "/tmp/a".into(),
        };
        let json = tool.input_json();
        assert_eq!(json["file_path"], "/tmp/a");
        assert_eq!(tool.name(), "Read");
        assert_eq!(tool.tool_use_id(), "call_cursor_1");
    }

    #[test]
    fn pending_write_input_matches_claude_write_tool() {
        let tool = PendingCursorTool::Write {
            tool_use_id: "call_cursor_2".into(),
            path: "/tmp/b".into(),
            content: "hello".into(),
        };
        let json = tool.input_json();
        assert_eq!(json["file_path"], "/tmp/b");
        assert_eq!(json["content"], "hello");
        assert_eq!(tool.name(), "Write");
    }

    #[test]
    fn pending_bash_input_matches_claude_bash_tool() {
        let tool = PendingCursorTool::Bash {
            tool_use_id: "call_cursor_3".into(),
            command: "pwd".into(),
            working_directory: "/tmp".into(),
            timeout_ms: 30_000,
        };
        let json = tool.input_json();
        assert_eq!(json["command"], "cd '/tmp' && pwd");
        assert_eq!(json["timeout"], 30_000);
        assert_eq!(json["description"], "Run Cursor-requested shell command");
        assert_eq!(tool.name(), "Bash");
    }

    #[test]
    fn pending_bash_no_working_directory() {
        let tool = PendingCursorTool::Bash {
            tool_use_id: "call_cursor_4".into(),
            command: "ls".into(),
            working_directory: "".into(),
            timeout_ms: 10_000,
        };
        let json = tool.input_json();
        // Without a working directory, command is passed as-is
        assert_eq!(json["command"], "ls");
    }

    // -----------------------------------------------------------------------
    // Result builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn with_exec_ids_adds_id_and_exec_id() {
        let exec = CursorExec {
            id: Some(7),
            exec_id: Some("exec-1".into()),
            args: serde_json::json!({}),
        };
        let mut payload = serde_json::Map::new();
        payload.insert("test".into(), serde_json::json!("value"));
        let result = with_exec_ids(&exec, payload);
        assert_eq!(result["id"], 7);
        assert_eq!(result["execId"], "exec-1");
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn with_exec_ids_omits_missing_fields() {
        let exec = CursorExec {
            id: None,
            exec_id: None,
            args: serde_json::json!({}),
        };
        let mut payload = serde_json::Map::new();
        payload.insert("test".into(), serde_json::json!("v"));
        let result = with_exec_ids(&exec, payload);
        assert!(result.get("id").is_none());
        assert!(result.get("execId").is_none());
        assert_eq!(result["test"], "v");
    }

    #[test]
    fn read_result_from_successful_result() {
        let exec = CursorExec {
            id: Some(1),
            exec_id: None,
            args: serde_json::json!({"file_path": "/tmp/a"}),
        };
        let result = CursorNativeToolResult {
            content: "file content".into(),
            is_error: false,
        };
        let msg = build_read_result_from_native(&exec, &result);
        assert_eq!(msg["id"], 1);
        assert_eq!(msg["readResult"]["success"]["path"], "/tmp/a");
        assert_eq!(msg["readResult"]["success"]["content"], "file content");
        assert_eq!(msg["readResult"]["success"]["totalLines"], 1);
    }

    #[test]
    fn write_result_from_successful_result() {
        let exec = CursorExec {
            id: Some(2),
            exec_id: None,
            args: serde_json::json!({"file_path": "/tmp/b", "content": "hi"}),
        };
        let result = CursorNativeToolResult {
            content: "success".into(),
            is_error: false,
        };
        let msg = build_write_result_from_native(&exec, &result);
        assert_eq!(msg["id"], 2);
        assert_eq!(msg["writeResult"]["success"]["path"], "/tmp/b");
        assert_eq!(msg["writeResult"]["success"]["linesCreated"], 1);
    }

    #[test]
    fn write_result_from_error_result() {
        let exec = CursorExec {
            id: Some(3),
            exec_id: None,
            args: serde_json::json!({"file_path": "/tmp/c"}),
        };
        let result = CursorNativeToolResult {
            content: "permission denied".into(),
            is_error: true,
        };
        let msg = build_write_result_from_native(&exec, &result);
        assert_eq!(msg["writeResult"]["error"]["path"], "/tmp/c");
        assert_eq!(msg["writeResult"]["error"]["error"], "permission denied");
    }

    #[test]
    fn shell_stream_result_emits_start_output_exit_and_close() {
        let exec = CursorExec {
            id: Some(7),
            exec_id: Some("e".into()),
            args: serde_json::json!({}),
        };
        let messages = build_shell_stream_result(
            &exec,
            &CursorNativeToolResult {
                content: "hi".into(),
                is_error: false,
            },
            std::time::Duration::from_millis(3),
            "/tmp",
        );
        assert_eq!(messages.len(), 4);
        // Start
        assert!(
            messages[0]
                .get("shellStream")
                .and_then(|s| s.get("start"))
                .is_some()
        );
        // Stdout content
        assert_eq!(messages[1]["shellStream"]["stdout"]["data"], "hi");
        // Exit
        assert_eq!(messages[2]["shellStream"]["exit"]["code"], 0);
        assert_eq!(messages[2]["shellStream"]["exit"]["cwd"], "/tmp");
        // Stream close
        assert_eq!(
            messages[3]["execClientControlMessage"]["streamClose"]["id"],
            7
        );
    }

    #[test]
    fn shell_stream_handles_error_result() {
        let exec = CursorExec {
            id: Some(8),
            exec_id: None,
            args: serde_json::json!({}),
        };
        let messages = build_shell_stream_result(
            &exec,
            &CursorNativeToolResult {
                content: "error msg".into(),
                is_error: true,
            },
            std::time::Duration::from_millis(5),
            "/tmp",
        );
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["shellStream"]["stderr"]["data"], "error msg");
        assert_eq!(messages[2]["shellStream"]["exit"]["code"], 1);
    }

    // -----------------------------------------------------------------------
    // find_tool_result tests
    // -----------------------------------------------------------------------

    #[test]
    fn finds_tool_result_in_request() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "result text"}]}
            ]
        }))
        .unwrap();
        let result = find_tool_result(&body, "call_1");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("content").and_then(|c| c.as_str()),
            Some("result text")
        );
    }

    #[test]
    fn find_tool_result_returns_none_when_not_found() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        }))
        .unwrap();
        assert!(find_tool_result(&body, "call_1").is_none());
    }

    #[test]
    fn find_tool_result_scans_newest_first() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "old"}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "new"}]}
            ]
        }))
        .unwrap();
        let result = find_tool_result(&body, "call_1");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("content").and_then(|c| c.as_str()),
            Some("new")
        );
    }

    // -----------------------------------------------------------------------
    // advertised_tool_names tests
    // -----------------------------------------------------------------------

    #[test]
    fn advertised_tool_names_extracts_read_write_bash() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "Read", "description": "read", "input_schema": {}},
                {"name": "Write", "description": "write", "input_schema": {}},
                {"name": "Bash", "description": "bash", "input_schema": {}}
            ]
        }))
        .unwrap();
        let names = advertised_tool_names(&body).unwrap();
        assert!(names.contains("Read"));
        assert!(names.contains("Write"));
        assert!(names.contains("Bash"));
    }

    #[test]
    fn advertised_tool_names_no_tools_returns_none() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        assert!(advertised_tool_names(&body).is_none());
    }

    #[test]
    fn can_bridge_returns_true_for_stream_with_read_tool() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(can_bridge_cursor_native_tools(&body, Some("session-1")));
    }

    #[test]
    fn can_bridge_returns_false_for_non_streaming() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(!can_bridge_cursor_native_tools(&body, Some("session-1")));
    }

    #[test]
    fn can_bridge_returns_false_without_session_id() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(!can_bridge_cursor_native_tools(&body, None));
        assert!(!can_bridge_cursor_native_tools(&body, Some("")));
    }

    // -----------------------------------------------------------------------
    // BridgeRegistry tests
    // -----------------------------------------------------------------------

    #[test]
    fn bridge_registry_manages_sessions() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        assert_eq!(BridgeRegistry::active_count(), 0);

        let state = CursorBridgeState::new(
            "session-test".into(),
            "msg-1".into(),
            "cursor-test".into(),
            None,
            Box::new(|| "id".into()),
        );
        BridgeRegistry::insert(state);
        assert_eq!(BridgeRegistry::active_count(), 1);
        assert!(BridgeRegistry::get("session-test").is_some());

        let state = BridgeRegistry::take("session-test");
        assert!(state.is_some());
        assert_eq!(BridgeRegistry::active_count(), 0);
    }

    #[test]
    fn bridge_registry_set_and_get_pending_tool() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        let state = CursorBridgeState::new(
            "session-pt".into(),
            "msg-1".into(),
            "cursor-test".into(),
            None,
            Box::new(|| "id".into()),
        );
        BridgeRegistry::insert(state);

        let tool = PendingCursorTool::Read {
            tool_use_id: "call_1".into(),
            path: "/tmp/a".into(),
        };
        BridgeRegistry::set_pending_tool("session-pt", tool);

        let retrieved = BridgeRegistry::pending_tool("session-pt");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name(), "Read");

        BridgeRegistry::clear();
    }

    // -----------------------------------------------------------------------
    // render_tool_result_content tests
    // -----------------------------------------------------------------------

    #[test]
    fn renders_string_content() {
        let result = serde_json::json!({
            "type": "tool_result",
            "content": "plain string"
        });
        assert_eq!(render_tool_result_content(&result), "plain string");
    }

    #[test]
    fn renders_array_content() {
        let result = serde_json::json!({
            "type": "tool_result",
            "content": [
                {"type": "text", "text": "part one"},
                {"type": "text", "text": "part two"}
            ]
        });
        let rendered = render_tool_result_content(&result);
        assert!(rendered.contains("part one"));
        assert!(rendered.contains("part two"));
    }

    #[test]
    fn renders_mixed_content_types() {
        let result = serde_json::json!({
            "type": "tool_result",
            "content": [
                {"type": "text", "text": "text result"},
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}}
            ]
        });
        let rendered = render_tool_result_content(&result);
        assert!(rendered.contains("text result"));
        assert!(rendered.contains("[image result omitted]"));
    }

    #[test]
    fn render_empty_tool_result() {
        let result = serde_json::json!({"type": "tool_result"});
        assert_eq!(render_tool_result_content(&result), "");
    }

    #[test]
    fn detects_error_from_tool_result() {
        let result = serde_json::json!({"type": "tool_result", "is_error": true});
        assert!(tool_result_is_error(&result));

        let result = serde_json::json!({"type": "tool_result", "is_error": false});
        assert!(!tool_result_is_error(&result));

        let result = serde_json::json!({"type": "tool_result"});
        assert!(!tool_result_is_error(&result));
    }
}
