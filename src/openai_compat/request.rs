use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::{
    anthropic::schema::{Message, MessagesRequest},
    registry::normalize_incoming_model,
};

use super::{OpenAiError, OpenAiResponseMetadata, OpenAiSurface};

const MAX_MESSAGES: usize = 1_024;
const MAX_TOOLS: usize = 128;
const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_DATA_BYTES: usize = 12 * 1024 * 1024;

const CHAT_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "tools",
    "tool_choice",
    "reasoning_effort",
    "n",
    "parallel_tool_calls",
];

const RESPONSES_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "stream",
    "max_output_tokens",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "store",
];

#[derive(Debug, Clone)]
pub struct ParsedOpenAiRequest {
    pub messages: MessagesRequest,
    pub requested_model: String,
    pub normalized_model: String,
    pub stream: bool,
    pub include_usage: bool,
    pub response_metadata: OpenAiResponseMetadata,
}

pub fn extract_model(body: &Value) -> Result<(String, String), OpenAiError> {
    let object = body.as_object().ok_or_else(|| {
        OpenAiError::invalid("Request body must be a JSON object", None::<String>)
    })?;
    let requested = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| OpenAiError::invalid("Missing or invalid 'model'", Some("model")))?
        .to_string();
    Ok((requested.clone(), normalize_incoming_model(&requested)))
}

pub fn parse_request(
    surface: OpenAiSurface,
    body: Value,
    provider: &str,
    session_id: Option<&str>,
) -> Result<ParsedOpenAiRequest, OpenAiError> {
    let (requested_model, normalized_model) = extract_model(&body)?;
    let object = body
        .as_object()
        .expect("extract_model validated request object");
    reject_fields(
        object,
        match surface {
            OpenAiSurface::ChatCompletions => CHAT_FIELDS,
            OpenAiSurface::Responses => RESPONSES_FIELDS,
        },
    )?;
    let stream = optional_bool(object, "stream")?.unwrap_or(false);
    let mut include_usage = false;
    let (messages, system) = match surface {
        OpenAiSurface::ChatCompletions => {
            include_usage = parse_stream_options(object.get("stream_options"), stream)?;
            parse_chat_messages(object.get("messages"))?
        }
        OpenAiSurface::Responses => {
            parse_responses_input(object.get("input"), object.get("instructions"))?
        }
    };
    if messages.is_empty() {
        return Err(OpenAiError::invalid(
            match surface {
                OpenAiSurface::ChatCompletions => "'messages' must contain at least one message",
                OpenAiSurface::Responses => "'input' must contain at least one input item",
            },
            Some(match surface {
                OpenAiSurface::ChatCompletions => "messages",
                OpenAiSurface::Responses => "input",
            }),
        ));
    }
    if messages.len() > MAX_MESSAGES {
        return Err(OpenAiError::invalid(
            format!("Request contains more than {MAX_MESSAGES} messages"),
            Some(match surface {
                OpenAiSurface::ChatCompletions => "messages",
                OpenAiSurface::Responses => "input",
            }),
        ));
    }
    validate_single_choice(object)?;
    validate_parallel_tools(object)?;
    validate_store(surface, object)?;
    let max_tokens = parse_max_tokens(surface, object)?;
    let tools = parse_tools(object.get("tools"), surface)?;
    let tool_choice = parse_tool_choice(object.get("tool_choice"), &tools, surface)?;
    let response_metadata = if surface == OpenAiSurface::Responses {
        OpenAiResponseMetadata {
            tools: object
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            tool_choice: object
                .get("tool_choice")
                .filter(|value| !value.is_null())
                .cloned()
                .unwrap_or_else(|| json!("auto")),
        }
    } else {
        OpenAiResponseMetadata::default()
    };
    let effort = parse_effort(surface, object)?;
    validate_cursor(provider, session_id, stream, &messages, &tools)?;

    let mut extra = Map::new();
    if !system.is_empty() {
        extra.insert(
            "system".to_string(),
            Value::Array(
                system
                    .into_iter()
                    .map(|text| json!({"type":"text", "text":text}))
                    .collect(),
            ),
        );
    }
    if !tools.is_empty() {
        extra.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(choice) = tool_choice {
        extra.insert("tool_choice".to_string(), choice);
    }
    if let Some(effort) = effort {
        extra.insert("output_config".to_string(), json!({"effort":effort}));
    }

    Ok(ParsedOpenAiRequest {
        messages: MessagesRequest {
            model: Some(normalized_model.clone()),
            max_tokens,
            messages,
            stream: true,
            bypass_provider_model_override: false,
            bypass_provider_effort_override: false,
            extra,
        },
        requested_model,
        normalized_model,
        stream,
        include_usage,
        response_metadata,
    })
}

fn reject_fields(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), OpenAiError> {
    for (key, value) in object {
        if !allowed.contains(&key.as_str()) && !value.is_null() {
            return Err(OpenAiError::unsupported(key));
        }
    }
    Ok(())
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<bool>, OpenAiError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(OpenAiError::invalid(
            format!("'{key}' must be a boolean"),
            Some(key),
        )),
    }
}

