use std::collections::HashMap;

use super::reducer::{
    GrokUsage, ReducerEvent, parse_function_arguments, reduce_upstream_bytes_with_tool_policy,
};
use super::stream::anthropic_usage;
use super::tool_policy::ToolCallPolicy;
use crate::traffic::TrafficCapture;
use serde_json::Value;

pub fn accumulate_response(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> anyhow::Result<Value> {
    accumulate_response_with_traffic(upstream, message_id, model, None)
}

pub fn accumulate_response_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
) -> anyhow::Result<Value> {
    accumulate_response_with_traffic_and_tool_policy(
        upstream,
        message_id,
        model,
        traffic,
        &ToolCallPolicy::permissive(),
    )
}

pub(crate) fn accumulate_response_with_traffic_and_tool_policy(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
    tool_policy: &ToolCallPolicy,
) -> anyhow::Result<Value> {
    let mut capture = traffic.map(TrafficCapture::stream_capture);
    let mut decoder = super::stream::SseDecoder::default();
    let events = match decoder.push(upstream) {
        Ok(events) => events,
        Err(error) => {
            if let Some(capture) = capture.as_mut() {
                capture.malformed("decoder", "malformed_sse");
            }
            finish_capture(capture.take(), traffic, "error", None);
            return Err(error);
        }
    };
    if let Some(capture) = capture.as_mut() {
        for event in events {
            match serde_json::from_str::<Value>(&event.data) {
                Ok(value) => capture.upstream_event(event.event.as_deref(), &value),
                Err(_) => capture.malformed("json", "malformed_event"),
            }
        }
    }
    if let Err(error) = decoder.finish() {
        if let Some(capture) = capture.as_mut() {
            capture.malformed("decoder", "incomplete_stream");
        }
        finish_capture(capture.take(), traffic, "error", None);
        return Err(error);
    }
    let mut blocks: Vec<Value> = Vec::new();
    let mut block_positions = HashMap::new();
    let mut stop = "end_turn".to_string();
    let mut input = 0;
    let mut output = 0;
    let mut reported_usage: Option<GrokUsage> = None;
    let mut web_search_requests = 0;
    let reduced = match reduce_upstream_bytes_with_tool_policy(upstream, tool_policy) {
        Ok(events) => events,
        Err(error) => {
            let usage = error.usage().cloned();
            if let Some(capture) = capture.as_mut() {
                capture.malformed("reducer", "invalid_event");
            }
            finish_capture(capture.take(), traffic, "error", usage.as_ref());
            return Err(error.into());
        }
    };
    for event in reduced {
        match event {
            ReducerEvent::ThinkingStart(_)
            | ReducerEvent::ThinkingDelta(_, _)
            | ReducerEvent::ThinkingStop(_) => {}
            ReducerEvent::TextStart(index) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"text","text":"","citations":null}))
            }
            ReducerEvent::TextDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    append_string_delta(&mut block["text"], text);
                }
            }
            ReducerEvent::ToolStart(index, id, name) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"tool_use","id":id,"name":name,"input":{},"caller":{"type":"direct"}}))
            }
            ReducerEvent::ToolDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    append_string_delta(&mut block["_args"], text);
                }
            }
            ReducerEvent::ToolStop(index) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    let raw = block.get("_args").and_then(Value::as_str).unwrap_or("{}");
                    match parse_function_arguments(raw) {
                        Ok(input) => block["input"] = input,
                        Err(error) => {
                            if let Some(capture) = capture.as_mut() {
                                capture.malformed("reducer", "invalid_tool_arguments");
                            }
                            finish_capture(
                                capture.take(),
                                traffic,
                                "error",
                                reported_usage.as_ref(),
                            );
                            return Err(error);
                        }
                    }
                    block.as_object_mut().unwrap().remove("_args");
                }
            }
            ReducerEvent::HostedSearch {
                index,
                result_index,
                id,
                name,
                query,
            } => {
                if name != "web_search" {
                    continue;
                }
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"server_tool_use","id":id,"name":name,"input":{"query":query},"caller":{"type":"direct"}}));
                block_positions.insert(result_index, blocks.len());
                blocks.push(serde_json::json!({"type":"web_search_tool_result","tool_use_id":id,"content":[],"caller":{"type":"direct"}}));
            }
            ReducerEvent::Citation(index, annotation) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    let citations = block
                        .as_object_mut()
                        .unwrap()
                        .entry("citations")
                        .or_insert(Value::Null);
                    if citations.is_null() {
                        *citations = Value::Array(Vec::new());
                    }
                    citations.as_array_mut().unwrap().push(serde_json::json!({
                        "type":"web_search_result_location",
                        "url":annotation.get("url").and_then(Value::as_str).unwrap_or_default(),
                        "title":annotation.get("title").and_then(Value::as_str).unwrap_or_default(),
                        "cited_text":annotation.get("text").and_then(Value::as_str).unwrap_or_default(),
                        "encrypted_index":""
                    }));
                }
            }
            ReducerEvent::Usage(usage) => reported_usage = Some(usage),
            ReducerEvent::Finish {
                stop_reason,
                input_tokens,
                output_tokens,
                web_search_requests: web_requests,
                x_search_requests: _,
            } => {
                stop = stop_reason;
                input = input_tokens;
                output = output_tokens;
                web_search_requests = web_requests;
            }
            _ => {}
        }
    }
    let usage = anthropic_usage(
        reported_usage.as_ref().unwrap_or(&GrokUsage::default()),
        input,
        output,
        web_search_requests,
    );
    let response = serde_json::json!({"id":message_id,"type":"message","role":"assistant","model":model,"content":blocks,"stop_reason":stop,"stop_sequence":null,"usage":usage});
    if let Some(mut capture) = capture {
        capture.downstream_event("response", response.clone());
        finish_capture(Some(capture), traffic, "completed", reported_usage.as_ref());
    }
    Ok(response)
}

