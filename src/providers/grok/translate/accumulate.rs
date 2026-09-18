use std::collections::HashMap;

use super::reducer::{ReducerEvent, reduce_upstream_bytes};
use super::search_text::search_line;
use crate::config::GrokSearchBlocks;
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
    accumulate_response_with_options(
        upstream,
        message_id,
        model,
        traffic,
        crate::config::grok_search_blocks(),
    )
}

/// Tests choose the hosted-search block shape directly rather than through the
/// environment.
pub fn accumulate_response_with_options(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
    search_blocks: GrokSearchBlocks,
) -> anyhow::Result<Value> {
    let mut capture = traffic.map(TrafficCapture::stream_capture);
    let mut decoder = super::stream::SseDecoder::default();
    let events = match decoder.push(upstream) {
        Ok(events) => events,
        Err(error) => {
            if let Some(capture) = capture.as_mut() {
                capture.malformed("decoder", "malformed_sse");
            }
            finish_capture(capture.take(), traffic, "error");
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
        finish_capture(capture.take(), traffic, "error");
        return Err(error);
    }
    let mut blocks: Vec<Value> = Vec::new();
    let mut block_positions = HashMap::new();
    let mut stop = "end_turn".to_string();
    let mut input = 0;
    let mut output = 0;
    let mut web_search_requests = 0;
    let mut x_search_requests = 0;
    let reduced = match reduce_upstream_bytes(upstream) {
        Ok(events) => events,
        Err(error) => {
            if let Some(capture) = capture.as_mut() {
                capture.malformed("reducer", "invalid_event");
            }
            finish_capture(capture.take(), traffic, "error");
            return Err(error);
        }
    };
    for event in reduced {
        match event {
            ReducerEvent::ThinkingStart(index) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"thinking","thinking":"","signature":""}))
            }
            ReducerEvent::ThinkingDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    block["thinking"] = Value::String(format!(
                        "{}{}",
                        block["thinking"].as_str().unwrap_or(""),
                        text
                    ));
                }
            }
            ReducerEvent::TextStart(index) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"text","text":""}))
            }
            ReducerEvent::TextDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    block["text"] =
                        Value::String(format!("{}{}", block["text"].as_str().unwrap_or(""), text));
                }
            }
            ReducerEvent::ToolStart(index, id, name) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"tool_use","id":id,"name":name,"input":{}}))
            }
            ReducerEvent::ToolDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    let raw = format!(
                        "{}{}",
                        block.get("_args").and_then(Value::as_str).unwrap_or(""),
                        text
                    );
                    block["_args"] = Value::String(raw);
                }
            }
            ReducerEvent::ToolStop(index) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    let raw = block.get("_args").and_then(Value::as_str).unwrap_or("{}");
                    match serde_json::from_str(raw) {
                        Ok(input) => block["input"] = input,
                        Err(error) => {
                            if let Some(capture) = capture.as_mut() {
                                capture.malformed("reducer", "invalid_tool_arguments");
                            }
                            finish_capture(capture.take(), traffic, "error");
                            return Err(error.into());
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
                if search_blocks == GrokSearchBlocks::Text {
                    // One text block, not two. Blocks are collected into an
                    // array here and addressed through `block_positions`, so
                    // leaving `result_index` unmapped costs nothing. The
                    // streaming path emits an empty second block instead,
                    // because there the index goes on the wire.
                    block_positions.insert(index, blocks.len());
                    blocks
                        .push(serde_json::json!({"type":"text","text":search_line(&name,&query)}));
                    continue;
                }
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"server_tool_use","id":id,"name":name,"input":{"query":query}}));
                block_positions.insert(result_index, blocks.len());
                blocks.push(serde_json::json!({"type":format!("{name}_tool_result"),"tool_use_id":id,"content":[]}));
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
                        .or_insert_with(|| Value::Array(Vec::new()));
                    citations.as_array_mut().unwrap().push(serde_json::json!({
                        "type":"web_search_result_location",
                        "url":annotation.get("url").and_then(Value::as_str).unwrap_or_default(),
                        "title":annotation.get("title").and_then(Value::as_str).unwrap_or_default(),
                        "cited_text":annotation.get("text").and_then(Value::as_str).unwrap_or_default()
                    }));
                }
            }
            ReducerEvent::Finish {
                stop_reason,
                input_tokens,
                output_tokens,
                web_search_requests: web_requests,
                x_search_requests: x_requests,
            } => {
                stop = stop_reason;
                // The non-streaming response always carries an input count, so
                // a missing provider field falls back to zero here rather than
                // dropping the key. The streaming path preserves absence
                // instead, because it must not erase the seeded estimate.
                input = input_tokens.unwrap_or(0);
                output = output_tokens;
                web_search_requests = web_requests;
                x_search_requests = x_requests;
            }
            _ => {}
        }
    }
    let hosted_search_requests = web_search_requests + x_search_requests;
    let response = serde_json::json!({"id":message_id,"type":"message","role":"assistant","model":model,"content":blocks,"stop_reason":stop,"stop_sequence":null,"usage":{"input_tokens":input,"output_tokens":output,"server_tool_use":{"web_search_requests":hosted_search_requests,"x_search_requests":x_search_requests}}});
    if let Some(mut capture) = capture {
        capture.downstream_event("response", response.clone());
        finish_capture(Some(capture), traffic, "completed");
    }
    Ok(response)
}

fn finish_capture(
    capture: Option<crate::traffic::StreamTrafficCapture>,
    traffic: Option<&TrafficCapture>,
    outcome: &str,
) {
    if let (Some(capture), Some(traffic)) = (capture, traffic) {
        capture.finish(
            traffic,
            serde_json::json!({"kind":"non_streaming","outcome":outcome}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTED_WEB_SEARCH: &[u8] = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"action\":{\"query\":\"rust news\"}}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Result\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";

    #[test]
    fn accumulated_hosted_search_renders_as_text_by_default() {
        let response = accumulate_response_with_options(
            HOSTED_WEB_SEARCH,
            "message",
            "grok-4.5",
            None,
            GrokSearchBlocks::Text,
        )
        .unwrap();
        let types: Vec<&str> = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        // One text block represents the search and one contains the answer.
        assert_eq!(types, vec!["text", "text"]);
        assert_eq!(response["content"][0]["text"], "[web search: rust news]\n");
        assert_eq!(
            response["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
    }

    #[test]
    fn accumulated_hosted_search_keeps_the_native_shape_on_request() {
        let response = accumulate_response_with_options(
            HOSTED_WEB_SEARCH,
            "message",
            "grok-4.5",
            None,
            GrokSearchBlocks::Native,
        )
        .unwrap();
        let types: Vec<&str> = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec!["server_tool_use", "web_search_tool_result", "text"]
        );
    }

    #[test]
    fn accumulate_response_tracks_two_interleaved_tool_calls() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"first\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"second\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{\\\"value\\\":1}\"}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_2\",\"delta\":\"{\\\"value\\\":2}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(response["content"][0]["input"]["value"], 1);
        assert_eq!(response["content"][1]["input"]["value"], 2);
    }

    #[test]
    fn accumulate_response_ignores_data_less_frames() {
        let input = b": keepalive\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"complete\"}\n\nid: 42\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(response["content"][0]["text"], "complete");
        assert_eq!(response["stop_reason"], "end_turn");
    }
}
