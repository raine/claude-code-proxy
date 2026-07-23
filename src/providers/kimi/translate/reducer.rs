use crate::anthropic::sse::parse_sse_events;

#[derive(Debug, Clone)]
pub struct UpstreamStreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    RateLimit,
    Failed,
}

#[derive(Debug, Clone, Default)]
pub struct KimiUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    ThinkingStart {
        index: usize,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ThinkingStop {
        index: usize,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    TextStop {
        index: usize,
    },
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolDelta {
        index: usize,
        partial_json: String,
    },
    ToolStop {
        index: usize,
    },
    Finish {
        stop_reason: StopReason,
        usage: Option<KimiUsage>,
    },
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Option<Vec<StreamChoice>>,
    #[serde(default)]
    usage: Option<StreamUsage>,
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamDelta {
    #[allow(dead_code)]
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    function: Option<StreamToolCallFunction>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StreamError {
    #[serde(default)]
    message: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    r#type: Option<String>,
}

#[allow(dead_code)]
struct ToolSlot {
    tc_index: usize,
    block_index: usize,
    id: String,
    name: String,
}

pub fn reduce_upstream_bytes(input: &[u8]) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    let sse_events = parse_sse_events(input);
    let mut out = Vec::new();
    let mut next_block_index = 0usize;
    let mut thinking_index: Option<usize> = None;
    let mut text_index: Option<usize> = None;
    let mut tool_slots: Vec<ToolSlot> = Vec::new();
    let mut saw_tool_calls = false;
    let mut finish_reason: Option<String> = None;
    let mut final_usage: Option<KimiUsage> = None;

    for evt in &sse_events {
        let data = evt.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }

        let chunk: StreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        if let Some(ref err) = chunk.error {
            return Err(UpstreamStreamError {
                kind: UpstreamErrorKind::Failed,
                message: err
                    .message
                    .clone()
                    .unwrap_or_else(|| "Upstream error".to_string()),
                retry_after_seconds: None,
            });
        }

        if chunk.usage.is_some() && chunk.choices.as_ref().map(|c| c.is_empty()).unwrap_or(true) {
            final_usage = chunk.usage.map(|u| kimi_usage_from_stream(&u));
            continue;
        }

        let choice = match chunk.choices.as_ref().and_then(|c| c.first()) {
            Some(c) => c,
            None => continue,
        };
        let delta = match choice.delta.as_ref() {
            Some(d) => d,
            None => {
                if choice.finish_reason.is_some() {
                    finish_reason = choice.finish_reason.clone();
                    if chunk.usage.is_some() {
                        final_usage = chunk.usage.map(|u| kimi_usage_from_stream(&u));
                    }
                }
                continue;
            }
        };

        // Reasoning content
        if let Some(ref reasoning) = delta.reasoning_content
            && !reasoning.is_empty()
        {
            if thinking_index.is_none() {
                thinking_index = Some(next_block_index);
                next_block_index += 1;
                out.push(ReducerEvent::ThinkingStart {
                    index: thinking_index.unwrap(),
                });
            }
            out.push(ReducerEvent::ThinkingDelta {
                index: thinking_index.unwrap(),
                text: reasoning.clone(),
            });
        }

        // Content text
        if let Some(ref content) = delta.content
            && !content.is_empty()
        {
            // Close thinking before text
            if let Some(ti) = thinking_index.take() {
                out.push(ReducerEvent::ThinkingStop { index: ti });
            }
            if text_index.is_none() {
                text_index = Some(next_block_index);
                next_block_index += 1;
                out.push(ReducerEvent::TextStart {
                    index: text_index.unwrap(),
                });
            }
            out.push(ReducerEvent::TextDelta {
                index: text_index.unwrap(),
                text: content.clone(),
            });
        }

        // Tool calls
        if let Some(ref tool_calls) = delta.tool_calls
            && !tool_calls.is_empty()
        {
            // Close thinking and text before tools
            if let Some(ti) = thinking_index.take() {
                out.push(ReducerEvent::ThinkingStop { index: ti });
            }
            if let Some(ti) = text_index.take() {
                out.push(ReducerEvent::TextStop { index: ti });
            }

            for tc in tool_calls {
                let existing_pos = tool_slots.iter().position(|s| s.tc_index == tc.index);
                let block_index = if let Some(pos) = existing_pos {
                    tool_slots[pos].block_index
                } else {
                    let id = tc.id.clone().unwrap_or_default();
                    let name = tc
                        .function
                        .as_ref()
                        .and_then(|f| f.name.clone())
                        .unwrap_or_default();
                    if id.is_empty() || name.is_empty() {
                        continue;
                    }
                    saw_tool_calls = true;
                    let bi = next_block_index;
                    next_block_index += 1;
                    tool_slots.push(ToolSlot {
                        tc_index: tc.index,
                        block_index: bi,
                        id: id.clone(),
                        name: name.clone(),
                    });
                    out.push(ReducerEvent::ToolStart {
                        index: bi,
                        id,
                        name,
                    });
                    bi
                };

                if let Some(args) = tc.function.as_ref().and_then(|f| f.arguments.as_ref())
                    && !args.is_empty()
                {
                    out.push(ReducerEvent::ToolDelta {
                        index: block_index,
                        partial_json: args.to_string(),
                    });
                }
            }
        }

        // Finish reason on choice level
        if let Some(ref reason) = choice.finish_reason {
            finish_reason = Some(reason.clone());
            if chunk.usage.is_some() {
                final_usage = chunk.usage.map(|u| kimi_usage_from_stream(&u));
            }
        }
    }

    // Close any open blocks
    if let Some(ti) = thinking_index.take() {
        out.push(ReducerEvent::ThinkingStop { index: ti });
    }
    if let Some(ti) = text_index.take() {
        out.push(ReducerEvent::TextStop { index: ti });
    }
    for slot in tool_slots.iter() {
        out.push(ReducerEvent::ToolStop {
            index: slot.block_index,
        });
    }

    let stop_reason = match finish_reason.as_deref() {
        Some("length") => StopReason::MaxTokens,
        Some("tool_calls") => StopReason::ToolUse,
        _ if saw_tool_calls => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };

    out.push(ReducerEvent::Finish {
        stop_reason,
        usage: final_usage,
    });

    Ok(out)
}

fn kimi_usage_from_stream(u: &StreamUsage) -> KimiUsage {
    KimiUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        cached_tokens: u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .or(u.cached_tokens),
        reasoning_tokens: u
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
    }
}

