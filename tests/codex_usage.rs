use claude_code_proxy::providers::codex::translate::{
    accumulate::accumulate_response, live_stream::LiveStreamTranslator,
    stream::translate_stream_bytes,
};
use serde_json::{Value, json};

fn terminal_usage(wire: &[u8]) -> Value {
    String::from_utf8_lossy(wire)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "message_delta")
        .expect("terminal message_delta")["usage"]
        .clone()
}

fn all_usage(events: Vec<Value>) -> Vec<Value> {
    let upstream: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let buffered =
        translate_stream_bytes(upstream.as_bytes(), "msg_usage", "gpt-5").expect("buffered stream");
    let message = accumulate_response(upstream.as_bytes(), "msg_usage", "gpt-5")
        .expect("nonstreaming message");
    let mut live = LiveStreamTranslator::new("msg_usage", "gpt-5");
    let wire: Vec<u8> = events
        .iter()
        .flat_map(|event| live.accept(event, None).expect("live event"))
        .collect();
    let usages = vec![
        terminal_usage(&buffered),
        message["usage"].clone(),
        terminal_usage(&wire),
    ];
    assert_eq!(usages[0], usages[1]);
    assert_eq!(usages[1], usages[2]);
    usages
}

fn completed(usage: Value) -> Value {
    json!({"type":"response.completed", "response":{"id":"resp_usage", "status":"completed", "usage":usage}})
}

#[test]
fn codex_usage_extensions_match_across_wire_formats() {
    let limits = json!({"plan_type":"plus", "rate_limits":{
        "limit_reached":false,
        "primary":{"used_percent":0, "window_minutes":300, "reset_at":1789765755},
        "secondary":{"used_percent":42.5, "window_minutes":10080}
    }, "type":"codex.rate_limits"});
    for usage in all_usage(vec![
        json!({"type":"codex.rate_limits", "rate_limits":{"limit_reached":true}}),
        limits,
        // Unusable updates must not erase the last usable snapshot.
        json!({"type":"codex.rate_limits", "rate_limits":null}),
        json!({"type":"codex.rate_limits", "rate_limits":{"limit_reached":"false", "primary":{"used_percent":"bad", "window_minutes":-1}}}),
        json!({"type":"codex.rate_limits", "rate_limits":{}}),
        completed(
            json!({"input_tokens":100, "input_tokens_details":{"cached_tokens":20},
            "output_tokens":50, "output_tokens_details":{"reasoning_tokens":30}}),
        ),
    ]) {
        assert_eq!(usage["input_tokens"], 80);
        assert_eq!(usage["cache_read_input_tokens"], 20);
        assert_eq!(usage["output_tokens"], 50);
        assert_eq!(
            usage["output_tokens_details"],
            json!({"reasoning_tokens":30,"thinking_tokens":30})
        );
        assert_eq!(usage["codex_rate_limits"]["limit_reached"], false);
        assert_eq!(usage["codex_rate_limits"]["primary"]["used_percent"], 0.0);
        assert_eq!(
            usage["codex_rate_limits"]["secondary"]["used_percent"],
            42.5
        );
    }
}

#[test]
fn absent_or_malformed_usage_extensions_are_omitted() {
    for details in [
        Value::Null,
        json!({}),
        json!({"reasoning_tokens":-1}),
        json!({"reasoning_tokens":"3"}),
    ] {
        for usage in all_usage(vec![
            json!({"type":"codex.rate_limits", "rate_limits":{"primary":{}, "secondary":false}}),
            completed(json!({"output_tokens_details":details})),
        ]) {
            assert!(usage.get("output_tokens_details").is_none());
            assert!(usage.get("codex_rate_limits").is_none());
        }
    }
}

#[test]
fn explicit_zero_reasoning_is_not_missing_usage() {
    for usage in all_usage(vec![completed(json!({
        "output_tokens":0, "output_tokens_details":{"reasoning_tokens":0}
    }))]) {
        assert_eq!(
            usage["output_tokens_details"],
            json!({"reasoning_tokens":0,"thinking_tokens":0})
        );
        assert!(usage.get("codex_rate_limits").is_none());
    }
}
