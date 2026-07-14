use std::collections::BTreeMap;

use crate::anthropic::sse::encode_sse_event;
use crate::traffic::TrafficCapture;

use super::reducer::{
    ReducerEvent, UpstreamStreamError, map_codex_usage_to_anthropic, reduce_upstream_bytes,
};
use super::web_search_compat::build_web_search_compat_blocks;

#[allow(dead_code)]
enum OpenBlock {
    Thinking,
    Text,
    Tool { id: String, name: String },
}

fn emit(
    out: &mut Vec<u8>,
    traffic: Option<&TrafficCapture>,
    event: &str,
    data: &serde_json::Value,
) {
    if let Some(traffic) = traffic {
        traffic.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": event,
                "data": data,
            }),
        );
    }
    out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
}

fn ensure_message_start(
    out: &mut Vec<u8>,
    traffic: Option<&TrafficCapture>,
    message_started: &mut bool,
    message_id: &str,
    model: &str,
) {
    if !*message_started {
        *message_started = true;
        let data = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                }
            }
        });
        emit(out, traffic, "message_start", &data);
    }
}

fn emit_content_event(
    out: &mut Vec<u8>,
    traffic: Option<&TrafficCapture>,
    message_started: &mut bool,
    open_blocks: &mut BTreeMap<usize, OpenBlock>,
    message_id: &str,
    model: &str,
    event: &ReducerEvent,
) -> bool {
    match event {
        ReducerEvent::ThinkingStart { index } => {
            ensure_message_start(out, traffic, message_started, message_id, model);
            open_blocks.insert(*index, OpenBlock::Thinking);
            emit(
                out,
                traffic,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                }),
            );
            true
        }
        ReducerEvent::ThinkingDelta { index, text } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "thinking_delta", "thinking": text}
                }),
            );
            true
        }
        ReducerEvent::ThinkingSignature { index, signature } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            );
            true
        }
        ReducerEvent::ThinkingStop { index } => {
            open_blocks.remove(index);
            emit(
                out,
                traffic,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            true
        }
        ReducerEvent::TextStart { index } => {
            ensure_message_start(out, traffic, message_started, message_id, model);
            open_blocks.insert(*index, OpenBlock::Text);
            emit(
                out,
                traffic,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
            true
        }
        ReducerEvent::TextDelta { index, text } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": text}
                }),
            );
            true
        }
        ReducerEvent::TextStop { index } => {
            open_blocks.remove(index);
            emit(
                out,
                traffic,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            true
        }
        ReducerEvent::ToolStart { index, id, name } => {
            ensure_message_start(out, traffic, message_started, message_id, model);
            open_blocks.insert(
                *index,
                OpenBlock::Tool {
                    id: id.clone(),
                    name: name.clone(),
                },
            );
            emit(
                out,
                traffic,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {}
                    }
                }),
            );
            true
        }
        ReducerEvent::ToolDelta {
            index,
            partial_json,
        } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": partial_json
                    }
                }),
            );
            true
        }
        ReducerEvent::ToolStop { index } => {
            open_blocks.remove(index);
            emit(
                out,
                traffic,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            true
        }
        _ => false,
    }
}

fn is_content_event(event: &ReducerEvent) -> bool {
    matches!(
        event,
        ReducerEvent::ThinkingStart { .. }
            | ReducerEvent::ThinkingDelta { .. }
            | ReducerEvent::ThinkingSignature { .. }
            | ReducerEvent::ThinkingStop { .. }
            | ReducerEvent::TextStart { .. }
            | ReducerEvent::TextDelta { .. }
            | ReducerEvent::TextStop { .. }
            | ReducerEvent::ToolStart { .. }
            | ReducerEvent::ToolDelta { .. }
            | ReducerEvent::ToolStop { .. }
    )
}

pub fn translate_stream_bytes(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    translate_stream_bytes_with_traffic(upstream, message_id, model, None)
}

