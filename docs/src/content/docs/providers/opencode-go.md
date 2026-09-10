---
title: OpenCode Go
description: Configure an OpenCode Go API key, account usage, model routing, streaming, tools, and provider overrides.
---

OpenCode Go uses the API at `https://opencode.ai/zen/go/v1`. Its catalog spans
OpenAI-compatible chat completions, Anthropic-compatible messages, and OpenAI
Responses; the proxy selects the wire protocol for each registered model. Each
mapping comes from the [official OpenCode Go endpoint table](https://opencode.ai/docs/go/#endpoints)
or direct protocol verification against the API.

## Account and authentication

Subscribe to OpenCode Go, copy your API key, and provide it to the proxy:

```sh
export OPENCODE_API_KEY=YOUR_OPENCODE_GO_API_KEY
claude-code-proxy serve
```

`CCP_OPENCODE_API_KEY` takes precedence over `OPENCODE_API_KEY`. The
`opencode.apiKey` configuration key is also supported. The proxy does not
implement an OpenCode login flow.

To see the current percentage used and reset time for each account limit, run:

```sh
claude-code-proxy opencode usage
claude-code-proxy opencode usage --json
```

This fetches OpenCode Go's rolling five-hour, weekly, and monthly windows. The
endpoint is implemented by OpenCode but is not yet listed in its public API
table, so its response format may change.

The proxy also exposes the same limits in the standard Claude Code Router
account format. In Claude Code Router, enable **Fetch usage** for the proxy
provider and select **Standard usage endpoint**. The dashboard will discover
`/.well-known/ccr/account`; `/v1/account/limits` is available as a compatible
alias. These routes use the proxy's configured OpenCode key and ignore the
incoming placeholder key.

## Models

Run `claude-code-proxy models` for the statically registered catalog. Every
registered model has a provider-qualified form. Bare IDs are also accepted when
they do not belong to another provider:

```sh
ANTHROPIC_MODEL=opencode-go/glm-5.2 \
ANTHROPIC_SMALL_FAST_MODEL=opencode-go/glm-5.2 \
  claude --model opencode-go/glm-5.2
```

The bare IDs `gpt-5.6-luna`, `grok-4.5`, `grok-4.6`, `kimi-k3`, and `kimi-k2.6`
remain owned by the existing Codex, Grok, or Kimi providers. Prefix those IDs
with `opencode-go/` to select the OpenCode Go version.

## Tools and streaming

Claude function definitions, tool choices, tool calls, and tool results are
translated for chat-completions models. Tool-call argument fragments are
streamed incrementally and reassembled into Anthropic `tool_use` blocks.
Upstream tool behavior remains model-dependent.

Models served through the Anthropic-compatible endpoint retain their native
messages stream. Responses-backed models are translated to the same Anthropic
event stream as other providers. The proxy handles `/v1/messages/count_tokens`
locally and does not send that request to OpenCode Go.

## Configuration

- `CCP_OPENCODE_API_KEY`, `OPENCODE_API_KEY`, or `opencode.apiKey` supplies the key.
- `CCP_OPENCODE_BASE_URL` or `opencode.baseUrl` changes the API base URL.

OpenCode Go may expose additional model IDs through `/models`, but the proxy
registers only models whose wire protocol is documented or has been verified
against the live API. Unknown IDs are rejected locally. Access or usage-limit
errors for registered models are surfaced from OpenCode.
