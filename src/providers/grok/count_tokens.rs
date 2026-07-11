use super::translate::request::{
    ResponsesContentPart, ResponsesInputItem, ResponsesRequest, ResponsesTool,
};

/// Approximate token counter for Codex translated requests.
/// Uses a simple monotonic estimator that satisfies Claude Code's
/// compaction logic (needs approximate, not exact counts).
pub fn count_translated_tokens(translated: &ResponsesRequest) -> u64 {
    let mut total = 0u64;

    // Instructions
    if let Some(ref instructions) = translated.instructions {
        total += approx_token_count(instructions);
    }

    // Input items
    for item in &translated.input {
        total += count_input_item_tokens(item);
    }

    // Tools
    if let Some(ref tools) = translated.tools {
        total += count_tool_tokens(tools);
    }

    // Overhead
    total += translated.input.len() as u64 * 4;
    total += translated.tools.as_ref().map_or(0, |t| t.len() as u64 * 4);

    // Model name
    total += approx_token_count(&translated.model);

    total.max(1)
}

fn count_input_item_tokens(item: &ResponsesInputItem) -> u64 {
    match item {
        ResponsesInputItem::AdditionalTools { tools, .. } => tools
            .iter()
            .map(|tool| approx_token_count(&serde_json::to_string(tool).unwrap_or_default()))
            .sum(),
        ResponsesInputItem::Message { content, .. } => {
            let mut total = 0u64;
            for part in content {
                total += count_content_part_tokens(part);
            }
            total
        }
        ResponsesInputItem::FunctionCall {
            name, arguments, ..
        } => approx_token_count(name) + approx_token_count(arguments),
        ResponsesInputItem::FunctionCallOutput { output, .. } => approx_token_count(output),
    }
}

fn count_content_part_tokens(part: &ResponsesContentPart) -> u64 {
    match part {
        ResponsesContentPart::InputText { text } => approx_token_count(text),
        ResponsesContentPart::OutputText { text } => approx_token_count(text),
        ResponsesContentPart::InputImage { .. } => 2000, // Image token estimate
    }
}

fn count_tool_tokens(tools: &[ResponsesTool]) -> u64 {
    let mut total = 0u64;
    for tool in tools {
        match tool {
            ResponsesTool::Function(f) => {
                total += approx_token_count(&f.name);
                if let Some(ref desc) = f.description {
                    total += approx_token_count(desc);
                }
                total +=
                    approx_token_count(&serde_json::to_string(&f.parameters).unwrap_or_default());
            }
            ResponsesTool::WebSearch(_) => {
                total += 10; // fixed overhead for web search tool
            }
        }
    }
    total
}

fn approx_token_count(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut count = 0u64;
    let mut in_word = false;

    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            if !in_word {
                count += 1;
                in_word = true;
            }
        } else {
            in_word = false;
            if !ch.is_whitespace() {
                count += 1;
            }
        }
    }

    count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_simple_request() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        let count = count_translated_tokens(&req);
        assert!(count > 0);
    }

    #[test]
    fn count_request_with_tools() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "use tool"}]}],
            "tools": [{"type": "function", "name": "search", "parameters": {"type": "object"}}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        let count = count_translated_tokens(&req);
        assert!(count > 0);
    }

    #[test]
    fn count_is_monotonic() {
        let short: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        let long: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "this is a much longer message with many words in it"}]}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        assert!(count_translated_tokens(&long) >= count_translated_tokens(&short));
    }
}
