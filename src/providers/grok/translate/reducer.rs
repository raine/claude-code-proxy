use std::collections::HashMap;

use serde_json::Value;

use super::stream::SseDecoder;

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_INCOMPLETE_TOOL_CALLS: usize = 128;

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
    Finish {
        stop_reason: String,
        // `None` when the provider omitted, emptied, or malformed the input
        // usage field. A present zero is a real provider-reported value.
        input_tokens: Option<u64>,
        output_tokens: u64,
        web_search_requests: u64,
        x_search_requests: u64,
    },
}

#[derive(Default)]
pub struct Reducer {
    next_index: usize,
    active: Option<(String, usize)>,
    calls: HashMap<String, (usize, String)>,
    item_calls: HashMap<String, String>,
    tool_args: HashMap<String, String>,
    completed_arguments: HashMap<String, bool>,
    hosted_calls: HashMap<String, (String, String)>,
    web_search_requests: u64,
    x_search_requests: u64,
    saw_tool: bool,
    completed: bool,
}

impl Reducer {
    pub fn push(&mut self, value: Value) -> anyhow::Result<Vec<ReducerEvent>> {
        if self.completed {
            anyhow::bail!("event after terminal completion");
        }
        let typ = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("event lacks type"))?;
        match typ {
            "response.created" | "response.in_progress" | "response.doom_loop_check" => Ok(vec![]),
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
                let (_, input) = self
                    .hosted_calls
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("custom tool delta is out of order"))?;
                if input.len().saturating_add(delta.len()) > MAX_TOOL_ARGUMENT_BYTES {
                    anyhow::bail!("custom tool input exceeds the size limit");
                }
                input.push_str(delta);
                Ok(vec![])
            }
            "response.custom_tool_call_input.done"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => Ok(vec![]),
            "response.output_text.annotation.added" => {
                let Some((kind, index)) = self.active.as_ref() else {
                    return Ok(vec![]);
                };
                let Some(annotation) = value.get("annotation") else {
                    return Ok(vec![]);
                };
                if kind == "text"
                    && annotation.get("type").and_then(Value::as_str) == Some("url_citation")
                {
                    Ok(vec![ReducerEvent::Citation(*index, annotation.clone())])
                } else {
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
                if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
                    let id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("custom tool call id is invalid"))?;
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .unwrap_or("x_search");
                    let name = if name.starts_with("x_") {
                        "x_search"
                    } else {
                        name
                    };
                    self.hosted_calls
                        .insert(id.into(), (name.into(), String::new()));
                    Ok(vec![])
                } else if item.get("type").and_then(Value::as_str) == Some("function_call") {
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
                    if self.calls.contains_key(id) {
                        anyhow::bail!("duplicate function call id");
                    }
                    if self.calls.len() >= MAX_INCOMPLETE_TOOL_CALLS {
                        anyhow::bail!("too many incomplete function calls");
                    }
                    let mut out = self.close_active()?;
                    let index = self.next_index;
                    self.next_index += 1;
                    self.calls.insert(id.into(), (index, name.into()));
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.item_calls.insert(item_id.into(), id.into());
                    }
                    self.tool_args.insert(id.into(), String::new());
                    self.completed_arguments.insert(id.into(), false);
                    self.saw_tool = true;
                    out.push(ReducerEvent::ToolStart(index, id.into(), name.into()));
                    Ok(out)
                } else {
                    Ok(vec![])
                }
            }
            "response.function_call_arguments.delta" => {
                let id = value
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        value
                            .get("item_id")
                            .and_then(Value::as_str)
                            .and_then(|item_id| self.item_calls.get(item_id).map(String::as_str))
                    })
                    .ok_or_else(|| anyhow::anyhow!("function delta lacks call id"))?;
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("function delta is invalid"))?;
                let (index, _) = self
                    .calls
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("function delta is out of order"))?
                    .clone();
                let args = self
                    .tool_args
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("function delta is out of order"))?;
                if args.len().saturating_add(delta.len()) > MAX_TOOL_ARGUMENT_BYTES {
                    anyhow::bail!("function arguments exceed the size limit");
                }
                args.push_str(delta);
                Ok(vec![ReducerEvent::ToolDelta(index, delta.into())])
            }
            "response.function_call_arguments.done" => {
                let id = value
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        value
                            .get("item_id")
                            .and_then(Value::as_str)
                            .and_then(|item_id| self.item_calls.get(item_id).map(String::as_str))
                    })
                    .ok_or_else(|| anyhow::anyhow!("function completion lacks call id"))?;
                let args = value.get("arguments").and_then(Value::as_str);
                let accumulated = self
                    .tool_args
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("function completion is out of order"))?;
                let index = self
                    .calls
                    .get(id)
                    .map(|(index, _)| *index)
                    .ok_or_else(|| anyhow::anyhow!("function completion is out of order"))?;
                let output = match args {
                    Some(args) if accumulated.is_empty() && !args.is_empty() => {
                        self.tool_args.get_mut(id).unwrap().push_str(args);
                        vec![ReducerEvent::ToolDelta(index, args.into())]
                    }
                    Some(args) if args != accumulated => {
                        anyhow::bail!("function completion arguments disagree with deltas")
                    }
                    _ => vec![],
                };
                self.completed_arguments.insert(id.into(), true);
                Ok(output)
            }
            "response.output_text.done" => self.close_kind("text"),
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                self.close_kind("thinking")
            }
            "response.content_part.done" => Ok(vec![]),
            "response.output_item.done" => {
                let item = value
                    .get("item")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("completed output item is invalid"))?;
                match item.get("type").and_then(Value::as_str) {
                    Some("web_search_call") => {
                        let id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("completed web search lacks id"))?;
                        let query = item
                            .get("action")
                            .and_then(|action| action.get("query"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let mut out = self.close_active()?;
                        let index = self.next_index;
                        let result_index = index + 1;
                        self.next_index += 2;
                        self.web_search_requests += 1;
                        out.push(ReducerEvent::HostedSearch {
                            index,
                            result_index,
                            id: format!("srvtoolu_{id}"),
                            name: "web_search".into(),
                            query: query.into(),
                        });
                        Ok(out)
                    }
                    Some("custom_tool_call") => {
                        let id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("completed custom tool lacks id"))?;
                        let (name, input) = self.hosted_calls.remove(id).unwrap_or_else(|| {
                            (
                                item.get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("x_search")
                                    .into(),
                                String::new(),
                            )
                        });
                        if name != "x_search" {
                            return Ok(vec![]);
                        }
                        let query = serde_json::from_str::<Value>(&input)
                            .ok()
                            .and_then(|input| {
                                input
                                    .get("query")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .unwrap_or_default();
                        let mut out = self.close_active()?;
                        let index = self.next_index;
                        let result_index = index + 1;
                        self.next_index += 2;
                        self.x_search_requests += 1;
                        out.push(ReducerEvent::HostedSearch {
                            index,
                            result_index,
                            id: format!("srvtoolu_{id}"),
                            name,
                            query,
                        });
                        Ok(out)
                    }
                    Some("function_call") => {
                        let id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                            anyhow::anyhow!("completed function call lacks call id")
                        })?;
                        let (index, _) = self
                            .calls
                            .remove(id)
                            .ok_or_else(|| anyhow::anyhow!("completed function call is unknown"))?;
                        let args = self.tool_args.remove(id).unwrap_or_default();
                        self.completed_arguments.remove(id);
                        serde_json::from_str::<Value>(&args)
                            .map_err(|_| anyhow::anyhow!("function arguments are incomplete"))?;
                        Ok(vec![ReducerEvent::ToolStop(index)])
                    }
                    _ => Ok(vec![]),
                }
            }
            "response.completed" | "response.incomplete" => {
                if !self.calls.is_empty() {
                    anyhow::bail!("function call is incomplete");
                }
                let mut out = self.close_active()?;
                let response = value.get("response").unwrap_or(&value);
                let usage = response.get("usage").unwrap_or(&Value::Null);
                let input = usage.get("input_tokens").and_then(Value::as_u64);
                let output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let stop = if typ == "response.incomplete" {
                    "max_tokens"
                } else if self.saw_tool {
                    "tool_use"
                } else {
                    "end_turn"
                };
                self.completed = true;
                out.push(ReducerEvent::Finish {
                    stop_reason: stop.into(),
                    input_tokens: input,
                    output_tokens: output,
                    web_search_requests: self.web_search_requests,
                    x_search_requests: self.x_search_requests,
                });
                Ok(out)
            }
            "error" | "response.failed" => anyhow::bail!("upstream Grok stream failed"),
            _ => anyhow::bail!("unsupported Grok stream event: {typ}"),
        }
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
            self.next_index += 1;
            self.active = Some((kind.into(), index));
            out.push(if kind == "thinking" {
                ReducerEvent::ThinkingStart(index)
            } else {
                ReducerEvent::TextStart(index)
            });
        }
        let index = self.active.as_ref().unwrap().1;
        out.push(if kind == "thinking" {
            ReducerEvent::ThinkingDelta(index, delta.into())
        } else {
            ReducerEvent::TextDelta(index, delta.into())
        });
        Ok(out)
    }
    fn close_active(&mut self) -> anyhow::Result<Vec<ReducerEvent>> {
        Ok(match self.active.take() {
            Some((kind, index)) if kind == "thinking" => vec![ReducerEvent::ThinkingStop(index)],
            Some((_, index)) => vec![ReducerEvent::TextStop(index)],
            None => vec![],
        })
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
}

