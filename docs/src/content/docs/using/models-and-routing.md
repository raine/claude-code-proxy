---
title: Models and routing
description: Understand provider model patterns, aliases, Codex fast mode, context hints, small models, model listing, and Claude Code gateway discovery.
---

The model ID in each request selects its provider. One proxy listener can serve every provider.

## Routing patterns

| Pattern | Provider |
| --- | --- |
| Registered `gpt-*` IDs and their `-fast` forms | Codex |
| `kimi-for-coding`, `kimi-k2.6`, `k2.6` | Kimi |
| `grok-composer-2.5-fast`, `grok-4.5` | Grok |
| `cursor`, Cursor legacy aliases, `cursor:<id>`, `cursor-plan:<id>`, `cursor-ask:<id>` | Cursor Agent |
| Anthropic-style aliases such as `haiku`, `sonnet`, `opus`, `fable`, and registered `claude-*` aliases | The `aliasProvider`, Codex by default |

An unknown ID returns HTTP 400 with the supported provider catalog. There is no implicit fallback for arbitrary model names.

## Prefer the live catalog

The model list changes faster than documentation. Ask the installed CLI:

```sh
claude-code-proxy models
claude-code-proxy models --full
```

The compact command abbreviates Cursor's dynamic aliases. `--full` prints every alias discovered through the installed Cursor Agent catalog.

The HTTP equivalent is:

```sh
curl http://127.0.0.1:18765/v1/models
```

## Codex fast mode

Every registered Codex model also has a local `-fast` form. The proxy removes `-fast` from the upstream model and requests the priority service tier. A configured `codex.serviceTier` or `CCP_CODEX_SERVICE_TIER` override wins.

## The `[1m]` hint

A trailing `[1m]` affects Claude Code's local compaction policy. The proxy strips it before matching and forwarding the model. Use it only when the upstream model and account can accept the resulting context, and set a safe `CLAUDE_CODE_AUTO_COMPACT_WINDOW` when the real limit is below one million tokens.

## Main and small models

Set both model variables to routable IDs:

```sh
ANTHROPIC_MODEL=gpt-5.6-sol[1m]
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna[1m]
```

Claude Code sends background work to its small/fast model. Its built-in Haiku ID can route through `aliasProvider`, but a concrete provider ID keeps the behavior explicit.

## Claude Code `/model`

When `ANTHROPIC_BASE_URL` already points to the proxy, these can change the request model:

- `claude --model <id>` at launch
- Claude Code's `/model` command in a session
- `ANTHROPIC_MODEL` for a new process

A model change can switch upstream providers because routing is per request. It does not change `ANTHROPIC_BASE_URL` or move the process back to direct Anthropic.

## Gateway model discovery

Enable Claude Code discovery at process start:

```sh
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-5.6-sol[1m] \
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna[1m] \
  claude
```

`/v1/models` includes raw provider IDs and Anthropic-style aliases. Claude Code's gateway picker filters for IDs beginning with `claude` or `anthropic`, so configured aliases are the entries most likely to appear. Raw IDs remain usable through `--model`, `/model`, and environment variables.

## Alias routing

`CCP_ALIAS_PROVIDER=kimi` or `"aliasProvider": "kimi"` routes recognized Anthropic-style aliases to Kimi. Accepted values are `codex` and `kimi`. Explicit provider IDs always use their provider.
