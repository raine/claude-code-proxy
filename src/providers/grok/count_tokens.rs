use super::translate::request::{
    GrokContentPart, GrokFunctionCallOutput, GrokInputItem, GrokResponsesRequest,
};

const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const TOOL_OVERHEAD_TOKENS: u64 = 4;
// Rough per-image cost for token estimation; actual usage is billed by Grok.
const IMAGE_TOKENS: u64 = 1024;

pub fn count_tokens(request: &GrokResponsesRequest) -> u64 {
    let instructions = request
        .instructions
        .as_deref()
        .map(approx_token_count)
        .unwrap_or(0);
    let input: u64 = request.input.iter().map(count_input_item).sum();
    let tools: u64 = request
        .tools
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|tool| {
            tool.name.as_deref().map(approx_token_count).unwrap_or(0)
                + tool
                    .description
                    .as_deref()
                    .map(approx_token_count)
                    .unwrap_or(0)
                + approx_token_count(&serde_json::to_string(&tool.parameters).unwrap_or_default())
                + TOOL_OVERHEAD_TOKENS
        })
        .sum();

    (instructions
        + input
        + tools
        + request.input.len() as u64 * MESSAGE_OVERHEAD_TOKENS
        + approx_token_count(&request.model))
    .max(1)
}

fn count_input_item(item: &GrokInputItem) -> u64 {
    match item {
        GrokInputItem::Message { content, .. } => content.iter().map(count_content_part).sum(),
        GrokInputItem::FunctionCall {
            name, arguments, ..
        } => approx_token_count(name) + approx_token_count(arguments),
        GrokInputItem::FunctionCallOutput { output, .. } => match output {
            GrokFunctionCallOutput::Text(text) => approx_token_count(text),
            GrokFunctionCallOutput::Content(content) => {
                content.iter().map(count_content_part).sum()
            }
        },
    }
}

fn count_content_part(part: &GrokContentPart) -> u64 {
    match part {
        GrokContentPart::InputText { text } | GrokContentPart::OutputText { text } => {
            approx_token_count(text)
        }
        GrokContentPart::InputImage { .. } => IMAGE_TOKENS,
    }
}

fn approx_token_count(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut in_word = false;
    for character in text.chars() {
        if character.is_alphanumeric() || character == '-' || character == '_' {
            if !in_word {
                count += 1;
                in_word = true;
            }
        } else {
            in_word = false;
            if !character.is_whitespace() {
                count += 1;
            }
        }
    }
    count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::grok::translate::request::translate_request;
    use serde_json::json;

    fn translated_request(value: serde_json::Value) -> GrokResponsesRequest {
        let request: MessagesRequest = serde_json::from_value(value).unwrap();
        translate_request(&request, "grok-4.5".into()).unwrap()
    }

    #[test]
    fn count_tokens_returns_a_positive_count() {
        let request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert!(count_tokens(&request) > 0);
    }

    #[test]
    fn count_tokens_increases_for_more_input() {
        let short = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "hello"}]
        }));
        let long = translated_request(json!({
            "model": "grok-4.5",
            "system": "Follow all instructions carefully.",
            "messages": [{"role": "user", "content": "hello, please explain this request in detail"}],
            "tools": [{"name": "lookup", "description": "Look up a record", "input_schema": {"type": "object"}}]
        }));

        assert!(count_tokens(&long) > count_tokens(&short));
    }

    #[test]
    fn count_tokens_includes_tool_result_images() {
        let request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"screenshot","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
                ]}]}
            ]
        }));

        assert!(count_tokens(&request) >= IMAGE_TOKENS);
    }

    #[test]
    fn count_tokens_is_deterministic() {
        let request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "repeatable input"}]
        }));

        assert_eq!(count_tokens(&request), count_tokens(&request));
    }
}
