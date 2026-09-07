use bytes::Bytes;
use serde_json::Value;

use crate::providers::translate_shared::previous_reasoning_text;

pub(super) fn resolve_model(model: &str) -> &str {
    match model {
        "haiku" => "claude-haiku-4-5",
        "sonnet" => "claude-sonnet-5",
        "opus" => "claude-opus-5",
        "fable" => "claude-fable-5",
        _ => model,
    }
}

/// Preserve the original bytes unless a local alias or foreign reasoning needs
/// translation. Unknown Anthropic fields and native signatures remain intact.
pub(super) fn prepare_body(raw: Bytes, model: &str) -> Result<Bytes, serde_json::Error> {
    let mut document: Value = serde_json::from_slice(&raw)?;
    let mut changed = document.get("model").and_then(Value::as_str) != Some(model);
    if changed {
        document["model"] = Value::String(model.to_owned());
    }
    if let Some(messages) = document.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    if let Some(text) = foreign_reasoning(block) {
                        *block = serde_json::json!({"type": "text", "text": previous_reasoning_text(text)});
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        serde_json::to_vec(&document).map(Bytes::from)
    } else {
        Ok(raw)
    }
}

fn foreign_reasoning(block: &Value) -> Option<&str> {
    if block.get("type").and_then(Value::as_str) != Some("thinking") {
        return None;
    }
    let signature = block.get("signature").and_then(Value::as_str);
    // Only interpret the proxy's own namespace. Anthropic signatures are opaque.
    if signature.is_some_and(|signature| !signature.is_empty() && !is_proxy_signature(signature)) {
        return None;
    }
    block.get("thinking").and_then(Value::as_str)
}

fn is_proxy_signature(signature: &str) -> bool {
    // Kimi and OpenCode encode the complete `ccp:kimi:v1:` prefix as base64url.
    signature.starts_with("ccp:") || signature.starts_with("Y2NwOmtpbWk6djE6")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encoded_kimi_reasoning_is_not_mistaken_for_anthropic_thinking() {
        let signature =
            crate::providers::kimi::translate::signature::make_thinking_signature("msg_1", 2);
        let raw = Bytes::from(json!({"model":"claude-opus-5", "messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"Kimi summary","signature":signature}]}]}).to_string());
        let document: Value =
            serde_json::from_slice(&prepare_body(raw, "claude-opus-5").unwrap()).unwrap();
        assert_eq!(
            document["messages"][0]["content"][0],
            json!({"type":"text","text":previous_reasoning_text("Kimi summary")})
        );
    }

    #[test]
    fn native_requests_preserve_exact_bytes_and_unknown_fields() {
        let raw = Bytes::from_static(br#"{ "model":"claude-opus-5", "future":true, "messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"native","signature":"opaque"},{"type":"redacted_thinking","data":"encrypted"},{"type":"future_block","value":12}]}] }"#);
        assert_eq!(prepare_body(raw.clone(), "claude-opus-5").unwrap(), raw);
    }

    #[test]
    fn foreign_reasoning_becomes_text_without_disturbing_tool_history() {
        let mut document = json!({"model":"claude-opus-5", "messages":[{"role":"assistant", "content":[
            {"type":"thinking","thinking":"unsigned","signature":""},
            {"type":"thinking","thinking":"summary","signature":"ccp:codex:v1:cnNfMQ:encrypted"},
            {"type":"thinking","thinking":"native","signature":"opaque"},
            {"type":"redacted_thinking","data":"opaque"},
            {"type":"tool_use","id":"call1","name":"Read","input":{"path":"x"}}
        ]}, {"role":"user","content":[{"type":"tool_result","tool_use_id":"call1","content":"value"}]}]});
        let raw = Bytes::from(serde_json::to_vec(&document).unwrap());
        let result: Value =
            serde_json::from_slice(&prepare_body(raw, "claude-opus-5").unwrap()).unwrap();
        for (index, summary) in [(0, "unsigned"), (1, "summary")] {
            document["messages"][0]["content"][index] =
                json!({"type":"text","text":previous_reasoning_text(summary)});
        }
        assert_eq!(result, document);
        assert!(!result.to_string().contains("ccp:codex"));
    }

    #[test]
    fn model_rewrite_preserves_native_reasoning_and_is_idempotent() {
        let raw = Bytes::from_static(br#"{"model":"opus[1m]","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"native","signature":"opaque"}]}]}"#);
        let prepared = prepare_body(raw, resolve_model("opus")).unwrap();
        let document: Value = serde_json::from_slice(&prepared).unwrap();
        assert_eq!(document["model"], "claude-opus-5");
        assert_eq!(document["messages"][0]["content"][0]["signature"], "opaque");
        assert_eq!(
            prepare_body(prepared.clone(), "claude-opus-5").unwrap(),
            prepared
        );
    }
}
