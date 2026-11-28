# claude-code-proxy

Claude Code, powered by **Anthropic Claude**, **OpenAI Codex**, **Kimi**, **Grok**, **OpenCode Go**,
or **Cursor Agent**.

Docs: <https://claude-code-proxy.raine.dev>

LLM docs: <https://claude-code-proxy.raine.dev/llms.txt>

<img src="meta/claude-code-screenshot-2026-07.webp" alt="Claude Code running through claude-code-proxy" />

<a href="https://aventasks.dev/">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://aventasks.dev/readme/aven-banner-dark.webp">
    <img src="https://aventasks.dev/readme/aven-banner-light.webp" alt="aven: one task queue for you and your coding agents" width="100%">
  </picture>
</a>

<sub>Also from the author of claude-code-proxy: <a href="https://aventasks.dev/"><b>aven</b></a>, one
queue for every project, with a keyboard-driven TUI for you and a CLI for your
agents.</sub>

## Why?

Claude Code remains an excellent coding harness, with strong tools, skills,
hooks, subagents, and editor integrations. claude-code-proxy keeps that client
experience while translating its Anthropic API traffic for subscription-backed
provider services.

One local process handles provider authentication, model-based routing,
protocol translation, streaming responses, and diagnostics. The built-in
monitor shows sessions, active and recent requests, errors, token usage, and
throughput.

## Quick start with Codex

Install on macOS or Linux:

```sh
brew install raine/claude-code-proxy/claude-code-proxy
```

Or use the release installer:

```sh
curl -fsSL https://raw.githubusercontent.com/raine/claude-code-proxy/main/scripts/install.sh | bash
```

