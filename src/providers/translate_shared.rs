use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;

#[derive(Debug)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
    },
}

#[derive(Debug)]
pub struct ImageSource {
    pub media_type: String,
    pub data: String,
    pub source_type: String,
}

pub fn flatten_system_text(system_val: Option<&Value>) -> Option<String> {
    let system = system_val?;
    let texts: Vec<String> = match system {
        Value::String(s) => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                let text = b.get("text").and_then(|v| v.as_str())?;
                if text.starts_with("x-anthropic-billing-header:") {
                    None
                } else {
                    Some(text.to_string())
                }
            })
            .collect(),
        _ => return None,
    };
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

pub fn read_effort(req: &MessagesRequest) -> Result<Option<&str>, anyhow::Error> {
    let output_config = match req.extra.get("output_config") {
        Some(Value::Object(m)) => m,
        _ => return Ok(None),
    };
    match output_config.get("effort") {
        Some(Value::String(s)) => {
            let valid = ["low", "medium", "high", "xhigh", "max"];
            if valid.contains(&s.as_str()) {
                Ok(Some(s.as_str()))
            } else {
                anyhow::bail!("Invalid output_config.effort: {s}")
            }
        }
        _ => Ok(None),
    }
}

pub fn normalize_content(content: &Value, missing_tool_input: Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => {
            vec![ContentBlock::Text { text: s.clone() }]
        }
        Value::Array(arr) => {
            let mut blocks = Vec::new();
            for item in arr {
                if let Some(block) = parse_content_block(item, missing_tool_input.clone()) {
                    blocks.push(block);
                }
            }
            blocks
        }
        _ => Vec::new(),
    }
}

pub fn image_source_to_url(source: &ImageSource) -> String {
    if source.source_type == "url" {
        source.data.clone()
    } else {
        format!("data:{};base64,{}", source.media_type, source.data)
    }
}

pub fn image_block_to_url(block: &Value) -> String {
    let source_type = block
        .get("source")
        .and_then(|s| s.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("base64");
    if source_type == "url" {
        block
            .get("source")
            .and_then(|s| s.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        let media_type = block
            .get("source")
            .and_then(|s| s.get("media_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("image/png");
        let data = block
            .get("source")
            .and_then(|s| s.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        format!("data:{media_type};base64,{data}")
    }
}

fn parse_content_block(value: &Value, missing_tool_input: Value) -> Option<ContentBlock> {
    let kind = value.get("type").and_then(|v| v.as_str())?;
    match kind {
        "text" => {
            let text = value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(ContentBlock::Text { text })
        }
        "image" => {
            let source = value.get("source")?;
            let media_type = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let source_type = source
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("base64")
                .to_string();
            let data = if source_type == "url" {
                source.get("url").and_then(|v| v.as_str()).unwrap_or("")
            } else {
                source.get("data").and_then(|v| v.as_str()).unwrap_or("")
            }
            .to_string();
            Some(ContentBlock::Image {
                source: ImageSource {
                    media_type,
                    data,
                    source_type,
                },
            })
        }
        "tool_use" => {
            let id = value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = value.get("input").cloned().unwrap_or(missing_tool_input);
            Some(ContentBlock::ToolUse { id, name, input })
        }
        "tool_result" => {
            let tool_use_id = value
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = value
                .get("content")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let is_error = value.get("is_error").and_then(|v| v.as_bool());
            Some(ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            })
        }
        "thinking" => {
            let thinking = value
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(ContentBlock::Thinking { thinking })
        }
        _ => None,
    }
}
