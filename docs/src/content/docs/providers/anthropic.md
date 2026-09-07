---
title: Anthropic
description: Keep Claude models on Anthropic while switching other models through the proxy in the same Claude Code session.
---

Anthropic passthrough is opt-in. Set `aliasProvider` to `anthropic` in `config.json`, or start the proxy with:

```sh
CCP_ALIAS_PROVIDER=anthropic claude-code-proxy serve
```

This sends `claude-*` models and the built-in `haiku`, `sonnet`, `opus`, and `fable` aliases to `api.anthropic.com`. Explicit GPT, Kimi, Grok, OpenCode Go and Cursor IDs retain their provider. A preceding GPT or Kimi turn does not change where the Claude aliases go. Restart the proxy after changing `aliasProvider`.

## Authentication and model switching

Sign in using Claude Code. The proxy forwards the credentials Claude Code includes in each request; it does not read the keychain, store Anthropic credentials, or refresh them. Codex continues to use its own `claude-code-proxy codex auth login`.

Launch Claude Code using its existing subscription login:

```sh
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_API_KEY \
  ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  claude --model claude-opus-5
```

Also remove dummy credentials from Claude Code settings if configured there. `ANTHROPIC_AUTH_TOKEN=unused` replaces the subscription login and causes the Anthropic route to fail. Deliberately configured Anthropic API credentials are forwarded too.

Use `/model gpt-6-astra` and `/model claude-opus-5` to switch between the providers. Availability still depends on the selected provider account. The model catalog advertises known aliases, but explicit new `claude-*` IDs can be forwarded without a proxy release.

## Thinking and conversation history

Native Anthropic thinking and `redacted_thinking` blocks remain unchanged, including signatures. Requests with no local model rewrite or foreign thinking conversion retain their original bytes, including unknown fields, cache controls and beta headers.

When switching from GPT to Claude, unsigned thinking and thinking with proxy-owned `ccp:` signatures become `<previous_reasoning>` text blocks containing only the visible summary. When switching from Claude to GPT, visible thinking summaries similarly become text; opaque Claude signatures and redacted content are not sent as Codex reasoning. Valid native Codex reasoning continues to replay in its encrypted form on Codex turns. Responses are streamed unchanged, so the conversation keeps the original provider blocks for a later switch back.

This carries visible summaries across providers, not encrypted internal reasoning. Switch after a completed turn. Changing providers during an unfinished tool-use turn can still fail the receiving provider's history validation.

## HTTP behavior and monitor

`POST /v1/messages` and `POST /v1/messages/count_tokens` preserve query parameters and forward Anthropic authentication and beta headers. Local aliases and the `[1m]` suffix are resolved before forwarding. Redirects are not followed. Upstream status codes, request IDs, retry headers, response bodies and SSE events are preserved; connection-specific headers are removed.

Anthropic requests appear in the existing monitor, including usage and streamed errors. The provider does not expose Anthropic through the proxy's OpenAI-compatible endpoints. With `aliasProvider` set to `codex` (the default) or `kimi`, routing and session affinity keep their existing behavior.
