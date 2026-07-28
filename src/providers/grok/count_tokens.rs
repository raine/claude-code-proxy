use super::translate::request::{
    GrokContentPart, GrokInputItem, GrokResponsesRequest, GrokToolOutput,
};

const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const TOOL_OVERHEAD_TOKENS: u64 = 4;

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
        GrokInputItem::Message { content, .. } => count_parts(content),
        GrokInputItem::FunctionCall {
            name, arguments, ..
        } => approx_token_count(name) + approx_token_count(arguments),
        GrokInputItem::FunctionCallOutput { output, .. } => match output {
            GrokToolOutput::Text(text) => approx_token_count(text),
            GrokToolOutput::Parts(parts) => count_parts(parts),
        },
    }
}

fn count_parts(parts: &[GrokContentPart]) -> u64 {
    parts
        .iter()
        .map(|part| match part {
            GrokContentPart::InputText { text } | GrokContentPart::OutputText { text } => {
                approx_token_count(text)
            }
            // Images count toward tokens; charge a flat per-image estimate
            // instead of counting base64 payload characters.
            GrokContentPart::InputImage { .. } => IMAGE_TOKEN_ESTIMATE,
        })
        .sum()
}

const IMAGE_TOKEN_ESTIMATE: u64 = 1_024;

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
    fn count_tokens_is_deterministic() {
        let request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "repeatable input"}]
        }));

        assert_eq!(count_tokens(&request), count_tokens(&request));
    }
}
