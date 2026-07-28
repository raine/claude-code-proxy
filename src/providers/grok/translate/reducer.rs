use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::stream::SseDecoder;
use crate::providers::translate_shared::{JsonObjectError, parse_json_object};

use super::tool_policy::ToolCallPolicy;

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_INCOMPLETE_TOOL_CALLS: usize = 128;
const MAX_TOTAL_TOOL_ITEMS: usize = 256;
const MAX_SEQUENCE_HISTORY: usize = 1024;
const MAX_INCOMPLETE_REASON_BYTES: usize = 256;
const MAX_UPSTREAM_FAILURE_MESSAGE_BYTES: usize = 2 * 1024;

/// Return whether an event is known to be non-semantic telemetry after a response terminal.
///
/// Usage belongs to the terminal response payload, so it is intentionally absent. Extend this
/// exact allowlist only after a real upstream capture proves another event safe to discard.
fn is_ignorable_post_terminal_event(event_type: &str) -> bool {
    matches!(event_type, "rate_limits.updated")
}

#[derive(Clone, Default)]
enum SequenceState {
    #[default]
    Undecided,
    Legacy,
    Sequenced {
        last: u64,
        digests: HashMap<u64, [u8; 32]>,
        order: VecDeque<u64>,
    },
}

impl SequenceState {
    fn accept(&mut self, value: &Value) -> anyhow::Result<bool> {
        let sequence = value.get("sequence_number");
        match (&mut *self, sequence) {
            (Self::Undecided, None) => {
                *self = Self::Legacy;
                Ok(true)
            }
            (Self::Legacy, None) => Ok(true),
            (Self::Legacy, Some(_)) => {
                anyhow::bail!("Grok stream started sequence numbering after legacy events")
            }
            (Self::Sequenced { .. }, None) => {
                anyhow::bail!("Grok sequenced stream event omitted sequence_number")
            }
            (Self::Undecided, Some(sequence)) => {
                let sequence = parse_sequence(sequence)?;
                let digest = event_digest(value)?;
                *self = Self::Sequenced {
                    last: sequence,
                    digests: HashMap::from([(sequence, digest)]),
                    order: VecDeque::from([sequence]),
                };
                Ok(true)
            }
            (
                Self::Sequenced {
                    last,
                    digests,
                    order,
                },
                Some(sequence),
            ) => {
                let sequence = parse_sequence(sequence)?;
                let next_digest = event_digest(value)?;
                if let Some(known_digest) = digests.get(&sequence) {
                    if next_digest == *known_digest {
                        return Ok(false);
                    }
                    anyhow::bail!("Grok stream repeated sequence_number with different payload")
                }
                if sequence < *last {
                    anyhow::bail!("Grok stream sequence_number moved backwards")
                }
                if sequence != last.saturating_add(1) {
                    anyhow::bail!("Grok stream sequence_number has a gap")
                }
                *last = sequence;
                digests.insert(sequence, next_digest);
                order.push_back(sequence);
                while order.len() > MAX_SEQUENCE_HISTORY {
                    if let Some(evicted) = order.pop_front() {
                        digests.remove(&evicted);
                    }
                }
                Ok(true)
            }
        }
    }
}

fn parse_sequence(value: &Value) -> anyhow::Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("Grok sequence_number is not an unsigned integer"))
}

fn event_digest(value: &Value) -> anyhow::Result<[u8; 32]> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| anyhow::anyhow!("failed to canonicalize Grok event: {error}"))?;
    Ok(Sha256::digest(encoded).into())
}

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    ThinkingStart(usize),
    ThinkingDelta(usize, String),
    ThinkingStop(usize),
    TextStart(usize),
    TextDelta(usize, String),
    TextStop(usize),
    ToolStart(usize, String, String),
    ToolDelta(usize, String),
    ToolStop(usize),
    HostedSearch {
        index: usize,
        result_index: usize,
        id: String,
        name: String,
        query: String,
    },
    Citation(usize, Value),
    Terminal(GrokTerminal),
    Usage(GrokUsage),
    Finish {
        stop_reason: String,
        input_tokens: u64,
        output_tokens: u64,
        web_search_requests: u64,
        x_search_requests: u64,
    },
}

impl ReducerEvent {
    pub fn is_semantic(&self) -> bool {
        !matches!(
            self,
            Self::ThinkingStart(_)
                | Self::ThinkingDelta(_, _)
                | Self::ThinkingStop(_)
                | Self::Terminal(_)
                | Self::Usage(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokTerminal {
    Completed,
    IncompleteMaxOutputTokens,
    IncompleteContentFilter,
    IncompleteMissingReason,
    IncompleteOther,
}

impl GrokTerminal {
    pub fn outcome(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::IncompleteMaxOutputTokens
            | Self::IncompleteContentFilter
            | Self::IncompleteMissingReason
            | Self::IncompleteOther => "incomplete",
        }
    }

    pub fn incomplete_reason(self) -> Option<&'static str> {
        match self {
            Self::Completed => None,
            Self::IncompleteMaxOutputTokens => Some("max_output_tokens"),
            Self::IncompleteContentFilter => Some("content_filter"),
            Self::IncompleteMissingReason => Some("missing"),
            Self::IncompleteOther => Some("other"),
        }
    }

    fn stop_reason(self) -> &'static str {
        match self {
            Self::Completed => "end_turn",
            Self::IncompleteMaxOutputTokens => "max_tokens",
            Self::IncompleteContentFilter
            | Self::IncompleteMissingReason
            | Self::IncompleteOther => "refusal",
        }
    }
}

/// Token counters reported by the upstream Responses API.
///
/// Every field remains optional so an omitted counter is distinguishable from a real zero. The
/// upstream input counter includes cached tokens; [`GrokUsage::mapped_input_tokens`] splits them
/// out for the Anthropic usage shape when the cached-token detail is internally consistent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrokUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl GrokUsage {
    pub fn availability_state(&self) -> &'static str {
        let any = self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_input_tokens.is_some()
            || self.reasoning_tokens.is_some()
            || self.total_tokens.is_some();
        if !any {
            "unavailable"
        } else if self.input_tokens.is_some() && self.output_tokens.is_some() {
            "reported"
        } else {
            "partial"
        }
    }

    pub fn mapped_input_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.cached_input_tokens) {
            (Some(total), Some(cached)) if cached <= total => Some(total - cached),
            (Some(total), _) => Some(total),
            (None, _) => None,
        }
    }

    pub fn mapped_cache_read_input_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.cached_input_tokens) {
            (Some(total), Some(cached)) if cached <= total => Some(cached),
            _ => None,
        }
    }

    pub fn cache_breakdown_is_inconsistent(&self) -> bool {
        matches!(
            (self.input_tokens, self.cached_input_tokens),
            (Some(total), Some(cached)) if cached > total
        )
    }
}

/// A non-streaming reduction failure together with any usage reported by the failing response.
///
/// `response.failed` is still a hard protocol error. Keeping its usage beside the error lets the
/// provider update diagnostics without turning the failed response into a successful message.
#[derive(Debug)]
pub struct GrokReductionError {
    source: anyhow::Error,
    usage: Option<GrokUsage>,
}

impl GrokReductionError {
    fn new(source: anyhow::Error, usage: Option<GrokUsage>) -> Self {
        Self { source, usage }
    }

    pub(crate) fn malformed_event() -> Self {
        Self::new(anyhow::anyhow!("malformed Grok SSE event"), None)
    }

    pub fn usage(&self) -> Option<&GrokUsage> {
        self.usage.as_ref()
    }

    pub fn upstream_failure(&self) -> Option<&GrokUpstreamFailure> {
        self.source.downcast_ref()
    }
}

impl std::fmt::Display for GrokReductionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for GrokReductionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// A structured failure delivered inside an otherwise successful Grok SSE response.
///
/// The Responses API can report model, capacity, or request failures after the HTTP 200 headers.
/// Keep the transport semantics intact so a caller can retry a known transient failure before it
/// commits output, or render the matching Anthropic in-band error after output has started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokUpstreamFailure {
    pub event_type: String,
    pub status: u16,
    pub retry_after: Option<String>,
    pub message: String,
    pub retryable: bool,
}

impl std::fmt::Display for GrokUpstreamFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.event_type, self.message)
    }
}

impl std::error::Error for GrokUpstreamFailure {}

#[derive(Clone, Default)]
pub struct Reducer {
    tool_policy: ToolCallPolicy,
    sequence: SequenceState,
    next_index: usize,
    active: Option<(String, usize)>,
    active_text: String,
    /// Text closed by a tool boundary before `response.output_text.done`, kept for one exact
    /// late-snapshot reconciliation.
    pending_tool_boundary_text: Option<String>,
    saw_text_output: bool,
    calls: HashMap<String, ActiveFunctionCall>,
    item_calls: HashMap<String, String>,
    completed_calls: HashMap<String, CompletedFunctionCall>,
    tool_args: HashMap<String, String>,
    completed_arguments: HashMap<String, bool>,
    hosted_calls: HashMap<String, ActiveHostedCall>,
    completed_hosted_inputs: HashSet<String>,
    completed_hosted_calls: HashMap<String, CompletedHostedCall>,
    tool_item_ids: HashSet<String>,
    downstream_tool_use_ids: HashSet<String>,
    total_tool_items: usize,
    pending_citations: Vec<Value>,
    web_search_requests: u64,
    x_search_requests: u64,
    saw_tool: bool,
    completed: bool,
    incomplete_reason: Option<String>,
    usage: Option<GrokUsage>,
}

#[derive(Clone)]
struct ActiveFunctionCall {
    index: usize,
    item_id: Option<String>,
    name: String,
}

#[derive(Clone)]
struct CompletedFunctionCall {
    item_id: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Clone)]
struct ActiveHostedCall {
    item_kind: HostedItemKind,
    search_kind: HostedSearchKind,
    phase: HostedPhase,
    input: String,
}

