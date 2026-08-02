---
title: Codex
description: Configure ChatGPT Codex authentication, models, reasoning, tools, images, transports, continuation, compaction, and OpenAI-compatible APIs.
---

Codex uses the ChatGPT subscription Responses endpoint at `https://chatgpt.com/backend-api/codex/responses`.

## Account and authentication

Sign in with a **ChatGPT Plus or Pro account**, not OpenAI API credentials.

```sh
claude-code-proxy codex auth login
# Headless device-code flow
claude-code-proxy codex auth device
claude-code-proxy codex auth status
```

The proxy owns its tokens and does not read native Codex CLI credentials. It refreshes expiring access tokens with a single-flight guard. See [Files and storage](/reference/files-and-storage/) for credential locations.

## Models and fast mode

Use `claude-code-proxy models` as the current catalog. Model access depends on your ChatGPT account. A model rejected by the subscription produces the upstream error verbatim.

Append `-fast` to any registered Codex model to request `service_tier: "priority"`. For example, `gpt-5.6-sol-fast` selects `gpt-5.6-sol` with fast service. `CCP_CODEX_SERVICE_TIER` or `codex.serviceTier` takes precedence.

## Reasoning

Claude Code's `/effort` value maps to Codex `reasoning.effort`: `low`, `medium`, `high`, `xhigh`, or `max`. A proxy override can also force `none`.

When reasoning is enabled, the proxy requests an automatic reasoning summary and translates summary deltas into Claude Code thinking blocks. Codex may omit a summary for a simple prompt. `CCP_CODEX_REASONING_SUMMARY=off` suppresses summaries while preserving effort and encrypted continuation content.

Claude Code summary compaction requests are capped at low effort by default because they perform extraction over a large transcript. `CCP_COMPACT_EFFORT=off` disables the cap, `none` removes reasoning, and another valid effort sets a different maximum. The cap never raises effort.

## Tools and multimodal input

- Claude function tools and tool results map to Responses API function calls and outputs.
- Claude Code's forced `web_search_20250305` subrequest uses Codex's standalone
  `/alpha/search` endpoint. It keeps the resolved model, omits search reasoning,
  and preserves non-empty domain filters, so Luna searches do not require a Sol
  Responses turn. Automatic hosted-search requests remain on the full Responses
  API because the standalone endpoint cannot decide whether to invoke a tool.
  Structured result DTOs map back to Anthropic `server_tool_use` and
  `web_search_tool_result` blocks, while standalone text output remains text.
  The proxy locally estimates input and output tokens and reports search usage.
- Top-level base64 user images map to `input_image`.
- Supported base64 images nested in tool results also map to `input_image`.
- Remote image URLs, malformed images, and unsupported tool-result image forms remain textual placeholders.
- Strict JSON schema output maps to Responses `text.format`.

## Transport and continuation

WebSocket is the default transport. Set `CCP_CODEX_TRANSPORT=http` for HTTP SSE, or `auto` to use WebSocket with HTTP fallback only when setup fails before a request is sent.

WebSocket setup honors `HTTP_PROXY` for `ws://`, `HTTPS_PROXY` for the default `wss://` endpoint, `ALL_PROXY` as a fallback, and `NO_PROXY` exclusions. A normal HTTP proxy can therefore carry the default WebSocket connection with CONNECT; TUN mode is not required. Set proxy variables before starting the process and restart after changing them. For example, setting `HTTPS_PROXY` to `http://127.0.0.1:7890` sends HTTPS/WSS destinations through the HTTP proxy at port 7890; it does not require an `https://` proxy URL.

`CCP_CODEX_PREVIOUS_RESPONSE_ID=1` enables append-only WebSocket continuation. A valid identity containing only a Claude Code session ID owns the Main continuation for that session. Each valid direct Agent ID owns an independent continuation and reusable WebSocket within the same session. Nested Agents are keyed by their direct child ID; the parent ID is validated but does not become part of the owner key. The proxy sends `previous_response_id` only when the translated request shape and transcript extension are safe, and only on the exact live WebSocket that produced that response.

An absent, malformed, or ambiguous identity does not reject the HTTP request; that request proceeds without continuation or WebSocket reuse. If the originating socket is missing, dead, or has been replaced, the proxy retries once with the full translated input and without the stale response ID. Continuation and connection state is held only in memory and is lost when the proxy restarts.