fn validate_store(surface: OpenAiSurface, object: &Map<String, Value>) -> Result<(), OpenAiError> {
    if surface != OpenAiSurface::Responses {
        return Ok(());
    }
    match object.get("store") {
        None | Some(Value::Null | Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) => Err(OpenAiError::unsupported("store")),
        Some(_) => Err(OpenAiError::invalid(
            "'store' must be a boolean",
            Some("store"),
        )),
    }
}

fn parse_stream_options(value: Option<&Value>, stream: bool) -> Result<bool, OpenAiError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(false);
    };
    if !stream {
        return Err(OpenAiError::invalid(
            "'stream_options' is only supported when 'stream' is true",
            Some("stream_options"),
        ));
    }
    let object = value.as_object().ok_or_else(|| {
        OpenAiError::invalid("'stream_options' must be an object", Some("stream_options"))
    })?;
    reject_fields(object, &["include_usage"])?;
    optional_bool(object, "include_usage").map(|value| value.unwrap_or(false))
}

fn validate_single_choice(object: &Map<String, Value>) -> Result<(), OpenAiError> {
    match object.get("n") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Number(number)) if number.as_u64() == Some(1) => Ok(()),
        Some(Value::Number(_)) => Err(OpenAiError::unsupported("n")),
        Some(_) => Err(OpenAiError::invalid("'n' must be an integer", Some("n"))),
    }
}

fn validate_parallel_tools(object: &Map<String, Value>) -> Result<(), OpenAiError> {
    match object.get("parallel_tool_calls") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) => Err(OpenAiError::unsupported("parallel_tool_calls")),
        Some(_) => Err(OpenAiError::invalid(
            "'parallel_tool_calls' must be a boolean",
            Some("parallel_tool_calls"),
        )),
    }
}

fn parse_max_tokens(
    surface: OpenAiSurface,
    object: &Map<String, Value>,
) -> Result<Option<u32>, OpenAiError> {
    let (primary, alternate) = match surface {
        OpenAiSurface::ChatCompletions => ("max_completion_tokens", Some("max_tokens")),
        OpenAiSurface::Responses => ("max_output_tokens", None),
    };
    if let Some(alternate) = alternate
        && object.get(primary).is_some_and(|value| !value.is_null())
        && object.get(alternate).is_some_and(|value| !value.is_null())
    {
        return Err(OpenAiError::invalid(
            format!("'{primary}' and '{alternate}' cannot both be set"),
            Some(primary),
        ));
    }
    let (key, value) = object
        .get(primary)
        .filter(|value| !value.is_null())
        .map(|value| (primary, value))
        .or_else(|| {
            alternate.and_then(|key| {
                object
                    .get(key)
                    .filter(|value| !value.is_null())
                    .map(|value| (key, value))
            })
        })
        .unwrap_or((primary, &Value::Null));
    if value.is_null() {
        return Ok(None);
    }
    let value = value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
        OpenAiError::invalid(format!("'{key}' must be a positive integer"), Some(key))
    })?;
    u32::try_from(value)
        .map(Some)
        .map_err(|_| OpenAiError::invalid(format!("'{key}' is too large"), Some(key)))
}

