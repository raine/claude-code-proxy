---
title: For coding agents
description: Give coding agents machine-readable docs, model discovery, the minimal environment contract, authoritative references, diagnostics, and safe data-handling rules.
---

This site publishes machine-readable documentation for coding agents alongside the human pages.

## Machine-readable docs

- [`/llms.txt`](/llms.txt) is the compact index and is linked in the site header.
- [`/llms-full.txt`](/llms-full.txt) contains the assembled documentation body for retrieval when the index is not enough.

Start with `llms.txt`, follow only the pages relevant to the task, and prefer canonical reference pages over copied command tables in prompts or project notes.

## Discover models at runtime

The installed CLI and running server are authoritative for the active build:

```sh
claude-code-proxy models
claude-code-proxy models --full
curl -s http://127.0.0.1:18765/v1/models
```

Use the compact CLI output for a provider overview. Use `--full` when a Cursor model alias must be resolved. Do not maintain an exhaustive model list in agent memory because provider catalogs and account access change.

## Minimal environment contract

To launch Claude Code through a running local proxy, set:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765
ANTHROPIC_AUTH_TOKEN=unused
ANTHROPIC_MODEL=<routable-model-id>
ANTHROPIC_SMALL_FAST_MODEL=<routable-model-id>
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1
```

`ANTHROPIC_*` and `CLAUDE_CODE_*` configure the Claude Code client. `CCP_*`, `PORT`, and `config.json` configure the proxy process. Keep those layers separate when editing settings or debugging a launch.

## Authoritative references

Use these pages for exact operational details:

- [Command reference](/reference/command-reference/) for CLI syntax and exit behavior
- [Configuration](/reference/configuration/) for keys, environment variables, precedence, and defaults
- [Files and storage](/reference/files-and-storage/) for credential, log, error, and traffic paths
- [HTTP API](/reference/http-api/) for local route contracts
- [Compatibility and limitations](/reference/compatibility-and-limitations/) for translation boundaries
- [Changelog](/reference/changelog/) for released behavior

For a source checkout, implementation and tests are the final authority when documentation and behavior disagree. Relevant surfaces include `src/main.rs`, `src/config.rs`, `src/registry.rs`, `src/server.rs`, `src/paths.rs`, `src/providers/`, and integration tests under `tests/`.

## Diagnostics workflow

1. Check liveness with `curl http://127.0.0.1:18765/healthz`.
2. Check the selected model with `claude-code-proxy models`.
3. Check stored provider credentials with `<provider> auth status`. For OpenCode Go, inspect whether its API-key environment variable or config key is configured without printing the secret.
4. Read the monitor request detail and structured `proxy.log`.
5. Read the redacted payload under `errors/` for a failed response.
6. Enable verbose logging or traffic capture only for a focused reproduction.

A source checkout includes `./scripts/debug-proxy`, which starts an isolated proxy on a random loopback port and prints every artifact path.

## Handle sensitive data safely

<div class="security-callout">
<strong>Prompts and traffic captures are sensitive.</strong> Traffic captures preserve inbound prompts, tool definitions, tool inputs, tool outputs, translated requests, and stream events. Redacted headers do not make that content safe to publish.
</div>

- Never print, commit, upload, or paste provider credentials, refresh tokens, session cookies, or account identifiers.
- Treat Claude prompts, source excerpts, tool output, file paths, and traffic captures as user data.
- Share the smallest redacted error payload that explains the failure.
- Ask before sending diagnostics to an external service.
- Keep traffic capture disabled outside a focused debugging session.
- Delete sensitive temporary captures after the investigation.
- Do not bind the unauthenticated listener beyond loopback without an access control layer.

Structured logs redact known credential fields, but verbose values and prompts can still contain secrets supplied by the user or tools. Inspect before sharing.