pub fn map_usage_to_anthropic(u: &Option<KimiUsage>) -> AnthropicUsage {
    let usage = match u {
        Some(u) => u,
        None => return AnthropicUsage::default(),
    };
    let cached = usage.cached_tokens.unwrap_or(0);
    let total_prompt = usage.prompt_tokens.unwrap_or(0);

    // Subtract cached from input_tokens like reference does
    let input_tokens = total_prompt.saturating_sub(cached);
    AnthropicUsage {
        input_tokens,
        output_tokens: usage.completion_tokens.unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_maps_reasoning_text_tool_and_usage() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\"\"}}]}}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"rust\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReducerEvent::ThinkingDelta { text, .. } if text == "think"))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            ReducerEvent::Finish {
                stop_reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn reducer_handles_simple_text() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReducerEvent::TextStart { .. }))
        );
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ReducerEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hello", " world"]);
        assert!(events.iter().any(|e| matches!(
            e,
            ReducerEvent::Finish {
                stop_reason: StopReason::EndTurn,
                ..
            }
        )));
    }

    #[test]
    fn reducer_handles_max_tokens() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            ReducerEvent::Finish {
                stop_reason: StopReason::MaxTokens,
                ..
            }
        )));
    }

    #[test]
    fn reducer_ignores_invalid_json() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"valid\"}}]}\n\n",
            "data: not json\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
        );
        let events = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ReducerEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["valid"]);
    }

    #[test]
    fn reducer_returns_error_on_upstream_error() {
        let upstream = "data: {\"error\":{\"message\":\"rate limit exceeded\"}}\n\n";
        let result = reduce_upstream_bytes(upstream.as_bytes());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, UpstreamErrorKind::Failed);
    }

    #[test]
    fn map_usage_subtracts_cached() {
        let usage = KimiUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            cached_tokens: Some(20),
            ..Default::default()
        };
        let mapped = map_usage_to_anthropic(&Some(usage));
        assert_eq!(mapped.input_tokens, 80);
        assert_eq!(mapped.output_tokens, 50);
        assert_eq!(mapped.cache_read_input_tokens, 20);
    }
}
