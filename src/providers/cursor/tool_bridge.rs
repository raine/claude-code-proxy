//! Cursor tool bridge: the live exec pump and the registry of open runs.
//!
//! `AgentService/Run` is bidirectional. One run stays open for the whole turn,
//! and every `ExecServerMessage` the agent sends is answered on that same
//! stream with an `ExecClientMessage` carrying the echoed `id`/`exec_id`. The
//! bridge drives that loop: it turns upstream events into Anthropic SSE, pauses
//! when the agent asks for a tool, and hands the still-open run back to the
//! registry. The next Claude Code request (which carries the `tool_result`)
//! resumes that run instead of starting a new one -- a new run makes the agent
//! re-plan from scratch and re-issue the tool call it was already waiting on.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::anthropic::schema::MessagesRequest;
use crate::providers::cursor::client::{LiveEvent, LiveRun};
use crate::providers::cursor::proto;
use crate::providers::cursor::sse::{CursorSseFramer, format_sse_error};
use crate::providers::cursor::tool_use_xml::{CursorToolUseXmlParser, RecoveredCursorEvent};

/// A run untouched for this long was abandoned (the user aborted, the client
/// died) and is dropped, closing its HTTP/2 stream.
const RUN_MAX_IDLE: Duration = Duration::from_secs(600);

/// Claude Code's Bash tool accepts at most this many milliseconds.
const MAX_BASH_TIMEOUT_MS: i64 = 600_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The tool Claude Code was asked to run for a Cursor exec.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingExecKind {
    Read {
        path: String,
    },
    Write {
        path: String,
        content: String,
    },
    Shell {
        command: String,
        cwd: String,
        /// `shell_stream_args` (tag 14) streams; bare `shell_args` (tag 2) is
        /// answered with a single result.
        stream: bool,
    },
}

/// A Cursor exec handed to Claude Code, awaiting its `tool_result`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingExec {
    /// The `tool_use` id Claude Code answers with.
    pub tool_use_id: String,
    /// Echoed back unchanged: the server drops a result it cannot match.
    pub id: u32,
    pub exec_id: String,
    pub kind: PendingExecKind,
}

/// Why the pump stopped.
#[derive(Debug)]
pub enum PumpOutcome {
    /// Claude Code owes this exec a result; the run stays open.
    Tool(PendingExec),
    /// A tool_use recovered from the text channel. The run cannot answer it, so
    /// it ends here and the next request rebuilds the prompt with the result.
    Replay,
    /// The turn ended.
    Ended,
    /// Upstream transport/idle stream failed before any non-thinking block was emitted.
    Aborted(String),
    Failed(String),
}

/// Everything kept between two Claude Code requests of the same session.
pub struct LiveBridge {
    pub session_id: String,
    pub run: LiveRun,
    pub pending: Option<PendingExec>,
    pub allowed: Option<BTreeSet<String>>,
    /// Estimated size of the request that opened or resumed the run.
    pub input_floor: u64,
    last_activity: Instant,
}

impl LiveBridge {
    pub fn new(session_id: String, run: LiveRun, allowed: Option<BTreeSet<String>>) -> Self {
        Self {
            session_id,
            run,
            pending: None,
            allowed,
            input_floor: 0,
            last_activity: Instant::now(),
        }
    }

    fn touch(&mut self) {
        self.last_activity = Instant::now();
    }
}

impl std::fmt::Debug for LiveBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveBridge")
            .field("session_id", &self.session_id)
            .field("pending", &self.pending)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Registry of open runs
// ---------------------------------------------------------------------------

static BRIDGE_REGISTRY: Lazy<Mutex<Vec<LiveBridge>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// One open run per session.
///
/// Every path out of the pump removes the entry: a finished turn, a dead run
/// and a new run for the same session all drop it, and [`Self::sweep`] drops
/// what nobody came back for. An entry without a pending exec never blocks the
/// next request's own exec -- the caller only resumes when the request carries
/// the matching `tool_result`, and drops the entry otherwise.
pub struct BridgeRegistry;

impl BridgeRegistry {
    /// Store a run, replacing any entry the session already had.
    pub fn insert(bridge: LiveBridge) {
        let mut registry = BRIDGE_REGISTRY.lock().unwrap();
        registry.retain(|entry| entry.session_id != bridge.session_id);
        registry.push(bridge);
    }

    /// Remove and return the session's run.
    pub fn take(session_id: &str) -> Option<LiveBridge> {
        let mut registry = BRIDGE_REGISTRY.lock().unwrap();
        let position = registry
            .iter()
            .position(|entry| entry.session_id == session_id)?;
        Some(registry.swap_remove(position))
    }

    pub fn remove(session_id: &str) {
        let mut registry = BRIDGE_REGISTRY.lock().unwrap();
        registry.retain(|entry| entry.session_id != session_id);
    }

    /// Drop runs idle for longer than `max_idle`, returning how many went.
    pub fn sweep(max_idle: Duration) -> usize {
        let mut registry = BRIDGE_REGISTRY.lock().unwrap();
        let before = registry.len();
        registry.retain(|entry| entry.last_activity.elapsed() < max_idle);
        before - registry.len()
    }

    pub fn sweep_abandoned() -> usize {
        Self::sweep(RUN_MAX_IDLE)
    }

    pub fn clear() {
        let mut registry = BRIDGE_REGISTRY.lock().unwrap();
        registry.clear();
    }

