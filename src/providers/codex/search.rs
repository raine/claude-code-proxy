use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::anthropic::sse::encode_sse_event;
use crate::traffic::TrafficCapture;

use super::count_tokens::{approx_token_count, truncate_to_token_budget};

const SEARCH_OUTPUT_TOKEN_BUDGET: u64 = 2_500;
const SEARCH_ASSISTANT_CONTEXT_TOKEN_BUDGET: u64 = 1_000;
const SEARCH_USER_CONTEXT_MESSAGES: usize = 2;
const CLAUDE_SEARCH_PROMPT_PREFIX: &str = "Perform a web search for the query:";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchRequest {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    pub commands: SearchCommands,
    pub settings: SearchSettings,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchCommands {
    pub search_query: Vec<SearchQuery>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchQuery {
    pub q: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<SearchFilters>,
    pub allowed_callers: Vec<&'static str>,
    pub external_web_access: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub encrypted_output: Option<String>,
    pub output: String,
    #[serde(default)]
    pub results: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
}

pub fn is_standalone_search_request(req: &MessagesRequest) -> bool {
    let Some(choice) = req.extra.get("tool_choice").and_then(Value::as_object) else {
        return false;
    };
    if choice.get("type").and_then(Value::as_str) != Some("tool") {
        return false;
    }
    let Some(selected_name) = choice.get("name").and_then(Value::as_str) else {
        return false;
    };
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(Value::as_str) == Some("web_search_20250305")
                    && tool.get("name").and_then(Value::as_str) == Some(selected_name)
            })
        })
}

pub fn build_search_request(
    req: &MessagesRequest,
    model: &str,
    session_id: Option<&str>,
) -> Result<(SearchRequest, String), anyhow::Error> {
    let query = extract_search_query(req)
        .ok_or_else(|| anyhow::anyhow!("web_search request does not contain a text query"))?;
    let input = search_input(req);
    let filters = search_filters(req);
    let id = session_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("search-{}", uuid::Uuid::new_v4()));

    Ok((
        SearchRequest {
            id,
            model: model.to_string(),
            reasoning: None,
            input,
            commands: SearchCommands {
                search_query: vec![SearchQuery { q: query.clone() }],
            },
            settings: SearchSettings {
                filters,
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
            max_output_tokens: SEARCH_OUTPUT_TOKEN_BUDGET,
        },
        query,
    ))
}

pub fn search_request_input_tokens(request: &SearchRequest) -> u64 {
    let mut tokens = approx_token_count(&request.model);
    tokens += request
        .commands
        .search_query
        .iter()
        .map(|query| approx_token_count(&query.q))
        .sum::<u64>();
    tokens += request.input.as_ref().map(value_text_tokens).unwrap_or(0);
    if let Some(filters) = &request.settings.filters {
        tokens += filters
            .allowed_domains
            .iter()
            .flatten()
            .chain(filters.blocked_domains.iter().flatten())
            .map(|domain| approx_token_count(domain))
            .sum::<u64>();
    }
    tokens.max(1)
}

pub fn search_response_output_tokens(response: &SearchResponse) -> u64 {
    (approx_token_count(&response.output)
        + response
            .results
            .as_ref()
            .map(|results| results.iter().map(value_text_tokens).sum())
            .unwrap_or(0))
    .max(1)
}

fn value_text_tokens(value: &Value) -> u64 {
    match value {
        Value::String(text) => approx_token_count(text),
        Value::Array(values) => values.iter().map(value_text_tokens).sum(),
        Value::Object(values) => values.values().map(value_text_tokens).sum(),
        _ => 0,
    }
}

pub fn anthropic_search_response(
    response: &SearchResponse,
    query: &str,
    message_id: &str,
    model: &str,
    stream: bool,
    input_tokens: u64,
    traffic: Option<&TrafficCapture>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let tool_use_id = format!("srvtoolu_ws_{}", uuid::Uuid::new_v4().simple());
    let results = search_results(response);
    let content = response_content(response, query, &tool_use_id, &results);
    let output_tokens = search_response_output_tokens(response);
    let usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "server_tool_use": {"web_search_requests": 1}
    });

    if !stream {
        return (
            http::StatusCode::OK,
            axum::Json(json!({
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": usage,
            })),
        )
            .into_response();
    }

    let mut body = Vec::new();
    emit(
        &mut body,
        traffic,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": input_tokens, "output_tokens": 0}
            }
        }),
    );
    emit(
        &mut body,
        traffic,
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": "web_search",
                "input": {}
            }
        }),
    );
    emit(
        &mut body,
        traffic,
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": json!({"query": query}).to_string()
            }
        }),
    );
    emit_block_stop(&mut body, traffic, 0);
    emit(
        &mut body,
        traffic,
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": web_search_result_values(&results)
            }
        }),
    );
    emit_block_stop(&mut body, traffic, 1);
    if !response.output.is_empty() {
        emit(
            &mut body,
            traffic,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "text", "text": ""}
            }),
        );
        emit(
            &mut body,
            traffic,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "text_delta", "text": response.output}
            }),
        );
        emit_block_stop(&mut body, traffic, 2);
    }
    emit(
        &mut body,
        traffic,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": usage
        }),
    );
    emit(
        &mut body,
        traffic,
        "message_stop",
        &json!({"type": "message_stop"}),
    );

    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, body).into_response()
}

fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .find_map(|message| {
            let text = content_text(&message.content);
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(
                text.strip_prefix(CLAUDE_SEARCH_PROMPT_PREFIX)
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
                    .unwrap_or(text)
                    .to_string(),
            )
        })
}

fn search_input(req: &MessagesRequest) -> Option<Value> {
    let mut messages: Vec<(&str, String)> = req
        .messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
        .filter_map(|message| {
            let text = content_text(&message.content);
            (!text.is_empty()).then_some((message.role.as_str(), text))
        })
        .collect();
    let latest_user = messages.iter().rposition(|(role, _)| *role == "user")?;
    messages.truncate(latest_user + 1);
    let first_user = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, (role, _))| *role == "user")
        .take(SEARCH_USER_CONTEXT_MESSAGES)
        .last()
        .map(|(index, _)| index)
        .unwrap_or(latest_user);
    messages.drain(..first_user);

    let mut assistant_budget = SEARCH_ASSISTANT_CONTEXT_TOKEN_BUDGET;
    let items: Vec<Value> = messages
        .into_iter()
        .filter_map(|(role, text)| {
            let (content_type, text) = if role == "assistant" {
                if assistant_budget == 0 {
                    return None;
                }
                let text = truncate_to_token_budget(&text, assistant_budget);
                assistant_budget = assistant_budget.saturating_sub(approx_token_count(&text));
                ("output_text", text)
            } else {
                ("input_text", text)
            };
            (!text.is_empty()).then(|| {
                json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": content_type, "text": text}]
                })
            })
        })
        .collect();
    (!items.is_empty()).then_some(Value::Array(items))
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn search_filters(req: &MessagesRequest) -> Option<SearchFilters> {
    let tool = req
        .extra
        .get("tools")?
        .as_array()?
        .iter()
        .find(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search_20250305"))?;
    let allowed_domains = string_array(tool.get("allowed_domains"));
    let blocked_domains = string_array(tool.get("blocked_domains"));
    (allowed_domains.is_some() || blocked_domains.is_some()).then_some(SearchFilters {
        allowed_domains,
        blocked_domains,
    })
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    let values: Vec<String> = values
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    (!values.is_empty()).then_some(values)
}

fn search_results(response: &SearchResponse) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for result in response.results.iter().flatten() {
        let Some(url) = result.get("url").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(url.to_string()) {
            continue;
        }
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| result.get("ref_id").and_then(Value::as_str))
            .unwrap_or(url);
        results.push(SearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }
    results
}

fn response_content(
    response: &SearchResponse,
    query: &str,
    tool_use_id: &str,
    results: &[SearchResult],
) -> Vec<Value> {
    let mut content = vec![
        json!({
            "type": "server_tool_use",
            "id": tool_use_id,
            "name": "web_search",
            "input": {"query": query}
        }),
        json!({
            "type": "web_search_tool_result",
            "tool_use_id": tool_use_id,
            "content": web_search_result_values(results)
        }),
    ];
    if !response.output.is_empty() {
        content.push(json!({"type": "text", "text": response.output}));
    }
    content
}

fn web_search_result_values(results: &[SearchResult]) -> Vec<Value> {
    results
        .iter()
        .map(|result| {
            json!({
                "type": "web_search_result",
                "title": result.title,
                "url": result.url,
            })
        })
        .collect()
}

fn emit_block_stop(out: &mut Vec<u8>, traffic: Option<&TrafficCapture>, index: usize) {
    emit(
        out,
        traffic,
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    );
}