pub fn reduce_upstream_bytes(bytes: &[u8]) -> anyhow::Result<Vec<ReducerEvent>> {
    let mut reducer = Reducer::default();
    let mut out = Vec::new();
    let mut decoder = SseDecoder::default();
    for event in decoder.push(bytes)? {
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| anyhow::anyhow!("malformed Grok SSE event"))?;
        out.extend(reducer.push(value)?);
    }
    decoder.finish()?;
    if !reducer.finished() {
        anyhow::bail!("Grok stream ended without completion");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grok_reducer_handles_text_tool_and_completion() {
        let input = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(
            matches!(events.last(), Some(ReducerEvent::Finish { stop_reason, .. }) if stop_reason == "tool_use")
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
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_keyword_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"search_1\",\"delta\":\"{\\\"query\\\":\\\"test\\\"}\"}\n\ndata: {\"type\":\"response.custom_tool_call_input.done\",\"item_id\":\"search_1\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_keyword_search\",\"id\":\"search_1\"}}\n\ndata: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"result\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ReducerEvent::HostedSearch { name, query, .. }
                if name == "x_search" && query == "test"
        )));
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                x_search_requests: 1,
                ..
            })
        ));
    }

    #[test]
    fn grok_reducer_accepts_reasoning_summary_part_completion() {
        let input = b"data: {\"type\":\"response.reasoning_summary_part.added\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.done\"}\n\ndata: {\"type\":\"response.reasoning_summary_part.done\",\"part\":{\"type\":\"summary_text\",\"text\":\"think\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(matches!(events.last(), Some(ReducerEvent::Finish { .. })));
    }

    #[test]
    fn grok_reducer_ignores_doom_loop_check_and_accepts_incomplete() {
        let input = b"data: {\"type\":\"response.doom_loop_check\",\"doom_loop_check\":{\"triggers\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2},\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(
            events.iter().any(
                |event| matches!(event, ReducerEvent::TextDelta(_, delta) if delta == "partial")
            )
        );
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                stop_reason,
                input_tokens: Some(4),
                output_tokens: 2,
                ..
            }) if stop_reason == "max_tokens"
        ));
    }

    #[test]
    fn grok_reducer_accepts_complete_observed_lifecycle() {
        let input = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.in_progress\"}\n\ndata: {\"type\":\"response.reasoning_summary_part.added\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.done\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.content_part.added\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.content_part.done\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.function_call_arguments.done\",\"call_id\":\"call_1\",\"arguments\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n";
        let events = reduce_upstream_bytes(input).unwrap();
        assert!(matches!(
            events.last(),
            Some(ReducerEvent::Finish {
                input_tokens: Some(4),
                output_tokens: 2,
                ..
            })
        ));
    }

    fn finish_input_tokens(response: &str) -> Option<u64> {
        let input =
            format!("data: {{\"type\":\"response.completed\",\"response\":{response}}}\n\n");
        let events = reduce_upstream_bytes(input.as_bytes()).unwrap();
        match events.last() {
            Some(ReducerEvent::Finish { input_tokens, .. }) => *input_tokens,
            other => panic!("expected a finish event, got {other:?}"),
        }
    }

    #[test]
    fn grok_reducer_preserves_input_usage_presence() {
        assert_eq!(
            finish_input_tokens("{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}"),
            Some(4)
        );
        // A provider-reported zero stays a value, not a missing field.
        assert_eq!(
            finish_input_tokens("{\"usage\":{\"input_tokens\":0,\"output_tokens\":2}}"),
            Some(0)
        );
        assert_eq!(finish_input_tokens("{\"usage\":{}}"), None);
        assert_eq!(
            finish_input_tokens("{\"usage\":{\"output_tokens\":2}}"),
            None
        );
        assert_eq!(
            finish_input_tokens("{\"usage\":{\"input_tokens\":\"4\"}}"),
            None
        );
        // No usage object at all is also absent, not zero.
        assert_eq!(finish_input_tokens("{}"), None);
    }
}