pub fn translate_stream_bytes_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
) -> Result<Vec<u8>, anyhow::Error> {
    let events = match reduce_upstream_bytes(upstream) {
        Ok(events) => events,
        Err(err) => {
            write_reducer_error_capture(traffic, &err);
            return Err(anyhow::anyhow!(
                "upstream stream error: {} ({:?})",
                err.message,
                err.kind
            ));
        }
    };

    let mut out = Vec::new();
    let mut message_started = false;
    let mut open_blocks: BTreeMap<usize, OpenBlock> = BTreeMap::new();
    let mut web_search_events: Vec<ReducerEvent> = Vec::new();
    let mut deferred_content_events: Vec<ReducerEvent> = Vec::new();

    for event in &events {
        if matches!(event, ReducerEvent::WebSearch { .. }) {
            web_search_events.push(event.clone());
            continue;
        }
        if !web_search_events.is_empty() && is_content_event(event) {
            deferred_content_events.push(event.clone());
            continue;
        }

        if emit_content_event(
            &mut out,
            traffic,
            &mut message_started,
            &mut open_blocks,
            message_id,
            model,
            event,
        ) {
            continue;
        }

        match event {
            ReducerEvent::ToolProgress { .. } | ReducerEvent::Progress => {}
            ReducerEvent::Finish {
                stop_reason,
                usage,
                web_search_requests,
                ..
            } => {
                // Emit web search compat blocks
                if !web_search_events.is_empty() {
                    let text_from_deferred: String = deferred_content_events
                        .iter()
                        .filter_map(|e| match e {
                            ReducerEvent::TextDelta { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let compat_blocks =
                        build_web_search_compat_blocks(&web_search_events, &text_from_deferred);
                    for block in &compat_blocks {
                        use super::web_search_compat::WebSearchCompatContent;
                        match &block.content {
                            WebSearchCompatContent::ServerToolUse { id, name, input } => {
                                ensure_message_start(
                                    &mut out,
                                    traffic,
                                    &mut message_started,
                                    message_id,
                                    model,
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_start",
                                    &serde_json::json!({
                                        "type": "content_block_start",
                                        "index": block.index,
                                        "content_block": {
                                            "type": "server_tool_use",
                                            "id": id,
                                            "name": name,
                                            "input": {}
                                        }
                                    }),
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_delta",
                                    &serde_json::json!({
                                        "type": "content_block_delta",
                                        "index": block.index,
                                        "delta": {
                                            "type": "input_json_delta",
                                            "partial_json": serde_json::to_string(input).unwrap_or_default()
                                        }
                                    }),
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_stop",
                                    &serde_json::json!({
                                        "type": "content_block_stop",
                                        "index": block.index,
                                    }),
                                );
                            }
                            WebSearchCompatContent::WebSearchToolResult {
                                tool_use_id,
                                content: results,
                            } => {
                                let result_content: Vec<serde_json::Value> = results
                                    .iter()
                                    .map(|r| {
                                        serde_json::json!({
                                            "type": "web_search_result",
                                            "title": r.title,
                                            "url": r.url,
                                        })
                                    })
                                    .collect();
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_start",
                                    &serde_json::json!({
                                        "type": "content_block_start",
                                        "index": block.index,
                                        "content_block": {
                                            "type": "web_search_tool_result",
                                            "tool_use_id": tool_use_id,
                                            "content": result_content,
                                        }
                                    }),
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_stop",
                                    &serde_json::json!({
                                        "type": "content_block_stop",
                                        "index": block.index,
                                    }),
                                );
                            }
                        }
                    }
                }

                // Emit deferred content
                for deferred in &deferred_content_events {
                    emit_content_event(
                        &mut out,
                        traffic,
                        &mut message_started,
                        &mut open_blocks,
                        message_id,
                        model,
                        deferred,
                    );
                }

                ensure_message_start(&mut out, traffic, &mut message_started, message_id, model);

                let mapped = map_codex_usage_to_anthropic(usage, Some(*web_search_requests));
                emit(
                    &mut out,
                    traffic,
                    "message_delta",
                    &serde_json::json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": stop_reason,
                            "stop_sequence": null
                        },
                        "usage": mapped,
                    }),
                );
                emit(
                    &mut out,
                    traffic,
                    "message_stop",
                    &serde_json::json!({"type": "message_stop"}),
                );
            }
            _ => {}
        }
    }

    Ok(out)
}

