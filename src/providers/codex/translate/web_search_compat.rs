use std::sync::LazyLock;

use base64::Engine as _;
use serde_json::Value;

static MARKDOWN_LINK_REGEX: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r#"\[([^\]\n]+)\]\((https?://[^)\s]+)\)"#)
        .expect("markdown link regex must compile")
});
static BARE_URL_REGEX: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"https?://[^\s<>()|]+").expect("bare URL regex must compile")
});

pub struct WebSearchCompatBlock {
    pub index: usize,
    pub content: WebSearchCompatContent,
}

pub enum WebSearchCompatContent {
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
        caller: Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Vec<WebSearchResult>,
        caller: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
}

pub fn server_tool_use_id_from_codex_web_search_id(id: &str) -> Result<String, String> {
    if id.is_empty() {
        return Err("Codex web search item id must be a non-empty string".to_string());
    }
    // Preserve the common legacy shape, while reserving `b64_` as an escape
    // namespace for every value that the legacy sanitizer could collapse.
    // Encoding the full UTF-8 byte sequence makes the mapping injective.
    let safe_legacy = id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !id.starts_with("b64_");
    let suffix = if safe_legacy {
        id.to_string()
    } else {
        format!(
            "b64_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.as_bytes())
        )
    };
    Ok(format!("srvtoolu_{suffix}"))
}

