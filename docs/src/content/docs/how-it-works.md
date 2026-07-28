---
title: How it works
description: Follow authentication, routing, protocol translation, streaming, session state, and diagnostics through claude-code-proxy.
---

claude-code-proxy exposes an Anthropic-compatible HTTP surface to Claude Code and translates each request into the selected provider's native protocol.

<div class="route-rail" aria-label="claude-code-proxy request architecture">
  <div class="route-node"><strong>Claude Code</strong><span>POST /v1/messages<br/>Anthropic SSE</span></div>
  <div class="route-arrow" aria-hidden="true">→</div>
  <div class="route-node"><strong>Proxy pipeline</strong><span>route model<br/>refresh auth<br/>translate events</span></div>
  <div class="route-arrow" aria-hidden="true">→</div>
  <div class="route-node provider-stack"><code>Codex Responses</code><code>Kimi Chat Completions</code><code>Grok Responses</code><code>Cursor Connect</code></div>
</div>

## Request lifecycle

1. Claude Code sends an Anthropic Messages request to `/v1/messages`.
2. The registry normalizes a trailing `[1m]`, resolves aliases, and selects a provider from the model ID.
3. The provider loads its proxy-owned credential and refreshes an expiring access token when needed.
4. The request translator maps system content, user and assistant messages, images, tools, tool results, thinking controls, and output settings into the upstream shape.
5. The upstream stream is reduced into typed text, thinking, tool, usage, and completion events.
6. The proxy emits Anthropic SSE events or accumulates a non-streaming Anthropic response.
7. The monitor, JSONL logger, and optional traffic capture record operational details.

## Authentication boundary

Each provider login belongs to claude-code-proxy. The proxy does not read native Codex, Grok, or Cursor Agent credentials. Credentials live in the platform credential store described in [Files and storage](/reference/files-and-storage/). Incoming `ANTHROPIC_AUTH_TOKEN` values are accepted for client compatibility and are not used as upstream credentials.

## Routing boundary

Routing happens per request, not per server process. Codex IDs, Kimi IDs, Grok IDs, Cursor prefixes, and configured Anthropic-style aliases can share one listener. Unknown model IDs return HTTP 400 with the supported catalog.

## Session state

Claude Code sends `x-claude-code-session-id`. The proxy uses it for monitor grouping and provider features that need continuity. Cursor conversation IDs, optional Codex `previous_response_id`, and optional Codex server compaction state live in memory. A proxy restart clears that state and portable Claude Code history remains the fallback.

## Count tokens

`POST /v1/messages/count_tokens` performs a local estimate with `gpt-tokenizer` and the `o200k_base` encoding. It supports Claude Code's compaction decisions without an upstream request.

See [HTTP API](/reference/http-api/) for route contracts and [Compatibility and limitations](/reference/compatibility-and-limitations/) for translation boundaries.
