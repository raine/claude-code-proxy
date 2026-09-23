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

The registered IDs are `grok-composer-2.5-fast`, `grok-4.5`, and `grok-4.6`. Account and regional access can vary. Use the same concrete Grok ID for `ANTHROPIC_MODEL` and `ANTHROPIC_SMALL_FAST_MODEL`.

```sh
ANTHROPIC_MODEL=grok-4.6 \
ANTHROPIC_SMALL_FAST_MODEL=grok-4.6 \
  claude --model grok-4.6
```

Grok 4.5 and Grok 4.6 have a 500,000 token context window. Claude Code treats unknown model IDs as 200,000 tokens, so set `CLAUDE_CODE_MAX_CONTEXT_TOKENS=500000` when using those IDs. Do not append `[1m]`; that is a one-million-token client hint.

## Reasoning and tools

The proxy translates Claude messages, function tools, tool results, thinking controls, token usage, and streaming events. Grok reasoning text appears as Claude Code thinking blocks. Grok supports `none`, `low`, `medium`, and `high` effort levels. `xhigh` is forwarded for `grok-4.6`; higher compatibility levels are mapped to the highest supported Grok level for other registered models.

Search reaches Grok-native tools when the caller asks for it:

- Anthropic `web_search_20250305` maps to Grok hosted web search.
  Claude Code sends Anthropic search options. The proxy maps them onto Grok
  where it can.
  - Mapped automatically:
    - Nested `user_location` copies onto Grok `web_search.user_location`.
    - Anthropic constraints `allowed_domains` or `blocked_domains`, up to 5 names
      per list, map onto Grok `filters`. Anthropic `blocked_domains` becomes Grok
      `excluded_domains`. Grok only accepts one of these at the same time.
      Specifying both lists on one tool returns HTTP 400.

  - Emulated via Grok's system prompt (`instructions`):
    - A domain list over 5 names. Always copied into `instructions`.
    - A non-object `user_location`, and unknown hosted keys. These follow
      `CCP_SEARCH_CONSTRAINTS`:
      - `system_prompt` (default; old `soft`) copies them into `instructions`.
      - `warning` drops them and logs.
      - `reject` (old `hard`) returns HTTP 400.

  - Not supported:
    - Anthropic `max_uses`. Grok has no field for it. The proxy drops it with
      a warning.
    - Grok `enable_image_understanding` on hosted web search. Claude Code
      hosted search is text-only, so the proxy does not send this flag.

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
- `CCP_GROK_TOOL_IMAGE` selects `omit` (default), `reattach`, `inline`, or
  `reject`. Native Claude Code sends pasted images. `omit` strips pixels.
  Use `inline` to match that chat behaviour.
- `CCP_GROK_HOSTED_SEARCH` enables hosted search replacement and forcing.
- `CCP_GROK_SEARCH_BLOCKS` selects `text` or `native` hosted-search reporting.
- `CCP_SEARCH_CONSTRAINTS` selects `system_prompt` (default; old `soft`),
  `warning`, or `reject` (old `hard`) for options that can go through the
  Grok `instructions` field. Native mapping and `max_uses` do not follow
  this setting.

See [Configuration](/reference/configuration/) for defaults.

## Limitations and troubleshooting

Hosted Grok web search has no `max_uses` field, so that Anthropic option is
dropped. See the mapping story above for what maps automatically and what can
go through the Grok `instructions` field.

A successful login does not guarantee every model is enabled for the account or region. Model rejection and upstream errors are surfaced to Claude Code. Use `grok auth status` for token state, inspect the failed request in the monitor, and use the structured log or error capture for the full redacted response.