#[derive(Clone)]
struct CompletedHostedCall {
    item_kind: HostedItemKind,
    search_kind: HostedSearchKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostedItemKind {
    Web,
    X,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostedSearchKind {
    Web,
    X,
}

impl HostedSearchKind {
    fn name(self) -> &'static str {
        match self {
            Self::Web => "web_search",
            Self::X => "x_search",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostedPhase {
    Added,
    InProgress,
    Searching,
    Completed,
}

impl Reducer {
    pub(crate) fn with_tool_policy(tool_policy: ToolCallPolicy) -> Self {
        Self {
            tool_policy,
            ..Self::default()
        }
    }

    pub fn push(&mut self, value: Value) -> anyhow::Result<Vec<ReducerEvent>> {
        let typ = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("event lacks type"))?;
        // Rate-limit telemetry is transport metadata rather than part of the sequenced response
        // event stream. Once the response is terminal, ignore it before sequence validation so a
        // numbered response can safely be followed by unnumbered or repeated telemetry.
        if self.completed && is_ignorable_post_terminal_event(typ) {
            return Ok(vec![]);
        }
        if !self.sequence.accept(&value)? {
            return Ok(vec![]);
        }
        if self.completed {
            // Text, tools, and unknown future events fail closed until a real capture proves that
            // they are safe to ignore.
            anyhow::bail!("event after terminal completion: {typ}");
        }
        match typ {
            "response.created" | "response.in_progress" => Ok(vec![]),
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.content_part.added" => Ok(vec![]),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => self
                .delta(
                    "thinking",
                    value
                        .get("delta")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("reasoning delta is invalid"))?,
                ),
            "response.custom_tool_call_input.delta" => {
                let id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("custom tool delta lacks item id"))?;
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("custom tool delta is invalid"))?;
                if self.completed_hosted_inputs.contains(id) {
                    anyhow::bail!("custom tool delta arrived after input completion");
                }
                let call = self
                    .hosted_calls
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("custom tool delta is out of order"))?;
                if call.item_kind != HostedItemKind::Custom {
                    anyhow::bail!("custom tool delta disagrees with output_item.added");
                }
                if call.input.len().saturating_add(delta.len()) > MAX_TOOL_ARGUMENT_BYTES {
                    anyhow::bail!("custom tool input exceeds the size limit");
                }
                call.input.push_str(delta);
                Ok(vec![])
            }
            "response.custom_tool_call_input.done" => {
                let id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("custom tool completion lacks item id"))?;
                let call = self
                    .hosted_calls
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("custom tool completion is out of order"))?;
                if call.item_kind != HostedItemKind::Custom {
                    anyhow::bail!("custom tool completion disagrees with output_item.added");
                }
                if !self.completed_hosted_inputs.insert(id.into()) {
                    anyhow::bail!("duplicate custom tool input completion");
                }
                Ok(vec![])
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed"
            | "response.x_search_call.in_progress"
            | "response.x_search_call.searching"
            | "response.x_search_call.completed" => self.advance_hosted_search(typ, &value),
            "response.output_text.annotation.added" => {
                let Some(annotation) = value.get("annotation") else {
                    return Ok(vec![]);
                };
                if annotation.get("type").and_then(Value::as_str) != Some("url_citation")
                    || annotation
                        .get("url")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    Ok(vec![])
                } else if let Some((kind, index)) = self.active.as_ref()
                    && kind == "text"
                {
                    Ok(vec![ReducerEvent::Citation(*index, annotation.clone())])
                } else {
                    self.pending_citations.push(annotation.clone());
                    Ok(vec![])
                }
            }
            "response.output_text.delta" => self.delta(
                "text",
                value
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("text delta is invalid"))?,
            ),
            "response.output_item.added" => {
                let item = value
                    .get("item")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("output item is invalid"))?;
                let item_type = optional_non_empty_identity(item.get("type"), "output item type")?
                    .ok_or_else(|| anyhow::anyhow!("output item lacks type"))?;
                match item_type {
                    "custom_tool_call" | "web_search_call" | "x_search_call" => {
                        let id =
                            optional_non_empty_identity(item.get("id"), "hosted tool call id")?
                                .ok_or_else(|| anyhow::anyhow!("hosted tool call lacks id"))?;
                        let downstream_id = hosted_downstream_tool_use_id(id);
                        if self.downstream_tool_use_ids.contains(&downstream_id) {
                            anyhow::bail!("duplicate downstream Anthropic tool-use id");
                        }
                        if self.hosted_calls.contains_key(id)
                            || self.completed_hosted_calls.contains_key(id)
                        {
                            anyhow::bail!("duplicate hosted tool call id");
                        }
                        if self.tool_item_ids.contains(id) {
                            anyhow::bail!("duplicate output tool item id");
                        }
                        if self.total_tool_items >= MAX_TOTAL_TOOL_ITEMS {
                            anyhow::bail!("too many tool items in one Grok response");
                        }
                        if self.hosted_calls.len() >= MAX_INCOMPLETE_TOOL_CALLS {
                            anyhow::bail!("too many incomplete hosted tool calls");
                        }
                        let (item_kind, search_kind) = hosted_search_identity(item, item_type)?;
                        let prior_kind_calls = self
                            .hosted_calls
                            .values()
                            .filter(|call| call.search_kind == search_kind)
                            .count()
                            + self
                                .completed_hosted_calls
                                .values()
                                .filter(|call| call.search_kind == search_kind)
                                .count();
                        self.tool_policy.validate_hosted_call(
                            search_kind.name(),
                            self.total_tool_items,
                            prior_kind_calls,
                        )?;
                        self.hosted_calls.insert(
                            id.into(),
                            ActiveHostedCall {
                                item_kind,
                                search_kind,
                                phase: HostedPhase::Added,
                                input: String::new(),
                            },
                        );
                        self.downstream_tool_use_ids.insert(downstream_id);
                        self.tool_item_ids.insert(id.into());
                        self.total_tool_items += 1;
                        Ok(vec![])
                    }
                    "function_call" => {
                        let id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("function call id is invalid"))?;
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("function call name is invalid"))?;
                        if self.downstream_tool_use_ids.contains(id) {
                            anyhow::bail!("duplicate downstream Anthropic tool-use id");
                        }
                        self.tool_policy
                            .validate_function_call(name, self.total_tool_items)?;
                        if self.calls.contains_key(id) || self.completed_calls.contains_key(id) {
                            anyhow::bail!("duplicate function call id");
                        }
                        if self.total_tool_items >= MAX_TOTAL_TOOL_ITEMS {
                            anyhow::bail!("too many tool items in one Grok response");
                        }
                        if self.calls.len() >= MAX_INCOMPLETE_TOOL_CALLS {
                            anyhow::bail!("too many incomplete function calls");
                        }
                        let item_id =
                            optional_non_empty_identity(item.get("id"), "function call item id")?
                                .map(str::to_owned);
                        if item_id
                            .as_ref()
                            .is_some_and(|item_id| self.tool_item_ids.contains(item_id))
                        {
                            anyhow::bail!("duplicate output tool item id");
                        }
                        let mut out = self.close_active_for_tool_boundary()?;
                        let index = self.next_index;
                        self.next_index += 1;
                        self.downstream_tool_use_ids.insert(id.into());
                        self.calls.insert(
                            id.into(),
                            ActiveFunctionCall {
                                index,
                                item_id: item_id.clone(),
                                name: name.into(),
                            },
                        );
                        if let Some(item_id) = item_id {
                            self.item_calls.insert(item_id.clone(), id.into());
                            self.tool_item_ids.insert(item_id);
                        }
                        self.total_tool_items += 1;
                        self.tool_args.insert(id.into(), String::new());
                        self.completed_arguments.insert(id.into(), false);
                        self.saw_tool = true;
                        out.push(ReducerEvent::ToolStart(index, id.into(), name.into()));
                        Ok(out)
                    }
                    "message" | "reasoning" => Ok(vec![]),
                    _ => anyhow::bail!("unsupported Grok output item type: {item_type}"),
                }
            }
            "response.function_call_arguments.delta" => {
                let id = self.resolve_function_event_call_id(&value, "function delta")?;
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("function delta is invalid"))?;
                let index = self
                    .calls
                    .get(&id)
                    .map(|call| call.index)
                    .ok_or_else(|| anyhow::anyhow!("function delta is out of order"))?;
                if self.completed_arguments.get(&id) == Some(&true) {
                    anyhow::bail!("function delta arrived after arguments completion");
                }
                let args = self
                    .tool_args
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("function delta is out of order"))?;
                if args.len().saturating_add(delta.len()) > MAX_TOOL_ARGUMENT_BYTES {
                    anyhow::bail!("function arguments exceed the size limit");
                }
                args.push_str(delta);
                Ok(vec![ReducerEvent::ToolDelta(index, delta.into())])
            }
            "response.function_call_arguments.done" => {
                let id = self.resolve_function_event_call_id(&value, "function completion")?;
                let args =
                    optional_string_field(value.get("arguments"), "function completion arguments")?;
                if self.completed_arguments.get(&id) == Some(&true) {
                    anyhow::bail!("duplicate function arguments completion");
                }
                if args.is_some_and(|args| args.len() > MAX_TOOL_ARGUMENT_BYTES) {
                    anyhow::bail!("function arguments exceed the size limit");
                }
                let accumulated = self
                    .tool_args
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("function completion is out of order"))?;
                let index = self
                    .calls
                    .get(&id)
                    .map(|call| call.index)
                    .ok_or_else(|| anyhow::anyhow!("function completion is out of order"))?;
                let output = match args {
                    Some(args) if accumulated.is_empty() && !args.is_empty() => {
                        self.tool_args.get_mut(&id).unwrap().push_str(args);
                        vec![ReducerEvent::ToolDelta(index, args.into())]
                    }
                    Some(args) if args != accumulated => {
                        anyhow::bail!("function completion arguments disagree with deltas")
                    }
                    _ => vec![],
                };
                self.completed_arguments.insert(id, true);
                Ok(output)
            }
            "response.output_text.done" => {
                let mut out = value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| self.complete_text_snapshot(text))
                    .transpose()?
                    .unwrap_or_default();
                out.extend(self.close_kind("text")?);
                // A done event consumes any snapshot retained when a tool boundary closed the
                // text early. Do not let a duplicate done event reopen the text.
                self.pending_tool_boundary_text = None;
                Ok(out)
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                self.close_kind("thinking")
            }
            "response.content_part.done" => Ok(vec![]),
            "response.output_item.done" => {
                let item = value
                    .get("item")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("completed output item is invalid"))?;
                let item_type =
                    optional_non_empty_identity(item.get("type"), "completed output item type")?
                        .ok_or_else(|| anyhow::anyhow!("completed output item lacks type"))?;
                match item_type {
                    kind @ ("web_search_call" | "x_search_call" | "custom_tool_call") => {
                        self.complete_hosted_search(item, kind)
                    }
                    "function_call" => {
                        let id = optional_non_empty_identity(
                            item.get("call_id"),
                            "completed function call id",
                        )?
                        .ok_or_else(|| anyhow::anyhow!("completed function call lacks call id"))?;
                        let completed =
                            self.calls.get(id).cloned().ok_or_else(|| {
                                anyhow::anyhow!("completed function call is unknown")
                            })?;
                        if let Some(actual_item_id) = optional_non_empty_identity(
                            item.get("id"),
                            "completed function call item id",
                        )? && completed.item_id.as_deref() != Some(actual_item_id)
                        {
                            anyhow::bail!(
                                "completed function call item id disagrees with output_item.added"
                            );
                        }
                        if let Some(actual_name) = optional_non_empty_identity(
                            item.get("name"),
                            "completed function call name",
                        )? && actual_name != completed.name
                        {
                            anyhow::bail!(
                                "completed function call name disagrees with output_item.added"
                            );
                        }
                        let index = completed.index;
                        let mut args = self.tool_args.get(id).cloned().unwrap_or_default();
                        let mut out = Vec::new();
                        if let Some(snapshot) = optional_string_field(
                            item.get("arguments"),
                            "completed function call arguments",
                        )? {
                            if snapshot.len() > MAX_TOOL_ARGUMENT_BYTES {
                                anyhow::bail!("function arguments exceed the size limit");
                            }
                            if args.is_empty() && !snapshot.is_empty() {
                                args.push_str(snapshot);
                                out.push(ReducerEvent::ToolDelta(index, snapshot.into()));
                            } else if snapshot != args {
                                anyhow::bail!("completed function arguments disagree with deltas");
                            }
                        }
                        parse_function_arguments(&args)?;
                        let completed = self.calls.remove(id).expect("validated function call");
                        if let Some(item_id) = completed.item_id.as_ref() {
                            self.item_calls.remove(item_id);
                        }
                        self.tool_args.remove(id);
                        self.completed_arguments.remove(id);
                        self.completed_calls.insert(
                            id.into(),
                            CompletedFunctionCall {
                                item_id: completed.item_id,
                                name: completed.name,
                                arguments: args,
                            },
                        );
                        out.push(ReducerEvent::ToolStop(index));
                        Ok(out)
                    }
                    "message" | "reasoning" => Ok(vec![]),
                    _ => anyhow::bail!("unsupported completed Grok output item type: {item_type}"),
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = value
                    .get("response")
                    .filter(|response| response.is_object())
                    .ok_or_else(|| anyhow::anyhow!("terminal event response is not an object"))?;
                let expected_status = if typ == "response.completed" {
                    "completed"
                } else {
                    "incomplete"
                };
                if let Some(status) = response.get("status") {
                    let status = status.as_str().ok_or_else(|| {
                        anyhow::anyhow!("terminal response status must be a string")
                    })?;
                    if status != expected_status {
                        anyhow::bail!(
                            "terminal response status disagrees with the terminal event type"
                        );
                    }
                }
                let usage = grok_usage(response);
                self.usage = Some(usage.clone());
                if !self.calls.is_empty() {
                    anyhow::bail!("function call is incomplete");
                }
                if !self.hosted_calls.is_empty() {
                    anyhow::bail!("hosted tool call is incomplete");
                }
                let snapshot_text = self.validate_terminal_snapshot(response)?;
                if typ == "response.completed" {
                    let completed_visible_hosted_calls = self
                        .completed_hosted_calls
                        .values()
                        .filter(|call| call.search_kind == HostedSearchKind::Web)
                        .count();
                    let completed_hidden_x_calls = self
                        .completed_hosted_calls
                        .values()
                        .filter(|call| call.search_kind == HostedSearchKind::X)
                        .count();
                    let has_observable_output = self.saw_text_output
                        || !snapshot_text.is_empty()
                        || !self.completed_calls.is_empty()
                        || completed_visible_hosted_calls > 0;
                    if completed_hidden_x_calls > 0 && !has_observable_output {
                        anyhow::bail!(
                            "Grok completed with only an unobservable hosted x_search call"
                        );
                    }
                    self.tool_policy.validate_success_terminal(
                        self.completed_calls.len(),
                        completed_visible_hosted_calls,
                    )?;
                }
                let fallback_text = if self.saw_text_output {
                    Vec::new()
                } else {
                    snapshot_text
                };
                let mut out = self.close_active()?;
                self.pending_tool_boundary_text = None;
                for text in fallback_text {
                    out.extend(self.delta("text", &text)?);
                    out.extend(self.close_kind("text")?);
                }
                // Keep the legacy Finish field as the upstream total for the non-streaming
                // accumulator. The streaming renderer uses GrokUsage to split cached input into
                // Anthropic's input/cache-read counters without double counting.
                let input = usage.input_tokens.unwrap_or(0);
                let output = usage.output_tokens.unwrap_or(0);
                let incomplete_reason = response
                    .get("incomplete_details")
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str);
                if typ == "response.completed"
                    && response
                        .get("incomplete_details")
                        .is_some_and(|details| !details.is_null())
                {
                    anyhow::bail!(
                        "completed terminal response unexpectedly contains incomplete_details"
                    );
                }
                let response_is_incomplete = typ == "response.incomplete";
                let terminal = if response_is_incomplete {
                    match incomplete_reason {
                        Some("content_filter") => GrokTerminal::IncompleteContentFilter,
                        Some("max_output_tokens" | "max_tokens" | "length") => {
                            GrokTerminal::IncompleteMaxOutputTokens
                        }
                        None => GrokTerminal::IncompleteMissingReason,
                        Some(_) => GrokTerminal::IncompleteOther,
                    }
                } else {
                    GrokTerminal::Completed
                };
                self.incomplete_reason = response_is_incomplete
                    .then(|| incomplete_reason.map(bounded_incomplete_reason))
                    .flatten();
                let stop = if terminal == GrokTerminal::Completed && self.saw_tool {
                    "tool_use"
                } else {
                    terminal.stop_reason()
                };
                self.completed = true;
                out.push(ReducerEvent::Terminal(terminal));
                out.push(ReducerEvent::Usage(usage));
                out.push(ReducerEvent::Finish {
                    stop_reason: stop.into(),
                    input_tokens: input,
                    output_tokens: output,
                    web_search_requests: self.web_search_requests,
                    x_search_requests: self.x_search_requests,
                });
                Ok(out)
            }
            "error" | "response.failed" | "response.error" => {
                let response = value.get("response").unwrap_or(&value);
                self.usage = Some(grok_usage(response));
                Err(anyhow::Error::new(parse_upstream_failure(&value, typ)))
            }
            _ => anyhow::bail!("unsupported Grok stream event: {typ}"),
        }
    }

    fn resolve_function_event_call_id(&self, value: &Value, event: &str) -> anyhow::Result<String> {
        let call_id = optional_event_identity(value.get("call_id"), event, "call_id")?;
        let item_id = optional_event_identity(value.get("item_id"), event, "item_id")?;
        if call_id.is_none() && item_id.is_none() {
            anyhow::bail!("{event} lacks call_id and item_id");
        }

        let resolved_item_call = item_id
            .map(|item_id| {
                self.item_calls
                    .get(item_id)
                    .map(String::as_str)
                    .ok_or_else(|| anyhow::anyhow!("{event} references an unknown item_id"))
            })
            .transpose()?;
        if let Some(call_id) = call_id
            && !self.calls.contains_key(call_id)
        {
            anyhow::bail!("{event} references an unknown call_id");
        }
        if let (Some(call_id), Some(item_call_id)) = (call_id, resolved_item_call)
            && call_id != item_call_id
        {
            anyhow::bail!("{event} call_id and item_id identify different function calls");
        }

        Ok(call_id
            .or(resolved_item_call)
            .expect("one identity is present")
            .into())
    }

    fn validate_terminal_snapshot(&self, response: &Value) -> anyhow::Result<Vec<String>> {
        let Some(output) = response.get("output") else {
            return Ok(Vec::new());
        };
        let output = output
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("terminal response output is not an array"))?;
        let mut text = Vec::new();
        let mut function_ids = HashSet::new();
        let mut hosted_ids = HashSet::new();

        for item in output {
            let item = item
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("terminal response output item is not an object"))?;
            let item_type = optional_non_empty_identity(
                item.get("type"),
                "terminal response output item type",
            )?
            .ok_or_else(|| anyhow::anyhow!("terminal response output item lacks type"))?;
            match item_type {
                "message" => {
                    if let Some(content) = item.get("content") {
                        let content = content.as_array().ok_or_else(|| {
                            anyhow::anyhow!("terminal response message content is not an array")
                        })?;
                        for part in content {
                            if part.get("type").and_then(Value::as_str) == Some("output_text")
                                && let Some(value) = part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .filter(|value| !value.is_empty())
                            {
                                text.push(value.to_owned());
                            }
                        }
                    }
                }
                "reasoning" => {}
                "function_call" => {
                    let call_id = optional_non_empty_identity(
                        item.get("call_id"),
                        "terminal function call id",
                    )?
                    .ok_or_else(|| anyhow::anyhow!("terminal function call lacks call id"))?;
                    if !function_ids.insert(call_id.to_owned()) {
                        anyhow::bail!("terminal response repeats a function call");
                    }
                    let completed = self.completed_calls.get(call_id).ok_or_else(|| {
                        anyhow::anyhow!("terminal response contains an unobserved function call")
                    })?;
                    let item_id = optional_non_empty_identity(
                        item.get("id"),
                        "terminal function call item id",
                    )?;
                    if item_id != completed.item_id.as_deref() {
                        anyhow::bail!(
                            "terminal function call item id disagrees with its lifecycle"
                        );
                    }
                    let name = optional_non_empty_identity(
                        item.get("name"),
                        "terminal function call name",
                    )?
                    .ok_or_else(|| anyhow::anyhow!("terminal function call lacks name"))?;
                    if name != completed.name {
                        anyhow::bail!("terminal function call name disagrees with its lifecycle");
                    }
                    let arguments = optional_string_field(
                        item.get("arguments"),
                        "terminal function call arguments",
                    )?
                    .ok_or_else(|| anyhow::anyhow!("terminal function call lacks arguments"))?;
                    if arguments != completed.arguments {
                        anyhow::bail!(
                            "terminal function call arguments disagree with its lifecycle"
                        );
                    }
                }
                kind @ ("web_search_call" | "x_search_call" | "custom_tool_call") => {
                    let id = optional_non_empty_identity(
                        item.get("id"),
                        "terminal hosted tool call id",
                    )?
                    .ok_or_else(|| anyhow::anyhow!("terminal hosted tool call lacks id"))?;
                    if !hosted_ids.insert(id.to_owned()) {
                        anyhow::bail!("terminal response repeats a hosted tool call");
                    }
                    let completed = self.completed_hosted_calls.get(id).ok_or_else(|| {
                        anyhow::anyhow!("terminal response contains an unobserved hosted tool call")
                    })?;
                    let (item_kind, search_kind) = hosted_search_identity(item, kind)?;
                    if item_kind != completed.item_kind || search_kind != completed.search_kind {
                        anyhow::bail!("terminal hosted tool call disagrees with its lifecycle");
                    }
                }
                _ => anyhow::bail!("unsupported Grok terminal output item type: {item_type}"),
            }
        }

        if function_ids.len() != self.completed_calls.len() {
            anyhow::bail!("terminal response omits a completed function call");
        }
        if hosted_ids.len() != self.completed_hosted_calls.len() {
            anyhow::bail!("terminal response omits a completed hosted tool call");
        }
        Ok(text)
    }

    fn advance_hosted_search(
        &mut self,
        event_type: &str,
        value: &Value,
    ) -> anyhow::Result<Vec<ReducerEvent>> {
        let (item_kind, search_kind, expected_phase, next_phase) = match event_type {
            "response.web_search_call.in_progress" => (
                HostedItemKind::Web,
                HostedSearchKind::Web,
                HostedPhase::Added,
                HostedPhase::InProgress,
            ),
            "response.web_search_call.searching" => (
                HostedItemKind::Web,
                HostedSearchKind::Web,
                HostedPhase::InProgress,
                HostedPhase::Searching,
            ),
            "response.web_search_call.completed" => (
                HostedItemKind::Web,
                HostedSearchKind::Web,
                HostedPhase::Searching,
                HostedPhase::Completed,
            ),
            "response.x_search_call.in_progress" => (
                HostedItemKind::X,
                HostedSearchKind::X,
                HostedPhase::Added,
                HostedPhase::InProgress,
            ),
            "response.x_search_call.searching" => (
                HostedItemKind::X,
                HostedSearchKind::X,
                HostedPhase::InProgress,
                HostedPhase::Searching,
            ),
            "response.x_search_call.completed" => (
                HostedItemKind::X,
                HostedSearchKind::X,
                HostedPhase::Searching,
                HostedPhase::Completed,
            ),
            _ => anyhow::bail!("unsupported hosted search lifecycle event: {event_type}"),
        };
        let id = optional_event_identity(value.get("item_id"), event_type, "item_id")?
            .ok_or_else(|| anyhow::anyhow!("{event_type} lacks item_id"))?;
        if self.completed_hosted_calls.contains_key(id) {
            anyhow::bail!("hosted search lifecycle event arrived after output_item.done");
        }
        let call = self
            .hosted_calls
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("hosted search lifecycle event is out of order"))?;
        if call.item_kind != item_kind || call.search_kind != search_kind {
            anyhow::bail!("hosted search lifecycle event disagrees with output_item.added");
        }
        if call.phase != expected_phase {
            anyhow::bail!("hosted search lifecycle event is out of order");
        }
        call.phase = next_phase;
        Ok(vec![])
    }

    fn complete_hosted_search(
        &mut self,
        item: &serde_json::Map<String, Value>,
        item_type: &str,
    ) -> anyhow::Result<Vec<ReducerEvent>> {
        let id = optional_non_empty_identity(item.get("id"), "completed hosted search id")?
            .ok_or_else(|| anyhow::anyhow!("completed hosted search lacks id"))?;
        let (item_kind, search_kind) = hosted_search_identity(item, item_type)?;
        if self.completed_hosted_calls.contains_key(id) {
            anyhow::bail!("duplicate hosted search completion");
        }
        let active = self
            .hosted_calls
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("completed hosted search is unknown"))?;
        if active.item_kind != item_kind || active.search_kind != search_kind {
            anyhow::bail!("completed hosted search disagrees with output_item.added");
        }
        if active.item_kind != HostedItemKind::Custom && active.phase != HostedPhase::Completed {
            anyhow::bail!("completed hosted search is out of order");
        }
        let query = hosted_search_query(item, &active.input);
        let mut out = self.close_active_for_tool_boundary()?;
        self.hosted_calls
            .remove(id)
            .expect("validated hosted search call");
        self.completed_hosted_inputs.remove(id);
        self.completed_hosted_calls.insert(
            id.into(),
            CompletedHostedCall {
                item_kind,
                search_kind,
            },
        );
        // Anthropic's response ContentBlock union has no x_search server-tool variant. Keep the
        // model's subsequent text and citation events, but do not synthesize an invalid hosted
        // block or charge it as an Anthropic web-search request.
        if search_kind == HostedSearchKind::X {
            return Ok(out);
        }
        let index = self.next_index;
        let result_index = index + 1;
        self.next_index += 2;
        self.web_search_requests += 1;
        out.push(ReducerEvent::HostedSearch {
            index,
            result_index,
            id: hosted_downstream_tool_use_id(id),
            name: search_kind.name().into(),
            query,
        });
        Ok(out)
    }

    /// Apply every event decoded from one upstream byte chunk atomically.
    ///
    /// A terminal event followed by an invalid semantic tail must not commit the terminal or any
    /// preceding text/tool state from that same chunk. Callers can render the returned events only
    /// after this method succeeds.
    pub fn push_batch(
        &mut self,
        values: impl IntoIterator<Item = Value>,
    ) -> anyhow::Result<Vec<ReducerEvent>> {
        let mut staged = self.clone();
        let mut out = Vec::new();
        for value in values {
            let failed_usage = failed_event_usage(&value);
            match staged.push(value) {
                Ok(events) => out.extend(events),
                Err(error) => {
                    // Usage in an explicit upstream failure is diagnostic telemetry, not semantic
                    // output. Preserve it while rolling back text/tool/terminal state.
                    if let Some(usage) = failed_usage {
                        self.usage = Some(usage);
                    }
                    return Err(error);
                }
            }
        }
        *self = staged;
        Ok(out)
    }

    /// Apply a decoded chunk without cloning all accumulated text/tool state.
    ///
    /// Callers must terminate the stream if this returns an error. Events remain staged in the
    /// returned vector until the entire chunk succeeds, so an invalid tail cannot leak earlier
    /// deltas downstream; retaining the partial reducer state also preserves failed-event usage.
    pub fn push_batch_in_place(
        &mut self,
        values: impl IntoIterator<Item = Value>,
    ) -> anyhow::Result<Vec<ReducerEvent>> {
        let mut out = Vec::new();
        for value in values {
            out.extend(self.push(value)?);
        }
        Ok(out)
    }

    pub fn stage_batch(
        &self,
        values: impl IntoIterator<Item = Value>,
    ) -> anyhow::Result<(Self, Vec<ReducerEvent>)> {
        let mut staged = self.clone();
        let events = staged.push_batch_in_place(values)?;
        Ok((staged, events))
    }
    fn delta(&mut self, kind: &str, delta: &str) -> anyhow::Result<Vec<ReducerEvent>> {
        let mut out = Vec::new();
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| active != kind)
        {
            out.extend(self.close_active()?);
            let index = self.next_index;
            // Grok reasoning is progress, not an Anthropic content block. Reserving an index for
            // it would leave a hole once the plaintext reasoning is hidden downstream.
            if kind != "thinking" {
                self.next_index += 1;
            }
            self.active = Some((kind.into(), index));
            if kind == "text" {
                self.active_text.clear();
                self.pending_tool_boundary_text = None;
            }
            out.push(if kind == "thinking" {
                ReducerEvent::ThinkingStart(index)
            } else {
                ReducerEvent::TextStart(index)
            });
        }
        let index = self.active.as_ref().unwrap().1;
        if kind == "text" {
            self.active_text.push_str(delta);
            self.saw_text_output |= !delta.is_empty();
        }
        out.push(if kind == "thinking" {
            ReducerEvent::ThinkingDelta(index, delta.into())
        } else {
            ReducerEvent::TextDelta(index, delta.into())
        });
        if kind == "text" {
            out.extend(
                self.pending_citations
                    .drain(..)
                    .map(|citation| ReducerEvent::Citation(index, citation)),
            );
        }
        Ok(out)
    }
    fn close_active(&mut self) -> anyhow::Result<Vec<ReducerEvent>> {
        Ok(match self.active.take() {
            Some((kind, index)) if kind == "thinking" => vec![ReducerEvent::ThinkingStop(index)],
            Some((_, index)) => {
                self.active_text.clear();
                vec![ReducerEvent::TextStop(index)]
            }
            None => vec![],
        })
    }
    fn close_active_for_tool_boundary(&mut self) -> anyhow::Result<Vec<ReducerEvent>> {
        if self.active.as_ref().is_some_and(|(kind, _)| kind == "text") {
            // Grok can cross a function or hosted-tool boundary before the preceding
            // output_text.done. Keep this exact text once so that the late snapshot can be
            // reconciled after TextStop. Adjacent tool boundaries do not replace it because only
            // the first boundary still has an active text block.
            self.pending_tool_boundary_text = Some(std::mem::take(&mut self.active_text));
        }
        self.close_active()
    }
    fn complete_text_snapshot(&mut self, snapshot: &str) -> anyhow::Result<Vec<ReducerEvent>> {
        let suffix = match self.active.as_ref() {
            Some((kind, _)) if kind == "text" => {
                snapshot.strip_prefix(&self.active_text).ok_or_else(|| {
                    anyhow::anyhow!("completed text snapshot disagrees with streamed deltas")
                })?
            }
            // Grok can cross a function or hosted-tool boundary before
            // `response.output_text.done`. The text block is already closed and forwarded;
            // accept the one matching snapshot. A strict extension cannot be inserted before
            // the already-forwarded tool block, so keep that malformed ordering fail-closed.
            _ if self.pending_tool_boundary_text.as_deref() == Some(snapshot) => "",
            _ if !self.saw_text_output && self.pending_tool_boundary_text.is_none() => snapshot,
            _ => {
                return Err(anyhow::anyhow!(
                    "completed text snapshot disagrees with streamed deltas"
                ));
            }
        };
        if suffix.is_empty() {
            Ok(Vec::new())
        } else {
            self.delta("text", suffix)
        }
    }
    fn close_kind(&mut self, kind: &str) -> anyhow::Result<Vec<ReducerEvent>> {
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| active == kind)
        {
            self.close_active()
        } else {
            Ok(vec![])
        }
    }
    pub fn finished(&self) -> bool {
        self.completed
    }

    pub fn usage(&self) -> Option<&GrokUsage> {
        self.usage.as_ref()
    }

    pub fn incomplete_reason(&self) -> Option<&str> {
        self.incomplete_reason.as_deref()
    }
}

