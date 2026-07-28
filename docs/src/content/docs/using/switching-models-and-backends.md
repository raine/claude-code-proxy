---
title: Switching models and backends
description: Choose launch-time patterns for switching between claude-code-proxy and direct Anthropic, then switch routed models within a proxy session.
---

Claude Code binds its base URL and client auth when the process starts. A **backend switch** needs a new Claude Code process. A **model switch** can stay in the same proxy-backed session because the proxy routes each request by model ID.

| Goal | Pattern |
| --- | --- |
| Always use the proxy | Put client variables in `~/.claude/settings.json` |
| Try one model once | Prefix `claude` with environment variables or use an alias |
| Toggle between proxy and direct Anthropic | Use a launch wrapper controlled by a flag |
| Stay on the proxy and change provider or model | Use `/model`, `--model`, or a new `ANTHROPIC_MODEL` |

## One-shot aliases

```sh
alias csol='ANTHROPIC_BASE_URL=http://127.0.0.1:18765 ANTHROPIC_AUTH_TOKEN=unused ANTHROPIC_MODEL=gpt-5.6-sol[1m] ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna[1m] CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 claude'
alias cgrok='ANTHROPIC_BASE_URL=http://127.0.0.1:18765 ANTHROPIC_AUTH_TOKEN=unused ANTHROPIC_MODEL=grok-4.5 ANTHROPIC_SMALL_FAST_MODEL=grok-4.5 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 claude'
```

These affect only the launched process.

## Flag-controlled wrapper

Put a wrapper named `claude` ahead of the real binary on `PATH`. Set `REAL_CLAUDE` to a path that cannot resolve back to the wrapper.

```bash
#!/usr/bin/env bash
# Route Claude Code through the proxy when the flag file exists.
set -euo pipefail

real_claude="${REAL_CLAUDE:-$HOME/.local/bin/claude-real}"
flag="$HOME/.claude/claude-code-proxy-enabled"
model_file="$HOME/.claude/claude-code-proxy-model"

if [ -f "$flag" ]; then
  model="gpt-5.6-sol[1m]"
  [ ! -f "$model_file" ] || model="$(tr -d '[:space:]' <"$model_file")"

  export ANTHROPIC_BASE_URL="http://127.0.0.1:18765"
  export ANTHROPIC_AUTH_TOKEN="unused"
  export ANTHROPIC_MODEL="${ANTHROPIC_MODEL:-$model}"
  export ANTHROPIC_SMALL_FAST_MODEL="${ANTHROPIC_SMALL_FAST_MODEL:-$model}"
  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
  export CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1
fi

exec "$real_claude" "$@"
```

Toggle it with ordinary file operations:

```sh
mkdir -p ~/.claude
touch ~/.claude/claude-code-proxy-enabled
printf '%s\n' 'kimi-for-coding[1m]' > ~/.claude/claude-code-proxy-model
# Disable for future sessions
rm ~/.claude/claude-code-proxy-enabled
```

New processes read the flag and model. Running processes retain their launch environment.

## In-session model changes

With the base URL already pointing to the proxy:

```text
/model gpt-5.6-sol-fast[1m]
/model kimi-for-coding[1m]
/model grok-4.5
/model cursor:gpt-5.5
```

The provider changes with the model. Provider auth must already exist. Claude Code preserves one conversation, while provider-specific in-memory continuation can reset when the provider or model changes.

## Scope

claude-code-proxy does not provide a profile GUI, rewrite Claude Code settings, change base URLs in a running process, or configure Desktop and IDE launch environments. Use process environment, a wrapper, or a dedicated profile manager for those concerns.