fn parse_effort(
    surface: OpenAiSurface,
    object: &Map<String, Value>,
) -> Result<Option<String>, OpenAiError> {
    let value = match surface {
        OpenAiSurface::ChatCompletions => object.get("reasoning_effort"),
        OpenAiSurface::Responses => {
            let Some(reasoning) = object.get("reasoning").filter(|value| !value.is_null()) else {
                return Ok(None);
            };
            let reasoning = reasoning.as_object().ok_or_else(|| {
                OpenAiError::invalid("'reasoning' must be an object", Some("reasoning"))
            })?;
            reject_fields(reasoning, &["effort"])?;
            reasoning.get("effort")
        }
    };
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let effort = value.as_str().ok_or_else(|| {
        OpenAiError::invalid(
            "Reasoning effort must be a string",
            Some(match surface {
                OpenAiSurface::ChatCompletions => "reasoning_effort",
                OpenAiSurface::Responses => "reasoning.effort",
            }),
        )
    })?;
    match effort {
        "low" | "medium" | "high" | "xhigh" | "max" => Ok(Some(effort.to_string())),
        _ => Err(OpenAiError::invalid(
            format!("Unsupported reasoning effort: '{effort}'"),
            Some(match surface {
                OpenAiSurface::ChatCompletions => "reasoning_effort",
                OpenAiSurface::Responses => "reasoning.effort",
            }),
        )),
    }
}

fn parse_chat_messages(value: Option<&Value>) -> Result<(Vec<Message>, Vec<String>), OpenAiError> {
    let messages = value
        .and_then(Value::as_array)
        .ok_or_else(|| OpenAiError::invalid("Missing or invalid 'messages'", Some("messages")))?;
    let mut out = Vec::new();
    let mut system = Vec::new();
    let mut calls = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        let param = format!("messages[{index}]");
        let object = message
            .as_object()
            .ok_or_else(|| OpenAiError::invalid("Each message must be an object", Some(&param)))?;
        reject_nested_fields(
            object,
            &[
                "role",
                "content",
                "name",
                "tool_calls",
                "tool_call_id",
                "reasoning_content",
            ],
            &param,
        )?;
        let role = required_string(object, "role", &format!("{param}.role"))?;
        match role.as_str() {
            "system" | "developer" => {
                system.push(content_text(
                    object.get("content"),
                    &format!("{param}.content"),
                )?);
            }
            "user" => out.push(Message {
                role: "user".to_string(),
                content: parse_content(object.get("content"), &format!("{param}.content"), true)?,
            }),
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(reasoning) = object
                    .get("reasoning_content")
                    .filter(|value| !value.is_null())
                {
                    let reasoning = reasoning.as_str().ok_or_else(|| {
                        OpenAiError::invalid(
                            "'reasoning_content' must be a string",
                            Some(format!("{param}.reasoning_content")),
                        )
                    })?;
                    blocks.push(json!({"type":"thinking", "thinking":reasoning}));
                }
                append_text_blocks(
                    &mut blocks,
                    object.get("content"),
                    &format!("{param}.content"),
                )?;
                parse_chat_tool_calls(object.get("tool_calls"), &param, &mut blocks, &mut calls)?;
                if blocks.is_empty() {
                    return Err(OpenAiError::invalid(
                        "Assistant message requires content or tool calls",
                        Some(format!("{param}.content")),
                    ));
                }
                out.push(Message {
                    role: "assistant".to_string(),
                    content: Value::Array(blocks),
                });
            }
            "tool" => {
                let id = required_string(object, "tool_call_id", &format!("{param}.tool_call_id"))?;
                if !calls.remove(&id) {
                    return Err(OpenAiError::invalid(
                        format!("Tool result references unknown call '{id}'"),
                        Some(format!("{param}.tool_call_id")),
                    ));
                }
                let result = json!({
                    "type":"tool_result",
                    "tool_use_id":id,
                    "content":content_text(object.get("content"), &format!("{param}.content"))?,
                });
                push_tool_result(&mut out, result);
            }
            _ => {
                return Err(OpenAiError::invalid(
                    format!("Unsupported message role: '{role}'"),
                    Some(format!("{param}.role")),
                ));
            }
        }
    }
    Ok((out, system))
}

