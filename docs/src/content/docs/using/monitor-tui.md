---
title: Monitor TUI
description: Use the claude-code-proxy monitor to inspect sessions, active and recent requests, providers, errors, token usage, throughput, and setup.
---

`claude-code-proxy serve` opens the monitor when stdout is an interactive terminal. The same process runs the HTTP listener.

![claude-code-proxy monitor showing sessions, active requests, recent requests, and events](/monitor-tui.webp)

## What the monitor shows

- Sessions grouped by Claude Code session ID and project
- Active request lifecycle and selected provider or model
- Recent requests, HTTP status, elapsed time, and errors
- Input and output token totals
- Output throughput based on matched upstream timing and cumulative usage samples
- Paths to traffic captures when capture is enabled
- Configuration overrides and a ready-to-copy Claude Code setup

## Keyboard controls

| Key | Action |
| --- | --- |
| `Tab`, `←`, `→` | Change focused pane |
| `j`, `k`, `↓`, `↑` | Move selection |
| `Enter` | Open session or request details |
| `Esc` | Close details or an overlay |
| `?` | Toggle shortcut help |
| `b` | Toggle the setup overlay |
| `q` | Request a graceful shutdown |
| `Ctrl-C` | Force shutdown |

The request table changes columns as the terminal width changes.

## Plain logs

Use plain output when the process runs under a service manager, in CI, or through a pipe:

```sh
claude-code-proxy serve --no-monitor
```

Non-terminal stdout also selects plain mode. `CCP_LOG_STDERR=1` mirrors JSONL log events to stderr in plain mode.

## Demo mode

Explore the full interface without binding a port or using provider credentials:

```sh
claude-code-proxy demo
```

The deterministic simulation covers active, successful, and failed requests across providers, projects, throughput states, and responsive layouts.

## Background service

A Homebrew installation can run at login:

```sh
brew services start claude-code-proxy
```

Service output lives in `~/.local/state/claude-code-proxy/service.log` on macOS and Linux. The structured `proxy.log` shares the state directory. Provider login remains an interactive one-time command.
