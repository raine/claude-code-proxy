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

The registered IDs are `grok-composer-2.5-fast` and `grok-4.5`. Account and regional access can vary. Use the same concrete Grok ID for `ANTHROPIC_MODEL` and `ANTHROPIC_SMALL_FAST_MODEL`.

```sh
ANTHROPIC_MODEL=grok-4.5 \
ANTHROPIC_SMALL_FAST_MODEL=grok-4.5 \
  claude --model grok-4.5
```

## Reasoning and tools

The proxy translates Claude messages, function tools, tool results, thinking controls, token usage, and streaming events. Grok reasoning text appears as Claude Code thinking blocks.

Claude Code hosted search tools map to Grok-native tools:

- General web queries use hosted web search.
- X queries use hosted `x_search`.
- Citations and search usage return in Anthropic-compatible content and usage fields.

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

See [Configuration](/reference/configuration/) for defaults.

## Limitations and troubleshooting

A successful login does not guarantee every model is enabled for the account or region. Model rejection and upstream errors are surfaced to Claude Code. Use `grok auth status` for token state, inspect the failed request in the monitor, and use the structured log or error capture for the full redacted response.
