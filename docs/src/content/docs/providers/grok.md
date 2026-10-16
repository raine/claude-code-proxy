---
title: Grok
description: Configure grok.com authentication, Grok models, reasoning, function tools, hosted web and X search, citations, and provider overrides.
---

Grok uses the Responses endpoint at `https://cli-chat-proxy.grok.com/v1/responses`.

## Account and authentication

Use a **grok.com account**. Browser login uses S256 PKCE through `auth.x.ai` and an ephemeral loopback callback:

```sh
claude-code-proxy grok auth login
```

For a headless host, use the device-code flow:

```sh
claude-code-proxy grok auth device
claude-code-proxy grok auth status
```

The proxy owns and refreshes its Grok tokens. It does not read `~/.grok/auth.json`.

## Models

The registered IDs are `grok-composer-2.5-fast`, `grok-4.5`, `grok-4.6`, `grok-4.7`, and `grok-4.7-build-fast`, plus the local alias `grok-4.7-fast`, which the proxy sends as `grok-4.7-build-fast`. Account and regional access can vary. Use the same concrete Grok ID for `ANTHROPIC_MODEL` and `ANTHROPIC_SMALL_FAST_MODEL`.

```sh
ANTHROPIC_MODEL=grok-4.6 \
ANTHROPIC_SMALL_FAST_MODEL=grok-4.6 \
  claude --model grok-4.6
```

Grok 4.5 and Grok 4.6 have a 500,000 token context window. Claude Code treats unknown model IDs as 200,000 tokens, so set `CLAUDE_CODE_MAX_CONTEXT_TOKENS=500000` when using those IDs. Do not append `[1m]`; that is a one-million-token client hint.

The Grok 4.7 models (`grok-4.7`, `grok-4.7-build-fast`, and the `grok-4.7-fast` alias) run with a 256,000 token context window through the proxy, so set `CLAUDE_CODE_MAX_CONTEXT_TOKENS=256000` for them. The backend catalog also offers 500,000 tokens for these models, but the proxy has no way to select it, and a conversation that grows past 256,000 tokens fails upstream.

### Grok 4.7 Fast

`grok-4.7-fast` (sent as `grok-4.7-build-fast`) serves the same model on faster infrastructure.

- It costs twice the standard rate, or 1.5 times for long-context requests. Usage is billed to a Grok Build plan; the free tier does not include it, and an account without access receives the upstream error.
- `service_tier` does not select Fast. The backend treats it as priority processing at a higher price, so the Messages route accepts and drops it, and the OpenAI-compatible routes reject it as an unsupported field. Choose the model ID instead.
- Anthropic Messages responses report the requested name, such as `grok-4.7-fast`. OpenAI-compatible responses report the backend ID, `grok-4.7-build-fast`.

## Reasoning and tools

The proxy translates Claude messages, function tools, tool results, thinking controls, token usage, and streaming events. Grok reasoning text appears as Claude Code thinking blocks. Grok supports `none`, `low`, `medium`, and `high` effort levels. `xhigh` is forwarded for `grok-4.6`, `grok-4.7`, and `grok-4.7-build-fast`; higher compatibility levels are mapped to the highest supported Grok level for other registered models.

Search reaches Grok-native tools when the caller asks for it:

- Anthropic's `web_search_20250305` declaration maps to Grok hosted web search.
  The Grok CLI endpoint accepts the minimal declaration without domain or
  location constraints.
- A caller-managed search tool remains a function tool for the caller to run.
- An X or Twitter query is additionally offered hosted `x_search`, which the
  model can use or ignore alongside the caller's tools.
- Citations and search usage return in Anthropic-compatible usage fields.
- `CCP_GROK_HOSTED_SEARCH=1` enables a policy where hosted tools replace caller
  search tools and explicit search turns require a tool call.

A hosted search is reported as a text block naming the query.
`CCP_GROK_SEARCH_BLOCKS=native` preserves `server_tool_use` plus
`web_search_tool_result` or `x_search_tool_result` for clients that consume
hosted-tool blocks.

## OpenAI-compatible APIs

When the OpenAI routes are enabled, Grok models work with both `POST /v1/chat/completions` and `POST /v1/responses`. Text, reasoning, function tools, tool results, token limits, streaming, usage, and errors use the standard shape for the chosen route. Set `reasoning_effort` on Chat Completions or `reasoning.effort` on Responses. Responses requests also return Grok searches as `web_search_call` items. Citations are available on both routes.

## Multimodal support

`CCP_GROK_TOOL_IMAGE` controls image blocks in user messages and tool results:

- `omit`, the default, replaces each image with an `[image omitted: ...]`
  placeholder. The model does not receive the pixels.
- `reattach` keeps the placeholder in each tool result and sends accepted images
  in a following user message.
- `inline` sends accepted tool-result images alongside text as `input_image`
  parts. Text-only outputs retain their string shape.
- `reject` returns the image validation error used by older versions.

Vision modes accept base64 PNG, JPEG, and GIF images with a minimum side of 8
pixels, a minimum area of 512 square pixels, and a decoded size up to 5 MB. At
most the last four accepted images in a request are sent. Images that fail a
gate, WebP images, and remote URL sources degrade to placeholders with a reason.

Traffic captures redact Anthropic image data and upstream image data URLs.

## Configuration

- `CCP_GROK_BASE_URL` or `grok.baseUrl` changes the API base URL.
- `CCP_GROK_CLIENT_VERSION` or `grok.clientVersion` changes the client version header.
- `CCP_GROK_TOOL_IMAGE` selects `omit`, `reattach`, `inline`, or `reject`.
- `CCP_GROK_HOSTED_SEARCH` enables hosted search replacement and forcing.
- `CCP_GROK_SEARCH_BLOCKS` selects `text` or `native` hosted-search reporting.

See [Configuration](/reference/configuration/) for defaults.

## Limitations and troubleshooting

The Grok CLI hosted web-search endpoint has no equivalent for Anthropic's
`max_uses`, domain filters, or user location. The proxy accepts and omits a
valid `max_uses` value, while non-null domain and location constraints return a
request error rather than weakening the requested search scope.

A successful login does not guarantee every model is enabled for the account or region. Model rejection and upstream errors are surfaced to Claude Code. Use `grok auth status` for token state, inspect the failed request in the monitor, and use the structured log or error capture for the full redacted response.