fn emit(out: &mut Vec<u8>, traffic: Option<&TrafficCapture>, event: &str, data: &Value) {
    if let Some(traffic) = traffic {
        traffic.write_json_event(
            "050-downstream-event",
            &json!({"event": event, "data": data}),
        );
    }
    out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::sse::parse_sse_events;

    fn request() -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 32000,
            "stream": true,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "Perform a web search for the query: Codex standalone search"
                }]
            }],
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "allowed_domains": ["openai.com"]
            }],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }))
        .unwrap()
    }

    #[test]
    fn request_preserves_luna_and_omits_reasoning() {
        let (request, query) =
            build_search_request(&request(), "gpt-5.6-luna", Some("session-1")).unwrap();
        assert_eq!(request.model, "gpt-5.6-luna");
        assert_eq!(request.reasoning, None);
        assert_eq!(request.id, "session-1");
        assert_eq!(request.max_output_tokens, 2_500);
        assert_eq!(query, "Codex standalone search");
        assert_eq!(request.commands.search_query[0].q, query);
        assert_eq!(
            request.settings.filters.unwrap().allowed_domains,
            Some(vec!["openai.com".to_string()])
        );
    }

    #[test]
    fn only_forced_claude_search_uses_standalone_endpoint() {
        let forced = request();
        assert!(is_standalone_search_request(&forced));

        let mut automatic = forced.clone();
        automatic
            .extra
            .insert("tool_choice".to_string(), json!({"type": "auto"}));
        assert!(!is_standalone_search_request(&automatic));
    }

    #[test]
    fn search_input_uses_role_specific_content_and_recent_context() {
        let mut req = request();
        req.messages = serde_json::from_value(json!([
            {"role": "user", "content": "old user"},
            {"role": "assistant", "content": "old assistant"},
            {"role": "user", "content": "previous user"},
            {"role": "assistant", "content": "previous assistant"},
            {
                "role": "user",
                "content": "Perform a web search for the query: current query"
            },
            {"role": "assistant", "content": "content after latest user"}
        ]))
        .unwrap();

        let (search, _) = build_search_request(&req, "gpt-5.6-luna", None).unwrap();
        let input = search.input.unwrap();
        let items = input.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["content"][0]["text"], "previous user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["content"][0]["text"], "previous assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[2]["content"][0]["type"], "input_text");
    }

    #[test]
    fn search_input_bounds_assistant_context() {
        let mut req = request();
        let long_assistant = (0..2_000)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        req.messages = serde_json::from_value(json!([
            {"role": "user", "content": "previous user"},
            {"role": "assistant", "content": long_assistant},
            {
                "role": "user",
                "content": "Perform a web search for the query: current query"
            }
        ]))
        .unwrap();

        let (search, _) = build_search_request(&req, "gpt-5.6-luna", None).unwrap();
        let assistant = search.input.unwrap()[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(approx_token_count(&assistant) <= SEARCH_ASSISTANT_CONTEXT_TOKEN_BUDGET);
        assert!(!assistant.contains("word1999"));
    }

    #[test]
    fn missing_structured_results_does_not_infer_urls_from_output() {
        let response = SearchResponse {
            encrypted_output: None,
            output: "Result from https://github.com with an embedded https://example.com link"
                .to_string(),
            results: None,
        };

        assert!(search_results(&response).is_empty());
    }

    #[test]
    fn standalone_usage_estimates_are_nonzero() {
        let (request, _) = build_search_request(&request(), "gpt-5.6-luna", None).unwrap();
        let response = SearchResponse {
            encrypted_output: None,
            output: "search output".to_string(),
            results: None,
        };

        assert!(search_request_input_tokens(&request) > 0);
        assert!(search_response_output_tokens(&response) > 0);
    }

    #[test]
    fn streamed_response_matches_claude_server_tool_shape() {
        let response = SearchResponse {
            encrypted_output: Some("opaque".to_string()),
            output: "See [OpenAI](https://openai.com).".to_string(),
            results: Some(vec![json!({
                "type": "text_result",
                "ref_id": "turn0search0",
                "url": "https://openai.com",
                "title": "OpenAI"
            })]),
        };
        let response = anthropic_search_response(
            &response,
            "Codex standalone search",
            "msg_test",
            "claude-haiku-4-5-20251001",
            true,
            12,
            None,
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let body = runtime.block_on(axum::body::to_bytes(response.into_body(), usize::MAX));
        let events = parse_sse_events(&body.unwrap());
        let payloads: Vec<Value> = events
            .iter()
            .filter_map(|event| serde_json::from_str(&event.data).ok())
            .collect();
        assert!(payloads.iter().any(|payload| {
            payload
                .pointer("/content_block/type")
                .and_then(Value::as_str)
                == Some("server_tool_use")
        }));
        assert!(payloads.iter().any(|payload| {
            payload
                .pointer("/content_block/type")
                .and_then(Value::as_str)
                == Some("web_search_tool_result")
        }));
        assert!(payloads.iter().any(|payload| {
            payload
                .pointer("/usage/server_tool_use/web_search_requests")
                .and_then(Value::as_u64)
                == Some(1)
        }));
    }
}
