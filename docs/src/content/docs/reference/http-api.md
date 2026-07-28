---
title: HTTP API
description: Canonical local HTTP routes for liveness, Anthropic Messages, token counting, model discovery, and optional native Codex Responses passthrough.
---

The server speaks the subset of Anthropic and OpenAI protocols needed by Claude Code and the optional Codex native passthrough.

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

## `POST /v1/responses`

This route exists only when `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi` is true. It accepts a native OpenAI Responses request for a registered Codex model.

The proxy:

- validates the model against the Codex catalog
- replaces incoming auth with proxy-owned ChatGPT Codex auth
- refreshes rejected access tokens before forwarding a response
- preserves native JSON responses and SSE bodies
- records the request in the monitor and optional traffic capture

It does not implement Images API, stored response retrieval or deletion, or WebSocket client ingress.

## Other routes

Unmatched paths return the proxy's not-found response. The server has no administrative mutation API, credential API, or remote shutdown route.
