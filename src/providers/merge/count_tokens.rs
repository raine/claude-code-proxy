//! Local heuristic token counts for Anthropic-compatible upstreams that may
//! not expose `/v1/messages/count_tokens`.

use crate::anthropic::schema::MessagesRequest;

pub const IMAGE_TOKEN_ESTIMATE: u64 = 2000;
pub const MESSAGE_OVERHEAD_TOKENS: u64 = 4;

pub fn count_tokens(req: &MessagesRequest) -> u64 {
    let mut total = 0u64;

    if let Some(system) = req.extra.get("system") {
        total += count_value_tokens(system);
    }

    for msg in &req.messages {
        total += count_value_tokens(&msg.content);
        total += MESSAGE_OVERHEAD_TOKENS;
    }

    if let Some(tools) = req.extra.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            total += count_value_tokens(tool);
        }
    }

    total.max(1)
}

fn count_value_tokens(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::String(s) => approx_token_count(s),
        serde_json::Value::Array(items) => items.iter().map(count_value_tokens).sum(),
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("image") {
                return IMAGE_TOKEN_ESTIMATE;
            }
            map.values().map(count_value_tokens).sum()
        }
        _ => 0,
    }
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