fn extract_web_search_results_from_text(text: &str) -> Vec<WebSearchResult> {
    let mut results: Vec<WebSearchResult> = Vec::new();
    let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Match markdown links [title](url)
    for cap in MARKDOWN_LINK_REGEX.captures_iter(text) {
        let title = clean_title(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        let url = clean_url(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        if url.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        seen_urls.insert(url.clone());
        let display_title = if title.is_empty() {
            fallback_title(&url)
        } else {
            title
        };
        results.push(WebSearchResult {
            title: display_title,
            url,
        });
    }

    // Match bare URLs
    for cap in BARE_URL_REGEX.captures_iter(text) {
        let raw_url = cap.get(0).map(|m| m.as_str()).unwrap_or("");
        let url = clean_url(raw_url);
        if url.is_empty() || seen_urls.contains(&url) {
            continue;
        }
        seen_urls.insert(url.clone());
        results.push(WebSearchResult {
            title: fallback_title(&url),
            url,
        });
    }

    results
}

pub fn build_web_search_compat_blocks(
    searches: &[super::reducer::ReducerEvent],
    text: &str,
) -> Vec<WebSearchCompatBlock> {
    let fallback_results =
        (searches.len() == 1).then(|| extract_web_search_results_from_text(text));
    let mut blocks = Vec::new();

    for event in searches {
        if let super::reducer::ReducerEvent::WebSearch {
            index,
            result_index,
            id,
            query,
            sources,
        } = event
        {
            let results = if sources.is_empty() {
                fallback_results.clone().unwrap_or_default()
            } else {
                sources.clone()
            };
            blocks.push(WebSearchCompatBlock {
                index: *index,
                content: WebSearchCompatContent::ServerToolUse {
                    id: id.clone(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": query}),
                    caller: serde_json::json!({"type":"direct"}),
                },
            });
            blocks.push(WebSearchCompatBlock {
                index: *result_index,
                content: WebSearchCompatContent::WebSearchToolResult {
                    tool_use_id: id.clone(),
                    content: results,
                    caller: serde_json::json!({"type":"direct"}),
                },
            });
        }
    }

    blocks
}

fn clean_url(value: &str) -> String {
    let mut out = value.trim().to_string();
    while out.ends_with('.')
        || out.ends_with(',')
        || out.ends_with(';')
        || out.ends_with(':')
        || out.ends_with('!')
        || out.ends_with('?')
    {
        out.pop();
    }
    out
}

fn clean_title(value: &str) -> String {
    let no_markers = value
        .trim_start()
        .trim_start_matches(|c: char| c == '-' || c == '*' || c == '+' || c.is_ascii_digit())
        .trim_start_matches(['.', ')', ' '])
        .replace("**", "")
        .replace('`', "")
        .trim()
        .to_string();
    // Take text before dash or em-dash
    if let Some(pos) = no_markers.find(" - ") {
        no_markers[..pos].trim().to_string()
    } else if let Some(find_pos) = no_markers.find(" \u{2013} ") {
        no_markers[..find_pos].trim().to_string()
    } else {
        no_markers
    }
}

fn fallback_title(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_tool_use_id_format() {
        let id = server_tool_use_id_from_codex_web_search_id("ws_123").unwrap();
        assert_eq!(id, "srvtoolu_ws_123");
    }

    #[test]
    fn server_tool_use_id_mapping_is_injective_and_rejects_empty_ids() {
        let dashed = server_tool_use_id_from_codex_web_search_id("ws-1").unwrap();
        let underscored = server_tool_use_id_from_codex_web_search_id("ws_1").unwrap();
        let reserved = server_tool_use_id_from_codex_web_search_id("b64_d3MtMQ").unwrap();

        assert_ne!(dashed, underscored);
        assert_ne!(dashed, reserved);
        assert!(dashed.starts_with("srvtoolu_b64_"));
        assert!(reserved.starts_with("srvtoolu_b64_"));
        assert!(server_tool_use_id_from_codex_web_search_id("").is_err());
    }

    #[test]
    fn extract_results_from_text() {
        let text = "Check [Example](https://example.com) and https://other.com/page.";
        let results = extract_web_search_results_from_text(text);
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.url == "https://example.com"));
        assert!(results.iter().any(|r| r.url == "https://other.com/page"));
    }

    #[test]
    fn build_compat_blocks() {
        let searches = vec![super::super::reducer::ReducerEvent::WebSearch {
            index: 0,
            result_index: 1,
            id: "ws_1".to_string(),
            query: "test".to_string(),
            sources: vec![WebSearchResult {
                title: "Bound result".to_string(),
                url: "https://bound.example".to_string(),
            }],
        }];
        let text = "See [Result](https://result.com)";
        let blocks = build_web_search_compat_blocks(&searches, text);
        assert_eq!(blocks.len(), 2);
        match &blocks[0].content {
            WebSearchCompatContent::ServerToolUse {
                name,
                input,
                caller,
                ..
            } => {
                assert_eq!(name, "web_search");
                assert_eq!(input.get("query").and_then(|v| v.as_str()), Some("test"));
                assert_eq!(caller["type"], "direct");
            }
            _ => panic!("expected ServerToolUse"),
        }
        match &blocks[1].content {
            WebSearchCompatContent::WebSearchToolResult {
                content, caller, ..
            } => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].url, "https://bound.example");
                assert_eq!(caller["type"], "direct");
            }
            _ => panic!("expected WebSearchToolResult"),
        }
    }

    #[test]
    fn multiple_searches_keep_sources_bound_to_each_call() {
        let searches = vec![
            super::super::reducer::ReducerEvent::WebSearch {
                index: 0,
                result_index: 1,
                id: "ws_1".to_string(),
                query: "first".to_string(),
                sources: vec![WebSearchResult {
                    title: "First".to_string(),
                    url: "https://first.example".to_string(),
                }],
            },
            super::super::reducer::ReducerEvent::WebSearch {
                index: 2,
                result_index: 3,
                id: "ws_2".to_string(),
                query: "second".to_string(),
                sources: vec![WebSearchResult {
                    title: "Second".to_string(),
                    url: "https://second.example".to_string(),
                }],
            },
        ];
        let blocks = build_web_search_compat_blocks(
            &searches,
            "Unrelated https://final-text.example must not be copied.",
        );
        let results: Vec<&[WebSearchResult]> = blocks
            .iter()
            .filter_map(|block| match &block.content {
                WebSearchCompatContent::WebSearchToolResult { content, .. } => {
                    Some(content.as_slice())
                }
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0][0].url, "https://first.example");
        assert_eq!(results[1][0].url, "https://second.example");
    }
}