fn parse_responses_input(
    value: Option<&Value>,
    instructions: Option<&Value>,
) -> Result<(Vec<Message>, Vec<String>), OpenAiError> {
    let mut system = Vec::new();
    if let Some(instructions) = instructions.filter(|value| !value.is_null()) {
        system.push(
            instructions
                .as_str()
                .filter(|text| !text.is_empty())
                .ok_or_else(|| {
                    OpenAiError::invalid(
                        "'instructions' must be a non-empty string",
                        Some("instructions"),
                    )
                })?
                .to_string(),
        );
    }
    let Some(value) = value else {
        return Err(OpenAiError::invalid("Missing 'input'", Some("input")));
    };
    if let Some(text) = value.as_str() {
        if text.is_empty() {
            return Err(OpenAiError::invalid(
                "'input' must not be empty",
                Some("input"),
            ));
        }
        return Ok((
            vec![Message {
                role: "user".to_string(),
                content: Value::String(text.to_string()),
            }],
            system,
        ));
    }
    let items = value
        .as_array()
        .ok_or_else(|| OpenAiError::invalid("'input' must be a string or array", Some("input")))?;
    let mut out = Vec::new();
    let mut calls = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let param = format!("input[{index}]");
        let object = item
            .as_object()
            .ok_or_else(|| OpenAiError::invalid("Input items must be objects", Some(&param)))?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match kind {
            "message" => {
                reject_nested_fields(object, &["type", "role", "content", "id", "status"], &param)?;
                let role = required_string(object, "role", &format!("{param}.role"))?;
                let content = parse_responses_message_content(
                    object.get("content"),
                    &format!("{param}.content"),
                )?;
                match role.as_str() {
                    "system" | "developer" => system.push(blocks_text(&content)),
                    "user" | "assistant" => out.push(Message {
                        role,
                        content: Value::Array(content),
                    }),
                    _ => {
                        return Err(OpenAiError::invalid(
                            format!("Unsupported input message role: '{role}'"),
                            Some(format!("{param}.role")),
                        ));
                    }
                }
            }
            "function_call" => {
                reject_nested_fields(
                    object,
                    &["type", "id", "call_id", "name", "arguments", "status"],
                    &param,
                )?;
                let id = object
                    .get("call_id")
                    .or_else(|| object.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        OpenAiError::invalid(
                            "Function call requires 'call_id'",
                            Some(format!("{param}.call_id")),
                        )
                    })?
                    .to_string();
                if !calls.insert(id.clone()) {
                    return Err(OpenAiError::invalid(
                        format!("Duplicate function call id '{id}'"),
                        Some(format!("{param}.call_id")),
                    ));
                }
                let name = required_string(object, "name", &format!("{param}.name"))?;
                let input =
                    parse_arguments(object.get("arguments"), &format!("{param}.arguments"))?;
                out.push(Message {
                    role: "assistant".to_string(),
                    content: json!([{"type":"tool_use", "id":id, "name":name, "input":input}]),
                });
            }
            "function_call_output" => {
                reject_nested_fields(
                    object,
                    &["type", "id", "call_id", "output", "status"],
                    &param,
                )?;
                let id = required_string(object, "call_id", &format!("{param}.call_id"))?;
                if !calls.remove(&id) {
                    return Err(OpenAiError::invalid(
                        format!("Function output references unknown call '{id}'"),
                        Some(format!("{param}.call_id")),
                    ));
                }
                let output = object.get("output").ok_or_else(|| {
                    OpenAiError::invalid(
                        "Function output requires 'output'",
                        Some(format!("{param}.output")),
                    )
                })?;
                let content = match output {
                    Value::String(text) => Value::String(text.clone()),
                    value => Value::String(value.to_string()),
                };
                push_tool_result(
                    &mut out,
                    json!({"type":"tool_result", "tool_use_id":id, "content":content}),
                );
            }
            _ => return Err(OpenAiError::unsupported(format!("{param}.type"))),
        }
    }
    Ok((out, system))
}

fn parse_chat_tool_calls(
    value: Option<&Value>,
    parent: &str,
    blocks: &mut Vec<Value>,
    calls: &mut HashSet<String>,
) -> Result<(), OpenAiError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let items = value.as_array().ok_or_else(|| {
        OpenAiError::invalid(
            "'tool_calls' must be an array",
            Some(format!("{parent}.tool_calls")),
        )
    })?;
    for (index, item) in items.iter().enumerate() {
        let param = format!("{parent}.tool_calls[{index}]");
        let object = item
            .as_object()
            .ok_or_else(|| OpenAiError::invalid("Tool calls must be objects", Some(&param)))?;
        reject_nested_fields(object, &["id", "type", "function"], &param)?;
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Err(OpenAiError::unsupported(format!("{param}.type")));
        }
        let id = required_string(object, "id", &format!("{param}.id"))?;
        if !calls.insert(id.clone()) {
            return Err(OpenAiError::invalid(
                format!("Duplicate tool call id '{id}'"),
                Some(format!("{param}.id")),
            ));
        }
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                OpenAiError::invalid(
                    "Tool call requires a function object",
                    Some(format!("{param}.function")),
                )
            })?;
        reject_nested_fields(
            function,
            &["name", "arguments"],
            &format!("{param}.function"),
        )?;
        let name = required_string(function, "name", &format!("{param}.function.name"))?;
        let input = parse_arguments(
            function.get("arguments"),
            &format!("{param}.function.arguments"),
        )?;
        blocks.push(json!({"type":"tool_use", "id":id, "name":name, "input":input}));
    }
    Ok(())
}

