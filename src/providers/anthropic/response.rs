use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use http::{HeaderMap, header};
use serde_json::Value;

use crate::{
    openai_compat::stream::SseDecoder,
    provider::{RequestContext, ResponseOutcome},
};

use super::headers::forwarded_headers;

const MAX_JSON_OBSERVATION_BYTES: usize = 2 * 1024 * 1024;

enum Observation {
    Events {
        decoder: SseDecoder,
        completed: bool,
    },
    Json(Vec<u8>),
    Opaque,
}

impl Observation {
    fn for_headers(headers: &HeaderMap) -> Self {
        if headers
            .get(header::CONTENT_ENCODING)
            .is_some_and(|encoding| encoding != "identity")
        {
            return Self::Opaque;
        }
        let media_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        match media_type {
            Some("text/event-stream") => Self::Events {
                decoder: SseDecoder::default(),
                completed: false,
            },
            Some("application/json") => Self::Json(Vec::new()),
            _ => Self::Opaque,
        }
    }

    fn receive(&mut self, bytes: &[u8], ctx: &RequestContext, outcome: &ResponseOutcome) {
        match self {
            Self::Events { decoder, completed } => {
                match decoder.push(bytes) {
                    Ok(events) => {
                        for event in events {
                            if event.data.get("type").and_then(Value::as_str)
                                == Some("message_stop")
                            {
                                *completed = true;
                            }
                            observe_value(&event.data, ctx, outcome);
                        }
                    }
                    Err(_) => {
                        // Observation must never alter the relayed bytes or grow without bound.
                        *self = Self::Opaque;
                    }
                }
                if let Some(monitor) = &ctx.monitor {
                    monitor.stream_progress(&ctx.req_id, bytes.len() as u64, 1, None, None);
                }
            }
            Self::Json(buffer)
                if buffer.len().saturating_add(bytes.len()) <= MAX_JSON_OBSERVATION_BYTES =>
            {
                buffer.extend_from_slice(bytes);
            }
            Self::Json(_) => *self = Self::Opaque,
            Self::Opaque => {}
        }
    }

    fn finish(self, ctx: &RequestContext, outcome: &ResponseOutcome) {
        match self {
            Self::Json(buffer) => {
                if let Ok(value) = serde_json::from_slice(&buffer) {
                    observe_value(&value, ctx, outcome);
                }
            }
            Self::Events {
                completed: false, ..
            } => {
                outcome.fail("Anthropic stream ended before message_stop".into());
            }
            _ => {}
        }
    }
}

fn observe_value(value: &Value, ctx: &RequestContext, outcome: &ResponseOutcome) {
    if value.get("type").and_then(Value::as_str) == Some("error") {
        outcome.fail(
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Anthropic stream error")
                .to_owned(),
        );
    }
    if let Some(monitor) = &ctx.monitor {
        for usage in [value.get("usage"), value.pointer("/message/usage")]
            .into_iter()
            .flatten()
        {
            monitor.usage_updated(
                &ctx.req_id,
                usage.get("input_tokens").and_then(Value::as_u64),
                usage.get("output_tokens").and_then(Value::as_u64),
            );
        }
        if let Some(tokens) = value.get("input_tokens").and_then(Value::as_u64) {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        if value.get("type").and_then(Value::as_str) == Some("message_start") {
            monitor.generation_started(&ctx.req_id);
        }
    }
}

pub(super) fn relay(upstream: reqwest::Response, ctx: RequestContext) -> Response {
    let observation = Observation::for_headers(upstream.headers());
    let outcome = ResponseOutcome::default();
    let response = http::Response::builder().status(upstream.status());
    let headers = forwarded_headers(upstream.headers());
    let stream = futures_util::stream::unfold(
        (upstream.bytes_stream(), observation, ctx, outcome.clone()),
        |(mut upstream, mut observation, ctx, outcome)| async move {
            match upstream.next().await {
                Some(chunk) => {
                    if let Ok(bytes) = &chunk {
                        observation.receive(bytes, &ctx, &outcome);
                    }
                    Some((chunk, (upstream, observation, ctx, outcome)))
                }
                None => {
                    observation.finish(&ctx, &outcome);
                    None
                }
            }
        },
    );
    let response = headers.iter().fold(response, |response, (name, value)| {
        response.header(name, value)
    });
    response
        .extension(outcome)
        .body(Body::from_stream(stream))
        .expect("valid upstream response")
}
