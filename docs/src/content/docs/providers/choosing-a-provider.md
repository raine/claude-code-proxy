---
title: Choosing a provider
description: Compare claude-code-proxy providers by account, protocol, models, reasoning, multimodal input, tools, and operational tradeoffs.
---

The default `serve` process supports the existing providers together. The opt-in
`siwc serve` process selects an isolated official ChatGPT plan-usage provider.
Choose based on your account, model access, and the capabilities your work needs.

| Provider | Account | Upstream protocol | Model selection | Notable capabilities |
| --- | --- | --- | --- | --- |
| [Sign in with ChatGPT](/providers/chatgpt-siwc/) | Eligible ChatGPT account with explicit plan permission | Public Responses API over HTTP SSE | Live account-specific catalog | Local function tools, image input, isolated loopback service |
| [Codex](/providers/codex/) | ChatGPT Plus or Pro | OpenAI Responses over WebSocket or HTTP SSE | Named Codex catalog, `-fast` and `-ultrafast` variants | Function tools, image input, hosted web search, reasoning summaries, optional native Responses route |
| [Kimi](/providers/kimi/) | kimi.com with Kimi Code access | OpenAI-style chat completions | `kimi-for-coding` and aliases | Function tools, reasoning, image and video input |
| [Grok](/providers/grok/) | grok.com | Responses API | `grok-composer-2.5-fast`, `grok-4.5`, `grok-4.6` | Function tools, reasoning, web search, X search, citations |
| [OpenCode Go](/providers/opencode-go/) | OpenCode Go subscription | OpenAI-compatible Chat Completions, OpenAI Responses, or Anthropic-compatible Messages | Non-conflicting bare IDs and `opencode-go/<model-id>` forms | Curated models tested and benchmarked for coding-agent use |
| [Cursor Agent](/providers/cursor-agent/) | Cursor account | HTTP/2 Connect stream | Cursor modes and `cursor:<model-id>` prefixes | Dynamic model catalog, effort variants, images, plan and ask modes, session continuation |

## Practical guidance

- Choose **Sign in with ChatGPT** for OpenAI's documented OAuth and public Responses flow in a local open-source tool.
- Start with **Codex** when you have a ChatGPT subscription and want the most developed Claude Code translation path.
- Choose **Kimi** for the Kimi Code model and multimodal coding input.
- Choose **Grok** for Grok models and hosted web or X search.
- Choose **OpenCode Go** when you have a Go subscription and want its documented model catalog in Claude Code.
- Choose **Cursor Agent** when you want Cursor's model catalog and agent modes. It depends on an installed Cursor Agent bundle for protobuf schemas.

## Shared behavior

All providers route by model ID, use proxy-owned credentials, refresh tokens when supported, stream responses, translate Claude Code tool definitions, and report failures through the same Anthropic-shaped API.

<div class="security-callout">
<strong>Account policy matters.</strong> Provider subscriptions, model access, regional availability, rate limits, and rules for unofficial clients can change. Review the terms for your account before using a provider through the proxy.
</div>

Use `claude-code-proxy models` for the default registry and `claude-code-proxy models --full` for all advertised aliases. SIWC uses `claude-code-proxy siwc models` for its live catalog. The [Models and routing](/using/models-and-routing/) page explains aliases and discovery.
