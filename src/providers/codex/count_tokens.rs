use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContent,
    ResponsesInputItem, ResponsesRequest, ResponsesTool,
};
use tiktoken_rs::o200k_base_singleton;

const TEXT_FORMAT_OVERHEAD_TOKENS: u64 = 4;

/// Local token counter for Codex translated requests using GPT-5's o200k_base
/// tokenizer plus fixed estimates for non-text model inputs.
pub fn count_translated_tokens(translated: &ResponsesRequest) -> u64 {
    let mut total = 0u64;

    // Instructions
    if let Some(ref instructions) = translated.instructions {
        total += token_count(instructions);
    }

    // Input items
    for item in &translated.input {
        total += count_input_item_tokens(item);
    }

    // Tools
    if let Some(ref tools) = translated.tools {
        total += count_tool_tokens(tools);
    }

    // Structured-output formats are injected into the model's prompt upstream.
    // Count the translated wire representation so schema normalization is
    // reflected in the same estimate Claude Code uses for context admission.
    if let Some(ref format) = translated.text.format {
        total += token_count(&serde_json::to_string(format).unwrap_or_default());
        total += TEXT_FORMAT_OVERHEAD_TOKENS;
    }

    // Overhead
    total += translated.input.len() as u64 * 4;
    total += translated.tools.as_ref().map_or(0, |t| t.len() as u64 * 4);

    // Model name
    total += token_count(&translated.model);

    total.max(1)
}

fn count_input_item_tokens(item: &ResponsesInputItem) -> u64 {
    match item {
        ResponsesInputItem::AdditionalTools { tools, .. } => tools
            .iter()
            .map(|tool| token_count(&serde_json::to_string(tool).unwrap_or_default()))
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
        } => token_count(name) + token_count(arguments),
        ResponsesInputItem::FunctionCallOutput { output, .. } => {
            count_function_call_output_tokens(output)
        }
        ResponsesInputItem::Reasoning {
            encrypted_content, ..
        } => approx_reasoning_token_count(encrypted_content),
    }
}

fn approx_reasoning_token_count(encoded_content: &str) -> u64 {
    let model_visible_bytes = encoded_content
        .len()
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650);
    u64::try_from(model_visible_bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

fn count_content_part_tokens(part: &ResponsesContentPart) -> u64 {
    match part {
        ResponsesContentPart::InputText { text } => token_count(text),
        ResponsesContentPart::OutputText { text } => token_count(text),
        ResponsesContentPart::InputImage { .. } => 2000, // Image token estimate
    }
}

fn count_function_call_output_tokens(output: &ResponsesFunctionCallOutput) -> u64 {
    match output {
        ResponsesFunctionCallOutput::Text(text) => token_count(text),
        ResponsesFunctionCallOutput::Content(parts) => parts
            .iter()
            .map(|part| match part {
                ResponsesFunctionCallOutputContent::InputText { text } => token_count(text),
                ResponsesFunctionCallOutputContent::InputImage { .. } => 2000,
            })
            .sum(),
    }
}

fn count_tool_tokens(tools: &[ResponsesTool]) -> u64 {
    let mut total = 0u64;
    for tool in tools {
        match tool {
            ResponsesTool::Function(f) => {
                total += token_count(&f.name);
                if let Some(ref desc) = f.description {
                    total += token_count(desc);
                }
                total += token_count(&serde_json::to_string(&f.parameters).unwrap_or_default());
            }
            ResponsesTool::WebSearch(_) => {
                total += 10; // fixed overhead for web search tool
            }
        }
    }
    total
}

fn token_count(text: &str) -> u64 {
    u64::try_from(o200k_base_singleton().count_with_special_tokens(text)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::codex::translate::request::{
        TranslateOptions, TranslationOverrides, translate_request_with_overrides,
    };
    use serde_json::json;

    fn translated_request(value: serde_json::Value) -> ResponsesRequest {
        let request: MessagesRequest = serde_json::from_value(value).unwrap();
        translate_request_with_overrides(
            &request,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.5".to_string(),
                use_responses_lite: false,
            },
            TranslationOverrides::default(),
        )
        .unwrap()
    }

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
    fn count_structured_tool_result_includes_image_estimate() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [
                    {"type": "input_text", "text": "screenshot"},
                    {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                ]
            }],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();
        assert!(count_translated_tokens(&req) >= 2000);
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

    #[test]
    fn count_tokens_structured_schema_increases_estimate() {
        let baseline = translated_request(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "return a result"}]
        }));
        let schema = json!({
            "type": "object",
            "properties": {
                "result": {
                    "type": "string",
                    "description": "x".repeat(10 * 1024)
                }
            },
            "required": ["result"],
            "additionalProperties": false
        });
        assert!(
            serde_json::to_vec(&schema).unwrap().len() >= 10 * 1024,
            "fixture must exercise a schema of at least 10 KiB"
        );
        let structured = translated_request(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "return a result"}],
            "output_config": {"format": {
                "type": "json_schema",
                "name": "large_result",
                "schema": schema
            }}
        }));

        let baseline_count = count_translated_tokens(&baseline);
        let structured_count = count_translated_tokens(&structured);
        assert!(structured_count > baseline_count);
    }

    #[test]
    fn count_cjk_text_uses_real_tokenizer() {
        let text = "这是一个用于验证中文上下文计数不会被严重低估的测试句子。".repeat(20);
        assert!(token_count(&text) > 100);
    }

    #[test]
    fn encrypted_reasoning_uses_codex_model_visible_size_estimate() {
        let encoded_content = "A".repeat(4000);
        assert_eq!(approx_reasoning_token_count(&encoded_content), 588);
        assert_eq!(approx_reasoning_token_count("short"), 0);
    }
}
