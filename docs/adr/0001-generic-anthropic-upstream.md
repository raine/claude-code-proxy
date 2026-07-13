# Design: Generic Anthropic-compatible upstream (Merge as first consumer)

## Intent

Upstream contribution to [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy)
should land as a **generic Anthropic Messages-compatible provider**, not a
Merge-only one-off.

Merge Gateway is the first configured consumer:

- Default base URL: `https://api-gateway.merge.dev/v1/anthropic`
- Default catalog prefix: `merge:`
- Default Anthropic model slugs (v1): matching the existing `claude-mg` helper
  (`anthropic/claude-opus-4-8`, `anthropic/fable-5`, `anthropic/claude-sonnet-5`,
  `anthropic/claude-haiku-4-5-20251001`)

Any other host that speaks Anthropic Messages can reuse the same provider by
setting `CCP_MERGE_BASE_URL` / `merge.baseUrl` and `CCP_MERGE_AUTH_TOKEN`.

## Why passthrough (not translation)

Claude Code already emits Anthropic Messages JSON/SSE. Merge’s Anthropic path
accepts the same shape. Unlike Codex/Kimi/Grok/Cursor, there is no proprietary
upstream protocol to translate — we rewrite the model id (strip `merge:`),
attach auth, and forward bytes.

## Configuration surface

| Concern | Env | Config file |
| --- | --- | --- |
| Base URL | `CCP_MERGE_BASE_URL` | `merge.baseUrl` |
| Auth token | `CCP_MERGE_AUTH_TOKEN` / `MERGE_AUTH_TOKEN` / `CCP_MERGE_API_KEY` | `merge/auth.json` `{"access":"..."}` |

Auth injection from 1Password is intentionally owned by the macOS app
(`claude-code-routes`); this proxy only requires a resolved bearer/token.

## v1 scope

- Anthropic models only under `merge:`
- No Merge-hosted OpenAI/GPT catalog entries yet
- Unknown / non-claimed models continue to 400 with a clear error

## Upstream PR shape (later)

When contributing back to raine:

1. Keep the provider implementation generic (Anthropic Messages forwarder).
2. Treat Merge defaults as configuration examples / docs, not hard-coded
   product branding inside the request path.
3. Consider renaming the public provider id from `merge` to something like
   `anthropic` if raine prefers a host-agnostic name, while preserving a
   configurable model-id prefix for gateway discovery.
