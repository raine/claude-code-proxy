---
title: Getting started
description: Install claude-code-proxy, authenticate with Codex, start the server, and open one working Claude Code session.
---

This path gets one Codex-backed Claude Code session working. See [Choosing a provider](/providers/choosing-a-provider/) for Kimi, Grok, OpenCode Go, and Cursor Agent.

## 1. Install

On macOS or Linux with Homebrew:

```sh
brew install raine/claude-code-proxy/claude-code-proxy
```

Or use the release installer:

```sh
curl -fsSL https://raw.githubusercontent.com/raine/claude-code-proxy/main/scripts/install.sh | bash
```

Windows archives and binaries for every supported platform are on the [GitHub releases page](https://github.com/raine/claude-code-proxy/releases).

## 2. Sign in to Codex

Use a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
claude-code-proxy codex auth login
```

For SSH or another headless environment, use `claude-code-proxy codex auth device` instead.

## 3. Start the proxy

Keep this process running:

```sh
claude-code-proxy serve
```

It listens on `127.0.0.1:18765`. In an interactive terminal it opens the monitor TUI.

## 4. Start Claude Code

Open another terminal and run:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-5.6-sol[1m] \
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna[1m] \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

Send a prompt. The request appears in the monitor and the response streams into Claude Code.

The `[1m]` suffix is a Claude Code compaction hint. The proxy removes it before the upstream request. The explicit 272,000 token auto-compact window keeps the local threshold within the ChatGPT context limit used by these models.

For persistent settings, provider switching, and model discovery, continue to [Configure Claude Code](/using/configure-claude-code/) and [Models and routing](/using/models-and-routing/).