fn parse_arguments(value: Option<&Value>, param: &str) -> Result<Value, OpenAiError> {
    let arguments = value.and_then(Value::as_str).ok_or_else(|| {
        OpenAiError::invalid("Function arguments must be a JSON string", Some(param))
    })?;
    if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(OpenAiError::invalid(
            "Function arguments are too large",
            Some(param),
        ));
    }
    let value: Value = serde_json::from_str(arguments).map_err(|error| {
        OpenAiError::invalid(
            format!("Function arguments are invalid JSON: {error}"),
            Some(param),
        )
    })?;
    if !value.is_object() {
        return Err(OpenAiError::invalid(
            "Function arguments must decode to an object",
            Some(param),
        ));
    }
    Ok(value)
}

fn parse_tools(value: Option<&Value>, surface: OpenAiSurface) -> Result<Vec<Value>, OpenAiError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| OpenAiError::invalid("'tools' must be an array", Some("tools")))?;
    if tools.len() > MAX_TOOLS {
        return Err(OpenAiError::invalid(
            format!("'tools' cannot contain more than {MAX_TOOLS} entries"),
            Some("tools"),
        ));
    }
    let mut names = HashSet::new();
    let mut out = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let param = format!("tools[{index}]");
        let object = tool
            .as_object()
            .ok_or_else(|| OpenAiError::invalid("Tools must be objects", Some(&param)))?;
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Err(OpenAiError::unsupported(format!("{param}.type")));
        }
        let function = match surface {
            OpenAiSurface::ChatCompletions => {
                reject_nested_fields(object, &["type", "function"], &param)?;
                object
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        OpenAiError::invalid(
                            "Function tool requires a function object",
                            Some(format!("{param}.function")),
                        )
                    })?
            }
            OpenAiSurface::Responses => object,
        };
        let function_param = match surface {
            OpenAiSurface::ChatCompletions => format!("{param}.function"),
            OpenAiSurface::Responses => param.clone(),
        };
        reject_nested_fields(
            function,
            &["type", "name", "description", "parameters", "strict"],
            &function_param,
        )?;
        if function.get("strict").is_some_and(|value| !value.is_null()) {
            return Err(OpenAiError::unsupported(format!("{function_param}.strict")));
        }
        let name = required_string(function, "name", &format!("{function_param}.name"))?;
        if !names.insert(name.clone()) {
            return Err(OpenAiError::invalid(
                format!("Duplicate tool name '{name}'"),
                Some(format!("{function_param}.name")),
            ));
        }
        let schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object"}));
        if !schema.is_object() {
            return Err(OpenAiError::invalid(
                "Function parameters must be an object",
                Some(format!("{function_param}.parameters")),
            ));
        }
        let mut translated = Map::from_iter([
            ("name".to_string(), Value::String(name)),
            ("input_schema".to_string(), schema),
        ]);
        if let Some(description) = function.get("description").filter(|value| !value.is_null()) {
            translated.insert(
                "description".to_string(),
                Value::String(
                    description
                        .as_str()
                        .ok_or_else(|| {
                            OpenAiError::invalid(
                                "Function description must be a string",
                                Some(format!("{function_param}.description")),
                            )
                        })?
                        .to_string(),
                ),
            );
        }
        out.push(Value::Object(translated));
    }
    Ok(out)
}

