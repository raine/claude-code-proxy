# The `ccp` helper: running the proxy as a background service

`ccp` is a small wrapper that runs `claude-code-proxy` as a **systemd user service**, so the proxy stays up in the background (with auto-restart) instead of needing a terminal running `cargo run -- serve`. The installer writes the unit and the wrapper automatically.

> **Linux/WSL only — not macOS.** This uses a systemd `--user` service; macOS has no systemd — it would need a launchd agent instead. The installer refuses to run without systemd. It's an optional convenience helper, not part of what the proxy itself ships.

## Quick install

Run the installer, which writes both files below and reloads systemd:

```sh
bash scripts/ccp/install.sh
```

It's idempotent (safe to re-run), warns if the proxy binary isn't installed yet, and does **not** enable the service — run `ccp on` yourself when you want it live.

## What it installs

Two files, both written outside the repo (see `install.sh` for the exact contents):

1. **The systemd user unit** — `~/.config/systemd/user/claude-code-proxy.service`
2. **The `ccp` wrapper script** — `~/.local/bin/ccp` (must be on `PATH`)

Both drive the compiled binary at `~/.local/bin/claude-code-proxy` — the unit runs whatever is installed there, independent of the repo checkout. Rebuild/reinstall the binary (eg, `just install`, or copy a fresh build over it) and `ccp restart` picks it up.

The unit runs `... serve` with no TUI (no TTY as a service) and `CCP_LOG_STDERR=1`, so systemd captures the logs for `ccp logs`. It restarts on failure and listens on the default `127.0.0.1:18765` — to change the port add `Environment=PORT=...` to the unit and `systemctl --user daemon-reload` (or just re-run the installer after editing it there).

## Commands

| Command | What it does |
|-|-|
| `ccp start` | Start the proxy now (this boot only) |
| `ccp stop` | Stop the proxy now |
| `ccp restart` | Restart — use after reinstalling the binary or editing the unit (`daemon-reload` first for unit edits) |
| `ccp status` | Show service state (active/inactive, PID, recent log lines) |
| `ccp logs` | Print the current logs and exit |
| `ccp logs -f` | Follow live logs (`journalctl -f`) — Ctrl-C to stop |
| `ccp on` | **Enable + start** — start now and on every login |
| `ccp off` | **Disable + stop** — stop now and don't start on login |
| `ccp health` | `curl` the `/healthz` endpoint to confirm it's actually serving |

`on`/`off` toggle the persistent (enabled-at-login) state; `start`/`stop` are just for the current session.

## Turning it on / off

- **Run the proxy persistently:** `ccp on`, then point Claude Code at it (`ANTHROPIC_BASE_URL=http://127.0.0.1:18765` etc — see README §4).
- **Go back to direct Anthropic:** `ccp off` and remove/comment the proxy env from Claude Code's settings.

Verify which one is live: `ccp status` (should say `active (running)`) and `ccp health` (should print `ok`/200). When off, `ccp health` fails to connect — that's expected.
