# claude-code-proxy

Claude Code, powered by **OpenAI Codex**, **Kimi**, **Grok**, or **Cursor
Agent**.

Docs: <https://claude-code-proxy.raine.dev>

LLM docs: <https://claude-code-proxy.raine.dev/llms.txt>

<img src="meta/claude-code-screenshot-2026-07.webp" alt="Claude Code running through claude-code-proxy" />

> [!TIP]
> I'm building [aven](https://github.com/raine/aven), a local-first task manager
> for power users and agents.

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

Sign in with a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
claude-code-proxy codex auth login
```

Start the proxy in one terminal:

```sh
claude-code-proxy serve
```

Start Claude Code in another:

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
| Codex        | ChatGPT Plus or Pro            | Registered `gpt-*` models and `-fast` variants  |
| Kimi         | kimi.com with Kimi Code access | `kimi-for-coding` and aliases                   |
| Grok         | grok.com                       | Registered Grok models                          |
| Cursor Agent | Cursor account                 | Cursor aliases and `cursor:<model-id>` prefixes |

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