fn parse_tool_choice(
    value: Option<&Value>,
    tools: &[Value],
    surface: OpenAiSurface,
) -> Result<Option<Value>, OpenAiError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let translated = if let Some(choice) = value.as_str() {
        match choice {
            "auto" => json!({"type":"auto"}),
            "none" => json!({"type":"none"}),
            "required" => json!({"type":"any"}),
            _ => return Err(OpenAiError::unsupported("tool_choice")),
        }
    } else {
        let object = value.as_object().ok_or_else(|| {
            OpenAiError::invalid(
                "'tool_choice' must be a string or object",
                Some("tool_choice"),
            )
        })?;
        let name = match surface {
            OpenAiSurface::ChatCompletions => {
                if object.get("type").and_then(Value::as_str) != Some("function") {
                    return Err(OpenAiError::unsupported("tool_choice.type"));
                }
                object
                    .get("function")
                    .and_then(Value::as_object)
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            }
            OpenAiSurface::Responses => {
                if object.get("type").and_then(Value::as_str) != Some("function") {
                    return Err(OpenAiError::unsupported("tool_choice.type"));
                }
                object.get("name").and_then(Value::as_str)
            }
        }
        .filter(|name| !name.is_empty())
        .ok_or_else(|| OpenAiError::invalid("Tool choice requires a name", Some("tool_choice")))?;
        if !tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        {
            return Err(OpenAiError::invalid(
                format!("Tool choice references unknown tool '{name}'"),
                Some("tool_choice"),
            ));
        }
        json!({"type":"tool", "name":name})
    };
    Ok(Some(translated))
}

fn parse_content(
    value: Option<&Value>,
    param: &str,
    allow_images: bool,
) -> Result<Value, OpenAiError> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Ok(Value::String(text.clone())),
        Some(Value::Array(parts)) if !parts.is_empty() => {
            let mut out = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                out.push(parse_chat_content_part(
                    part,
                    &format!("{param}[{index}]"),
                    allow_images,
                )?);
            }
            Ok(Value::Array(out))
        }
        _ => Err(OpenAiError::invalid(
            "Message content must not be empty",
            Some(param),
        )),
    }
}

fn append_text_blocks(
    out: &mut Vec<Value>,
    value: Option<&Value>,
    param: &str,
) -> Result<(), OpenAiError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(text)) if text.is_empty() => Ok(()),
        Some(Value::String(text)) => {
            out.push(json!({"type":"text", "text":text}));
            Ok(())
        }
        Some(Value::Array(parts)) => {
            for (index, part) in parts.iter().enumerate() {
                let block = parse_chat_content_part(part, &format!("{param}[{index}]"), false)?;
                out.push(block);
            }
            Ok(())
        }
        _ => Err(OpenAiError::invalid(
            "Invalid assistant content",
            Some(param),
        )),
    }
}

fn parse_chat_content_part(
    part: &Value,
    param: &str,
    allow_images: bool,
) -> Result<Value, OpenAiError> {
    let object = part
        .as_object()
        .ok_or_else(|| OpenAiError::invalid("Content parts must be objects", Some(param)))?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => Ok(json!({
            "type":"text",
            "text":required_string(object, "text", &format!("{param}.text"))?,
        })),
        Some("image_url") if allow_images => {
            let image = object
                .get("image_url")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    OpenAiError::invalid(
                        "Image content requires 'image_url'",
                        Some(format!("{param}.image_url")),
                    )
                })?;
            image_url_block(
                image.get("url").and_then(Value::as_str).ok_or_else(|| {
                    OpenAiError::invalid(
                        "Image URL must be a string",
                        Some(format!("{param}.image_url.url")),
                    )
                })?,
                &format!("{param}.image_url.url"),
            )
        }
        _ => Err(OpenAiError::unsupported(format!("{param}.type"))),
    }
}

fn parse_responses_message_content(
    value: Option<&Value>,
    param: &str,
) -> Result<Vec<Value>, OpenAiError> {
    if let Some(text) = value.and_then(Value::as_str) {
        return Ok(vec![json!({"type":"text", "text":text})]);
    }
    let parts = value.and_then(Value::as_array).ok_or_else(|| {
        OpenAiError::invalid("Message content must be a string or array", Some(param))
    })?;
    let mut out = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let part_param = format!("{param}[{index}]");
        let object = part.as_object().ok_or_else(|| {
            OpenAiError::invalid("Content parts must be objects", Some(&part_param))
        })?;
        match object.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => out.push(json!({
                "type":"text",
                "text":required_string(object, "text", &format!("{part_param}.text"))?,
            })),
            Some("input_image") => {
                let url = object
                    .get("image_url")
                    .or_else(|| object.get("url"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        OpenAiError::invalid(
                            "Input image requires 'image_url'",
                            Some(format!("{part_param}.image_url")),
                        )
                    })?;
                out.push(image_url_block(url, &format!("{part_param}.image_url"))?);
            }
            _ => return Err(OpenAiError::unsupported(format!("{part_param}.type"))),
        }
    }
    Ok(out)
}

