---
title: Anthropic Claude
description: Use the official Anthropic Messages API through claude-code-proxy with an API key.
---

The Anthropic provider forwards requests to the official Claude Messages API without translating tool calls, thinking blocks, signatures, or content blocks. It supports non-streaming JSON, live SSE, and upstream token counting.

## Credentials

Configure the **proxy process** using one of these sources, in precedence order:

1. `CCP_ANTHROPIC_API_KEY`
2. `ANTHROPIC_API_KEY`
3. `anthropic.apiKey` in `config.json`

Whitespace-only values are ignored. API usage is billed separately from Claude Pro/Max subscriptions. No browser or device login is required.

```sh
export CCP_ANTHROPIC_API_KEY="YOUR_ANTHROPIC_API_KEY"
claude-code-proxy anthropic auth status
claude-code-proxy serve
```

`auth status` checks whether a key is configured; it does not validate it or make a paid request. `auth logout` explains how to remove externally managed credentials; it does not modify environment variables or config files.

## Models and routing

Use an explicit namespace to avoid the existing Claude-style aliases:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=anthropic/claude-opus-5-5 \
ANTHROPIC_SMALL_FAST_MODEL=anthropic/claude-haiku-5-5 \
  claude
```

The proxy strips `anthropic/` and the optional `[1m]` client hint before sending the model upstream. Any explicit `anthropic/claude-...` model ID is accepted for routing; actual model availability is checked by Anthropic. The model catalog advertises Opus 5.5, Sonnet 5.5, Haiku 5.5, and Fable 5.1.

Existing bare Claude model aliases retain their configured routing. To route recognized aliases such as `opus`, `sonnet`, and `haiku` to Anthropic, set `CCP_ALIAS_PROVIDER=anthropic` in the proxy environment or `"aliasProvider": "anthropic"` in `config.json`. Explicit namespaced requests always select Anthropic, regardless of session affinity.

## Configuration

```json
{
  "aliasProvider": "anthropic",
  "anthropic": {
    "apiKey": "YOUR_ANTHROPIC_API_KEY",
    "baseUrl": "https://api.anthropic.com"
  }
}
```

Prefer environment-based secret management over storing keys in a file. `CCP_ANTHROPIC_BASE_URL` overrides `anthropic.baseUrl`; both root and `/v1` URLs are accepted. Only use trusted endpoints: the configured endpoint receives your key and prompts. Redirects are not followed.

`ANTHROPIC_BASE_URL` is deliberately **not** an upstream override: it configures the downstream client and may point back at the proxy.

## Protocol behavior

- The provider sends `x-api-key` using proxy-owned credentials. Downstream authorization, API keys, cookies, and unrelated headers are not forwarded.
- Native Messages and count-tokens routes forward `anthropic-version` and `anthropic-beta`. If no API version is supplied, the provider uses `2023-06-01` for compatibility with the proxy's existing request schema.
- Streaming passes upstream bytes through live while observing usage and recording traffic when enabled. Native error events are preserved; interrupted streams emit an Anthropic-shaped error event.
- Native request content and top-level parameters are preserved. The shared proxy message schema currently retains only `role` and `content` at message level; additional message-level fields are not supported.
- The provider does not inject thinking settings or refusal fallbacks, alter history, or silently substitute a different model. If `max_tokens` is omitted, it defaults to 64,000. Explicit limits are preserved.
- Token counting calls `/v1/messages/count_tokens`; generation-only parameters are omitted.
- Optional OpenAI-compatible routes use the existing proxy conversion and this provider's live Anthropic stream.
