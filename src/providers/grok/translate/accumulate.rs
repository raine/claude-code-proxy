use std::collections::HashMap;

use super::reducer::{ReducerEvent, reduce_upstream_bytes};
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
            ReducerEvent::ThinkingSignature(index, signature) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    block["signature"] = Value::String(signature);
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
                input = input_tokens;
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

    #[test]
    fn accumulate_response_tracks_two_interleaved_tool_calls() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"first\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"second\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{\\\"value\\\":1}\"}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_2\",\"delta\":\"{\\\"value\\\":2}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(response["content"][0]["input"]["value"], 1);
        assert_eq!(response["content"][1]["input"]["value"], 2);
    }
}
