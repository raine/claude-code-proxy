use serde_json::Value;

use crate::traffic::TrafficCapture;

use super::reducer::{
    AnthropicUsage, FinishMetadata, ReducerEvent, STOP_END_TURN, UpstreamStreamError,
    map_codex_usage_to_anthropic, reduce_upstream_bytes_with_tool_policy,
};
use super::schema_bridge::SchemaBridge;
use super::tool_policy::ToolCallPolicy;
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
    accumulate_response_with_traffic_and_schema(upstream, message_id, model, traffic, None)
}

pub(crate) fn accumulate_response_with_traffic_and_schema(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
    schema_bridge: Option<&SchemaBridge>,
) -> Result<Value, anyhow::Error> {
    accumulate_response_with_traffic_schema_and_tool_policy(
        upstream,
        message_id,
        model,
        traffic,
        schema_bridge,
        &ToolCallPolicy::permissive(),
    )
}

pub(crate) fn accumulate_response_with_traffic_schema_and_tool_policy(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
    schema_bridge: Option<&SchemaBridge>,
    tool_policy: &ToolCallPolicy,
) -> Result<Value, anyhow::Error> {
    accumulate_response_with_traffic_schema_tool_policy_and_metadata(
        upstream,
        message_id,
        model,
        traffic,
        schema_bridge,
        tool_policy,
    )
    .map(|(response, _)| response)
}

