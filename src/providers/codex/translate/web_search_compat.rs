use serde_json::Value;

pub struct WebSearchCompatBlock {
    pub index: usize,
    pub content: WebSearchCompatContent,
}

pub enum WebSearchCompatContent {
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Vec<WebSearchResult>,
    },
}

#[derive(Clone)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
}

pub fn server_tool_use_id_from_codex_web_search_id(id: &str) -> String {
    let suffix: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("srvtoolu_{suffix}")
}

fn extract_web_search_results_from_text(text: &str) -> Vec<WebSearchResult> {
    let mut results: Vec<WebSearchResult> = Vec::new();
    let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Match markdown links [title](url)
    let re = regex_lite::Regex::new(r#"\[([^\]\n]+)\]\((https?://[^)\s]+)\)"#).unwrap();
    for cap in re.captures_iter(text) {
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
    let re2 = regex_lite::Regex::new(r"https?://[^\s<>()|]+").unwrap();
    for cap in re2.captures_iter(text) {
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
    let results = extract_web_search_results_from_text(text);
    let mut blocks = Vec::new();

    for event in searches {
        if let super::reducer::ReducerEvent::WebSearch {
            index,
            result_index,
            id,
            query,
        } = event
        {
            blocks.push(WebSearchCompatBlock {
                index: *index,
                content: WebSearchCompatContent::ServerToolUse {
                    id: id.clone(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": query}),
                },
            });
            blocks.push(WebSearchCompatBlock {
                index: *result_index,
                content: WebSearchCompatContent::WebSearchToolResult {
                    tool_use_id: id.clone(),
                    content: results.clone(),
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
        let id = server_tool_use_id_from_codex_web_search_id("ws_123");
        assert_eq!(id, "srvtoolu_ws_123");
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
        }];
        let text = "See [Result](https://result.com)";
        let blocks = build_web_search_compat_blocks(&searches, text);
        assert_eq!(blocks.len(), 2);
        match &blocks[0].content {
            WebSearchCompatContent::ServerToolUse { name, input, .. } => {
                assert_eq!(name, "web_search");
                assert_eq!(input.get("query").and_then(|v| v.as_str()), Some("test"));
            }
            _ => panic!("expected ServerToolUse"),
        }
        match &blocks[1].content {
            WebSearchCompatContent::WebSearchToolResult { content, .. } => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].url, "https://result.com");
            }
            _ => panic!("expected WebSearchToolResult"),
        }
    }
}