Windows and other prebuilt artifacts are available from
[GitHub Releases](https://github.com/raine/claude-code-proxy/releases).

To build your own Windows x64 executable without installing Rust locally:

1. Push your changes and `.github/workflows/build-windows.yml` to your GitHub
   repository's default branch (GitHub needs the workflow there to show the manual trigger).
2. Open **Actions → Build Windows → Run workflow**, select the branch to build,
   and confirm **Run workflow**. If Actions is disabled on your fork, enable it first.
3. After tests and the build succeed, download **claude-code-proxy-windows-x64**
   from the run's **Artifacts** section and extract the ZIP.
4. Stop the old proxy, replace its executable with `claude-code-proxy.exe`, and
   restart it using the same configuration directory to retain existing credentials.

The download includes a SHA-256 checksum and is retained for 14 days. This manual
workflow does not publish a release or require repository secrets.

Sign in with a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
claude-code-proxy codex auth login
```

Start the proxy in one terminal:

```sh
claude-code-proxy serve
```

For a background service, use `claude-code-proxy serve --no-monitor` and attach
from another terminal with `claude-code-proxy monitor`. Closing an attached
dashboard leaves the proxy running.

Start Claude Code in another:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-6-sol[1m] \
ANTHROPIC_SMALL_FAST_MODEL=gpt-6-luna[1m] \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

See [Getting started](https://claude-code-proxy.raine.dev/getting-started/)
for the complete first session.

Optional Codex image generation and editing can reuse the same ChatGPT login:

```sh
CCP_CODEX_IMAGES_API=1 claude-code-proxy serve
curl http://127.0.0.1:18765/v1/images/generations \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"A paper-cut fox","model":"gpt-image-2"}'
```

The opt-in Images API returns base64 image data and consumes the signed-in account's image quota. Image prompts and payloads are excluded from traffic captures. See the [HTTP API](https://claude-code-proxy.raine.dev/reference/http-api/) for generation and edit schemas.

## Providers

| Provider     | Account                        | Model selection                                 |
| ------------ | ------------------------------ | ----------------------------------------------- |
| Anthropic    | Anthropic API key (separate API billing) | `anthropic/<claude-model-id>`          |
| Codex        | ChatGPT Plus or Pro            | Registered `gpt-*` models, `-fast`/`-ultrafast` |
| Kimi         | kimi.com with Kimi Code access | `kimi-for-coding` and aliases                   |
| Grok         | grok.com                       | Registered Grok models                          |
| OpenCode Go  | OpenCode Go subscription       | Non-conflicting IDs and `opencode-go/<model-id>` |
| Cursor Agent | Cursor account                 | Cursor aliases and `cursor:<model-id>` prefixes |

### Anthropic Claude

Set `CCP_ANTHROPIC_API_KEY` (or `ANTHROPIC_API_KEY`) in the **proxy process**, then
start `claude-code-proxy serve`. Select `anthropic/claude-opus-5-5` in the client.
The explicit `anthropic/` namespace avoids existing Claude-to-Codex/Kimi aliases.
Set `CCP_ALIAS_PROVIDER=anthropic` on the proxy if those aliases should use Claude too.
Claude Pro/Max subscriptions do not include API billing. See
[Anthropic setup](https://claude-code-proxy.raine.dev/providers/anthropic/).

### Multiple accounts

Codex OAuth accounts and Anthropic API keys can be stored as named profiles:

```sh
claude-code-proxy codex auth login --profile work
claude-code-proxy anthropic auth login --profile personal # key from env or piped stdin
claude-code-proxy codex profile list
claude-code-proxy codex profile use work
claude-code-proxy serve --codex-profile work --anthropic-profile personal
```

Each process uses one selected account per provider; there is no automatic rotation.
Press `a` in the monitor to list all profiles, local statuses, selected/default markers,
and Codex token expiry without exposing keys or tokens. Select a profile, press `Enter`,
then `y` to switch at runtime without restarting. Wait for active requests/streams to
finish first; switching affects all sessions for that provider, not the saved default.
See [Account profiles](https://claude-code-proxy.raine.dev/using/account-profiles/)
for selection precedence, credential storage, and removal.

Run `claude-code-proxy models` for the current catalog or
`claude-code-proxy models --full` for every dynamic Cursor alias.

> [!WARNING]
> The proxy accepts local requests without client authentication. It binds to
> `127.0.0.1` by default. Protect any non-loopback listener with a firewall or
> authenticating reverse proxy. Provider subscriptions, model access, terms,
> and account enforcement remain under each provider's control. Unofficial
> clients may carry account risk.

## Documentation

- [What is claude-code-proxy?](https://claude-code-proxy.raine.dev/)
- [Choosing a provider](https://claude-code-proxy.raine.dev/providers/choosing-a-provider/)
- [Configure Claude Code](https://claude-code-proxy.raine.dev/using/configure-claude-code/)
- [Models and routing](https://claude-code-proxy.raine.dev/using/models-and-routing/)
- [Monitor TUI](https://claude-code-proxy.raine.dev/using/monitor-tui/)
- [Troubleshooting](https://claude-code-proxy.raine.dev/using/troubleshooting/)
- [Command reference](https://claude-code-proxy.raine.dev/reference/command-reference/)
- [Configuration](https://claude-code-proxy.raine.dev/reference/configuration/)
- [HTTP API](https://claude-code-proxy.raine.dev/reference/http-api/)
- [Compatibility and limitations](https://claude-code-proxy.raine.dev/reference/compatibility-and-limitations/)
- [For coding agents](https://claude-code-proxy.raine.dev/using/for-coding-agents/)

## Related projects

- [aven](https://github.com/raine/aven): local-first task management for power
  users and agents
- [claude-history](https://github.com/raine/claude-history): search Claude Code
  conversation history from the terminal
- [git-surgeon](https://github.com/raine/git-surgeon): non-interactive
  hunk-level git staging for coding agents
- [workmux](https://github.com/raine/workmux): parallel coding tasks in git
  worktrees and tmux
- [consult-llm](https://github.com/raine/consult-llm): consult other AI models
  from an agent workflow

## License

[MIT](LICENSE)
