---
title: Monitor TUI
description: Use the claude-code-proxy monitor to inspect sessions, active and recent requests, providers, errors, token usage, throughput, and setup.
---

`claude-code-proxy serve` opens the monitor when stdout is an interactive terminal. The same process runs the HTTP listener.

To run the proxy as a service and attach the dashboard separately:

```sh
# Run this under your service manager, or leave it in another terminal.
claude-code-proxy serve --no-monitor

# Attach from any terminal; repeat for additional dashboards.
claude-code-proxy monitor
```

Use `claude-code-proxy monitor --url http://127.0.0.1:19999` for a different port. Without `--url`, the port follows the usual proxy configuration. The attached dashboard reads the running service's existing history; it does not start a proxy or need provider credentials.

In an attached dashboard, `q` and `Ctrl-C` detach immediately and leave the service running. Multiple dashboards can attach independently. If the service becomes unavailable, the dashboard marks its last snapshot as stale and reconnects automatically. Network polling runs outside the terminal event loop.

![claude-code-proxy monitor showing sessions, active requests, recent requests, and events](/monitor-tui.webp)

## What the monitor shows

- Sessions grouped by Claude Code session ID and project
- Active request lifecycle and selected provider or model
- Recent requests, HTTP status, elapsed time, and errors
- Input and output token totals
- Output throughput based on matched upstream timing and cumulative usage samples
- Paths to traffic captures when capture is enabled
- Configuration overrides and a ready-to-copy Claude Code setup
- Codex and Anthropic account profiles, selected/saved-default markers, local credential status, and Codex token expiry

## Account profile status

The Accounts view (`a`) lists all named Codex and Anthropic profiles plus the implicit `default` profile. It distinguishes `[selected]`, the active runtime account, from `[saved-default]`, the default used by future processes. Missing selected/default credential files remain visible as unauthenticated profiles. Scroll with `↑`/`↓` or `j`/`k`, `PgUp`/`PgDn`, and `Home`/`End`; press `Enter` then `y` to switch the highlighted profile, or `n`/`Esc` to cancel. The switch affects all sessions for that provider but does not change the saved default. Active inference requests and streams must finish first. Close Accounts with `a` or `Esc`.

Statuses in Accounts are local observations, not a live login, quota, or billing check. Anthropic keys are labeled unverified; expired Codex access tokens may still have refresh credentials. Profile metadata is cached and refreshed in the background, including for remotely attached monitors. API keys, access/refresh tokens, upstream account identifiers, and credential file paths are never included in account-status snapshots.

### Account limits below Events

The main dashboard lists every saved Codex login in **Account limits**, directly below Events. Each row shows the **5-hour** and **7-day** used percentages, reset countdowns, retrieval status, and last successful update. `*` marks the currently selected account. Press `Tab` until this pane is focused, then use `↑`/`↓`, `j`/`k`, `PgUp`/`PgDn`, or `Home`/`End` to reach every account.

The service polls ChatGPT's read-only usage endpoint roughly once per minute, with bounded concurrency and timeouts. This does not generate tokens, refresh OAuth credentials, or switch accounts. Failed reads retain explicitly stale values; absent windows show unavailable, never a fabricated zero. Expired credentials show an expiry status. Anthropic API keys do not have these subscription windows and are not included. Attached monitors receive safe quota metadata from the service, never credentials from their own machine.

Add and remove accounts through the [Account profiles](/using/account-profiles/) CLI commands. Both integrated and attached monitors support confirmed runtime switching through a loopback-only control endpoint; no restart is required.

## Keyboard controls

| Key | Action |
| --- | --- |
| `Tab`, `←`, `→` | Change focused pane |
| `j`, `k`, `↓`, `↑` | Move selection |
| `Enter` | Open session/request details; in Accounts, request a profile switch |
| `Esc` | Close details or an overlay |
| `?` | Toggle shortcut help |
| `b` | Toggle the setup overlay |
| `a` | Toggle Accounts: all profiles, local status, selected/saved-default markers, and expiry |
| `q` | Detach an attached dashboard; in the built-in dashboard, confirm proxy shutdown |
| `Ctrl-C` | Detach an attached dashboard; in the built-in dashboard, start shutdown (press again to force exit) |

The request table changes columns as the terminal width changes.

## Plain logs

Use plain output when the process runs under a service manager, in CI, or through a pipe:

```sh
claude-code-proxy serve --no-monitor
```

Non-terminal stdout also selects plain mode. `CCP_LOG_STDERR=1` mirrors JSONL log events to stderr in plain mode.

Plain mode retains monitor accounting even with no dashboard attached. On Unix, SIGTERM starts graceful proxy shutdown; Ctrl-C does the same. The service manager owns the process lifetime.

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

Run `claude-code-proxy monitor` to inspect that service. The monitor endpoint only accepts loopback connections, even if the inference listener binds a LAN address. For a service on another machine, forward its port with SSH and point `monitor --url` at the local end of the tunnel.

History remains in the proxy's memory and resets when the proxy restarts. The dashboard polls snapshots every 250 ms and displays server-computed durations and throughput. The attached setup overlay describes its connection; provider setup remains with the service and its built-in dashboard.