    pub fn active_count() -> usize {
        BRIDGE_REGISTRY.lock().unwrap().len()
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

// ---------------------------------------------------------------------------
// Live pump
// ---------------------------------------------------------------------------

/// Run the live pump: upstream events in, Anthropic SSE out.
///
/// Stops at the first exec the agent asks for (the run stays open for the
/// answer), at the end of the turn, or on a stream error. Bytes are handed to
/// `sink` as they are produced, so the client sees deltas while the turn runs.
pub async fn pump_live_bridge(
    bridge: &mut LiveBridge,
    message_id: &str,
    model: &str,
    sink: &mut (dyn FnMut(&[u8]) + Send),
) -> PumpOutcome {
    let mut framer = CursorSseFramer::new(message_id, model);
    framer.set_input_floor(bridge.input_floor);
    let mut xml = CursorToolUseXmlParser::new(bridge.allowed.clone());

    loop {
        let event = match bridge.run.events.recv().await {
            Some(event) => event,
            // The reader task only ends with the stream; treat it as the turn.
            None => {
                flush_xml(&mut xml, &mut framer, sink);
                framer.finalize();
                sink(&framer.take_output());
                return PumpOutcome::Ended;
            }
        };
        bridge.touch();

        match event {
            LiveEvent::Message(message) => {
                if let Some(exec) = message.exec_server_message.as_ref() {
                    match exec_action(exec, bridge.allowed.as_ref()) {
                        ExecAction::Call { name, input, kind } => {
                            let tool_use_id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                            let input_json =
                                serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                            framer.emit_tool_pause(&tool_use_id, name, &input_json);
                            sink(&framer.take_output());
                            return PumpOutcome::Tool(PendingExec {
                                tool_use_id,
                                id: exec.id.unwrap_or(0),
                                exec_id: exec.exec_id.clone().unwrap_or_default(),
                                kind,
                            });
                        }
                        ExecAction::Answer(reply) => {
                            // Never leave the request half-answered: the agent
                            // keeps its plan as long as every exec gets a reply.
                            if !answer(exec, reply, &bridge.run).await {
                                framer.finalize();
                                sink(&framer.take_output());
                                return PumpOutcome::Ended;
                            }
                        }
                    }
                }

                if let Some(details) = message
                    .conversation_checkpoint_update
                    .as_ref()
                    .and_then(|checkpoint| checkpoint.token_details.as_ref())
                {
                    framer.set_context_tokens(u64::from(details.used_tokens));
                }
                if let Some(update) = message.interaction_update.as_ref() {
                    if let Some(thinking) = update.thinking_delta.as_ref()
                        && !thinking.text.is_empty()
                    {
                        framer.emit_thinking_delta(&thinking.text);
                    }
                    if let Some(text) = update.text_delta.as_ref()
                        && !text.text.is_empty()
                        && pump_text(&text.text, &mut xml, &mut framer)
                    {
                        sink(&framer.take_output());
                        return PumpOutcome::Replay;
                    }
                    if let Some(usage) = update.turn_ended.as_ref() {
                        framer.record_usage(
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cache_read_tokens,
                            usage.cache_write_tokens,
                        );
                    }
                }

                sink(&framer.take_output());
            }
            LiveEvent::End => {
                flush_xml(&mut xml, &mut framer, sink);
                framer.finalize();
                sink(&framer.take_output());
                return PumpOutcome::Ended;
            }
            LiveEvent::Error(error) => {
                if !framer.has_non_thinking_output()
                    && (error.starts_with("read body:")
                        || error == "Cursor upstream went quiet with the run open")
                {
                    return PumpOutcome::Aborted(error);
                }
                framer.finalize();
                sink(&framer.take_output());
                sink(&format_sse_error(&error));
                return PumpOutcome::Failed(error);
            }
        }
    }
}

/// Feed one text delta through the XML recovery parser.
///
/// Returns `true` when a `<tool_use>` was recovered: the text channel cannot
/// answer it, so the pump stops there.
fn pump_text(
    text: &str,
    xml: &mut CursorToolUseXmlParser,
    framer: &mut CursorSseFramer<'_>,
) -> bool {
    for event in xml.push(text) {
        match event {
            RecoveredCursorEvent::Text(text) => framer.emit_text_delta(&text),
            RecoveredCursorEvent::ToolUse(tool_use) => {
                let input_json =
                    serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                return true;
            }
        }
    }
    false
}

fn flush_xml(
    xml: &mut CursorToolUseXmlParser,
    framer: &mut CursorSseFramer<'_>,
    sink: &mut (dyn FnMut(&[u8]) + Send),
) {
    for event in xml.flush() {
        match event {
            RecoveredCursorEvent::Text(text) => framer.emit_text_delta(&text),
            RecoveredCursorEvent::ToolUse(tool_use) => {
                let input_json =
                    serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
            }
        }
    }
    sink(&framer.take_output());
}

// ---------------------------------------------------------------------------
// Exec mapping
// ---------------------------------------------------------------------------

/// What the bridge does with one exec request.
enum ExecAction {
    /// Hand it to Claude Code and pause.
    Call {
        name: &'static str,
        input: serde_json::Value,
        kind: PendingExecKind,
    },
    /// Answer it here: a type the bridge has no Claude tool for, or a write it
    /// refuses.
    Answer(ExecAnswer),
}

/// The in-band answer to an exec the bridge serves itself.
enum ExecAnswer {
    /// A typed result. The oneof is never left empty: the server reads that as
    /// "the tool ran and produced nothing".
    Result(proto::exec_client_message::Message),
    /// A variant this build cannot name. `throw` travels on the control
    /// channel and is the protocol's failure path, not an empty result.
    Unsupported,
}

fn exec_action(exec: &proto::ExecServerMessage, allowed: Option<&BTreeSet<String>>) -> ExecAction {
    let allows = |name: &str| allowed.is_none_or(|names| names.contains(name));

    if let Some(read) = exec.read_args.as_ref() {
        if !allows("Read") {
            return ExecAction::Answer(ExecAnswer::Result(
                proto::exec_client_message::Message::ReadResult(proto::ReadResult {
                    result: Some(proto::read_result::Result::Rejected(proto::ReadRejected {
                        path: read.path.clone(),
                        reason: "Read is not available to this client".to_string(),
                    })),
                }),
            ));
        }
        let mut input = serde_json::json!({ "file_path": read.path });
        // Cursor may index the window from the end with a negative offset;
        // Claude Code's Read has no such form, and a wrong line number is worse
        // than reading the whole file.
        if let Some(offset) = read.offset.filter(|offset| *offset >= 0) {
            input["offset"] = offset.into();
        }
        if let Some(limit) = read.limit.filter(|limit| *limit > 0) {
            input["limit"] = limit.into();
        }
        return ExecAction::Call {
            name: "Read",
            input,
            kind: PendingExecKind::Read {
                path: read.path.clone(),
            },
        };
    }

    if let Some(write) = exec.write_args.as_ref() {
        let content = match write_content(write) {
            Ok(content) => content,
            Err(error) => {
                // Answering instead of writing an empty file over a real one.
                return ExecAction::Answer(ExecAnswer::Result(
                    proto::exec_client_message::Message::WriteResult(proto::WriteResult {
                        result: Some(proto::write_result::Result::Error(proto::WriteError {
                            path: write.path.clone(),
                            error,
                        })),
                    }),
                ));
            }
        };
        if !allows("Write") {
            return ExecAction::Answer(ExecAnswer::Result(
                proto::exec_client_message::Message::WriteResult(proto::WriteResult {
                    result: Some(proto::write_result::Result::Rejected(
                        proto::WriteRejected {
                            path: write.path.clone(),
                            reason: "Write is not available to this client".to_string(),
                        },
                    )),
                }),
            ));
        }
        return ExecAction::Call {
            name: "Write",
            input: serde_json::json!({ "file_path": write.path, "content": content }),
            kind: PendingExecKind::Write {
                path: write.path.clone(),
                content,
            },
        };
    }

    if let Some(shell) = exec.shell_stream_args.as_ref().or(exec.shell_args.as_ref()) {
        let stream = exec.shell_stream_args.is_some();
        if !allows("Bash") {
            return ExecAction::Answer(ExecAnswer::Result(
                proto::exec_client_message::Message::ShellResult(rejected_shell_result(
                    shell,
                    "Bash is not available to this client",
                )),
            ));
        }
        let cwd = shell.working_directory.clone().unwrap_or_default();
        let command = match cwd.is_empty() {
            true => shell.command.clone(),
            // Claude Code's Bash has no cwd field, so the directory rides in
            // the command. The exec's cwd is what the agent expects to run in.
            false => format!("cd {} && {}", shell_quote(&cwd), shell.command),
        };
        let mut input = serde_json::json!({ "command": command });
        if let Some(timeout) = shell_timeout_ms(shell) {
            input["timeout"] = timeout.into();
        }
        return ExecAction::Call {
            name: "Bash",
            input,
            kind: PendingExecKind::Shell {
                command: shell.command.clone(),
                cwd,
                stream,
            },
        };
    }

    if let Some(grep) = exec.grep_args.as_ref() {
        return ExecAction::Answer(ExecAnswer::Result(
            proto::exec_client_message::Message::GrepResult(proto::GrepResult {
                result: Some(proto::grep_result::Result::Error(proto::GrepError {
                    error: format!(
                        "This client has no Grep tool wired to Cursor: pattern {:?} not searched",
                        grep.pattern
                    ),
                })),
            }),
        ));
    }

    if let Some(ls) = exec.ls_args.as_ref() {
        return ExecAction::Answer(ExecAnswer::Result(
            proto::exec_client_message::Message::LsResult(proto::LsResult {
                result: Some(proto::ls_result::Result::Rejected(proto::LsRejected {
                    path: ls.path.clone(),
                    reason: "This client has no directory listing wired to Cursor".to_string(),
                })),
            }),
        ));
    }

    if let Some(delete) = exec.delete_args.as_ref() {
        return ExecAction::Answer(ExecAnswer::Result(
            proto::exec_client_message::Message::DeleteResult(proto::DeleteResult {
                result: Some(proto::delete_result::Result::Rejected(
                    proto::DeleteRejected {
                        path: delete.path.clone(),
                        reason: "This client has no Delete tool wired to Cursor".to_string(),
                    },
                )),
            }),
        ));
    }

    if exec.request_context_args.is_some() {
        // The agent asks for the workspace context. This client renders the
        // prompt itself, so the only part it can answer honestly is the
        // environment -- and the answer has to carry one, or the server fails
        // the whole run with "Failed to get request context".
        return ExecAction::Answer(ExecAnswer::Result(
            proto::exec_client_message::Message::RequestContextResult(
                proto::RequestContextResult {
                    result: Some(proto::request_context_result::Result::Success(
                        proto::RequestContextSuccess {
                            request_context: Some(proto::RequestContext {
                                env: Some(crate::providers::cursor::client::request_context_env()),
                            }),
                        },
                    )),
                },
            ),
        ));
    }

    ExecAction::Answer(ExecAnswer::Unsupported)
}

/// The text a `WriteArgs` carries.
///
/// A byte-backed write leaves `file_text` empty; writing it as an empty file
/// would destroy whatever is at `path`.
fn write_content(write: &proto::WriteArgs) -> Result<String, String> {
    if !write.file_text.is_empty() {
        return Ok(write.file_text.clone());
    }
    match write.file_bytes.as_ref() {
        Some(bytes) if !bytes.is_empty() => String::from_utf8(bytes.clone()).map_err(|_| {
            let hint = write
                .encoding_hint
                .as_deref()
                .map(|hint| format!(" (encoding hint {hint:?})"))
                .unwrap_or_default();
            format!(
                "Refusing to write {} bytes that are not UTF-8{hint}: this bridge writes text only",
                bytes.len()
            )
        }),
        _ => Ok(String::new()),
    }
}

/// Cursor's shell timeout, converted to the milliseconds Claude Code's Bash
/// takes.
///
/// Measured on the wire 12/10/2026 (official CLI run): `timeout` 30000 with
/// `hard_timeout` 86400000 -- both milliseconds, matching the agent bundle,
/// which feeds the value to `setTimeout` and defaults it to 30000.
fn shell_timeout_ms(shell: &proto::ShellArgs) -> Option<i64> {
    let requested = match shell.timeout {
        Some(timeout) if timeout > 0 => i64::from(timeout),
        _ => match shell.hard_timeout {
            Some(hard) if hard > 0 => i64::from(hard),
            _ => return None,
        },
    };
    Some(requested.min(MAX_BASH_TIMEOUT_MS))
}

/// Quote a path for `cd <path> &&` inside a shell command.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn rejected_shell_result(shell: &proto::ShellArgs, reason: &str) -> proto::ShellResult {
    proto::ShellResult {
        result: Some(proto::shell_result::Result::Failure(proto::ShellFailure {
            command: shell.command.clone(),
            working_directory: shell.working_directory.clone().unwrap_or_default(),
            exit_code: 1,
            stdout: String::new(),
            stderr: reason.to_string(),
            execution_time: 0,
        })),
    }
}

// ---------------------------------------------------------------------------
// Writing answers back on the open run
// ---------------------------------------------------------------------------

/// Send one exec answer, then close the exec stream.
///
/// Returns `false` when the run is gone; the pump stops rather than pretend the
/// answer landed.
async fn answer(exec: &proto::ExecServerMessage, message: ExecAnswer, run: &LiveRun) -> bool {
    let id = exec.id.unwrap_or(0);
    let messages = match message {
        ExecAnswer::Result(result) => vec![
            proto::AgentClientMessage {
                exec_client_message: Some(proto::ExecClientMessage {
                    id,
                    exec_id: exec.exec_id.clone().unwrap_or_default(),
                    message: Some(result),
                }),
                ..Default::default()
            },
            stream_close(id),
        ],
        ExecAnswer::Unsupported => vec![
            proto::AgentClientMessage {
                exec_client_control_message: Some(proto::ExecClientControlMessage {
                    message: Some(proto::exec_client_control_message::Message::Throw(
                        proto::ExecClientThrow {
                            id,
                            error: "Unknown exec message variant".to_string(),
                        },
                    )),
                }),
                ..Default::default()
            },
            stream_close(id),
        ],
    };

    for message in messages {
        if run.send(&message).await.is_err() {
            return false;
        }
    }
    true
}

/// Claude Code's Read answers in `cat -n` form (`     1\talpha`), and may add
/// `<system-reminder>` text after the file. Cursor's `ReadSuccess.content` is
/// the file itself: handing it the numbered form made the agent write the
/// numbers back into the file (measured 13/10: `1\tbeta` on disk).
fn raw_file_text(read_output: &str) -> String {
    let mut lines = Vec::new();
    for line in read_output.lines() {
        let digits = line.trim_start();
        let Some(tab) = digits.find('\t') else { break };
        if tab == 0 || !digits[..tab].bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        lines.push(&digits[tab + 1..]);
    }
    if lines.is_empty() {
        // An empty file comes back as a reminder only; anything else is not
        // the numbered form and passes through untouched.
        return if read_output.trim_start().starts_with("<system-reminder>") {
            String::new()
        } else {
            read_output.to_string()
        };
    }
    lines.join("\n")
}

fn stream_close(id: u32) -> proto::AgentClientMessage {
    proto::AgentClientMessage {
        exec_client_control_message: Some(proto::ExecClientControlMessage {
            message: Some(proto::exec_client_control_message::Message::StreamClose(
                proto::ExecClientStreamClose { id },
            )),
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Claude Code result -> Cursor result
// ---------------------------------------------------------------------------

/// Build the reply for a `tool_result` Claude Code produced.
pub fn exec_result_messages(
    pending: &PendingExec,
    result: &serde_json::Value,
) -> Vec<proto::AgentClientMessage> {
    let content = render_tool_result_content(result);
    let is_error = tool_result_is_error(result);

    let message = match &pending.kind {
        PendingExecKind::Read { path } => {
            proto::exec_client_message::Message::ReadResult(proto::ReadResult {
                result: Some(if is_error {
                    proto::read_result::Result::Error(proto::ReadError {
                        path: path.clone(),
                        error: content,
                    })
                } else {
                    let content = raw_file_text(&content);
                    proto::read_result::Result::Success(proto::ReadSuccess {
                        path: path.clone(),
                        total_lines: content.lines().count() as i32,
                        file_size: content.len() as i64,
                        truncated: false,
                        output: Some(proto::read_success::Output::Content(content)),
                    })
                }),
            })
        }
        PendingExecKind::Write {
            path,
            content: written,
        } => proto::exec_client_message::Message::WriteResult(proto::WriteResult {
            result: Some(if is_error {
                proto::write_result::Result::Error(proto::WriteError {
                    path: path.clone(),
                    error: content,
                })
            } else {
                proto::write_result::Result::Success(proto::WriteSuccess {
                    path: path.clone(),
                    lines_created: written.lines().count() as i32,
                    file_size: written.len() as i32,
                })
            }),
        }),
        PendingExecKind::Shell { stream, .. } => {
            let code = u32::from(is_error);
            if *stream {
                return shell_stream_messages(pending, &content, code, is_error);
            }
            proto::exec_client_message::Message::ShellResult(shell_result(
                pending,
                &content,
                code as i32,
                is_error,
            ))
        }
    };

    vec![
        client_message(pending, message),
        stream_close_message(pending),
    ]
}

/// The structured shell outcome: a success or a failure with the same text.
///
/// The command and directory travel with it: they are what the server renders
/// into the tool message the model reads.
fn shell_result(
    pending: &PendingExec,
    content: &str,
    code: i32,
    is_error: bool,
) -> proto::ShellResult {
    let (command, working_directory) = match &pending.kind {
        PendingExecKind::Shell { command, cwd, .. } => (command.clone(), shell_cwd(cwd)),
        _ => (String::new(), shell_cwd("")),
    };
    let (stdout, stderr) = match is_error {
        true => (String::new(), content.to_string()),
        false => (content.to_string(), String::new()),
    };
    proto::ShellResult {
        result: Some(if is_error {
            proto::shell_result::Result::Failure(proto::ShellFailure {
                command,
                working_directory,
                exit_code: code,
                stdout,
                stderr,
                execution_time: 0,
            })
        } else {
            proto::shell_result::Result::Success(proto::ShellSuccess {
                command,
                working_directory,
                exit_code: code,
                stdout,
                stderr,
                execution_time: 0,
            })
        }),
    }
}

/// The directory a shell exec ran in: what the agent asked for, or the proxy's
/// own working directory when it named none.
fn shell_cwd(cwd: &str) -> String {
    if !cwd.is_empty() {
        return cwd.to_string();
    }
    std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_default()
}

/// The streamed shell answer: stdout, exit, the structured acknowledgement
/// Cursor needs to close the run, then the stream close.
fn shell_stream_messages(
    pending: &PendingExec,
    content: &str,
    code: u32,
    is_error: bool,
) -> Vec<proto::AgentClientMessage> {
    let cwd = match &pending.kind {
        PendingExecKind::Shell { cwd, .. } => shell_cwd(cwd),
        _ => shell_cwd(""),
    };

    let mut stream = vec![proto::shell_stream::Event::Start(
        proto::ShellStreamStart {},
    )];
    if !content.is_empty() {
        stream.push(if is_error {
            proto::shell_stream::Event::Stderr(proto::ShellStreamStderr {
                data: content.to_string(),
            })
        } else {
            proto::shell_stream::Event::Stdout(proto::ShellStreamStdout {
                data: content.to_string(),
            })
        });
    }
    stream.push(proto::shell_stream::Event::Exit(proto::ShellStreamExit {
        code,
        cwd,
        aborted: false,
    }));

    let messages = stream
        .into_iter()
        .map(|event| {
            client_message(
                pending,
                proto::exec_client_message::Message::ShellStream(proto::ShellStream {
                    event: Some(event),
                }),
            )
        })
        .chain(std::iter::once(client_message(
            pending,
            proto::exec_client_message::Message::ShellResult(shell_result(
                pending,
                content,
                code as i32,
                is_error,
            )),
        )));

    messages
        .chain(std::iter::once(stream_close_message(pending)))
        .collect()
}

fn client_message(
    pending: &PendingExec,
    message: proto::exec_client_message::Message,
) -> proto::AgentClientMessage {
    proto::AgentClientMessage {
        exec_client_message: Some(proto::ExecClientMessage {
            id: pending.id,
            exec_id: pending.exec_id.clone(),
            message: Some(message),
        }),
        ..Default::default()
    }
}

fn stream_close_message(pending: &PendingExec) -> proto::AgentClientMessage {
    proto::AgentClientMessage {
        exec_client_control_message: Some(proto::ExecClientControlMessage {
            message: Some(proto::exec_client_control_message::Message::StreamClose(
                proto::ExecClientStreamClose { id: pending.id },
            )),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::cursor::test_frames;
    use prost::Message as _;
    use std::sync::Mutex;

    /// Serialize tests that share the global bridge registry.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    fn exec_message(id: u32, exec_id: &str) -> proto::ExecServerMessage {
        proto::ExecServerMessage {
            id: Some(id),
            exec_id: Some(exec_id.to_string()),
            ..Default::default()
        }
    }

    fn read_exec(id: u32, exec_id: &str, path: &str) -> proto::ExecServerMessage {
        let mut exec = exec_message(id, exec_id);
        exec.read_args = Some(proto::ReadArgs {
            path: path.to_string(),
            ..Default::default()
        });
        exec
    }

    fn answer_of(action: ExecAction) -> ExecAnswer {
        match action {
            ExecAction::Answer(answer) => answer,
            ExecAction::Call { name, .. } => {
                panic!("expected an in-band answer, got a {name} call")
            }
        }
    }

    fn result_of(action: ExecAction) -> proto::exec_client_message::Message {
        match answer_of(action) {
            ExecAnswer::Result(result) => result,
            ExecAnswer::Unsupported => panic!("expected a typed result, got a throw"),
        }
    }

    // -----------------------------------------------------------------------
    // Codec: the frames the server reads back, byte for byte
    // -----------------------------------------------------------------------

    /// Hand-written bytes for a read answer. prost writes fields in tag order,
    /// so `exec_id` (tag 15) lands after the result:
    /// AgentClientMessage tag 2 (exec_client_message) len 36 >
    ///   id tag 1 = 7,
    ///   read_result tag 7 len 21 > success tag 1 len 19 >
    ///     path tag 1 "/tmp/a", content tag 2 (inside the output oneof) "hello",
    ///     total_lines tag 3 = 1, file_size tag 4 = 5,
    ///   exec_id tag 15 = "exec-read".
    #[test]
    fn read_result_frame_matches_hand_written_bytes() {
        let client = proto::AgentClientMessage {
            exec_client_message: Some(proto::ExecClientMessage {
                id: 7,
                exec_id: "exec-read".to_string(),
                message: Some(proto::exec_client_message::Message::ReadResult(
                    proto::ReadResult {
                        result: Some(proto::read_result::Result::Success(proto::ReadSuccess {
                            path: "/tmp/a".to_string(),
                            total_lines: 1,
                            file_size: 5,
                            truncated: false,
                            output: Some(proto::read_success::Output::Content("hello".to_string())),
                        })),
                    },
                )),
            }),
            ..Default::default()
        };

        assert_eq!(
            hex::encode(client.encode_to_vec()),
            concat!(
                "1224",                   // exec_client_message, 36 bytes
                "0807",                   // id = 7
                "3a15",                   // read_result, 21 bytes
                "0a13",                   // success, 19 bytes
                "0a062f746d702f61",       // path = "/tmp/a"
                "120568656c6c6f",         // output.content = "hello"
                "1801",                   // total_lines = 1
                "2005",                   // file_size = 5
                "7a09657865632d72656164", // exec_id = "exec-read"
            )
        );
    }

    /// Hand-written bytes for a write answer:
    /// AgentClientMessage tag 2 (exec_client_message) len 18 >
    ///   id tag 1 = 3, exec_id empty (stays off the wire as the default),
    ///   write_result tag 3 len 14 > success tag 1 len 12 >
    ///     path tag 1 "/tmp/b", lines_created tag 2 = 2, file_size tag 3 = 11.
    #[test]
    fn write_result_frame_matches_hand_written_bytes() {
        let client = proto::AgentClientMessage {
            exec_client_message: Some(proto::ExecClientMessage {
                id: 3,
                exec_id: String::new(),
                message: Some(proto::exec_client_message::Message::WriteResult(
                    proto::WriteResult {
                        result: Some(proto::write_result::Result::Success(proto::WriteSuccess {
                            path: "/tmp/b".to_string(),
                            lines_created: 2,
                            file_size: 11,
                        })),
                    },
                )),
            }),
            ..Default::default()
        };

        assert_eq!(
            hex::encode(client.encode_to_vec()),
            concat!(
                "1212",             // exec_client_message, 18 bytes
                "0803",             // id = 3
                "1a0e",             // write_result, 14 bytes
                "0a0c",             // success, 12 bytes
                "0a062f746d702f62", // path = "/tmp/b"
                "1002",             // lines_created = 2
                "180b",             // file_size = 11
            )
        );
    }

    /// The shell stream: a sequence on ONE id that ends with `exit`. Without
    /// the exit the server never closes the execution.
    #[test]
    fn shell_stream_frames_match_hand_written_bytes() {
        let pending = PendingExec {
            tool_use_id: "toolu_1".to_string(),
            id: 9,
            exec_id: "e".to_string(),
            kind: PendingExecKind::Shell {
                command: "echo hi".to_string(),
                cwd: "/tmp".to_string(),
                stream: true,
            },
        };
        let result = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "hi"
        });

        let messages = exec_result_messages(&pending, &result);
        let frames: Vec<String> = messages
            .iter()
            .filter_map(|message| message.exec_client_message.as_ref())
            .map(|message| hex::encode(message.encode_to_vec()))
            .collect();

        // Each entry is the ExecClientMessage the AgentClientMessage wraps.
        assert_eq!(
            frames,
            vec![
                // start: id = 9, shell_stream tag 14 len 2 > start tag 4,
                // empty, exec_id "e"
                "0809720222007a0165".to_string(),
                // stdout: shell_stream len 6 > stdout tag 1 len 4 > data "hi"
                "080972060a040a0268697a0165".to_string(),
                // exit: code 0 is the proto3 default and stays off the wire,
                // cwd = "/tmp"; aborted=false likewise.
                "080972081a0612042f746d707a0165".to_string(),
                // the structured acknowledgement Cursor needs to close the
                // step: shell_result tag 2 len 21 > success tag 1 len 19 >
                // command tag 1 = "echo hi", working_directory tag 2 = "/tmp",
                // stdout tag 5 = "hi"
                "080912150a130a076563686f20686912042f746d702a0268697a0165".to_string(),
            ]
        );

        // ... and the control message that closes the exec stream.
        assert_eq!(
            hex::encode(messages.last().expect("stream close").encode_to_vec()),
            // exec_client_control_message tag 5 len 4 > stream_close tag 1
            // len 2 > id = 9
            "2a040a020809"
        );
    }

    /// A run that asks for a tool Claude Code can serve: the pump pauses with
    /// the exec's id/exec_id/kind, and the reply echoes both ids back.
    #[test]
    fn read_tool_result_echoes_the_exec_ids_on_the_reply() {
        let pending = PendingExec {
            tool_use_id: "toolu_7".to_string(),
            id: 7,
            exec_id: "exec-read".to_string(),
            kind: PendingExecKind::Read {
                path: "/tmp/a".to_string(),
            },
        };
        let result = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "toolu_7",
            "content": "file body",
            "is_error": false
        });

        let messages = exec_result_messages(&pending, &result);
        let reply = messages
            .first()
            .and_then(|message| message.exec_client_message.as_ref())
            .expect("read result");
        assert_eq!(reply.id, 7, "the exec id has to be echoed exactly");
        assert_eq!(reply.exec_id, "exec-read");
        match reply.message.as_ref() {
            Some(proto::exec_client_message::Message::ReadResult(result)) => {
                match result.result.as_ref() {
                    Some(proto::read_result::Result::Success(success)) => {
                        assert_eq!(success.path, "/tmp/a");
                        assert_eq!(
                            success.output,
                            Some(proto::read_success::Output::Content(
                                "file body".to_string()
                            ))
                        );
                    }
                    other => panic!("expected a read success, got {other:?}"),
                }
            }
            other => panic!("expected a read result, got {other:?}"),
        }

        // And the same ids close the exec stream.
        let close = messages
            .last()
            .and_then(|message| message.exec_client_control_message.as_ref())
            .and_then(|control| control.message.as_ref())
            .expect("stream close");
        match close {
            proto::exec_client_control_message::Message::StreamClose(close) => {
                assert_eq!(close.id, 7)
            }
            other => panic!("expected a streamClose, got {other:?}"),
        }
    }

    #[test]
    fn failed_read_tool_result_becomes_a_typed_read_error() {
        let pending = PendingExec {
            tool_use_id: "toolu_8".to_string(),
            id: 8,
            exec_id: "exec-read".to_string(),
            kind: PendingExecKind::Read {
                path: "/nope".to_string(),
            },
        };
        let result = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "toolu_8",
            "content": "ENOENT",
            "is_error": true
        });

        let messages = exec_result_messages(&pending, &result);
        match messages[0].exec_client_message.as_ref() {
            Some(message) => match message.message.as_ref() {
                Some(proto::exec_client_message::Message::ReadResult(result)) => {
                    match result.result.as_ref() {
                        Some(proto::read_result::Result::Error(error)) => {
                            assert_eq!(error.path, "/nope");
                            assert_eq!(error.error, "ENOENT");
                        }
                        other => panic!("expected a read error, got {other:?}"),
                    }
                }
                other => panic!("expected a read result, got {other:?}"),
            },
            None => panic!("missing exec message"),
        }
    }

    // -----------------------------------------------------------------------
    // Decode: an exec with its ids
    // -----------------------------------------------------------------------

    #[test]
    fn decodes_grep_exec_with_ids() {
        let frame = test_frames::native_grep_exec_frame("TODO", "/tmp", 4, "exec-grep");
        let mut decoder = crate::providers::cursor::connect::ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        let message = crate::providers::cursor::client::decode_frame_payload(&frames[0]).unwrap();
        let exec = message.exec_server_message.expect("exec frame");
        assert_eq!(exec.id, Some(4));
        assert_eq!(exec.exec_id.as_deref(), Some("exec-grep"));
        assert_eq!(exec.grep_args.as_ref().unwrap().pattern, "TODO");
        assert_eq!(
            exec.grep_args.as_ref().unwrap().path.as_deref(),
            Some("/tmp")
        );
    }

    /// An exec the bridge cannot serve still has to be answered, and the answer
    /// never carries an empty oneof: the server reads that as "ran and produced
    /// nothing" and waits for a reply that will not come.
    #[test]
    fn unsupported_exec_produces_a_typed_result_never_an_empty_oneof() {
        let mut grep_exec = exec_message(11, "exec-grep");
        grep_exec.grep_args = Some(proto::GrepArgs {
            pattern: "TODO".to_string(),
            path: None,
            tool_call_id: None,
        });

        match result_of(exec_action(&grep_exec, None)) {
            proto::exec_client_message::Message::GrepResult(result) => match result.result {
                Some(proto::grep_result::Result::Error(error)) => {
                    assert!(error.error.contains("TODO"))
                }
                other => panic!("expected a grep error, got {other:?}"),
            },
            other => panic!("expected a grep result, got {other:?}"),
        }

        let mut ls_exec = exec_message(12, "exec-ls");
        ls_exec.ls_args = Some(proto::LsArgs {
            path: "/tmp".to_string(),
            tool_call_id: None,
        });
        assert!(matches!(
            result_of(exec_action(&ls_exec, None)),
            proto::exec_client_message::Message::LsResult(_)
        ));

        let mut delete_exec = exec_message(13, "exec-delete");
        delete_exec.delete_args = Some(proto::DeleteArgs {
            path: "/tmp/a".to_string(),
            tool_call_id: None,
        });
        assert!(matches!(
            result_of(exec_action(&delete_exec, None)),
            proto::exec_client_message::Message::DeleteResult(_)
        ));

        // A variant this build cannot name goes out as a throw, not as an
        // empty result.
        let mut unknown = exec_message(14, "exec-unknown");
        unknown.exec_id = Some("exec-unknown".to_string());
        assert!(matches!(
            answer_of(exec_action(&unknown, None)),
            ExecAnswer::Unsupported
        ));

        // And every one of those survives the wire with a non-empty oneof and
        // its ids intact.
        let client = proto::AgentClientMessage {
            exec_client_message: Some(proto::ExecClientMessage {
                id: 11,
                exec_id: "exec-grep".to_string(),
                message: Some(proto::exec_client_message::Message::GrepResult(
                    proto::GrepResult {
                        result: Some(proto::grep_result::Result::Error(proto::GrepError {
                            error: "no".to_string(),
                        })),
                    },
                )),
            }),
            ..Default::default()
        };
        let decoded = proto::AgentClientMessage::decode(client.encode_to_vec().as_slice()).unwrap();
        let inner = decoded.exec_client_message.expect("exec_client_message");
        assert!(inner.message.is_some(), "empty oneof on the wire");
        assert_eq!(inner.id, 11);
        assert_eq!(inner.exec_id, "exec-grep");
    }

    /// Every typed answer is closed on the same id, so the server stops holding
    /// the execution open.
    #[tokio::test]
    async fn every_answer_ends_with_a_stream_close_on_the_same_id() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let run = LiveRun::from_channel_for_test(tx);
        let exec = grep_exec(21);

        let sent = answer(
            &exec,
            ExecAnswer::Result(proto::exec_client_message::Message::GrepResult(
                proto::GrepResult {
                    result: Some(proto::grep_result::Result::Error(proto::GrepError {
                        error: "no".to_string(),
                    })),
                },
            )),
            &run,
        )
        .await;
        assert!(sent);

        let mut close_id = None;
        let mut result_id = None;
        while let Ok(Ok(frame)) = rx.try_recv() {
            let message = decode_client_frame(&frame);
            if let Some(inner) = message.exec_client_message {
                result_id = Some(inner.id);
            }
            if let Some(close) = message
                .exec_client_control_message
                .and_then(|control| control.message)
                && let proto::exec_client_control_message::Message::StreamClose(close) = close
            {
                close_id = Some(close.id);
            }
        }
        assert_eq!(result_id, Some(21));
        assert_eq!(close_id, Some(21));
    }

