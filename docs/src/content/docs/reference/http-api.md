---
title: HTTP API
description: Canonical local HTTP routes for liveness, Anthropic Messages, token counting, model discovery, and optional Codex-backed OpenAI APIs.
---

The server speaks the Anthropic and OpenAI protocol subsets needed by Claude Code and Codex-backed OpenAI-compatible clients.

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

Codex, Kimi, and Grok use a local `gpt-tokenizer` estimate with `o200k_base`. Cursor estimates the rendered prompt from its character length. Counts support Claude Code compaction behavior and are estimates rather than provider billing values.

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

This route exists only when `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi` is true. It translates OpenAI Chat Completions requests into Codex Responses Lite requests. Incoming bearer credentials are accepted and replaced with the proxy's stored Codex authentication.

The compatibility surface supports:

- text messages with `system`, `developer`, `user`, and `assistant` roles
- streaming and buffered responses
- `reasoning_effort` values `none`, `low`, `medium`, `high`, `xhigh`, and `max`
- `response_format` values `text`, `json_object`, and strict `json_schema`
- `stream_options.include_usage`
- `temperature` and `top_p` on Codex models that use the full Responses lane
- `user` as a Responses safety identifier

Omitted reasoning effort defaults to `medium`. `CCP_CODEX_EFFORT` or `codex.effort` takes precedence over the request. Every translated request uses `store: false`, upstream streaming, and `reasoning.context: "all_turns"`.

A buffered response uses the standard `chat.completion` object. A streaming response emits `chat.completion.chunk` events and ends with `data: [DONE]`.

Function calls, hosted tools, images, audio, log probabilities, multiple choices, storage, and output token limits are outside this compatibility surface. Unsupported fields, including `max_tokens` and `max_completion_tokens`, return an OpenAI `invalid_request_error` with the field in `error.param`.

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

This route exists only when `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi` is true. It accepts a native OpenAI Responses request for a registered Codex model.

The proxy:

- validates the model against the Codex catalog
- replaces incoming auth with proxy-owned ChatGPT Codex auth
- refreshes rejected access tokens before forwarding a response
- preserves native JSON responses and SSE bodies
- records the request in the monitor and optional traffic capture

It does not implement stored response retrieval or deletion, image variations, or WebSocket client ingress.

## Other routes

Unmatched paths return the proxy's not-found response. The server has no administrative mutation API, credential API, or remote shutdown route.