fn image_url_block(url: &str, param: &str) -> Result<Value, OpenAiError> {
    if let Some(data) = url.strip_prefix("data:") {
        let (media_type, encoded) = data.split_once(";base64,").ok_or_else(|| {
            OpenAiError::invalid("Image data URL must use base64 encoding", Some(param))
        })?;
        if encoded.len() > MAX_IMAGE_DATA_BYTES * 4 / 3 + 4 {
            return Err(OpenAiError::invalid("Image data is too large", Some(param)));
        }
        let media_type = media_type.split(';').next().unwrap_or(media_type);
        if !matches!(
            media_type,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return Err(OpenAiError::invalid(
                format!("Unsupported image media type '{media_type}'"),
                Some(param),
            ));
        }
        Ok(json!({
            "type":"image",
            "source":{"type":"base64", "media_type":media_type, "data":encoded},
        }))
    } else if url.starts_with("https://") || url.starts_with("http://") {
        Ok(json!({"type":"image", "source":{"type":"url", "url":url}}))
    } else {
        Err(OpenAiError::invalid(
            "Image URL must use http, https, or a base64 data URL",
            Some(param),
        ))
    }
}

fn validate_cursor(
    provider: &str,
    session_id: Option<&str>,
    stream: bool,
    messages: &[Message],
    tools: &[Value],
) -> Result<(), OpenAiError> {
    if provider != "cursor" {
        return Ok(());
    }
    if messages
        .iter()
        .any(|message| contains_url_image(&message.content))
    {
        return Err(OpenAiError::unsupported(
            "input image URL for provider 'cursor'",
        ));
    }
    if tools.is_empty() {
        return Ok(());
    }
    if !stream {
        return Err(OpenAiError::invalid(
            "Cursor tools require 'stream' to be true",
            Some("stream"),
        ));
    }
    if session_id.is_none_or(str::is_empty) {
        return Err(OpenAiError::invalid(
            "Cursor tools require a stable session header",
            Some("tools"),
        ));
    }
    for (index, tool) in tools.iter().enumerate() {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        if !matches!(name, "Read" | "Write" | "Bash") {
            return Err(OpenAiError::invalid(
                format!("Cursor cannot bridge tool '{name}'"),
                Some(format!("tools[{index}].function.name")),
            ));
        }
    }
    Ok(())
}

fn contains_url_image(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("image")
                && block.pointer("/source/type").and_then(Value::as_str) == Some("url")
        })
    })
}

fn push_tool_result(messages: &mut Vec<Message>, result: Value) {
    if let Some(last) = messages.last_mut()
        && last.role == "user"
        && last.content.as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
    {
        last.content
            .as_array_mut()
            .expect("checked tool result array")
            .push(result);
    } else {
        messages.push(Message {
            role: "user".to_string(),
            content: Value::Array(vec![result]),
        });
    }
}

fn required_string(
    object: &Map<String, Value>,
    key: &str,
    param: &str,
) -> Result<String, OpenAiError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            OpenAiError::invalid(format!("'{param}' must be a non-empty string"), Some(param))
        })
}

fn reject_nested_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    parent: &str,
) -> Result<(), OpenAiError> {
    for (key, value) in object {
        if !allowed.contains(&key.as_str()) && !value.is_null() {
            return Err(OpenAiError::unsupported(format!("{parent}.{key}")));
        }
    }
    Ok(())
}

fn content_text(value: Option<&Value>, param: &str) -> Result<String, OpenAiError> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let object = part.as_object().ok_or_else(|| {
                    OpenAiError::invalid(
                        "Text content parts must be objects",
                        Some(format!("{param}[{index}]")),
                    )
                })?;
                if !matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                ) {
                    return Err(OpenAiError::unsupported(format!("{param}[{index}].type")));
                }
                out.push(required_string(
                    object,
                    "text",
                    &format!("{param}[{index}].text"),
                )?);
            }
            if out.is_empty() {
                Err(OpenAiError::invalid(
                    "Content must not be empty",
                    Some(param),
                ))
            } else {
                Ok(out.join(""))
            }
        }
        _ => Err(OpenAiError::invalid(
            "Content must be non-empty text",
            Some(param),
        )),
    }
}