    fn grep_exec(id: u32) -> proto::ExecServerMessage {
        let mut exec = exec_message(id, "exec-grep");
        exec.grep_args = Some(proto::GrepArgs {
            pattern: "TODO".to_string(),
            path: None,
            tool_call_id: None,
        });
        exec
    }

    fn decode_client_frame(frame: &bytes::Bytes) -> proto::AgentClientMessage {
        let mut decoder = crate::providers::cursor::connect::ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        proto::AgentClientMessage::decode(frames[0].payload.clone()).unwrap()
    }

    // -----------------------------------------------------------------------
    // Argument mapping
    // -----------------------------------------------------------------------

    #[test]
    fn shell_exec_carries_working_directory_and_timeout_to_bash() {
        let mut exec = exec_message(1, "exec-shell");
        exec.shell_stream_args = Some(proto::ShellArgs {
            command: "ls".to_string(),
            working_directory: Some("/tmp/it's here".to_string()),
            timeout: Some(30_000),
            tool_call_id: None,
            hard_timeout: None,
        });

        match exec_action(&exec, None) {
            ExecAction::Call { name, input, kind } => {
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "cd '/tmp/it'\"'\"'s here' && ls");
                assert_eq!(input["timeout"], 30_000);
                assert_eq!(
                    kind,
                    PendingExecKind::Shell {
                        command: "ls".to_string(),
                        cwd: "/tmp/it's here".to_string(),
                        stream: true,
                    }
                );
            }
            _ => panic!("expected a Bash call"),
        }
    }

    #[test]
    fn shell_without_a_working_directory_keeps_the_command_untouched() {
        let mut exec = exec_message(2, "exec-shell");
        exec.shell_args = Some(proto::ShellArgs {
            command: "pwd".to_string(),
            working_directory: None,
            timeout: None,
            tool_call_id: None,
            hard_timeout: None,
        });

        match exec_action(&exec, None) {
            ExecAction::Call { input, kind, .. } => {
                assert_eq!(input["command"], "pwd");
                assert!(input.get("timeout").is_none());
                assert!(matches!(kind, PendingExecKind::Shell { stream: false, .. }));
            }
            _ => panic!("expected a Bash call"),
        }
    }

    #[test]
    fn shell_timeout_is_clamped_to_what_bash_accepts() {
        let shell = proto::ShellArgs {
            command: "sleep 100000".to_string(),
            working_directory: None,
            timeout: Some(86_400_000),
            tool_call_id: None,
            hard_timeout: None,
        };
        assert_eq!(shell_timeout_ms(&shell), Some(MAX_BASH_TIMEOUT_MS));

        // Nothing asked for: the fallback is the hard timeout, not a guess.
        let shell = proto::ShellArgs {
            timeout: Some(0),
            hard_timeout: Some(5_000),
            ..shell
        };
        assert_eq!(shell_timeout_ms(&shell), Some(5_000));

        let shell = proto::ShellArgs {
            timeout: None,
            hard_timeout: None,
            ..shell
        };
        assert_eq!(shell_timeout_ms(&shell), None);
    }

    #[test]
    fn read_exec_carries_offset_and_limit() {
        let mut exec = exec_message(3, "exec-read");
        exec.read_args = Some(proto::ReadArgs {
            path: "/tmp/a".to_string(),
            tool_call_id: None,
            offset: Some(40),
            limit: Some(20),
            encoding_hint: None,
        });

        match exec_action(&exec, None) {
            ExecAction::Call { name, input, .. } => {
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "/tmp/a");
                assert_eq!(input["offset"], 40);
                assert_eq!(input["limit"], 20);
            }
            _ => panic!("expected a Read call"),
        }

        // A negative offset indexes from the end, which Claude Code's Read has
        // no form for: dropping it beats reading the wrong window.
        if let Some(read) = exec.read_args.as_mut() {
            read.offset = Some(-10);
        }
        match exec_action(&exec, None) {
            ExecAction::Call { input, .. } => assert!(input.get("offset").is_none()),
            _ => panic!("expected a Read call"),
        }
    }

    #[test]
    fn a_tool_the_client_did_not_advertise_is_rejected_in_band() {
        let allowed: BTreeSet<String> = ["Read"].into_iter().map(str::to_string).collect();
        let mut exec = exec_message(4, "exec-shell");
        exec.shell_stream_args = Some(proto::ShellArgs {
            command: "rm -rf /".to_string(),
            working_directory: None,
            timeout: None,
            tool_call_id: None,
            hard_timeout: None,
        });

        match result_of(exec_action(&exec, Some(&allowed))) {
            proto::exec_client_message::Message::ShellResult(result) => match result.result {
                Some(proto::shell_result::Result::Failure(failure)) => {
                    assert_eq!(failure.exit_code, 1);
                    assert!(failure.stderr.contains("not available"));
                }
                other => panic!("expected a shell failure, got {other:?}"),
            },
            other => panic!("expected a shell result, got {other:?}"),
        }
    }

    #[test]
    fn byte_backed_write_carries_its_bytes_as_text() {
        let mut exec = exec_message(5, "exec-write");
        exec.write_args = Some(proto::WriteArgs {
            path: "/tmp/hi.txt".to_string(),
            file_text: String::new(),
            tool_call_id: None,
            return_file_content_after_write: false,
            file_bytes: Some(b"hello\nworld".to_vec()),
            encoding_hint: Some("utf8".to_string()),
        });

        match exec_action(&exec, None) {
            ExecAction::Call { name, input, kind } => {
                assert_eq!(name, "Write");
                assert_eq!(input["content"], "hello\nworld");
                assert_eq!(
                    kind,
                    PendingExecKind::Write {
                        path: "/tmp/hi.txt".to_string(),
                        content: "hello\nworld".to_string(),
                    }
                );
            }
            _ => panic!("expected a Write call"),
        }
    }

    #[test]
    fn non_utf8_write_is_refused_instead_of_writing_an_empty_file() {
        let mut exec = exec_message(6, "exec-write");
        exec.write_args = Some(proto::WriteArgs {
            path: "/tmp/blob.bin".to_string(),
            file_text: String::new(),
            tool_call_id: None,
            return_file_content_after_write: false,
            file_bytes: Some(vec![0xff, 0xfe, 0x00]),
            encoding_hint: None,
        });

        match result_of(exec_action(&exec, None)) {
            proto::exec_client_message::Message::WriteResult(result) => match result.result {
                Some(proto::write_result::Result::Error(error)) => {
                    assert_eq!(error.path, "/tmp/blob.bin");
                    assert!(error.error.contains("not UTF-8"));
                }
                other => panic!("expected a write error, got {other:?}"),
            },
            other => panic!("expected a write result, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_write_still_writes() {
        let write = proto::WriteArgs {
            path: "/tmp/empty.txt".to_string(),
            file_text: String::new(),
            tool_call_id: None,
            return_file_content_after_write: false,
            file_bytes: None,
            encoding_hint: None,
        };
        assert_eq!(write_content(&write).unwrap(), "");
    }

    #[test]
    fn request_context_args_gets_an_empty_success() {
        let mut exec = exec_message(0, "");
        exec.id = None;
        exec.exec_id = None;
        exec.request_context_args = Some(proto::RequestContextArgs {
            notes_session_id: Some("975f5819".to_string()),
        });

        match result_of(exec_action(&exec, None)) {
            proto::exec_client_message::Message::RequestContextResult(result) => {
                assert!(matches!(
                    result.result,
                    Some(proto::request_context_result::Result::Success(_))
                ));
            }
            other => panic!("expected a request context result, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // find_tool_result / advertised tools
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
        assert_eq!(
            find_tool_result(&body, "call_1")
                .unwrap()
                .get("content")
                .and_then(|c| c.as_str()),
            Some("new")
        );
    }

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
    fn can_bridge_needs_stream_session_and_a_tool() {
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        }))
        .unwrap();
        assert!(can_bridge_cursor_native_tools(&body, Some("session-1")));
        assert!(!can_bridge_cursor_native_tools(&body, None));
        assert!(!can_bridge_cursor_native_tools(&body, Some("")));

        let mut non_stream = body.clone();
        non_stream.stream = false;
        assert!(!can_bridge_cursor_native_tools(
            &non_stream,
            Some("session-1")
        ));
    }

    // -----------------------------------------------------------------------
    // Registry lifecycle
    // -----------------------------------------------------------------------

    /// A new run for a session replaces the old entry instead of hiding behind
    /// it, a finished turn removes its own entry, and a run nobody came back
    /// for expires by age.
    #[tokio::test]
    async fn bridge_registry_never_keeps_a_stale_entry() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        assert_eq!(BridgeRegistry::active_count(), 0);

        insert_bridge("session-a", Some("toolu_old"));
        assert_eq!(BridgeRegistry::active_count(), 1);
        insert_bridge("session-a", None);
        assert_eq!(BridgeRegistry::active_count(), 1);
        assert!(
            BridgeRegistry::take("session-a").unwrap().pending.is_none(),
            "a new run replaces the old entry"
        );
        assert_eq!(BridgeRegistry::active_count(), 0);

        // Taking is what removes a finished turn's run.
        insert_bridge("session-b", Some("toolu_1"));
        assert!(BridgeRegistry::take("session-b").is_some());
        assert_eq!(BridgeRegistry::active_count(), 0);

        // A run nobody returns for expires by age.
        insert_bridge("session-c", None);
        assert_eq!(BridgeRegistry::sweep(Duration::from_secs(600)), 0);
        assert_eq!(BridgeRegistry::active_count(), 1);
        assert_eq!(BridgeRegistry::sweep(Duration::from_secs(0)), 1);
        assert_eq!(BridgeRegistry::active_count(), 0);

        BridgeRegistry::clear();
    }

    /// A parked run must never block the next exec: the caller resumes only
    /// when the request carries the matching tool_result, and drops the run
    /// otherwise, so the request's own exec runs.
    #[tokio::test]
    async fn a_run_without_a_pending_exec_does_not_block_the_next_exec() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        insert_bridge("session-d", None);

        let exec = read_exec(1, "exec-1", "/tmp/a");
        let parked = BridgeRegistry::take("session-d").expect("parked run");
        assert!(parked.pending.is_none());
        assert!(
            matches!(exec_action(&exec, None), ExecAction::Call { .. }),
            "the request's own exec still reaches Claude Code"
        );
        assert_eq!(BridgeRegistry::active_count(), 0);

        BridgeRegistry::clear();
    }

    /// Insert a run backed by an in-memory channel: enough for the registry,
    /// with no HTTP stream to leak in a test.
    fn insert_bridge(session_id: &str, pending: Option<&str>) {
        let (_events_tx, events) = tokio::sync::mpsc::channel(1);
        let run = LiveRun::for_test(events);
        let mut bridge = LiveBridge::new(session_id.to_string(), run, None);
        bridge.pending = pending.map(|tool_use_id| PendingExec {
            tool_use_id: tool_use_id.to_string(),
            id: 1,
            exec_id: "exec-1".to_string(),
            kind: PendingExecKind::Read {
                path: "/tmp/a".to_string(),
            },
        });
        BridgeRegistry::insert(bridge);
    }

    #[test]
    fn read_result_gives_cursor_the_file_without_line_numbers() {
        assert_eq!(raw_file_text("1\talpha\n2\t"), "alpha\n");
        assert_eq!(
            raw_file_text("     1\tfn a() {}\n     2\t\tx\n"),
            "fn a() {}\n\tx"
        );
        assert_eq!(
            raw_file_text("1\tline\n\n<system-reminder>\nnote\n</system-reminder>"),
            "line"
        );
        assert_eq!(
            raw_file_text(
                "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
            ),
            ""
        );
        assert_eq!(
            raw_file_text("plain text, not numbered"),
            "plain text, not numbered"
        );
    }
}