fn optional_non_empty_identity<'a>(
    value: Option<&'a Value>,
    label: &str,
) -> anyhow::Result<Option<&'a str>> {
    match value {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(_) => anyhow::bail!("{label} must be a non-empty string"),
    }
}

fn optional_event_identity<'a>(
    value: Option<&'a Value>,
    event: &str,
    field: &str,
) -> anyhow::Result<Option<&'a str>> {
    match value {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(_) => anyhow::bail!("{event} {field} must be a non-empty string"),
    }
}

fn optional_string_field<'a>(
    value: Option<&'a Value>,
    label: &str,
) -> anyhow::Result<Option<&'a str>> {
    match value {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => anyhow::bail!("{label} must be a string"),
    }
}

fn hosted_search_identity(
    item: &serde_json::Map<String, Value>,
    item_type: &str,
) -> anyhow::Result<(HostedItemKind, HostedSearchKind)> {
    let supplied_name = optional_non_empty_identity(item.get("name"), "hosted tool call name")?;
    let identity = match item_type {
        "web_search_call" => (HostedItemKind::Web, HostedSearchKind::Web),
        "x_search_call" => (HostedItemKind::X, HostedSearchKind::X),
        "custom_tool_call" => match supplied_name {
            Some("web_search") => (HostedItemKind::Custom, HostedSearchKind::Web),
            Some("x_search") => (HostedItemKind::Custom, HostedSearchKind::X),
            Some(_) => anyhow::bail!("unsupported custom hosted tool name"),
            None => anyhow::bail!("custom hosted tool call lacks name"),
        },
        _ => anyhow::bail!("unsupported hosted tool call type: {item_type}"),
    };
    if let Some(supplied_name) = supplied_name
        && supplied_name != identity.1.name()
    {
        anyhow::bail!("hosted tool call name disagrees with its type");
    }
    Ok(identity)
}