fn blocks_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_maps_tools_and_results() {
        let parsed = parse_request(
            OpenAiSurface::ChatCompletions,
            json!({
                "model":"kimi-k2.6",
                "messages":[
                    {"role":"user","content":"look up x"},
                    {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},
                    {"role":"tool","tool_call_id":"call_1","content":"answer"}
                ],
                "tools":[{"type":"function","function":{"name":"lookup","description":"lookup","parameters":{"type":"object"}}}],
                "tool_choice":{"type":"function","function":{"name":"lookup"}}
            }),
            "kimi",
            Some("session"),
        )
        .unwrap();
        assert_eq!(parsed.messages.extra["tools"][0]["name"], "lookup");
        assert_eq!(parsed.messages.messages[1].content[0]["type"], "tool_use");
        assert_eq!(
            parsed.messages.messages[2].content[0]["type"],
            "tool_result"
        );
        assert_eq!(parsed.messages.extra["tool_choice"]["name"], "lookup");
    }

    #[test]
    fn responses_maps_function_items() {
        let parsed = parse_request(
            OpenAiSurface::Responses,
            json!({
                "model":"grok-4.5",
                "instructions":"be concise",
                "input":[
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
                    {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call_1","output":"done"}
                ],
                "reasoning":{"effort":"high"}
            }),
            "grok",
            None,
        )
        .unwrap();
        assert_eq!(parsed.messages.extra["system"][0]["text"], "be concise");
        assert_eq!(parsed.messages.messages[1].content[0]["type"], "tool_use");
        assert_eq!(parsed.messages.extra["output_config"]["effort"], "high");
    }

    #[test]
    fn rejects_unsupported_fields_and_cursor_tools_without_session() {
        let error = parse_request(
            OpenAiSurface::ChatCompletions,
            json!({"model":"kimi-k2.6","messages":[{"role":"user","content":"x"}],"temperature":0.5}),
            "kimi",
            None,
        )
        .unwrap_err();
        assert_eq!(error.param.as_deref(), Some("temperature"));
        assert_eq!(error.code.as_deref(), Some("unsupported_parameter"));

        let error = parse_request(
            OpenAiSurface::ChatCompletions,
            json!({
                "model":"cursor:gpt-5.5",
                "stream":true,
                "messages":[{"role":"user","content":"x"}],
                "tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object"}}}]
            }),
            "cursor",
            None,
        )
        .unwrap_err();
        assert_eq!(error.param.as_deref(), Some("tools"));
    }

    #[test]
    fn rejects_duplicate_results_store_and_buffered_cursor_tools() {
        let duplicate = parse_request(
            OpenAiSurface::ChatCompletions,
            json!({
                "model":"kimi-k2.6",
                "messages":[
                    {"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
                    {"role":"tool","tool_call_id":"call_1","content":"first"},
                    {"role":"tool","tool_call_id":"call_1","content":"second"}
                ]
            }),
            "kimi",
            Some("session"),
        )
        .unwrap_err();
        assert!(duplicate.message.contains("unknown call"));

        let store = parse_request(
            OpenAiSurface::Responses,
            json!({"model":"grok-4.5","input":"hello","store":true}),
            "grok",
            None,
        )
        .unwrap_err();
        assert_eq!(store.param.as_deref(), Some("store"));

        let cursor = parse_request(
            OpenAiSurface::ChatCompletions,
            json!({
                "model":"cursor:gpt-5.5",
                "messages":[{"role":"user","content":"x"}],
                "tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object"}}}]
            }),
            "cursor",
            Some("session"),
        )
        .unwrap_err();
        assert_eq!(cursor.param.as_deref(), Some("stream"));
    }

    #[test]
    fn normalizes_model_and_preserves_requested_value() {
        let parsed = parse_request(
            OpenAiSurface::Responses,
            json!({"model":"grok-4.5[1m]","input":"hello"}),
            "grok",
            None,
        )
        .unwrap();
        assert_eq!(parsed.requested_model, "grok-4.5[1m]");
        assert_eq!(parsed.normalized_model, "grok-4.5");
        assert_eq!(parsed.messages.model.as_deref(), Some("grok-4.5"));
    }
}