fn append_string_delta(value: &mut Value, delta: String) {
    match value {
        Value::String(value) => value.push_str(&delta),
        _ => *value = Value::String(delta),
    }
}

fn finish_capture(
    capture: Option<crate::traffic::StreamTrafficCapture>,
    traffic: Option<&TrafficCapture>,
    outcome: &str,
    usage: Option<&GrokUsage>,
) {
    if let (Some(capture), Some(traffic)) = (capture, traffic) {
        capture.finish(
            traffic,
            serde_json::json!({
                "kind":"non_streaming",
                "outcome":outcome,
                "usage":super::super::grok_usage_observability(usage, None),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_tool_policy(tool_choice: serde_json::Value) -> ToolCallPolicy {
        let request: crate::anthropic::schema::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model":"grok-4.5",
                "messages":[{"role":"user","content":"use tools"}],
                "tools":[{"name":"Read","input_schema":{"type":"object"}}],
                "tool_choice":tool_choice
            }))
            .unwrap();
        let translated = super::super::request::translate_request(&request, "grok-4.5".into())
            .expect("test request must translate");
        ToolCallPolicy::from_request(&translated)
    }

    #[test]
    fn accumulate_rejects_a_function_forbidden_by_the_request_policy() {
        let policy = response_tool_policy(serde_json::json!({"type":"none"}));
        let upstream = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"Read\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"arguments\":\"{}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n";
        let error = accumulate_response_with_traffic_and_tool_policy(
            upstream, "msg_1", "grok-4.5", None, &policy,
        )
        .unwrap_err();
        assert!(error.to_string().contains("tool_choice forbids"), "{error}");
    }

    fn streaming_final_usage(upstream: &[u8]) -> Value {
        let bytes =
            super::super::stream::translate_stream_bytes(upstream, "message", "grok-4.5").unwrap();
        let mut decoder = super::super::stream::SseDecoder::default();
        decoder
            .push(&bytes)
            .unwrap()
            .into_iter()
            .find_map(|event| {
                let value: Value = serde_json::from_str(&event.data).ok()?;
                if value.get("type").and_then(Value::as_str) == Some("message_delta") {
                    value.get("usage").cloned()
                } else {
                    None
                }
            })
            .expect("stream must contain final usage")
    }

    #[test]
    fn accumulate_response_tracks_two_interleaved_tool_calls() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"first\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"second\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{\\\"value\\\":1}\"}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_2\",\"delta\":\"{\\\"value\\\":2}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(response["content"][0]["input"]["value"], 1);
        assert_eq!(response["content"][1]["input"]["value"], 2);
        assert_eq!(
            response["content"][0]["caller"],
            serde_json::json!({"type":"direct"})
        );
    }

    #[test]
    fn accumulate_response_omits_plaintext_reasoning() {
        let input = b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"draft \\ud83d\\ude0a\"}\n\ndata: {\"type\":\"response.reasoning_text.done\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(
            response["content"],
            serde_json::json!([{"type":"text","text":"answer","citations":null}])
        );
    }

    #[test]
    fn accumulate_response_splits_cached_input_usage_like_streaming() {
        let input = b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens\":7}}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(response["usage"]["input_tokens"], 9);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 3);
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 0);
        assert_eq!(response["usage"]["output_tokens"], 7);
    }

    #[test]
    fn streaming_and_non_streaming_degrade_x_search_to_standard_schema() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"xs_1\"}}\n\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"xs_1\",\"delta\":\"{\\\"query\\\":\\\"rust\\\"}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"name\":\"x_search\",\"id\":\"xs_1\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Recent post\"}\n\ndata: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://x.com/example/status/1\",\"title\":\"Example\"}}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens\":7}}}\n\n";
        let non_streaming = accumulate_response(input, "message", "grok-4.5").unwrap();
        let streaming = streaming_final_usage(input);
        let expected = serde_json::json!({
            "input_tokens":9,
            "output_tokens":7,
            "cache_creation_input_tokens":0,
            "cache_read_input_tokens":3,
            "server_tool_use":{"web_search_requests":0}
        });

        assert_eq!(non_streaming["usage"], expected);
        assert_eq!(streaming, expected);
        assert_eq!(non_streaming["content"].as_array().unwrap().len(), 1);
        assert_eq!(non_streaming["content"][0]["type"], "text");
        assert_eq!(non_streaming["content"][0]["text"], "Recent post");
        assert_eq!(
            non_streaming["content"][0]["citations"][0]["encrypted_index"],
            ""
        );
        assert_eq!(non_streaming["stop_reason"], "end_turn");
        let encoded = non_streaming.to_string();
        assert!(!encoded.contains("x_search_tool_result"));
        assert!(!encoded.contains("\"name\":\"x_search\""));
        assert!(non_streaming["usage"].get("x_search_requests").is_none());
        assert!(streaming.get("x_search_requests").is_none());
    }

    #[test]
    fn missing_usage_is_zero_filled_consistently_downstream() {
        let input = b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
        let non_streaming = accumulate_response(input, "message", "grok-4.5").unwrap();
        let streaming = streaming_final_usage(input);
        let expected = serde_json::json!({
            "input_tokens":0,
            "output_tokens":0,
            "cache_creation_input_tokens":0,
            "cache_read_input_tokens":0,
            "server_tool_use":{"web_search_requests":0}
        });

        assert_eq!(non_streaming["usage"], expected);
        assert_eq!(streaming, expected);
    }

    #[test]
    fn accumulate_response_rejects_non_object_tool_arguments() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.function_call_arguments.done\",\"call_id\":\"call_1\",\"arguments\":\"[]\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\n";

        assert!(accumulate_response(input, "message", "grok-4.5").is_err());
    }

    #[test]
    fn accumulate_response_preserves_emoji_in_final_text() {
        let input = "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"draft 😊\"}\n\ndata: {\"type\":\"response.reasoning_text.done\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer 😊\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input.as_bytes(), "message", "grok-4.5").unwrap();

        assert_eq!(
            response["content"],
            serde_json::json!([{"type":"text","text":"answer 😊","citations":null}])
        );
    }

    #[test]
    fn accumulate_incomplete_after_completed_tool_uses_terminal_stop_reason() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"arguments\":\"{}\"}}\n\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"content_filter\"},\"usage\":{}}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();
        assert_eq!(response["content"][0]["type"], "tool_use");
        assert_eq!(response["stop_reason"], "refusal");
    }

    #[test]
    fn terminal_tail_policy_matches_streaming_translation() {
        let telemetry_tail = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\ndata: {\"type\":\"rate_limits.updated\",\"remaining\":42}\n\n";
        let non_streaming = accumulate_response(telemetry_tail, "message", "grok-4.5").unwrap();
        let streaming =
            super::super::stream::translate_stream_bytes(telemetry_tail, "message", "grok-4.5")
                .unwrap();
        assert_eq!(non_streaming["content"][0]["text"], "answer");
        assert_eq!(
            String::from_utf8(streaming)
                .unwrap()
                .matches("answer")
                .count(),
            1
        );

        let semantic_tail = b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"unsafe\"}\n\n";
        let non_streaming_error =
            accumulate_response(semantic_tail, "message", "grok-4.5").unwrap_err();
        let streaming_error =
            super::super::stream::translate_stream_bytes(semantic_tail, "message", "grok-4.5")
                .unwrap_err();
        for error in [non_streaming_error, streaming_error] {
            assert!(
                error.to_string().contains("event after terminal"),
                "{error:#}"
            );
        }
    }
}