fn hosted_downstream_tool_use_id(upstream_id: &str) -> String {
    format!("srvtoolu_{upstream_id}")
}

fn hosted_search_query(item: &serde_json::Map<String, Value>, buffered_input: &str) -> String {
    item.get("action")
        .and_then(|action| action.get("query"))
        .and_then(Value::as_str)
        .or_else(|| item.get("query").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            item.get("action")
                .and_then(|action| action.get("queries"))
                .and_then(Value::as_array)
                .map(|queries| {
                    queries
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|queries| !queries.is_empty())
        })
        .or_else(|| {
            serde_json::from_str::<Value>(buffered_input)
                .ok()
                .and_then(|input| {
                    input
                        .get("query")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        })
        .unwrap_or_default()
}

fn bounded_incomplete_reason(reason: &str) -> String {
    super::super::text::truncate_utf8(reason, MAX_INCOMPLETE_REASON_BYTES)
        .0
        .to_owned()
}

fn grok_usage(response: &Value) -> GrokUsage {
    let Some(usage) = response.get("usage") else {
        return GrokUsage::default();
    };
    GrokUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
    }
}

fn failed_event_usage(value: &Value) -> Option<GrokUsage> {
    let event_type = value.get("type").and_then(Value::as_str)?;
    if !matches!(event_type, "error" | "response.failed" | "response.error") {
        return None;
    }
    let response = value.get("response").unwrap_or(value);
    response.get("usage")?;
    Some(grok_usage(response))
}

fn parse_upstream_failure(value: &Value, event_type: &str) -> GrokUpstreamFailure {
    let response = value.get("response").unwrap_or(value);
    let error = response
        .get("error")
        .or_else(|| value.get("error"))
        .unwrap_or(response);
    let message = failure_message(value, response, error);
    let explicit_status = [error, response, value]
        .into_iter()
        .find_map(failure_status);
    let classification = failure_code(error, response, value)
        .and_then(classify_failure_code)
        .or_else(|| classify_failure_message(&message));
    let (status, classified) = explicit_status
        .map(|status| (status, true))
        .or_else(|| classification.map(|status| (status, true)))
        .unwrap_or((502, false));
    let retry_after = [error, response, value]
        .into_iter()
        .find_map(|object| object.get("retry_after").and_then(stringify_retry_after));

    GrokUpstreamFailure {
        event_type: event_type.to_owned(),
        status,
        retry_after,
        message,
        retryable: classified && matches!(status, 429 | 500 | 502 | 503 | 504 | 529),
    }
}

fn failure_message(value: &Value, response: &Value, error: &Value) -> String {
    let message = [error, response, value]
        .into_iter()
        .find_map(|object| {
            object
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        })
        .or_else(|| error.as_str().filter(|message| !message.trim().is_empty()))
        .unwrap_or("Grok upstream stream failed");
    super::super::text::truncate_utf8(message, MAX_UPSTREAM_FAILURE_MESSAGE_BYTES)
        .0
        .to_owned()
}

fn failure_status(value: &Value) -> Option<u16> {
    ["status_code", "http_status", "status"]
        .into_iter()
        .find_map(|field| value.get(field).and_then(parse_status))
}

fn parse_status(value: &Value) -> Option<u16> {
    let status = value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| value.as_str()?.parse::<u16>().ok())?;
    (400..=599).contains(&status).then_some(status)
}

fn failure_code<'a>(error: &'a Value, response: &'a Value, value: &'a Value) -> Option<&'a str> {
    ["code", "error_type", "type"]
        .into_iter()
        .find_map(|field| error.get(field).and_then(Value::as_str))
        .or_else(|| {
            ["code", "error_type"]
                .into_iter()
                .find_map(|field| response.get(field).and_then(Value::as_str))
        })
        .or_else(|| {
            ["code", "error_type"]
                .into_iter()
                .find_map(|field| value.get(field).and_then(Value::as_str))
        })
}

fn classify_failure_code(code: &str) -> Option<u16> {
    let code = code.trim().to_ascii_lowercase();
    if let Ok(status) = code.parse::<u16>()
        && (400..=599).contains(&status)
    {
        return Some(status);
    }
    match code.as_str() {
        "rate_limit_error" | "rate_limit_exceeded" | "too_many_requests" => Some(429),
        "overloaded_error" | "overloaded" | "server_overloaded" => Some(529),
        "authentication_error" | "unauthorized" | "invalid_api_key" => Some(401),
        "permission_error" | "forbidden" => Some(403),
        "not_found_error" | "not_found" => Some(404),
        "conflict_error" | "conflict" => Some(409),
        "unprocessable_entity" => Some(422),
        "invalid_request_error" | "bad_request" | "context_length_exceeded" => Some(400),
        "gateway_timeout" | "timeout" => Some(504),
        "service_unavailable" | "temporarily_unavailable" => Some(503),
        "server_error" | "internal_server_error" | "internal_error" | "api_error" => Some(500),
        _ => None,
    }
}

fn classify_failure_message(message: &str) -> Option<u16> {
    let message = message.to_ascii_lowercase();
    if message.contains("rate limit") || message.contains("too many requests") {
        Some(429)
    } else if message.contains("overload") {
        Some(529)
    } else if message.contains("service unavailable") || message.contains("temporarily unavailable")
    {
        Some(503)
    } else if message.contains("gateway timeout")
        || message.contains("timed out")
        || message.contains("timeout")
    {
        Some(504)
    } else if message.contains("internal server") || message.contains("server error") {
        Some(500)
    } else if message.contains("context window")
        || message.contains("context length")
        || message.contains("invalid request")
    {
        Some(400)
    } else if message.contains("unauthorized") || message.contains("authentication") {
        Some(401)
    } else if message.contains("forbidden") || message.contains("permission denied") {
        Some(403)
    } else {
        None
    }
}

