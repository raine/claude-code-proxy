---
title: What is claude-code-proxy?
description: Run Claude Code with Codex, Kimi, Grok, OpenCode Go, or Cursor Agent through one local Anthropic-compatible proxy.
---

<div class="hero-copy">
claude-code-proxy lets you use Claude Code with Codex, Kimi, Grok, OpenCode Go, or Cursor Agent. Start one local app, choose a model, and keep working in the Claude Code interface you already know.
</div>

<div class="route-rail" aria-label="Claude Code connects through the proxy to a supported provider">
  <div class="route-node route-client">
    <span class="route-kicker">Your coding interface</span>
    <strong>Claude Code</strong>
  </div>
  <div class="route-connector" aria-hidden="true"><span>→</span></div>
  <div class="route-node route-proxy">
    <span class="route-kicker">Local bridge</span>
    <strong>claude-code-proxy</strong>
    <span>Chooses the provider from your model</span>
  </div>
  <div class="route-connector" aria-hidden="true"><span>→</span></div>
  <div class="route-node route-providers">
    <span class="route-kicker">Supported providers</span>
    <div class="provider-stack"><span>Codex</span><span>Kimi</span><span>Grok</span><span>OpenCode Go</span><span>Cursor Agent</span></div>
  </div>
</div>

## Why use it?

- **Keep the Claude Code experience.** Skills, tools, hooks, subagents, IDE integrations, and the terminal interface stay on the client side.
- **Use subscription-backed providers.** Authenticate with supported consumer accounts instead of putting provider API keys into Claude Code.
- **Switch providers by model.** A single proxy process routes every request from its model ID.
- **Use Claude Code normally.** Tools and streaming are translated across providers; images and reasoning depend on the selected provider and model.
- **See what is happening.** The monitor TUI shows sessions, requests, errors, models, token use, and throughput. Structured logs and optional traffic captures support deeper diagnosis.

![Claude Code running through claude-code-proxy](/claude-code-screenshot.webp)

## Next steps

Start with the [short Codex setup](/getting-started/), compare the [supported providers](/providers/choosing-a-provider/), or read [how requests flow](/how-it-works/).
