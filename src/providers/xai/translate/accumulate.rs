use serde_json::Value;

use crate::traffic::TrafficCapture;

use super::reducer::{
    AnthropicUsage, ReducerEvent, UpstreamStreamError, map_codex_usage_to_anthropic,
    reduce_upstream_bytes,
};
use super::web_search_compat::{WebSearchCompatContent, build_web_search_compat_blocks};

pub fn accumulate_response(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> Result<Value, anyhow::Error> {
    accumulate_response_with_traffic(upstream, message_id, model, None)
}

pub fn accumulate_response_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
) -> Result<Value, anyhow::Error> {
    let events = match reduce_upstream_bytes(upstream) {
        Ok(events) => events,
        Err(err) => {
            write_reducer_error_capture(traffic, &err);
            return Err(anyhow::anyhow!(
                "upstream error: {} ({:?})",
                err.message,
                err.kind
            ));
        }
    };

    let mut blocks: Vec<AccumulatedBlock> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut usage: Option<AnthropicUsage> = None;
    let mut web_search_events: Vec<ReducerEvent> = Vec::new();
    let mut deferred_text_parts: Vec<String> = Vec::new();

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

    struct AccumulatedBlock {
        index: usize,
        kind: BlockKind,
    }

    for event in &events {
        match event {
            ReducerEvent::WebSearch { .. } => {
                web_search_events.push(event.clone());
            }
            ReducerEvent::ThinkingStart { index } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Thinking {
                        text: String::new(),
                    },
                });
            }
            ReducerEvent::ThinkingDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Thinking { text: t } = &mut block.kind
                {
                    t.push_str(text);
                }
            }
            ReducerEvent::TextStart { index } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Text {
                        text: String::new(),
                    },
                });
            }
            ReducerEvent::TextDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Text { text: t } = &mut block.kind
                {
                    t.push_str(text);
                }
                deferred_text_parts.push(text.clone());
            }
            ReducerEvent::ToolStart { index, id, name } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        args: String::new(),
                    },
                });
            }
            ReducerEvent::ToolDelta {
                index,
                partial_json,
            } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Tool { args, .. } = &mut block.kind
                {
                    args.push_str(partial_json);
                }
            }
            ReducerEvent::Finish {
                stop_reason: sr,
                usage: u,
                web_search_requests,
                ..
            } => {
                stop_reason = Some(sr.to_string());
                let ws = Some(*web_search_requests).filter(|n| *n > 0);
                usage = Some(map_codex_usage_to_anthropic(u, ws));
            }
            _ => {}
        }
    }

    let text_from_deferred: String = deferred_text_parts.join("");

    let mut indexed_content: Vec<(usize, Value)> = Vec::new();

    if !web_search_events.is_empty() {
        let compat_blocks = build_web_search_compat_blocks(&web_search_events, &text_from_deferred);
        for block in &compat_blocks {
            match &block.content {
                WebSearchCompatContent::ServerToolUse { id, name, input } => {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "server_tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }),
                    ));
                }
                WebSearchCompatContent::WebSearchToolResult {
                    tool_use_id,
                    content: results,
                } => {
                    let result_content: Vec<Value> = results
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "type": "web_search_result",
                                "title": r.title,
                                "url": r.url,
                            })
                        })
                        .collect();
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "web_search_tool_result",
                            "tool_use_id": tool_use_id,
                            "content": result_content,
                        }),
                    ));
                }
            }
        }
    }

    for block in &blocks {
        match &block.kind {
            BlockKind::Thinking { text } => {
                if !text.is_empty() {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": "",
                        }),
                    ));
                }
            }
            BlockKind::Text { text } => {
                if !text.is_empty() {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "text",
                            "text": text,
                        }),
                    ));
                }
            }
            BlockKind::Tool { id, name, args } => {
                let parsed = serde_json::from_str::<Value>(args)
                    .unwrap_or_else(|_| Value::String(args.clone()));
                indexed_content.push((
                    block.index,
                    serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": parsed,
                    }),
                ));
            }
        }
    }

    indexed_content.sort_by_key(|(index, _)| *index);
    let content: Vec<Value> = indexed_content
        .into_iter()
        .map(|(_, content)| content)
        .collect();

    let response = serde_json::json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage.unwrap_or_default(),
    });

    Ok(response)
}