fn stringify_retry_after(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(super) fn parse_function_arguments(arguments: &str) -> anyhow::Result<Value> {
    match parse_json_object(arguments) {
        Ok(value) => Ok(value),
        Err(JsonObjectError::Empty | JsonObjectError::Invalid(_)) => {
            anyhow::bail!("function arguments are incomplete")
        }
        Err(JsonObjectError::NotObject) => {
            anyhow::bail!("function arguments must be a JSON object")
        }
    }
}

pub fn reduce_upstream_bytes(bytes: &[u8]) -> Result<Vec<ReducerEvent>, GrokReductionError> {
    reduce_upstream_bytes_with_tool_policy(bytes, &ToolCallPolicy::permissive())
}

pub(crate) fn reduce_decoded_values_with_tool_policy(
    values: impl IntoIterator<Item = Value>,
    tool_policy: &ToolCallPolicy,
) -> Result<Vec<ReducerEvent>, GrokReductionError> {
    let mut reducer = Reducer::with_tool_policy(tool_policy.clone());
    let out = reducer
        .push_batch(values)
        .map_err(|error| GrokReductionError::new(error, reducer.usage().cloned()))?;
    ensure_reducer_finished(&reducer)?;
    Ok(out)
}

pub(crate) fn reduce_upstream_bytes_with_tool_policy(
    bytes: &[u8],
    tool_policy: &ToolCallPolicy,
) -> Result<Vec<ReducerEvent>, GrokReductionError> {
    let mut reducer = Reducer::with_tool_policy(tool_policy.clone());
    let mut decoder = SseDecoder::default();
    let values = decoder
        .push(bytes)
        .map_err(|error| GrokReductionError::new(error, None))?
        .into_iter()
        .map(|event| {
            serde_json::from_str(&event.data).map_err(|_| GrokReductionError::malformed_event())
        })
        .collect::<Result<Vec<Value>, GrokReductionError>>()?;
    let out = reducer
        .push_batch(values)
        .map_err(|error| GrokReductionError::new(error, reducer.usage().cloned()))?;
    decoder
        .finish()
        .map_err(|error| GrokReductionError::new(error, reducer.usage().cloned()))?;
    ensure_reducer_finished(&reducer)?;
    Ok(out)
}

fn ensure_reducer_finished(reducer: &Reducer) -> Result<(), GrokReductionError> {
    if reducer.finished() {
        Ok(())
    } else {
        Err(GrokReductionError::new(
            anyhow::anyhow!("Grok stream ended without completion"),
            reducer.usage().cloned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_tool_policy(tools: Value, tool_choice: Value) -> ToolCallPolicy {
        let request: crate::anthropic::schema::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"use tools"}],
                "tools":tools,
                "tool_choice":tool_choice
            }))
            .unwrap();
        let translated = super::super::request::translate_request(&request, "grok-4.5".into())
            .expect("test request must translate");
        ToolCallPolicy::from_request(&translated)
    }

    fn function_added(id: &str, name: &str) -> Value {
        serde_json::json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","call_id":id,"name":name}
        })
    }

    #[test]
    fn response_tool_policy_enforces_none_named_and_effective_allowlist() {
        let tools = serde_json::json!([
            {"name":"Read","input_schema":{"type":"object"}},
            {"name":"Grep","input_schema":{"type":"object"}},
            {"name":"XSearch","input_schema":{"type":"object"}}
        ]);

        let mut none = Reducer::with_tool_policy(response_tool_policy(
            tools.clone(),
            serde_json::json!({"type":"none"}),
        ));
        let error = none.push(function_added("c1", "Read")).unwrap_err();
        assert!(error.to_string().contains("tool_choice forbids"), "{error}");
        assert_eq!(none.total_tool_items, 0);

        let mut named = Reducer::with_tool_policy(response_tool_policy(
            tools,
            serde_json::json!({"type":"tool","name":"Read"}),
        ));
        let error = named.push(function_added("c1", "Grep")).unwrap_err();
        assert!(error.to_string().contains("requires \"Read\""), "{error}");
        named.push(function_added("c1", "Read")).unwrap();

        let mut auto = Reducer::with_tool_policy(response_tool_policy(
            serde_json::json!([{"name":"Read","input_schema":{"type":"object"}}]),
            serde_json::json!({"type":"auto"}),
        ));
        let function_error = auto.push(function_added("c1", "Bash")).unwrap_err();
        assert!(
            function_error.to_string().contains("undeclared function"),
            "{function_error}"
        );
        let hosted_error = auto
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"web_search_call","id":"ws1"}
            }))
            .unwrap_err();
        assert!(
            hosted_error.to_string().contains("undeclared hosted"),
            "{hosted_error}"
        );
    }

    #[test]
    fn response_tool_policy_requires_a_tool_only_on_success_terminal() {
        let tools = serde_json::json!([{"name":"Read","input_schema":{"type":"object"}}]);
        let policy = response_tool_policy(tools.clone(), serde_json::json!({"type":"any"}));
        let terminal = serde_json::json!({
            "type":"response.completed",
            "response":{"status":"completed","usage":{}}
        });
        let error = Reducer::with_tool_policy(policy.clone())
            .push(terminal.clone())
            .unwrap_err();
        assert!(
            error.to_string().contains("required by tool_choice"),
            "{error}"
        );

        let named = response_tool_policy(tools, serde_json::json!({"type":"tool","name":"Read"}));
        let error = Reducer::with_tool_policy(named).push(terminal).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("required function tool \"Read\""),
            "{error}"
        );

        let incomplete = Reducer::with_tool_policy(policy)
            .push(serde_json::json!({
                "type":"response.incomplete",
                "response":{
                    "status":"incomplete",
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "usage":{}
                }
            }))
            .expect("an incomplete response is not required to contain a tool call");
        assert!(incomplete.iter().any(|event| matches!(
            event,
            ReducerEvent::Terminal(GrokTerminal::IncompleteMaxOutputTokens)
        )));
    }

    #[test]
    fn response_tool_policy_accepts_completed_required_and_named_calls() {
        let tools = serde_json::json!([{"name":"Read","input_schema":{"type":"object"}}]);
        for choice in [
            serde_json::json!({"type":"any"}),
            serde_json::json!({"type":"tool","name":"Read"}),
        ] {
            let mut reducer =
                Reducer::with_tool_policy(response_tool_policy(tools.clone(), choice));
            let events = reducer
                .push_batch([
                    function_added("c1", "Read"),
                    serde_json::json!({
                        "type":"response.output_item.done",
                        "item":{
                            "type":"function_call",
                            "call_id":"c1",
                            "name":"Read",
                            "arguments":"{}"
                        }
                    }),
                    serde_json::json!({
                        "type":"response.completed",
                        "response":{"status":"completed","usage":{}}
                    }),
                ])
                .expect("the required function lifecycle is complete");
            assert!(matches!(
                events.last(),
                Some(ReducerEvent::Finish { stop_reason, .. }) if stop_reason == "tool_use"
            ));
        }
    }

    #[test]
    fn response_tool_policy_enforces_serial_and_parallel_call_counts() {
        let tools = serde_json::json!([
            {"name":"Read","input_schema":{"type":"object"}},
            {"type":"x_search","name":"XSearch","input_schema":{"type":"object"}}
        ]);
        let serial_policy = response_tool_policy(
            tools.clone(),
            serde_json::json!({"type":"auto","disable_parallel_tool_use":true}),
        );

        let mut repeated = Reducer::with_tool_policy(serial_policy.clone());
        repeated.push(function_added("c1", "Read")).unwrap();
        let error = repeated.push(function_added("c2", "Read")).unwrap_err();
        assert!(
            error.to_string().contains("parallel_tool_calls is false"),
            "{error}"
        );
        assert_eq!(repeated.total_tool_items, 1);

        let mut mixed = Reducer::with_tool_policy(serial_policy);
        mixed.push(function_added("c1", "Read")).unwrap();
        let error = mixed
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"x_search_call","id":"xs1"}
            }))
            .unwrap_err();
        assert!(
            error.to_string().contains("parallel_tool_calls is false"),
            "{error}"
        );
        assert_eq!(mixed.total_tool_items, 1);

        let parallel_policy = response_tool_policy(tools, serde_json::json!({"type":"auto"}));
        let mut parallel = Reducer::with_tool_policy(parallel_policy);
        parallel.push(function_added("c1", "Read")).unwrap();
        parallel.push(function_added("c2", "Read")).unwrap();
        parallel
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"x_search_call","id":"xs1"}
            }))
            .unwrap();
        assert_eq!(parallel.total_tool_items, 3);
    }

    #[test]
    fn response_tool_policy_enforces_hosted_max_uses_before_state_mutation() {
        for (declared_type, name, item_type) in [
            ("web_search_20260318", "web_search", "web_search_call"),
            ("x_search", "XSearch", "x_search_call"),
        ] {
            let policy = response_tool_policy(
                serde_json::json!([{
                    "type":declared_type,
                    "name":name,
                    "input_schema":{"type":"object"},
                    "max_uses":1,
                    "allowed_callers":["direct"]
                }]),
                serde_json::json!({"type":"auto"}),
            );
            let mut reducer = Reducer::with_tool_policy(policy);
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":{"type":item_type,"id":"search_1"}
                }))
                .unwrap();
            let error = reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":{"type":item_type,"id":"search_2"}
                }))
                .unwrap_err();
            assert!(error.to_string().contains("max_uses"), "{name}: {error}");
            assert_eq!(reducer.total_tool_items, 1, "{name}");
            assert_eq!(reducer.hosted_calls.len(), 1, "{name}");
            assert!(!reducer.tool_item_ids.contains("search_2"), "{name}");
        }
    }

    #[test]
    fn required_policy_rejects_x_search_filtered_from_the_wire_request() {
        let policy = response_tool_policy(
            serde_json::json!([
                {"type":"x_search","name":"XSearch","input_schema":{"type":"object"}},
                {"name":"Read","input_schema":{"type":"object"}}
            ]),
            serde_json::json!({"type":"any"}),
        );
        let mut reducer = Reducer::with_tool_policy(policy);
        let error = reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"x_search_call","id":"xs1"}
            }))
            .unwrap_err();
        assert!(
            error.to_string().contains("undeclared hosted tool"),
            "{error}"
        );
    }

    #[test]
    fn sequence_validator_ignores_only_the_same_number_and_payload() {
        let mut reducer = Reducer::default();
        let first = serde_json::from_str(
            r#"{"type":"response.output_text.delta","delta":"yes 😊","sequence_number":7}"#,
        )
        .unwrap();
        let reordered = serde_json::from_str(
            r#"{"sequence_number":7,"delta":"yes 😊","type":"response.output_text.delta"}"#,
        )
        .unwrap();

        assert_eq!(reducer.push(first).unwrap().len(), 2);
        assert!(reducer.push(reordered).unwrap().is_empty());

        let repeated_text_new_sequence = serde_json::json!({
            "sequence_number": 8,
            "type": "response.output_text.delta",
            "delta": "yes 😊"
        });
        let events = reducer.push(repeated_text_new_sequence).unwrap();
        assert!(matches!(
            events.as_slice(),
            [ReducerEvent::TextDelta(_, text)] if text == "yes 😊"
        ));
        let old_duplicate = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "yes 😊",
            "sequence_number": 7
        });
        assert!(reducer.push(old_duplicate).unwrap().is_empty());

        let same_text_again = serde_json::json!({
            "sequence_number": 9,
            "type": "response.output_text.delta",
            "delta": "yes 😊"
        });
        assert!(matches!(
            reducer.push(same_text_again).unwrap().as_slice(),
            [ReducerEvent::TextDelta(_, text)] if text == "yes 😊"
        ));
    }

    #[test]
    fn sequence_validator_rejects_conflicts_backwards_and_mixed_streams() {
        let mut conflict = Reducer::default();
        conflict
            .push(serde_json::json!({
                "sequence_number": 1,
                "type": "response.output_text.delta",
                "delta": "a"
            }))
            .unwrap();
        let error = conflict
            .push(serde_json::json!({
                "sequence_number": 1,
                "type": "response.output_text.delta",
                "delta": "b"
            }))
            .unwrap_err();
        assert!(error.to_string().contains("different payload"));

        let mut backwards = Reducer::default();
        backwards
            .push(serde_json::json!({
                "sequence_number": 5,
                "type": "response.created"
            }))
            .unwrap();
        assert!(
            backwards
                .push(serde_json::json!({
                    "sequence_number": 4,
                    "type": "response.in_progress"
                }))
                .unwrap_err()
                .to_string()
                .contains("backwards")
        );

        let mut mixed = Reducer::default();
        mixed
            .push(serde_json::json!({"type": "response.created"}))
            .unwrap();
        assert!(
            mixed
                .push(serde_json::json!({
                    "sequence_number": 1,
                    "type": "response.in_progress"
                }))
                .is_err()
        );
    }

    #[test]
    fn sequence_validator_deduplicates_tool_deltas_before_state_mutation() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "sequence_number": 0,
                "type": "response.output_item.added",
                "item": {"type":"function_call","call_id":"call_1","name":"lookup"}
            }))
            .unwrap();
        let delta = serde_json::json!({
            "sequence_number": 1,
            "type": "response.function_call_arguments.delta",
            "call_id": "call_1",
            "delta": "{}"
        });
        assert_eq!(reducer.push(delta.clone()).unwrap().len(), 1);
        assert!(reducer.push(delta).unwrap().is_empty());
    }

    #[test]
    fn sequence_validator_rejects_gaps_and_ignores_repeated_terminal_event() {
        let mut gap = Reducer::default();
        gap.push(serde_json::json!({
            "sequence_number": 3,
            "type": "response.created"
        }))
        .unwrap();
        let error = gap
            .push(serde_json::json!({
                "sequence_number": 5,
                "type": "response.output_text.delta",
                "delta": "unsafe gap"
            }))
            .unwrap_err();
        assert!(error.to_string().contains("gap"));

        let mut terminal = Reducer::default();
        let completed = serde_json::json!({
            "sequence_number": 12,
            "type": "response.completed",
            "response": {"usage":{"input_tokens":1,"output_tokens":1}}
        });
        assert!(!terminal.push(completed.clone()).unwrap().is_empty());
        assert!(terminal.push(completed).unwrap().is_empty());
    }

    #[test]
    fn terminal_tail_policy_allows_only_rate_limit_telemetry() {
        let completed = serde_json::json!({
            "type": "response.completed",
            "response": {"usage": {}}
        });
        let mut allowed = Reducer::default();
        let events = allowed
            .push_batch([
                completed.clone(),
                serde_json::json!({"type":"rate_limits.updated","remaining":42}),
            ])
            .unwrap();
        assert!(allowed.finished());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ReducerEvent::Finish { .. }))
        );

        for tail in [
            serde_json::json!({"type":"response.output_text.delta","delta":"unsafe"}),
            serde_json::json!({"type":"response.output_item.added","item":{"type":"message"}}),
            serde_json::json!({"type":"response.usage","usage":{}}),
            serde_json::json!({"type":"future.telemetry"}),
        ] {
            let mut reducer = Reducer::default();
            let error = reducer.push_batch([completed.clone(), tail]).unwrap_err();
            assert!(
                error.to_string().contains("event after terminal"),
                "{error:#}"
            );
            assert!(
                !reducer.finished(),
                "a rejected same-batch tail must roll back the terminal"
            );
        }
    }

    #[test]
    fn terminal_events_require_a_response_object() {
        for event in [
            serde_json::json!({"type":"response.completed"}),
            serde_json::json!({"type":"response.completed","response":null}),
            serde_json::json!({"type":"response.incomplete","response":[]}),
        ] {
            let error = Reducer::default().push(event).unwrap_err();
            assert!(error.to_string().contains("response is not an object"));
        }
    }

    #[test]
    fn terminal_status_must_match_the_event_type_when_present() {
        for event in [
            serde_json::json!({
                "type":"response.completed",
                "response":{"status":"incomplete","usage":{}}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{"status":"failed","usage":{}}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{"status":42,"usage":{}}
            }),
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":"completed","usage":{}}
            }),
            serde_json::json!({
                "type":"response.completed",
                "response":{
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "usage":{}
                }
            }),
        ] {
            assert!(Reducer::default().push(event).is_err());
        }

        for event in [
            serde_json::json!({
                "type":"response.completed",
                "response":{"status":"completed","usage":{}}
            }),
            serde_json::json!({
                "type":"response.incomplete",
                "response":{"status":"incomplete","usage":{}}
            }),
        ] {
            assert!(Reducer::default().push(event).is_ok());
        }
    }

    #[test]
    fn terminal_snapshot_rejects_unobserved_or_omitted_tool_items() {
        for item in [
            serde_json::json!({
                "type":"function_call",
                "id":"item_1",
                "call_id":"call_1",
                "name":"lookup",
                "arguments":"{}"
            }),
            serde_json::json!({
                "type":"web_search_call",
                "id":"search_1"
            }),
        ] {
            let error = Reducer::default()
                .push(serde_json::json!({
                    "type":"response.completed",
                    "response":{"output":[item],"usage":{}}
                }))
                .unwrap_err();
            assert!(error.to_string().contains("unobserved"), "{error:#}");
        }

        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "id":"item_1",
                    "call_id":"call_1",
                    "name":"lookup"
                }
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{
                    "type":"function_call",
                    "id":"item_1",
                    "call_id":"call_1",
                    "name":"lookup",
                    "arguments":"{}"
                }
            }))
            .unwrap();
        let error = reducer
            .push(serde_json::json!({
                "type":"response.completed",
                "response":{"output":[],"usage":{}}
            }))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("omits a completed function call")
        );
    }

    #[test]
    fn terminal_snapshot_accepts_matching_function_lifecycle() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "id":"item_1",
                    "call_id":"call_1",
                    "name":"lookup"
                }
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{
                    "type":"function_call",
                    "id":"item_1",
                    "call_id":"call_1",
                    "name":"lookup",
                    "arguments":"{\"q\":1}"
                }
            }))
            .unwrap();
        let events = reducer
            .push(serde_json::json!({
                "type":"response.completed",
                "response":{
                    "status":"completed",
                    "output":[{
                        "type":"function_call",
                        "id":"item_1",
                        "call_id":"call_1",
                        "name":"lookup",
                        "arguments":"{\"q\":1}"
                    }],
                    "usage":{}
                }
            }))
            .unwrap();
        assert!(matches!(events.last(), Some(ReducerEvent::Finish { .. })));
    }

    #[test]
    fn terminal_snapshot_accepts_matching_hosted_lifecycle() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"custom_tool_call",
                    "id":"search_1",
                    "name":"web_search"
                }
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.custom_tool_call_input.delta",
                "item_id":"search_1",
                "delta":"{\"query\":\"rust\"}"
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.custom_tool_call_input.done",
                "item_id":"search_1"
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{
                    "type":"custom_tool_call",
                    "id":"search_1",
                    "name":"web_search"
                }
            }))
            .unwrap();
        let events = reducer
            .push(serde_json::json!({
                "type":"response.completed",
                "response":{
                    "output":[{
                        "type":"custom_tool_call",
                        "id":"search_1",
                        "name":"web_search"
                    }],
                    "usage":{}
                }
            }))
            .unwrap();
        assert!(matches!(events.last(), Some(ReducerEvent::Finish { .. })));
    }

    #[test]
    fn output_item_events_fail_closed_on_missing_or_unknown_types() {
        for event in [
            serde_json::json!({"type":"response.output_item.added","item":{}}),
            serde_json::json!({"type":"response.output_item.added","item":{"type":"future"}}),
            serde_json::json!({"type":"response.output_item.done","item":{}}),
            serde_json::json!({"type":"response.output_item.done","item":{"type":"future"}}),
        ] {
            assert!(Reducer::default().push(event).is_err());
        }

        for event_type in ["response.output_item.added", "response.output_item.done"] {
            for item_type in ["message", "reasoning"] {
                let events = Reducer::default()
                    .push(serde_json::json!({
                        "type":event_type,
                        "item":{"type":item_type}
                    }))
                    .unwrap();
                assert!(events.is_empty());
            }
        }
    }

    #[test]
    fn failed_event_usage_survives_transaction_rollback() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"discarded"}),
                serde_json::json!({
                    "type":"response.failed",
                    "response":{"usage":{
                        "input_tokens":12,
                        "input_tokens_details":{"cached_tokens":3},
                        "output_tokens":2
                    }}
                }),
            ])
            .unwrap_err();

        assert!(error.to_string().contains("failed"));
        let usage = reducer.usage().expect("failed response usage must survive");
        assert_eq!(usage.mapped_input_tokens(), Some(9));
        assert_eq!(usage.mapped_cache_read_input_tokens(), Some(3));
        assert_eq!(usage.output_tokens, Some(2));
        assert!(!reducer.finished());
    }

    #[test]
    fn numbered_terminal_ignores_unsequenced_and_duplicate_rate_limit_telemetry() {
        let completed = serde_json::json!({
            "sequence_number": 41,
            "type": "response.completed",
            "response": {"usage": {}}
        });
        let telemetry = serde_json::json!({
            "type": "rate_limits.updated",
            "remaining": 42
        });
        let mut reducer = Reducer::default();
        let events = reducer
            .push_batch([completed, telemetry.clone(), telemetry])
            .unwrap();

        assert!(reducer.finished());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ReducerEvent::Finish { .. }))
                .count(),
            1
        );

        let unsequenced_text = reducer
            .push(serde_json::json!({
                "type":"response.output_text.delta",
                "delta":"unsafe"
            }))
            .unwrap_err();
        assert!(
            unsequenced_text
                .to_string()
                .contains("omitted sequence_number"),
            "{unsequenced_text:#}"
        );

        let sequenced_text = reducer
            .push(serde_json::json!({
                "sequence_number":42,
                "type":"response.output_text.delta",
                "delta":"unsafe"
            }))
            .unwrap_err();
        assert!(
            sequenced_text.to_string().contains("event after terminal"),
            "{sequenced_text:#}"
        );
    }

    #[test]
    fn incomplete_reason_mapping_is_explicit_and_conservative() {
        let cases = [
            (
                Some("max_output_tokens"),
                GrokTerminal::IncompleteMaxOutputTokens,
                "max_tokens",
            ),
            (
                Some("max_tokens"),
                GrokTerminal::IncompleteMaxOutputTokens,
                "max_tokens",
            ),
            (
                Some("length"),
                GrokTerminal::IncompleteMaxOutputTokens,
                "max_tokens",
            ),
            (
                Some("content_filter"),
                GrokTerminal::IncompleteContentFilter,
                "refusal",
            ),
            (
                Some("upstream_capacity"),
                GrokTerminal::IncompleteOther,
                "refusal",
            ),
            (None, GrokTerminal::IncompleteMissingReason, "refusal"),
        ];

        for (reason, expected_terminal, expected_stop) in cases {
            let mut response = serde_json::json!({"status":"incomplete","usage":{}});
            if let Some(reason) = reason {
                response["incomplete_details"] = serde_json::json!({"reason":reason});
            }
            let mut reducer = Reducer::default();
            let events = reducer
                .push(serde_json::json!({
                    "type":"response.incomplete",
                    "response":response
                }))
                .unwrap();
            assert!(events.iter().any(
                |event| matches!(event, ReducerEvent::Terminal(status) if *status == expected_terminal)
            ));
            assert!(events.iter().any(|event| matches!(
                event,
                ReducerEvent::Finish { stop_reason, .. } if stop_reason == expected_stop
            )));
            assert_eq!(reducer.incomplete_reason(), reason);
        }
    }

    #[test]
    fn unknown_incomplete_reason_is_utf8_safely_bounded_for_logs() {
        let reason = format!("{}😊tail", "x".repeat(255));
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.incomplete",
                "response":{
                    "status":"incomplete",
                    "incomplete_details":{"reason":reason},
                    "usage":{}
                }
            }))
            .unwrap();

        let captured = reducer.incomplete_reason().unwrap();
        assert!(captured.len() <= MAX_INCOMPLETE_REASON_BYTES);
        assert_eq!(captured, "x".repeat(255));
    }

    #[test]
    fn incomplete_terminal_overrides_completed_function_tool_stop_reason() {
        for (reason, expected_stop) in [
            ("max_output_tokens", "max_tokens"),
            ("content_filter", "refusal"),
        ] {
            let mut reducer = Reducer::default();
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":{"type":"function_call","call_id":"call_1","name":"lookup"}
                }))
                .unwrap();
            let completed_tool = reducer
                .push(serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{"type":"function_call","call_id":"call_1","arguments":"{}"}
                }))
                .unwrap();
            assert!(
                completed_tool
                    .iter()
                    .any(|event| matches!(event, ReducerEvent::ToolStop(_)))
            );
            let terminal = reducer
                .push(serde_json::json!({
                    "type":"response.incomplete",
                    "response":{
                        "status":"incomplete",
                        "incomplete_details":{"reason":reason},
                        "usage":{}
                    }
                }))
                .unwrap();
            assert!(terminal.iter().any(|event| matches!(
                event,
                ReducerEvent::Finish { stop_reason, .. } if stop_reason == expected_stop
            )));
        }
    }

    #[test]
    fn grok_reducer_handles_text_tool_and_completion() {
        let input = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(
            matches!(events.last(), Some(ReducerEvent::Finish { stop_reason, .. }) if stop_reason == "tool_use")
        );
    }

    #[test]
    fn late_output_text_done_after_function_call_start_is_accepted() {
        // Real Grok captures commonly start tools before emitting
        // `response.output_text.done` for the preceding assistant text.
        let mut reducer = Reducer::default();
        let events = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_text.delta","delta":" world"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1"}}),
                serde_json::json!({"type":"response.function_call_arguments.delta","call_id":"c1","delta":"{\"command\":\"ls\"}"}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello world"}),
                serde_json::json!({"type":"response.function_call_arguments.done","call_id":"c1","arguments":"{\"command\":\"ls\"}"}),
                serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1","arguments":"{\"command\":\"ls\"}"}}),
                serde_json::json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1},"output":[
                    {"type":"message","id":"msg1","content":[{"type":"output_text","text":"Hello world"}]},
                    {"type":"function_call","call_id":"c1","name":"Bash","id":"fc1","arguments":"{\"command\":\"ls\"}"}
                ]}}),
            ])
            .expect("late matching output_text.done after tool start must not fail closed");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ReducerEvent::TextDelta(_, text) if text == "Hello")),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(event, ReducerEvent::ToolStart(_, id, name) if id == "c1" && name == "Bash")),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(ReducerEvent::Finish { stop_reason, .. }) if stop_reason == "tool_use"),
            "{events:?}"
        );
    }

    #[test]
    fn late_output_text_done_after_function_call_start_still_rejects_mismatch() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1"}}),
                serde_json::json!({"type":"response.output_text.done","text":"Goodbye"}),
            ])
            .expect_err("mismatched late text snapshot must still fail closed");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn completed_text_snapshot_cannot_be_extended_by_a_second_done_event() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello injected"}),
            ])
            .expect_err("a completed text block must not be reopened by another done event");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn late_output_text_done_is_consumed_once() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1"}}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
            ])
            .expect_err("a late text snapshot must not be accepted twice");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn late_output_text_done_rejects_unstreamed_extension_after_tool_start() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1"}}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello injected"}),
            ])
            .expect_err("late text cannot be extended after its content block was closed");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn non_tool_text_close_does_not_authorize_a_late_done_snapshot() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.reasoning_text.delta","delta":"thinking"}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
            ])
            .expect_err("only a tool boundary may retain text for a late done snapshot");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn textless_done_consumes_the_late_snapshot_window() {
        let mut reducer = Reducer::default();
        let error = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1"}}),
                serde_json::json!({"type":"response.output_text.done"}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
            ])
            .expect_err("a textless done must still consume the late snapshot window");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn parallel_function_calls_preserve_one_pending_late_text_snapshot() {
        let mut reducer = Reducer::default();
        let events = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Read","id":"fc1"}}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c2","name":"Grep","id":"fc2"}}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
            ])
            .expect("another function call must not discard the pending text snapshot");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ReducerEvent::ToolStart(_, _, _)))
                .count(),
            2
        );
    }

    fn complete_typed_hosted_search(
        reducer: &mut Reducer,
        event_name: &str,
        item_type: &str,
        id: &str,
    ) -> anyhow::Result<Vec<ReducerEvent>> {
        reducer.push_batch([
            serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":item_type,"id":id}
            }),
            serde_json::json!({
                "type":format!("response.{event_name}.in_progress"),
                "item_id":id
            }),
            serde_json::json!({
                "type":format!("response.{event_name}.searching"),
                "item_id":id
            }),
            serde_json::json!({
                "type":format!("response.{event_name}.completed"),
                "item_id":id
            }),
            serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":item_type,"id":id,"action":{"query":"rust"}}
            }),
        ])
    }

    #[test]
    fn late_output_text_done_after_typed_hosted_search_completion_is_accepted() {
        for (event_name, item_type, id) in [
            ("web_search_call", "web_search_call", "ws1"),
            ("x_search_call", "x_search_call", "xs1"),
        ] {
            let mut reducer = Reducer::default();
            reducer
                .push(serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"Hello"
                }))
                .unwrap();
            let boundary = complete_typed_hosted_search(&mut reducer, event_name, item_type, id)
                .expect("a completed hosted search must close the active text block");
            assert!(
                boundary
                    .iter()
                    .any(|event| matches!(event, ReducerEvent::TextStop(_))),
                "{boundary:?}"
            );

            let late_done = reducer
                .push(serde_json::json!({
                    "type":"response.output_text.done",
                    "text":"Hello"
                }))
                .expect("an exact late snapshot after a hosted search must be accepted");
            assert!(late_done.is_empty(), "{late_done:?}");
            assert!(reducer.pending_tool_boundary_text.is_none());
        }
    }

    #[test]
    fn hosted_tool_boundary_late_snapshot_is_exact_and_consumed_once() {
        for (event_name, item_type, id) in [
            ("web_search_call", "web_search_call", "ws1"),
            ("x_search_call", "x_search_call", "xs1"),
        ] {
            let mut extended = Reducer::default();
            extended
                .push(serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"Hello"
                }))
                .unwrap();
            complete_typed_hosted_search(&mut extended, event_name, item_type, id).unwrap();
            let error = extended
                .push(serde_json::json!({
                    "type":"response.output_text.done",
                    "text":"Hello injected"
                }))
                .expect_err("a hosted-tool boundary must reject an unstreamed extension");
            assert!(
                error
                    .to_string()
                    .contains("completed text snapshot disagrees with streamed deltas"),
                "{error}"
            );

            let mut repeated = Reducer::default();
            repeated
                .push(serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"Hello"
                }))
                .unwrap();
            complete_typed_hosted_search(&mut repeated, event_name, item_type, id).unwrap();
            repeated
                .push(serde_json::json!({
                    "type":"response.output_text.done",
                    "text":"Hello"
                }))
                .unwrap();
            let error = repeated
                .push(serde_json::json!({
                    "type":"response.output_text.done",
                    "text":"Hello"
                }))
                .expect_err("an exact hosted-tool snapshot must be consumed once");
            assert!(
                error
                    .to_string()
                    .contains("completed text snapshot disagrees with streamed deltas"),
                "{error}"
            );
        }
    }

    #[test]
    fn adjacent_web_and_x_search_boundaries_preserve_one_late_text_snapshot() {
        let mut reducer = Reducer::default();
        let events = reducer
            .push_batch([
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"web_search_call","id":"ws1"}}),
                serde_json::json!({"type":"response.output_item.added","item":{"type":"x_search_call","id":"xs1"}}),
                serde_json::json!({"type":"response.web_search_call.in_progress","item_id":"ws1"}),
                serde_json::json!({"type":"response.x_search_call.in_progress","item_id":"xs1"}),
                serde_json::json!({"type":"response.x_search_call.searching","item_id":"xs1"}),
                serde_json::json!({"type":"response.web_search_call.searching","item_id":"ws1"}),
                serde_json::json!({"type":"response.web_search_call.completed","item_id":"ws1"}),
                serde_json::json!({"type":"response.x_search_call.completed","item_id":"xs1"}),
                serde_json::json!({"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws1","action":{"query":"rust"}}}),
                serde_json::json!({"type":"response.output_item.done","item":{"type":"x_search_call","id":"xs1","action":{"query":"grok"}}}),
                serde_json::json!({"type":"response.output_text.done","text":"Hello"}),
            ])
            .expect("adjacent hosted-tool boundaries must preserve the first late snapshot");

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ReducerEvent::TextStop(_)))
                .count(),
            1,
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ReducerEvent::HostedSearch { .. }))
                .count(),
            1,
            "{events:?}"
        );
        assert!(reducer.pending_tool_boundary_text.is_none());
    }

    #[test]
    fn empty_text_delta_cannot_bypass_late_snapshot_exact_match() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({"type":"response.output_text.delta","delta":""}))
            .unwrap();
        reducer
            .push(serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"Bash","id":"fc1"}}))
            .unwrap();
        let error = reducer
            .push(serde_json::json!({"type":"response.output_text.done","text":"injected"}))
            .expect_err("an empty delta must still require an exact late snapshot");
        assert!(
            error
                .to_string()
                .contains("completed text snapshot disagrees with streamed deltas"),
            "{error}"
        );
    }

    #[test]
    fn grok_reducer_maps_item_id_argument_events() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"Bash\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"item_1\",\"arguments\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ReducerEvent::ToolDelta(_, delta) if delta == "{}"))
        );
        assert!(matches!(events.last(), Some(ReducerEvent::Finish { .. })));
    }

    fn reducer_with_two_identified_function_calls() -> Reducer {
        let mut reducer = Reducer::default();
        for (item_id, call_id, name) in [
            ("item_1", "call_1", "first"),
            ("item_2", "call_2", "second"),
        ] {
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":{
                        "type":"function_call",
                        "id":item_id,
                        "call_id":call_id,
                        "name":name
                    }
                }))
                .unwrap();
        }
        reducer
    }

    #[test]
    fn function_argument_events_reject_unknown_or_mixed_identities() {
        let base = reducer_with_two_identified_function_calls();

        for event in [
            serde_json::json!({
                "type":"response.function_call_arguments.delta",
                "call_id":"call_1",
                "item_id":"item_2",
                "delta":"{}"
            }),
            serde_json::json!({
                "type":"response.function_call_arguments.delta",
                "call_id":"call_1",
                "item_id":"missing_item",
                "delta":"{}"
            }),
            serde_json::json!({
                "type":"response.function_call_arguments.delta",
                "call_id":"missing_call",
                "item_id":"item_1",
                "delta":"{}"
            }),
            serde_json::json!({
                "type":"response.function_call_arguments.done",
                "call_id":"call_1",
                "item_id":"item_2",
                "arguments":"{}"
            }),
            serde_json::json!({
                "type":"response.function_call_arguments.done",
                "call_id":"call_1",
                "item_id":"missing_item",
                "arguments":"{}"
            }),
            serde_json::json!({
                "type":"response.function_call_arguments.done",
                "call_id":"missing_call",
                "item_id":"item_1",
                "arguments":"{}"
            }),
        ] {
            let mut reducer = base.clone();
            let error = reducer.push(event).unwrap_err();
            assert!(
                error.to_string().contains("unknown")
                    || error.to_string().contains("different function calls"),
                "{error:#}"
            );
            assert_eq!(reducer.calls.len(), 2);
            assert_eq!(reducer.item_calls.len(), 2);
        }

        let mut reducer = base;
        reducer
            .push(serde_json::json!({
                "type":"response.function_call_arguments.delta",
                "call_id":"call_1",
                "item_id":"item_1",
                "delta":"{}"
            }))
            .unwrap();
    }

    #[test]
    fn completed_function_item_rejects_conflicting_id_or_name() {
        let base = reducer_with_two_identified_function_calls();
        for item in [
            serde_json::json!({
                "type":"function_call",
                "id":"item_2",
                "call_id":"call_1",
                "name":"first",
                "arguments":"{}"
            }),
            serde_json::json!({
                "type":"function_call",
                "id":"item_1",
                "call_id":"call_1",
                "name":"second",
                "arguments":"{}"
            }),
            serde_json::json!({
                "type":"function_call",
                "id":"item_1",
                "call_id":"missing_call",
                "name":"first",
                "arguments":"{}"
            }),
        ] {
            let mut reducer = base.clone();
            let error = reducer
                .push(serde_json::json!({
                    "type":"response.output_item.done",
                    "item":item
                }))
                .unwrap_err();
            assert!(
                error.to_string().contains("disagrees") || error.to_string().contains("unknown"),
                "{error:#}"
            );
            assert_eq!(reducer.calls.len(), 2);
            assert_eq!(reducer.item_calls.len(), 2);
        }
    }

    #[test]
    fn function_argument_fields_reject_present_non_string_values() {
        let mut base = Reducer::default();
        base.push(serde_json::json!({
            "type":"response.output_item.added",
            "item":{
                "type":"function_call",
                "id":"item_1",
                "call_id":"call_1",
                "name":"lookup"
            }
        }))
        .unwrap();
        base.push(serde_json::json!({
            "type":"response.function_call_arguments.delta",
            "call_id":"call_1",
            "delta":"{}"
        }))
        .unwrap();

        for arguments in [
            serde_json::json!(42),
            serde_json::json!(null),
            serde_json::json!([]),
        ] {
            let mut reducer = base.clone();
            let error = reducer
                .push(serde_json::json!({
                    "type":"response.function_call_arguments.done",
                    "call_id":"call_1",
                    "arguments":arguments.clone()
                }))
                .unwrap_err();
            assert!(error.to_string().contains("must be a string"), "{error:#}");

            let mut reducer = base.clone();
            let error = reducer
                .push(serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{
                        "type":"function_call",
                        "id":"item_1",
                        "call_id":"call_1",
                        "name":"lookup",
                        "arguments":arguments
                    }
                }))
                .unwrap_err();
            assert!(error.to_string().contains("must be a string"), "{error:#}");
        }
    }

    #[test]
    fn completed_function_call_and_item_ids_cannot_be_reused() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "id":"item_1",
                    "call_id":"call_1",
                    "name":"lookup"
                }
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{
                    "type":"function_call",
                    "id":"item_1",
                    "call_id":"call_1",
                    "name":"lookup",
                    "arguments":"{}"
                }
            }))
            .unwrap();

        assert!(reducer.calls.is_empty());
        assert!(reducer.item_calls.is_empty());
        assert!(reducer.tool_args.is_empty());
        assert!(reducer.completed_arguments.is_empty());
        assert!(reducer.completed_calls.contains_key("call_1"));
        assert!(reducer.tool_item_ids.contains("item_1"));

        for item in [
            serde_json::json!({
                "type":"function_call",
                "id":"item_2",
                "call_id":"call_1",
                "name":"lookup"
            }),
            serde_json::json!({
                "type":"function_call",
                "id":"item_1",
                "call_id":"call_2",
                "name":"lookup"
            }),
        ] {
            let mut attempt = reducer.clone();
            let error = attempt
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":item
                }))
                .unwrap_err();
            assert!(error.to_string().contains("duplicate"));
            assert!(attempt.calls.is_empty());
            assert!(attempt.item_calls.is_empty());
        }
    }

    #[test]
    fn function_and_hosted_calls_share_one_output_item_id_namespace() {
        let mut function_first = Reducer::default();
        function_first
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "id":"shared_item",
                    "call_id":"call_1",
                    "name":"lookup"
                }
            }))
            .unwrap();
        let error = function_first
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"custom_tool_call",
                    "id":"shared_item",
                    "name":"web_search"
                }
            }))
            .unwrap_err();
        assert!(error.to_string().contains("duplicate output tool item id"));

        let mut hosted_first = Reducer::default();
        hosted_first
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"custom_tool_call",
                    "id":"shared_item",
                    "name":"web_search"
                }
            }))
            .unwrap();
        let error = hosted_first
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "id":"shared_item",
                    "call_id":"call_1",
                    "name":"lookup"
                }
            }))
            .unwrap_err();
        assert!(error.to_string().contains("duplicate output tool item id"));
    }

    #[test]
    fn function_and_hosted_calls_share_downstream_anthropic_tool_use_ids() {
        let hosted = serde_json::json!({
            "type":"response.output_item.added",
            "item":{"type":"web_search_call","id":"ws1"}
        });
        let function = serde_json::json!({
            "type":"response.output_item.added",
            "item":{
                "type":"function_call",
                "call_id":"srvtoolu_ws1",
                "name":"lookup"
            }
        });

        for values in [
            vec![hosted.clone(), function.clone()],
            vec![function.clone(), hosted.clone()],
        ] {
            let mut reducer = Reducer::default();
            let error = reducer.push_batch(values).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("duplicate downstream Anthropic tool-use id"),
                "{error}"
            );
            assert_eq!(reducer.total_tool_items, 0);
            assert!(reducer.calls.is_empty());
            assert!(reducer.hosted_calls.is_empty());
            assert!(reducer.downstream_tool_use_ids.is_empty());
        }
    }

    #[test]
    fn response_total_tool_item_history_has_an_explicit_limit() {
        let mut reducer = Reducer::default();
        for index in 0..MAX_TOTAL_TOOL_ITEMS {
            let call_id = format!("call_{index}");
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":{
                        "type":"function_call",
                        "call_id":call_id.clone(),
                        "name":"lookup"
                    }
                }))
                .unwrap();
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{
                        "type":"function_call",
                        "call_id":call_id,
                        "arguments":"{}"
                    }
                }))
                .unwrap();
        }
        assert_eq!(reducer.total_tool_items, MAX_TOTAL_TOOL_ITEMS);
        assert_eq!(reducer.completed_calls.len(), MAX_TOTAL_TOOL_ITEMS);
        assert!(reducer.calls.is_empty());

        let error = reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "call_id":"one_too_many",
                    "name":"lookup"
                }
            }))
            .unwrap_err();
        assert!(error.to_string().contains("too many tool items"));
    }

    #[test]
    fn completed_function_calls_release_all_active_lookup_state() {
        let mut reducer = Reducer::default();

        for index in 0..128 {
            let call_id = format!("call_{index}");
            let item_id = format!("item_{index}");
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":{
                        "type":"function_call",
                        "id":item_id,
                        "call_id":call_id,
                        "name":"lookup"
                    }
                }))
                .unwrap();
            assert_eq!(reducer.calls.len(), 1);
            assert_eq!(reducer.item_calls.len(), 1);

            reducer
                .push(serde_json::json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":item_id,
                    "delta":"{}"
                }))
                .unwrap();
            reducer
                .push(serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{
                        "type":"function_call",
                        "id":item_id,
                        "call_id":call_id
                    }
                }))
                .unwrap();

            assert!(reducer.calls.is_empty());
            assert!(reducer.item_calls.is_empty());
            assert!(reducer.tool_args.is_empty());
            assert!(reducer.completed_arguments.is_empty());
            assert_eq!(reducer.completed_calls.len(), index + 1);
            assert_eq!(reducer.tool_item_ids.len(), index + 1);
        }
    }

    #[test]
    fn grok_reducer_accepts_live_reasoning_text_events() {
        let input = b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"think\"}\n\ndata: {\"type\":\"response.reasoning_text.done\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(matches!(events[0], ReducerEvent::ThinkingStart(0)));
        assert!(matches!(
            &events[1],
            ReducerEvent::ThinkingDelta(0, delta) if delta == "think"
        ));
        assert!(matches!(events.last(), Some(ReducerEvent::Finish { .. })));
    }

    #[test]
    fn grok_reducer_accepts_hosted_search_lifecycle() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"web_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"search_1\",\"delta\":\"{\\\"query\\\":\\\"test\\\"}\"}\n\ndata: {\"type\":\"response.custom_tool_call_input.done\",\"item_id\":\"search_1\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"web_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"result\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ReducerEvent::HostedSearch { name, query, .. }
                if name == "web_search" && query == "test"
        )));
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                web_search_requests: 1,
                ..
            })
        ));
    }

    #[test]
    fn grok_reducer_rejects_x_search_only_empty_success() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"x_search_call\",\"id\":\"xs_1\"}}\n\ndata: {\"type\":\"response.x_search_call.in_progress\",\"item_id\":\"xs_1\"}\n\ndata: {\"type\":\"response.x_search_call.searching\",\"item_id\":\"xs_1\"}\n\ndata: {\"type\":\"response.x_search_call.completed\",\"item_id\":\"xs_1\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"x_search_call\",\"id\":\"xs_1\",\"action\":{\"query\":\"rust\"}}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let error = reduce_upstream_bytes(input).unwrap_err();
        assert!(
            error.to_string().contains("unobservable hosted x_search"),
            "{error}"
        );
    }

    #[test]
    fn hosted_search_declarations_accept_only_exact_known_identities() {
        for item in [
            serde_json::json!({"type":"custom_tool_call","id":"search_1"}),
            serde_json::json!({
                "type":"custom_tool_call",
                "name":"x_search_recent",
                "id":"search_1"
            }),
            serde_json::json!({
                "type":"web_search_call",
                "name":"x_search",
                "id":"search_1"
            }),
        ] {
            let mut reducer = Reducer::default();
            let error = reducer
                .push(serde_json::json!({
                    "type":"response.output_item.added",
                    "item":item
                }))
                .unwrap_err();
            assert!(error.to_string().contains("hosted") || error.to_string().contains("custom"));
            assert!(reducer.hosted_calls.is_empty());
        }
    }

    #[test]
    fn typed_hosted_search_lifecycle_rejects_unknown_mismatch_and_out_of_order() {
        let unknown = Reducer::default()
            .push(serde_json::json!({
                "type":"response.web_search_call.in_progress",
                "item_id":"missing"
            }))
            .unwrap_err();
        assert!(unknown.to_string().contains("out of order"));

        let mut mismatch = Reducer::default();
        mismatch
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"x_search_call","id":"search_1"}
            }))
            .unwrap();
        let error = mismatch
            .push(serde_json::json!({
                "type":"response.web_search_call.in_progress",
                "item_id":"search_1"
            }))
            .unwrap_err();
        assert!(error.to_string().contains("disagrees"));

        let mut out_of_order = Reducer::default();
        out_of_order
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"web_search_call","id":"search_1"}
            }))
            .unwrap();
        let error = out_of_order
            .push(serde_json::json!({
                "type":"response.web_search_call.searching",
                "item_id":"search_1"
            }))
            .unwrap_err();
        assert!(error.to_string().contains("out of order"));
    }

    #[test]
    fn hosted_search_completion_requires_matching_active_call() {
        let unknown = Reducer::default()
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"web_search_call","id":"missing"}
            }))
            .unwrap_err();
        assert!(unknown.to_string().contains("unknown"));

        let mut mismatch = Reducer::default();
        mismatch
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"custom_tool_call","name":"web_search","id":"search_1"}
            }))
            .unwrap();
        let error = mismatch
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"custom_tool_call","name":"x_search","id":"search_1"}
            }))
            .unwrap_err();
        assert!(error.to_string().contains("disagrees"));
        assert!(mismatch.hosted_calls.contains_key("search_1"));

        let mut out_of_order = Reducer::default();
        out_of_order
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"web_search_call","id":"search_1"}
            }))
            .unwrap();
        let error = out_of_order
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"web_search_call","id":"search_1"}
            }))
            .unwrap_err();
        assert!(error.to_string().contains("out of order"));
    }

    #[test]
    fn completed_hosted_search_ids_cannot_be_reused() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"custom_tool_call","name":"web_search","id":"search_1"}
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.custom_tool_call_input.delta",
                "item_id":"search_1",
                "delta":"{\"query\":\"rust\"}"
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.custom_tool_call_input.done",
                "item_id":"search_1"
            }))
            .unwrap();
        let events = reducer
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"custom_tool_call","name":"web_search","id":"search_1"}
            }))
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ReducerEvent::HostedSearch { id, .. }] if id == "srvtoolu_search_1"
        ));
        assert!(reducer.hosted_calls.is_empty());
        assert!(reducer.completed_hosted_inputs.is_empty());
        assert!(reducer.completed_hosted_calls.contains_key("search_1"));

        let duplicate_done = reducer
            .push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"custom_tool_call","name":"web_search","id":"search_1"}
            }))
            .unwrap_err();
        assert!(duplicate_done.to_string().contains("duplicate"));
        let duplicate_added = reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"custom_tool_call","name":"web_search","id":"search_1"}
            }))
            .unwrap_err();
        assert!(duplicate_added.to_string().contains("duplicate"));
    }

    #[test]
    fn grok_reducer_rejects_unfinished_hosted_search_at_terminal() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"web_search_call","id":"ws_1"}
            }))
            .unwrap();
        let error = reducer
            .push(serde_json::json!({
                "type":"response.completed",
                "response":{"usage":{}}
            }))
            .unwrap_err();
        assert!(error.to_string().contains("hosted tool call is incomplete"));
    }

    #[test]
    fn grok_reducer_rejects_deltas_after_argument_completion() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"function_call","call_id":"call_1","name":"lookup"}
            }))
            .unwrap();
        reducer
            .push(serde_json::json!({
                "type":"response.function_call_arguments.done",
                "call_id":"call_1",
                "arguments":"{}"
            }))
            .unwrap();
        let error = reducer
            .push(serde_json::json!({
                "type":"response.function_call_arguments.delta",
                "call_id":"call_1",
                "delta":" "
            }))
            .unwrap_err();
        assert!(error.to_string().contains("after arguments completion"));
    }

    #[test]
    fn grok_reducer_limits_snapshot_only_function_arguments() {
        let mut reducer = Reducer::default();
        reducer
            .push(serde_json::json!({
                "type":"response.output_item.added",
                "item":{"type":"function_call","call_id":"call_1","name":"lookup"}
            }))
            .unwrap();
        let error = reducer
            .push(serde_json::json!({
                "type":"response.function_call_arguments.done",
                "call_id":"call_1",
                "arguments":"x".repeat(MAX_TOOL_ARGUMENT_BYTES + 1)
            }))
            .unwrap_err();
        assert!(error.to_string().contains("size limit"));
    }

    #[test]
    fn grok_reducer_accepts_arguments_snapshot_on_output_item_done() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"arguments\":\"{\\\"q\\\":1}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(events.iter().any(
            |event| matches!(event, ReducerEvent::ToolDelta(_, arguments) if arguments == "{\"q\":1}")
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ReducerEvent::ToolStop(_)))
        );
    }

    #[test]
    fn grok_reducer_associates_early_citation_with_following_text() {
        let input = b"data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://example.com\",\"title\":\"Example\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"result\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        let text_position = events
            .iter()
            .position(|event| matches!(event, ReducerEvent::TextDelta(0, _)))
            .unwrap();
        let citation_position = events
            .iter()
            .position(|event| matches!(event, ReducerEvent::Citation(0, _)))
            .unwrap();
        assert!(citation_position > text_position);
    }

    #[test]
    fn grok_reducer_accepts_reasoning_summary_part_completion() {
        let input = b"data: {\"type\":\"response.reasoning_summary_part.added\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.done\"}\n\ndata: {\"type\":\"response.reasoning_summary_part.done\",\"part\":{\"type\":\"summary_text\",\"text\":\"think\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(matches!(events.last(), Some(ReducerEvent::Finish { .. })));
    }

    #[test]
    fn grok_reducer_accepts_complete_observed_lifecycle() {
        let input = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.in_progress\"}\n\ndata: {\"type\":\"response.reasoning_summary_part.added\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.done\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.content_part.added\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.content_part.done\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.function_call_arguments.done\",\"call_id\":\"call_1\",\"arguments\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                input_tokens: 4,
                output_tokens: 2,
                ..
            })
        ));
    }

    #[test]
    fn grok_reducer_uses_done_snapshot_without_replaying_deltas() {
        let input = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer \"}\n\ndata: {\"type\":\"response.output_text.done\",\"text\":\"answer 😊\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer 😊\"}]}]}}\n\n";
        let events = reduce_upstream_bytes(input.as_bytes()).unwrap();
        let text = events
            .iter()
            .filter_map(|event| match event {
                ReducerEvent::TextDelta(_, text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert_eq!(text, "answer 😊");
    }

    #[test]
    fn grok_reducer_rejects_done_snapshot_that_conflicts_with_streamed_text() {
        for snapshot in ["hullo", ""] {
            let mut reducer = Reducer::default();
            reducer
                .push(serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"hello"
                }))
                .unwrap();
            let error = reducer
                .push(serde_json::json!({
                    "type":"response.output_text.done",
                    "text":snapshot
                }))
                .unwrap_err();
            assert!(error.to_string().contains("disagrees with streamed deltas"));
        }
    }

    #[test]
    fn grok_reducer_recovers_snapshot_only_response() {
        let input = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"snapshot only 😊\"}]}],\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input.as_bytes()).unwrap();

        assert!(events.iter().any(
            |event| matches!(event, ReducerEvent::TextDelta(0, text) if text == "snapshot only 😊")
        ));
    }

    #[test]
    fn incomplete_response_closes_text_and_preserves_reported_usage_details() {
        let input = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":5},\"total_tokens\":19}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();

        assert!(
            events
                .iter()
                .any(|event| matches!(event, ReducerEvent::TextStop(0)))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ReducerEvent::Terminal(GrokTerminal::IncompleteMaxOutputTokens)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ReducerEvent::Usage(GrokUsage {
                input_tokens: Some(12),
                output_tokens: Some(7),
                cached_input_tokens: Some(3),
                reasoning_tokens: Some(5),
                total_tokens: Some(19),
            })
        )));
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                stop_reason,
                input_tokens: 12,
                output_tokens: 7,
                ..
            }) if stop_reason == "max_tokens"
        ));
    }

    #[test]
    fn missing_usage_is_distinct_from_reported_zero() {
        let missing = reduce_upstream_bytes(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        )
        .unwrap();
        let zero = reduce_upstream_bytes(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
        )
        .unwrap();

        assert!(missing.iter().any(|event| matches!(
            event,
            ReducerEvent::Usage(usage) if usage.availability_state() == "unavailable"
        )));
        assert!(zero.iter().any(|event| matches!(
            event,
            ReducerEvent::Usage(usage) if usage.availability_state() == "reported"
        )));
    }

    #[test]
    fn completed_function_call_rejects_non_object_arguments() {
        for arguments in ["null", "[]", "\"text\""] {
            let input = format!(
                "data: {{\"type\":\"response.output_item.added\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}}}\n\ndata: {{\"type\":\"response.function_call_arguments.done\",\"call_id\":\"call_1\",\"arguments\":{arguments:?}}}\n\ndata: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\"}}}}\n\n"
            );
            let error = reduce_upstream_bytes(input.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("JSON object"), "{error:#}");
        }
    }

    #[test]
    fn upstream_failure_events_preserve_retry_semantics() {
        let cases = [
            (
                concat!(
                    r#"data: {"type":"response.failed","response":{"error":{"type":"overloaded_error","message":"capacity exhausted"},"retry_after":0.25,"usage":{"input_tokens":7}}}"#,
                    "\n\n"
                ),
                "response.failed",
                529,
                true,
                Some("0.25"),
                "capacity exhausted",
            ),
            (
                concat!(
                    r#"data: {"type":"error","code":"rate_limit_exceeded","message":"slow down","retry_after":"1"}"#,
                    "\n\n"
                ),
                "error",
                429,
                true,
                Some("1"),
                "slow down",
            ),
            (
                concat!(
                    r#"data: {"type":"response.error","response":{"error":{"status_code":400,"message":"invalid tool schema"}}}"#,
                    "\n\n"
                ),
                "response.error",
                400,
                false,
                None,
                "invalid tool schema",
            ),
            (
                concat!(
                    r#"data: {"type":"response.failed","response":{"error":{"message":"model failed"}}}"#,
                    "\n\n"
                ),
                "response.failed",
                502,
                false,
                None,
                "model failed",
            ),
        ];

        for (wire, event_type, status, retryable, retry_after, message) in cases {
            let error = reduce_upstream_bytes(wire.as_bytes()).unwrap_err();
            let failure = error
                .upstream_failure()
                .expect("failure event must remain structured");
            assert_eq!(failure.event_type, event_type);
            assert_eq!(failure.status, status);
            assert_eq!(failure.retryable, retryable);
            assert_eq!(failure.retry_after.as_deref(), retry_after);
            assert_eq!(failure.message, message);
        }
    }

    #[test]
    fn response_error_preserves_failure_usage() {
        let error = reduce_upstream_bytes(
            b"data: {\"type\":\"response.error\",\"response\":{\"error\":{\"code\":\"service_unavailable\",\"message\":\"try later\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
        )
        .unwrap_err();

        let usage = error.usage().expect("failure usage must survive reduction");
        assert_eq!(usage.input_tokens, Some(9));
        assert_eq!(usage.output_tokens, Some(1));
        assert_eq!(error.upstream_failure().unwrap().status, 503);
    }

    #[test]
    fn byte_and_decoded_value_reduction_are_equivalent() {
        let values = vec![
            serde_json::json!({
                "type":"response.output_text.delta",
                "delta":"answer"
            }),
            serde_json::json!({"type":"response.output_text.done"}),
            serde_json::json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":7,"output_tokens":2}}
            }),
        ];
        let wire = values
            .iter()
            .map(|value| format!("data: {value}\n\n"))
            .collect::<String>();

        let from_bytes = reduce_upstream_bytes(wire.as_bytes()).unwrap();
        let from_values =
            reduce_decoded_values_with_tool_policy(values, &ToolCallPolicy::permissive()).unwrap();

        assert_eq!(format!("{from_bytes:#?}"), format!("{from_values:#?}"));
    }

    #[test]
    fn decoded_value_reduction_remains_fail_closed_after_terminal() {
        let error = reduce_decoded_values_with_tool_policy(
            [
                serde_json::json!({
                    "type":"response.completed",
                    "response":{"usage":{}}
                }),
                serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"must not escape"
                }),
            ],
            &ToolCallPolicy::permissive(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("event after terminal"),
            "{error}"
        );
    }

    #[test]
    fn byte_reduction_keeps_usage_on_incomplete_decoder_tail() {
        let error = reduce_upstream_bytes(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\ndata: {",
        )
        .unwrap_err();

        assert!(error.to_string().contains("incomplete frame"), "{error}");
        let usage = error.usage().expect("decoded terminal usage must survive");
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(2));
    }
}
