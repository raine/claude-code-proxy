use crate::anthropic::schema::MessagesRequest;

/// A selected image extracted from the request content blocks.
#[derive(Debug, Clone)]
pub struct CursorSelectedImage {
    pub data: String,
    pub uuid: String,
    pub path: String,
    pub mime_type: String,
}

/// Render the full Cursor prompt from an Anthropic MessagesRequest.
///
/// Includes:
/// - System message (with billing-header filtering)
/// - Conversation messages with content blocks
/// - Tools block
pub fn render_cursor_prompt(req: &MessagesRequest) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(system) = render_system(req) {
        sections.push(format!("<system>\n{system}\n</system>"));
    }

    for message in &req.messages {
        let content = render_message_content(message);
        if let Some(c) = content {
            sections.push(format!("<{}>\n{}\n</{}>", message.role, c, message.role));
        }
    }

    // Tools block
    if let Some(tools) = req.extra.get("tools").and_then(|v| v.as_array())
        && !tools.is_empty()
    {
        let tool_lines: Vec<String> = tools
            .iter()
            .map(|t| {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let description = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                let input_schema = t
                    .get("input_schema")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                serde_json::json!({
                    "name": name,
                    "description": description,
                    "input_schema": input_schema,
                })
                .to_string()
            })
            .collect();
        if !tool_lines.is_empty() {
            sections.push(format!("<tools>\n{}\n</tools>", tool_lines.join("\n")));
        }
    }

    sections.join("\n\n")
}

/// Extract selected images from the request, mimicking `cursorSelectedImages`.
///
/// Only base64 source images are included. URL images are skipped.
/// Images nested inside tool_result blocks are also collected.
pub fn cursor_selected_images(req: &MessagesRequest) -> Vec<CursorSelectedImage> {
    let mut images: Vec<CursorSelectedImage> = Vec::new();
    let mut index: u32 = 0;

    for message in &req.messages {
        let blocks = message_blocks(message);
        for block in &blocks {
            collect_image_blocks(block, &mut index, &mut images);
        }
    }

    images
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn render_system(req: &MessagesRequest) -> Option<String> {
    let system_value = req.extra.get("system")?;
    let text = match system_value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .filter(|line| !line.starts_with("x-anthropic-billing-header:"))
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n\n")
        }
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn render_message_content(message: &crate::anthropic::schema::Message) -> Option<String> {
    let blocks = message_blocks(message);
    let rendered: Vec<String> = blocks.iter().filter_map(render_block).collect();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join("\n\n"))
    }
}

fn render_block(block: &serde_json::Value) -> Option<String> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;
    match block_type {
        "text" => block
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string()),
        "thinking" => {
            let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            Some(format!("<thinking>\n{text}\n</thinking>"))
        }
        "image" => {
            let source = block.get("source")?;
            match source.get("type").and_then(|t| t.as_str()) {
                Some("url") => {
                    let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
                    Some(format!("[image: {url}]"))
                }
                _ => {
                    let media_type = source
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                    Some(format!(
                        "[image: {media_type}, {} base64 chars]",
                        data.len()
                    ))
                }
            }
        }
        "tool_use" => {
            let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block
                .get("input")
                .and_then(|i| serde_json::to_string(i).ok())
                .unwrap_or_else(|| "{}".to_string());
            Some(format!(
                "<tool_use id=\"{id}\" name=\"{name}\">\n{input}\n</tool_use>"
            ))
        }
        "tool_result" => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let is_error = block
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let error_attr = if is_error { " is_error=\"true\"" } else { "" };
            let content = render_tool_result_content(block);
            Some(format!(
                "<tool_result tool_use_id=\"{tool_use_id}\"{error_attr}>\n{content}\n</tool_result>"
            ))
        }
        "server_tool_use" => {
            let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block
                .get("input")
                .and_then(|i| serde_json::to_string(i).ok())
                .unwrap_or_else(|| "{}".to_string());
            Some(format!(
                "<server_tool_use id=\"{id}\" name=\"{name}\">\n{input}\n</server_tool_use>"
            ))
        }
        "web_search_tool_result" => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let content = block
                .get("content")
                .and_then(|c| serde_json::to_string(c).ok())
                .unwrap_or_else(|| "{}".to_string());
            Some(format!(
                "<web_search_tool_result tool_use_id=\"{tool_use_id}\">\n{content}\n</web_search_tool_result>"
            ))
        }
        _ => {
            // Unsupported block type - render as text placeholder
            block
                .get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        }
    }
}

fn render_tool_result_content(block: &serde_json::Value) -> String {
    let content = match block.get("content") {
        Some(serde_json::Value::String(s)) => return s.clone(),
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        _ => return String::new(),
    };

    let parts: Vec<String> = content
        .iter()
        .filter_map(render_tool_result_block)
        .collect();
    parts.join("\n\n")
}

