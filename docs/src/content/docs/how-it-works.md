---
title: How it works
description: Follow authentication, routing, protocol translation, streaming, session state, and diagnostics through claude-code-proxy.
---

claude-code-proxy exposes Anthropic Messages and optional OpenAI-compatible routes. Every route selects a provider from the request model and translates through that provider's native protocol.

<div class="route-rail" aria-label="claude-code-proxy request architecture">
  <div class="route-node"><strong>API client</strong><span>Anthropic Messages<br/>OpenAI Chat or Responses</span></div>
  <div class="route-arrow" aria-hidden="true">→</div>
  <div class="route-node"><strong>Proxy pipeline</strong><span>route model<br/>refresh auth<br/>translate events</span></div>
  <div class="route-arrow" aria-hidden="true">→</div>
  <div class="route-node provider-stack"><code>Codex Responses</code><code>Kimi Chat Completions</code><code>Grok Responses</code><code>Cursor Connect</code></div>
</div>

## Anthropic requests

1. Claude Code sends an Anthropic Messages request to `/v1/messages`.
2. The registry normalizes a trailing `[1m]`, resolves aliases, and selects a provider from the model ID.
3. The provider loads its proxy-owned credential and refreshes an expiring access token when needed.
4. The request translator maps system content, user and assistant messages, images, tools, tool results, thinking controls, and output settings into the upstream shape.
5. The upstream stream is reduced into typed text, thinking, tool, usage, and completion events.
6. The proxy emits Anthropic SSE events or accumulates a non-streaming Anthropic response.
7. The monitor, JSONL logger, and optional traffic capture record operational details.

## OpenAI-compatible requests

Enable the OpenAI routes to use `/v1/chat/completions` or `/v1/responses`. The `model` field chooses Codex, Kimi, Grok, or Cursor in the same way it does on `/v1/messages`.

Codex Responses requests go directly to the native Codex API. The proxy translates other OpenAI requests to the selected provider and returns either Chat Completions or Responses output. Streaming and non-streaming requests use the same translation rules, and unsupported fields return an error instead of being ignored.

## Authentication boundary

Each provider login belongs to claude-code-proxy. The proxy does not read native Codex, Grok, or Cursor Agent credentials. Credentials live in the platform credential store described in [Files and storage](/reference/files-and-storage/). Incoming `ANTHROPIC_AUTH_TOKEN` values are accepted for client compatibility and are not used as upstream credentials.

## Routing boundary

Routing happens per request, not per server process or API surface. Codex IDs, Kimi IDs, Grok IDs, Cursor prefixes, and configured Anthropic-style aliases can share one listener across `/v1/messages`, `/v1/chat/completions`, and `/v1/responses`. Unknown model IDs return HTTP 400 with the supported catalog.

## Session state

Claude Code sends `x-claude-code-session-id`. The proxy uses it for monitor grouping and provider features that need continuity. Cursor conversation IDs, optional Codex `previous_response_id`, and optional Codex server compaction state live in memory. A proxy restart clears that state and portable Claude Code history remains the fallback.

## Count tokens

`POST /v1/messages/count_tokens` stays local and does not make an upstream request. Codex uses `o200k_base` for text plus estimates for images, encrypted reasoning, and protocol framing. Other providers use their own local estimators. The result supports Claude Code's compaction decisions but is not a provider billing count.

See [HTTP API](/reference/http-api/) for route contracts and [Compatibility and limitations](/reference/compatibility-and-limitations/) for translation boundaries.