Detected auto-review classifier subrequests are intentionally stateless even when valid session and Agent headers are present. They neither consume nor publish continuation or WebSocket ownership.

## Server compaction

Claude Code normally compacts a long conversation by asking the active model to write a portable text summary. Later turns contain that summary instead of the original transcript. This works across providers, but a prose summary can flatten details from a long, tool-heavy Codex session.

Codex server compaction preserves the same boundary in a model-native form. Codex returns an opaque encrypted `compaction` item representing the earlier Responses history. On later turns, the proxy gives that item back to Codex together with selected recent messages and everything added after the boundary. Claude Code still receives its normal portable summary, so the session has a safe fallback.

This is most useful for long coding sessions where continuity after `/compact` or automatic compaction matters. It does not increase the model's context window or prevent Claude Code from compacting. The boundary also takes longer because it adds one Codex request.

### How it works

1. Claude Code reaches a manual or automatic compaction boundary.
2. The proxy sends the translated conversation to Codex with a trailing `compaction_trigger`.
3. Codex returns an encrypted `compaction` item, which the proxy keeps in memory for that Claude Code session and model.
4. Claude Code completes its normal summary request. The proxy uses the resulting summary as an exact anchor.
5. On subsequent matching turns, the proxy replaces the portable summary with the encrypted item, retained recent context, and post-compaction messages.

The encrypted item remains opaque to the proxy. It is stored only in memory and sent back to Codex as native Responses input.

### Enable server compaction

Server compaction is disabled by default. Enable it in `config.json`:

```json
{
  "codex": {
    "serverCompaction": true
  }
}
```

Or enable it for one proxy process:

```sh
CCP_CODEX_SERVER_COMPACTION=1 claude-code-proxy serve
```

### Fallbacks and visibility

Replay requires the same Claude Code session and Codex model with append-only history. A branch, proxy restart, provider or model change, malformed response, upstream failure, memory limit, or 30 minutes without matching activity discards the native state and uses Claude Code's portable summary instead.

While the native request is active, the monitor shows `compacting`. Structured log events named `server_compaction_triggered`, `server_compaction_completed`, and `server_compaction_failed` report each attempt and outcome.

## OpenAI-compatible APIs

`CCP_CODEX_RESPONSES_API=1` enables both `POST /v1/responses` and `POST /v1/chat/completions`. The setting is under Codex configuration, but the routes also accept Kimi, Grok, OpenCode Go, and Cursor models.

The Responses route preserves native JSON or SSE response bodies for registered Codex models. The Chat Completions route translates standard text messages, reasoning effort, JSON object or JSON Schema output, and buffered or streaming responses. Its omitted reasoning effort defaults to `medium`; the proxy-wide Codex effort override still takes precedence.

The proxy replaces incoming credentials with stored Codex auth for both routes. Response retrieval or deletion, function calling through Chat Completions, and WebSocket ingress are outside their scope. See [HTTP API](/reference/http-api/) for supported Chat Completions fields and error behavior.

## Images API

`CCP_CODEX_IMAGES_API=1` separately enables `POST /v1/images/generations` and `POST /v1/images/edits`. The routes reuse the proxy's stored ChatGPT OAuth session and target the ChatGPT Codex image backend; no OpenAI Platform API key is required.

```sh
CCP_CODEX_IMAGES_API=1 claude-code-proxy serve
```

The model defaults to and is restricted to `gpt-image-2`. Generation accepts JSON. Editing accepts either Codex JSON data URLs or OpenAI-style multipart uploads, which the proxy validates and converts into the Codex JSON contract. Results are returned as `data[].b64_json`. Masks, remote URLs, URL-formatted output, and image variations are not supported.

This is an internal ChatGPT Codex interface rather than the public Platform Images API. It consumes the signed-in account's image quota and can change without public API compatibility guarantees. Image prompts, uploads, generated base64, and upstream error bodies are excluded from traffic captures and persistent error diagnostics.

Because callers are not authenticated, binding to a LAN address lets every firewall-admitted host consume the signed-in account's quota. Restrict the listener to a trusted interface/subnet and never expose it through router forwarding, UPnP, a public tunnel, or permissive IPv6 rules.

See [Configuration](/reference/configuration/) for every Codex setting and [Troubleshooting](/using/troubleshooting/) for auth, model, and transport failures.