pub(crate) fn accumulate_response_with_traffic_schema_tool_policy_and_metadata(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
    schema_bridge: Option<&SchemaBridge>,
    tool_policy: &ToolCallPolicy,
) -> Result<(Value, Option<FinishMetadata>), anyhow::Error> {
    let mut events = match reduce_upstream_bytes_with_tool_policy(upstream, tool_policy) {
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
    let mut finish_metadata: Option<FinishMetadata> = None;
    let mut web_search_events: Vec<ReducerEvent> = Vec::new();
    let mut deferred_text_parts: Vec<String> = Vec::new();

    enum BlockKind {
        Thinking {
            text: String,
            signature: String,
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

    for event in &mut events {
        match event {
            ReducerEvent::WebSearch { .. } => {
                web_search_events.push(event.clone());
            }
            ReducerEvent::ThinkingStart { index } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                });
            }
            ReducerEvent::ThinkingDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Thinking { text: t, .. } = &mut block.kind
                {
                    t.push_str(text);
                }
            }
            ReducerEvent::ThinkingSignature { index, signature } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Thinking { signature: s, .. } = &mut block.kind
                {
                    *s = signature.clone();
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
                continuation_eligible,
                usage: u,
                web_search_requests,
                response_id,
                output_items,
                ..
            } => {
                stop_reason = Some(sr.to_string());
                let ws = Some(*web_search_requests).filter(|n| *n > 0);
                usage = Some(map_codex_usage_to_anthropic(u, ws));
                finish_metadata = Some(FinishMetadata {
                    continuation_eligible: *continuation_eligible,
                    response_id: response_id.take(),
                    output_items: std::mem::take(output_items),
                });
            }
            _ => {}
        }
    }

    if let Some(bridge) = schema_bridge
        && stop_reason.as_deref() == Some(STOP_END_TURN)
    {
        let text_block_indices = blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                matches!(&block.kind, BlockKind::Text { .. }).then_some(index)
            })
            .collect::<Vec<_>>();
        if text_block_indices.len() != 1 {
            anyhow::bail!(
                "Codex structured output expected exactly one completed text block, found {}",
                text_block_indices.len()
            );
        }
        let block_index = text_block_indices[0];
        let BlockKind::Text { text } = &blocks[block_index].kind else {
            unreachable!("text block index was selected above")
        };
        let normalized_text = bridge
            .normalize_completed_text(text)
            .map_err(|error| anyhow::anyhow!("Codex structured output validation failed: {error}"))?
            .text
            .into_owned();
        if let BlockKind::Text { text } = &mut blocks[block_index].kind {
            *text = normalized_text.clone();
        }
        deferred_text_parts.clear();
        deferred_text_parts.push(normalized_text);
    }

    let text_from_deferred: String = deferred_text_parts.join("");

    let mut indexed_content: Vec<(usize, Value)> = Vec::new();

    if !web_search_events.is_empty() {
        let compat_blocks = build_web_search_compat_blocks(&web_search_events, &text_from_deferred);
        for block in &compat_blocks {
            match &block.content {
                WebSearchCompatContent::ServerToolUse {
                    id,
                    name,
                    input,
                    caller,
                } => {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "server_tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                            "caller": caller,
                        }),
                    ));
                }
                WebSearchCompatContent::WebSearchToolResult {
                    tool_use_id,
                    content: results,
                    caller,
                } => {
                    let result_content: Vec<Value> = results
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "type": "web_search_result",
                                "title": r.title,
                                "url": r.url,
                                "encrypted_content": "",
                                "page_age": null,
                            })
                        })
                        .collect();
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "web_search_tool_result",
                            "tool_use_id": tool_use_id,
                            "content": result_content,
                            "caller": caller,
                        }),
                    ));
                }
            }
        }
    }

    for block in &blocks {
        match &block.kind {
            BlockKind::Thinking { text, signature } => {
                if !text.is_empty() || !signature.is_empty() {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": signature,
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
                            "citations": null,
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
                        "caller": {"type": "direct"},
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

    Ok((response, finish_metadata))
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
    fn buffered_usage_maps_reported_cache_write_tokens() {
        let upstream = sse_event(
            "response.completed",
            json!({
                "response":{
                    "id":"resp_cache_write",
                    "status":"completed",
                    "usage":{
                        "input_tokens":100,
                        "input_tokens_details":{
                            "cached_tokens":20,
                            "cache_write_tokens":30
                        },
                        "output_tokens":7
                    }
                }
            }),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.6-sol").unwrap();

        assert_eq!(response["usage"]["input_tokens"], 50);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 20);
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 30);
        assert_eq!(response["usage"]["output_tokens"], 7);
    }

    fn optional_reason_schema_bridge() -> SchemaBridge {
        SchemaBridge::build(&json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"},
                "reason": {"type": "string"}
            },
            "required": ["ok"],
            "additionalProperties": false
        }))
        .unwrap()
    }

    fn completed_text_response(text: &str) -> String {
        format!(
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
                json!({"output_index":0,"delta":text})
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}
                })
            ),
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
                    "output_index":0,"item":{"type":"message","id":"msg_up"}
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
    fn accumulator_returns_finish_metadata_from_the_same_reduction() {
        let upstream = completed_text_response("Hello world");
        let (response, finish) = accumulate_response_with_traffic_schema_tool_policy_and_metadata(
            upstream.as_bytes(),
            "msg_1",
            "gpt-5.6-sol",
            None,
            None,
            &ToolCallPolicy::permissive(),
        )
        .unwrap();

        assert_eq!(response["content"][0]["text"], "Hello world");
        let finish = finish.expect("completed response should carry finish metadata");
        assert!(finish.continuation_eligible);
        assert_eq!(finish.response_id.as_deref(), Some("resp_1"));
        assert_eq!(finish.output_items.len(), 1);
    }

    #[test]
    fn structured_response_elides_only_synthetic_optional_null() {
        let upstream = completed_text_response(r#"{"ok":true,"reason":null}"#);
        let bridge = optional_reason_schema_bridge();
        let response = accumulate_response_with_traffic_and_schema(
            upstream.as_bytes(),
            "msg_1",
            "gpt-5.6-sol",
            None,
            Some(&bridge),
        )
        .unwrap();

        assert_eq!(response["content"][0]["text"], r#"{"ok":true}"#);
        assert_eq!(response["stop_reason"], STOP_END_TURN);
    }

    #[test]
    fn structured_response_fails_closed_when_output_is_invalid() {
        let upstream = completed_text_response(r#"{"ok":"wrong","reason":null}"#);
        let bridge = optional_reason_schema_bridge();
        let error = accumulate_response_with_traffic_and_schema(
            upstream.as_bytes(),
            "msg_1",
            "gpt-5.6-sol",
            None,
            Some(&bridge),
        )
        .expect_err("invalid structured output must not become a successful response");

        assert!(
            error
                .to_string()
                .contains("structured output validation failed")
        );
    }

    #[test]
    fn accumulate_rejects_malformed_tool_arguments() {
        let upstream = format!(
            "{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"function_call","call_id":"call_1","name":"Bash"}
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index": 0,
                    "item": {
                        "type":"function_call",
                        "call_id":"call_1",
                        "name":"Bash",
                        "arguments":"{\"command\":"
                    }
                })
            ),
            sse_event(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            ),
        );

        let error = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.6-sol")
            .expect_err("non-stream responses must reject malformed tool arguments");
        assert!(error.to_string().contains("invalid JSON arguments"));
    }

    #[test]
    fn accumulate_completed_response_ignores_trailing_rate_limit_telemetry() {
        let upstream = format!(
            "{}{}{}{}{}",
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
                    "output_index":0,"delta":"completed answer"
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}
                })
            ),
            sse_event(
                "codex.rate_limits",
                json!({
                    "rate_limits":{
                        "limit_reached":true,
                        "primary":{"reset_after_seconds":60}
                    }
                })
            ),
        );

        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.6-sol")
            .expect("terminal response must remain successful after quota telemetry");

        assert_eq!(response["content"][0]["text"], "completed answer");
        assert_eq!(response["stop_reason"], "end_turn");
        assert_eq!(response["usage"]["input_tokens"], 5);
        assert_eq!(response["usage"]["output_tokens"], 2);
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
        assert_eq!(response["content"][0]["caller"]["type"], "direct");
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
                    "item":{"type":"web_search_call","id":"ws_1","action":{
                        "query":"test query",
                        "sources":[{"title":"Bound source","url":"https://bound.example"}]
                    }}
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
                    "output_index":1,"item":{"type":"message","id":"msg_up"}
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
        assert_eq!(content[0]["caller"]["type"], "direct");
        // Second should be web_search_tool_result
        assert_eq!(content[1]["type"], "web_search_tool_result");
        assert_eq!(content[1]["caller"]["type"], "direct");
        assert_eq!(content[1]["content"][0]["url"], "https://bound.example");
        assert_eq!(content[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(content[1]["content"][0]["encrypted_content"], "");
        assert!(content[1]["content"][0]["page_age"].is_null());
        // Third should be text
        assert_eq!(content[2]["type"], "text");
        assert!(content[2]["citations"].is_null());
    }

    #[test]
    fn accumulate_reasoning_summary_as_thinking_before_text() {
        let upstream = format!(
            "{}{}{}{}{}{}{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
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
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":1,"item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            )
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
                    "output_index":1,"item":{"type":"message","id":"msg_up"}
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

    #[test]
    fn accumulate_preserves_signature_without_visible_summary() {
        let upstream = format!(
            "{}{}{}",
            sse_event(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
                })
            ),
            sse_event(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1"}
                })
            ),
            sse_event(
                "response.completed",
                json!({"response":{"id":"resp_1","usage":{}}})
            ),
        );
        let response = accumulate_response(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        let content = response["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "");
        assert!(
            content[0]["signature"]
                .as_str()
                .unwrap()
                .starts_with("ccp:codex:v1:")
        );
    }
}