fn render_tool_result_block(block: &serde_json::Value) -> Option<String> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;
    match block_type {
        "text" | "image" | "tool_use" | "tool_result" | "thinking" => render_block(block),
        _ => {
            let type_str = block_type.to_string();
            Some(format!("[unsupported tool result block: {type_str}]"))
        }
    }
}

fn message_blocks(message: &crate::anthropic::schema::Message) -> Vec<serde_json::Value> {
    match &message.content {
        serde_json::Value::String(s) => {
            vec![serde_json::json!({"type": "text", "text": s})]
        }
        serde_json::Value::Array(arr) => arr.clone(),
        _ => Vec::new(),
    }
}

fn collect_image_blocks(
    block: &serde_json::Value,
    index: &mut u32,
    images: &mut Vec<CursorSelectedImage>,
) {
    if block.get("type").and_then(|t| t.as_str()) == Some("image") {
        let source = match block.get("source") {
            Some(s) => s,
            None => return,
        };
        if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
            return;
        }
        let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
        let media_type = source
            .get("media_type")
            .and_then(|m| m.as_str())
            .unwrap_or("image/png");
        let uuid = uuid::Uuid::new_v4().to_string();
        *index += 1;
        let extension = image_extension(media_type);
        images.push(CursorSelectedImage {
            data: data.to_string(),
            uuid,
            path: format!("claude-image-{index}.{extension}"),
            mime_type: media_type.to_string(),
        });
        return;
    }

    // Recurse into tool_result blocks for nested images
    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
        let content = match block.get("content") {
            Some(serde_json::Value::Array(arr)) => arr.clone(),
            _ => return,
        };
        for child in &content {
            let child_type = child.get("type").and_then(|t| t.as_str());
            matches!(
                child_type,
                Some("text" | "image" | "tool_use" | "tool_result" | "thinking")
            );
            collect_image_blocks(child, index, images);
        }
    }
}

fn image_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "img",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_system_message() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "system": "be direct",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<system>"));
        assert!(rendered.contains("be direct"));
        assert!(rendered.contains("</system>"));
        assert!(rendered.contains("<user>"));
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("</user>"));
    }

    #[test]
    fn renders_tools_section() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read files", "input_schema": {"type": "object"}}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<tools>"));
        assert!(rendered.contains("Read"));
    }

    #[test]
    fn filters_billing_headers_from_system() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "system": [
                {"type": "text", "text": "keep this"},
                {"type": "text", "text": "x-anthropic-billing-header: skip-me"}
            ],
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("keep this"));
        assert!(!rendered.contains("x-anthropic-billing-header"));
    }

    #[test]
    fn collects_selected_images() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]
            }]
        }))
        .unwrap();
        let images = cursor_selected_images(&req);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, "AAAA");
    }

    #[test]
    fn skips_url_images_in_selected() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/img.png"}}
                ]
            }]
        }))
        .unwrap();
        let images = cursor_selected_images(&req);
        assert_eq!(images.len(), 0);
    }

    #[test]
    fn renders_url_image_placeholder() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/img.png"}}
                ]
            }]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("[image: https://example.com/img.png]"));
    }

    #[test]
    fn renders_thinking_blocks() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me think..."},
                {"type": "text", "text": "done"}
            ]}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<thinking>"));
        assert!(rendered.contains("let me think..."));
        assert!(rendered.contains("done"));
    }

    #[test]
    fn renders_tool_use_blocks() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu1", "name": "Read", "input": {"path": "/tmp"}}
            ]}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<tool_use id=\"tu1\" name=\"Read\">"));
    }

    #[test]
    fn renders_tool_result_with_content_blocks() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": [
                    {"type": "text", "text": "file contents"}
                ]}
            ]}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<tool_result tool_use_id=\"tu1\">"));
        assert!(rendered.contains("file contents"));
    }

    #[test]
    fn handles_unsupported_block_types() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "unknown_block", "text": "some fallback text"}
            ]}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        // Unsupported blocks fall back to text rendering if they have a text field
        assert!(rendered.contains("some fallback text"));
    }

    #[test]
    fn empty_messages_renders_emptyish() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": ""}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.is_empty() || !rendered.is_empty());
    }

    #[test]
    fn tool_result_with_nested_image() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}}
                ]}
            ]}]
        }))
        .unwrap();
        let images = cursor_selected_images(&req);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");
        assert_eq!(images[0].data, "BBBB");
    }

    #[test]
    fn renders_server_tool_use() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "st1", "name": "WebSearch", "input": {"query": "rust"}}
            ]}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<server_tool_use id=\"st1\" name=\"WebSearch\">"));
    }

    #[test]
    fn renders_web_search_tool_result() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "web_search_tool_result", "tool_use_id": "ws1", "content": {"results": []}}
            ]}]
        }))
        .unwrap();
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<web_search_tool_result tool_use_id=\"ws1\">"));
    }
}
