---
title: Kimi
description: Configure Kimi Code authentication, model aliases, reasoning effort, tools, images, video, token refresh, and provider settings.
---

Kimi uses the OpenAI-style chat completions endpoint at `https://api.kimi.com/coding/v1/chat/completions`.

## Account and authentication

Use a **kimi.com account with Kimi Code access**. Authentication is an RFC 8628 device-code flow:

```sh
claude-code-proxy kimi auth login
claude-code-proxy kimi auth status
```

The login prints a verification URL and user code, then polls until authorization completes. Access tokens have a short lifetime and are refreshed before expiry. A persistent device ID is created with the Kimi credential because it is bound into the issued token.

## Model selection

The upstream wire model is `kimi-for-coding`, displayed by Kimi tooling as Kimi K2.6. The proxy also accepts `kimi-k2.6` and `k2.6` as aliases. Use `[1m]` as a Claude Code compaction hint only when the actual upstream context and your chosen threshold support it.

```sh
ANTHROPIC_MODEL=kimi-for-coding[1m]
ANTHROPIC_SMALL_FAST_MODEL=kimi-for-coding[1m]
```

## Reasoning

Claude Code's `/effort` setting maps to Kimi `reasoning_effort` at `low`, `medium`, or `high`. Returned reasoning streams into Claude Code thinking blocks. If the Anthropic request disables thinking, the proxy omits both Kimi reasoning controls.

## Tools and multimodal input

- Claude function tools and tool choice map to Kimi chat-completions tools.
- Tool calls and tool results round-trip through Anthropic content blocks.
- Image input maps to Kimi image content.
- Images inside tool results map to `image_url` parts in tool messages.
- Supported video input maps to Kimi video content.

## Configuration

`CCP_KIMI_BASE_URL`, `CCP_KIMI_OAUTH_HOST`, and `CCP_KIMI_USER_AGENT` provide explicit endpoint and client overrides. `CCP_USER_AGENT` is the fallback user-agent override. These settings are intended for compatibility and controlled debugging. See [Configuration](/reference/configuration/) for defaults and file keys.

## Limitations and troubleshooting

Account access and rate limits come from Kimi. HTTP 429 is returned to Claude Code with `retry-after`. If login succeeds but requests return 401, inspect token expiry with `kimi auth status`, then log in again. Preserve the Kimi device ID alongside the credential when copying a configuration directory.

The proxy estimates count-tokens locally rather than asking Kimi. See [Compatibility and limitations](/reference/compatibility-and-limitations/) for shared boundaries.