fn write_reducer_error_capture(traffic: Option<&TrafficCapture>, err: &UpstreamStreamError) {
    let Some(traffic) = traffic else {
        return;
    };
    traffic.write_json(
        "060-codex-stream-reducer-error",
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
    fn stream_translates_text_response() {
        let upstream = format!(
            "{}{}{}{}",
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"item_1"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                serde_json::json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                serde_json::json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":1}}
                })
            ),
        );
        let out = String::from_utf8(
            translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap(),
        )
        .unwrap();
        assert!(out.contains("message_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("message_stop"));
    }

    #[test]
    fn stream_translates_web_search_response() {
        let upstream = format!(
            "{}{}{}{}{}{}{}{}",
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                })
            ),
            sse_event(
                "response.web_search_call.in_progress",
                serde_json::json!({
                    "output_index":0,"item_id":"ws_1"
                })
            ),
            sse_event(
                "response.web_search_call.completed",
                serde_json::json!({
                    "output_index":0,"item_id":"ws_1"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1","action":{"query":"test query"}}
                })
            ),
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                serde_json::json!({
                    "output_index":1,"delta":"See [Result](https://result.com)"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                serde_json::json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":3,"output_tokens":1}}
                })
            ),
        );
        let result = translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        let out = String::from_utf8(result).unwrap();
        assert!(out.contains("server_tool_use"), "missing server_tool_use");
        assert!(
            out.contains("web_search_tool_result"),
            "missing web_search_tool_result"
        );
    }

    #[test]
    fn stream_translates_reasoning_summary_to_thinking() {
        let upstream = format!(
            "{}{}{}{}{}{}",
            sse_event(
                "response.reasoning_summary_text.delta",
                serde_json::json!({
                    "output_index":0,"summary_index":0,"delta":"Working it out"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                serde_json::json!({
                    "output_index":1,"delta":"answer"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                serde_json::json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let out = String::from_utf8(
            translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap(),
        )
        .unwrap();
        assert!(out.contains("\"type\":\"thinking\""));
        assert!(out.contains("\"signature\":\"\""));
        assert!(out.contains("\"type\":\"thinking_delta\""));
        assert!(out.contains("\"thinking\":\"Working it out\""));
        assert!(out.contains("event: message_stop"));
        assert!(
            out.find("\"type\":\"thinking_delta\"").unwrap()
                < out.find("\"type\":\"text_delta\"").unwrap()
        );
    }

    #[test]
    fn stream_omits_empty_reasoning_summary() {
        let upstream = format!(
            "{}{}{}{}{}",
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"encrypted_content":"enc"}
                })
            ),
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                serde_json::json!({
                    "output_index":1,"delta":"answer"
                })
            ),
            format!(
                "{}{}",
                sse_event(
                    "response.output_item.done",
                    serde_json::json!({
                        "output_index":1,"item":{"type":"message"}
                    })
                ),
                sse_event(
                    "response.completed",
                    serde_json::json!({
                        "response":{"id":"resp_1","usage":{}}
                    })
                )
            ),
        );
        let out = String::from_utf8(
            translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap(),
        )
        .unwrap();
        assert!(!out.contains("\"type\":\"thinking\""));
        assert!(!out.contains("\"type\":\"thinking_delta\""));
        assert!(out.contains("event: message_stop"));
    }

    #[test]
    fn stream_emits_signature_delta_before_thinking_stop() {
        let upstream = format!(
            "{}{}{}{}",
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
                })
            ),
            sse_event(
                "response.reasoning_summary_text.delta",
                serde_json::json!({"output_index":0,"delta":"plan"})
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1"}
                })
            ),
            sse_event(
                "response.completed",
                serde_json::json!({"response":{"id":"resp_1","usage":{}}})
            ),
        );
        let out = String::from_utf8(
            translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap(),
        )
        .unwrap();
        let thinking_delta = out.find(r#""type":"thinking_delta""#).unwrap();
        let signature_delta = out.find(r#""type":"signature_delta""#).unwrap();
        let thinking_stop = out[signature_delta..]
            .find("event: content_block_stop")
            .map(|offset| signature_delta + offset)
            .unwrap();
        assert!(thinking_delta < signature_delta);
        assert!(signature_delta < thinking_stop);
        assert!(out.contains("ccp:codex:v1:"));
    }
}
