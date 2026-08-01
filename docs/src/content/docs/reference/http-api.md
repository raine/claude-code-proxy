---
title: HTTP API
description: Local routes for health checks, Anthropic Messages, token counts, model discovery, OpenAI-compatible requests, and Codex images.
---

The server exposes the Anthropic and OpenAI routes supported by the proxy. Each route uses the stored login for the model's provider.

<div class="security-callout">
<strong>No client authentication.</strong> The listener accepts requests without validating `Authorization` or `x-api-key`. Loopback is the default. Protect every non-loopback listener with a firewall or authenticating reverse proxy.
</div>

## `GET /healthz`

Liveness check:

```json
{"ok":true}
```

It does not verify provider credentials or upstream availability.

## `POST /v1/messages`

Accepts an Anthropic Messages request in streaming or non-streaming mode. `POST /v1/messages?beta=true` reaches the same route.

The request `model` selects the provider. The proxy translates supported message content, system prompts, thinking settings, tool definitions, tool choice, tool calls, tool results, images, output configuration, metadata, and streaming behavior according to the provider.

Streaming responses use Anthropic SSE events such as `message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, and `message_stop`. Non-streaming requests are accumulated from the provider's stream.

Unknown models return HTTP 400 with the supported catalog. Missing provider auth returns HTTP 401.

## `POST /v1/messages/count_tokens`

Accepts the same basic Anthropic request shape and returns:

```json
{"input_tokens":1234}
```

Codex tokenizes text locally with `o200k_base` and adds estimates for images, encrypted reasoning, and protocol framing. Kimi and Grok use local text heuristics, while Cursor estimates the rendered prompt from its character length. Counts support Claude Code compaction behavior and are estimates rather than provider billing values.

## `GET /v1/models`

Returns Anthropic-shaped model discovery:

```json
{
  "data": [
    {
      "type": "model",
      "id": "gpt-5.6-sol",
      "display_name": "gpt-5.6-sol (codex)"
    }
  ],
  "has_more": false,
  "first_id": "gpt-5.6-sol",
  "last_id": "cursor:gpt-5.5"
}
```

An optional `limit` query truncates `data` and sets `has_more`. The route does not expose a pagination cursor.

Claude Code gateway discovery filters IDs according to its own model rules. See [Models and routing](/using/models-and-routing/).

## `POST /v1/chat/completions`

Enable this route with `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi: true`. The `model` field selects Codex, Kimi, Grok, or Cursor. The proxy ignores incoming bearer credentials and uses the stored login for that provider.

```sh
curl http://127.0.0.1:18765/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"kimi-k2.6","messages":[{"role":"user","content":"Hello"}]}'
```

For Kimi, Grok, and Cursor, the route accepts:

- `system`, `developer`, `user`, `assistant`, and `tool` messages
- text, supported user images, function calls, and tool results
- function tools and `tool_choice`
- `max_tokens` or `max_completion_tokens`
- `reasoning_effort`
- streaming, non-streaming responses, and `stream_options.include_usage`

Codex uses its own compatibility path. It supports text messages, `reasoning_effort`, `response_format`, `stream_options.include_usage`, `temperature`, `top_p`, and `user`. Reasoning effort defaults to `medium`, and the proxy-wide Codex effort setting takes precedence. Function calls, images, audio, log probabilities, multiple choices, storage, and output token limits are not supported on the Codex Chat Completions path.

Non-streaming requests return a `chat.completion` object. Streaming requests return `chat.completion.chunk` events followed by `data: [DONE]`. Grok citations are included in message annotations.

Unsupported non-null fields return `invalid_request_error` with the field named in `error.param`. Cursor tools are limited to `Read`, `Write`, and `Bash`. They require `stream: true` and a stable session header.

## `POST /v1/images/generations`

This route exists only when `CCP_CODEX_IMAGES_API=1` or `codex.imagesApi` is true. It reuses the proxy-owned ChatGPT/Codex OAuth session and forwards a bounded JSON request to the Codex image service:

```json
{
  "prompt": "A paper-cut fox in a moonlit forest",
  "model": "gpt-image-2",
  "background": "auto",
  "quality": "auto",
  "size": "auto"
}
```

`prompt` is required. `model` defaults to and is restricted to `gpt-image-2`; `background`, `quality`, and `size` default to `auto`. Optional `n` must be between 1 and 10. Unknown fields and URL response formats are rejected rather than silently forwarded. Successful responses contain `data[].b64_json`; the proxy never writes generated image data to traffic captures.

## `POST /v1/images/edits`

This route uses the same opt-in gate and accepts either:

- Codex JSON with `images: [{"image_url":"data:image/png;base64,..."}]`; or
- OpenAI-style `multipart/form-data` with one to five repeated `image` or `image[]` files and text fields `prompt`, `model`, `background`, `quality`, `size`, and `n`.

Multipart PNG, JPEG, WebP, and GIF signatures are validated and translated to Codex data URLs. The internal Codex edit contract is JSON, so multipart is an ingress compatibility adapter. Masks, remote image URLs, variations, unsupported fields, and other media types return a 4xx OpenAI error. Request bodies, individual files, aggregate inputs, responses, and concurrency are bounded to protect the proxy process.

The Images API is an internal ChatGPT Codex integration, not the public OpenAI Platform Images API. It consumes the signed-in ChatGPT account's entitlement and quota, and the internal contract can change independently of the public API.

## `POST /v1/responses`

Enable this route with `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi: true`. The `model` field selects Codex, Kimi, Grok, or Cursor.

```sh
curl http://127.0.0.1:18765/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"grok-4.5","input":"Hello"}'
```

Codex models use native Responses passthrough, including native JSON and SSE output. For Kimi, Grok, and Cursor, the route accepts:

- string input or message items
- `instructions`
- function calls, function-call outputs, tools, and `tool_choice`
- `max_output_tokens`
- `reasoning.effort`
- streaming or non-streaming output

Responses include the accepted tool settings. Grok search appears as a `web_search_call`, with sources in URL citation annotations. When a provider reaches its output token limit, the response status is `incomplete` and the reason is `max_output_tokens`.

`store: true` and other unsupported non-null fields return an error. Stored response retrieval, deletion, and WebSocket client connections are not supported.

## OpenAI routing, sessions, and errors

Both OpenAI routes strip a trailing `[1m]`, resolve configured aliases, and choose the provider from `model`. Aliases follow `aliasProvider`, while explicit provider model IDs keep their provider. Unknown models return HTTP 400 with the supported model list.

For requests that need a stable session, set one of these headers:

- `x-claude-code-session-id`
- `session_id`
- `x-client-request-id`

The proxy uses the first non-empty value. Cursor tool calls require a stable session. Session affinity also applies to providers that support it.

Authentication, permission, rate-limit, invalid-request, and provider failures use the OpenAI error format. Rate-limit responses keep the provider's `Retry-After` header. Malformed or oversized provider streams return a gateway error.

The monitor and logs show the selected provider and model. Traffic capture records request and response data for debugging, including prompts and tool content, so treat the capture directory as sensitive.

## Other routes

Unmatched paths return the proxy's not-found response. The server has no administrative mutation API, credential API, or remote shutdown route.