fn write_reducer_error_capture(traffic: Option<&TrafficCapture>, err: &UpstreamStreamError) {
    let Some(traffic) = traffic else {
        return;
    };
    traffic.write_json(
        "060-codex-reducer-error",
        &serde_json::json!({
            "kind": format!("{:?}", err.kind),
            "message": err.message,
            "retryAfterSeconds": err.retry_after_seconds,
            "diagnostics": err.diagnostics,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse_event(type_name: &str, payload: serde_json::Value) -> String {
        let mut obj = if let serde_json::Value::Object(m) = payload {
            m
        } else {
            return String::new();
        };
        obj.insert("type".into(), serde_json::json!(type_name));
        format!(
            "data: {}\n\n",
            serde_json::to_string(&serde_json::Value::Object(obj)).unwrap()
        )
    }

    #[test]
    fn accumulate_text_response() {
        let upstream = format!(
            "{}{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"Hello world"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}
                })
            ),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        assert_eq!(response["type"], "message");
        assert_eq!(response["content"][0]["type"], "text");
        assert_eq!(response["content"][0]["text"], "Hello world");
        assert_eq!(response["stop_reason"], "end_turn");
    }

    #[test]
    fn accumulate_tool_use_response() {
        let upstream = format!(
            "{}{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                })
            ),
            sse_event(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,"delta":"{\"file_path\":\"/tmp/a\"}"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"/tmp/a\"}"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":3}}
                })
            ),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        assert_eq!(response["content"][0]["type"], "tool_use");
        assert_eq!(response["content"][0]["input"]["file_path"], "/tmp/a");
    }

    #[test]
    fn accumulate_web_search_response() {
        let upstream = format!(
            "{}{}{}{}{}{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                })
            ),
            sse_event(
                "response.web_search_call.in_progress",
                json!({
                    "output_index":0,"item_id":"ws_1"
                })
            ),
            sse_event(
                "response.web_search_call.completed",
                json!({
                    "output_index":0,"item_id":"ws_1"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1","action":{"query":"test query"}}
                })
            ),
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                json!({
                    "output_index":1,"delta":"See [Result](https://result.com)"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":3,"output_tokens":1}}
                })
            ),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        let content = response["content"].as_array().unwrap();
        assert!(content.len() >= 3);
        // First should be server_tool_use
        assert_eq!(content[0]["type"], "server_tool_use");
        // Second should be web_search_tool_result
        assert_eq!(content[1]["type"], "web_search_tool_result");
        // Third should be text
        assert_eq!(content[2]["type"], "text");
    }

    #[test]
    fn accumulate_reasoning_summary_as_thinking_before_text() {
        let upstream = format!(
            "{}{}{}{}{}{}{}",
            sse_event(
                "response.reasoning_summary_text.delta",
                json!({
                    "output_index":0,"summary_index":0,"delta":"part one"
                })
            ),
            sse_event(
                "response.reasoning_summary_part.added",
                json!({
                    "output_index":0,
                    "summary_index":1,
                    "part":{"type":"summary_text","text":""}
                })
            ),
            sse_event(
                "response.reasoning_summary_text.delta",
                json!({
                    "output_index":0,"summary_index":1,"delta":"part two"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                json!({
                    "output_index":1,"delta":"answer"
                })
            ),
            format!(
                "{}{}",
                sse_event(
                    "response.output_item.done",
                    json!({
                        "output_index":1,"item":{"type":"message"}
                    })
                ),
                sse_event(
                    "response.completed",
                    json!({
                        "response":{"id":"resp_1","usage":{}}
                    })
                )
            ),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        let content = response["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "part one\n\npart two");
        assert_eq!(content[0]["signature"], "");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "answer");
    }

    #[test]
    fn accumulate_empty_reasoning_summary_emits_no_thinking() {
        let upstream = format!(
            "{}{}{}{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                json!({
                    "output_index":1,"delta":"answer"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        let content = response["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "answer");
    }

    #[test]
    fn accumulate_handles_upstream_error() {
        let upstream = sse_event("error", json!({"error":{"message":"upstream failure"}}));
        let result = accumulate_response(upstream.as_bytes(), "msg_e", "model");
        assert!(result.is_err());
    }
}
