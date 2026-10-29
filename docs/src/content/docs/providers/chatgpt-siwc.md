---
title: Sign in with ChatGPT
description: Authorize ChatGPT plan usage through official OAuth and the public Responses API in an isolated local proxy.
---

The opt-in `siwc` provider uses OpenAI's official Sign in with ChatGPT flow for
eligible ChatGPT plan usage. It connects to `https://api.openai.com/v1/responses`
with the selected account's OAuth access token. This project is independent of
OpenAI and Anthropic.

OpenAI documents this flow for open-source, locally hosted tools. Account,
workspace, model access, and usage limits remain under OpenAI's control. See
the [official overview](https://developers.openai.com/siwc/token-sharing-open-source).

## Authorize and discover models

```sh
claude-code-proxy siwc auth login --consent
claude-code-proxy siwc auth status
claude-code-proxy siwc models
```

Complete sign-in and plan permission in your browser. `plan_permission: true`
means the saved grant contains both inference scopes. Identity-only sign-in
does not permit inference; repeat login with `--consent` if needed.

Choose an exact `id` from `siwc models`; there is no assumed default GPT model.
The catalog is fetched for your account rather than copied from the Codex
registry. See OpenAI's [models and inference guide](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference).

Use `--profile <label>` on each `siwc` command for separate registrations.
Returning sign-in retains the selected profile's issued client ID and verifies
its account identity. A different account or workspace needs a separate profile.
`--no-browser` prints an authorization URL without retained identity-token hints.

## Run Claude Code

Replace `MODEL_SLUG_FROM_CATALOG` in both commands with the chosen model ID.
Start the proxy in one terminal:

```sh
claude-code-proxy siwc serve --model MODEL_SLUG_FROM_CATALOG --port 18765
```

Start native Claude Code in another terminal, from your project directory:

```sh
env -u ANTHROPIC_API_KEY \
  ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
  ANTHROPIC_AUTH_TOKEN=unused \
  ANTHROPIC_MODEL=MODEL_SLUG_FROM_CATALOG \
  ANTHROPIC_SMALL_FAST_MODEL=MODEL_SLUG_FROM_CATALOG \
  CLAUDE_CODE_AUTO_MODE_SERVER=0 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  CLAUDE_CODE_MAX_RETRIES=0 \
  claude
```

These variables apply only to that process. Add native arguments such as `-p`,
`--resume`, or `--continue` after `claude`. Claude Code owns project history and
executes tools locally; the proxy does not alter its session files or permissions.

`CLAUDE_CODE_AUTO_MODE_SERVER=0` disables the server classifier probe for this
gateway. Client-side classification and permission checks remain active, and
classifier requests can still consume model tokens. See Anthropic's
[Auto Mode classifier documentation](https://code.claude.com/docs/en/auto-mode-classifier-billing).

`siwc serve` is a foreground service with a loopback listener; Ctrl-C or SIGTERM
stops it. `--port 0` selects an available port and prints the actual address.
This command does not install a launcher or background service.

## Protocol and compatibility

Every upstream inference request uses HTTP/SSE, `store: false`, `stream: true`,
and a complete input history. System instructions become `instructions` or
developer messages. Local function definitions use `additional_tools` input
items. Tool call IDs, argument deltas, results, and terminal completion are
validated before reporting a successful turn.

Supported inputs include text and validated image forms. Local Claude Code
function tools such as Read, Write, and Bash are translated; Claude Code retains
execution and approval control. Unsupported hosted tools, document blocks,
broken tool history, and unsupported Responses parameters return explicit errors.
Anthropic sampling, cache-control, and `max_tokens` fields are accepted for client
compatibility but are not forwarded. Token counting is a local estimate.

Reasoning history from another provider is omitted. Resume sends portable message
and tool history; HTTP `previous_response_id`, opaque Codex continuation, hosted
search, native compaction, and OpenAI-compatible ingress routes are unavailable in
SIWC mode. Review OpenAI's [preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)
for the current service contract. This adapter deliberately supports a narrower
subset than every capability mentioned there.

## Failures and retries

Non-success HTTP responses retain their status, machine error code, parameter,
and safe message. Diagnostics include upstream status, Content-Type, request ID,
and a fixed response classification. They exclude prompts and tool arguments.
Legal SSE media-type parameters are accepted. A missing Content-Type requires a
complete, bounded Responses SSE event; a successful HTTP status alone is not
proof of a valid response. Only `response.completed` completes inference.

Claude Code has a separate API retry layer. In an isolated test with native
Claude Code 2.1.310, a local 503 mock received six requests, including the initial
request, in 35.05 seconds despite `x-should-retry: false`. Setting
`CLAUDE_CODE_MAX_RETRIES=0` in the SIWC test subprocess reduced the same case to
one request. This was a Mock test, not a real upstream overload or quota test.

The command above scopes this variable to the Claude Code process using SIWC.
It disables that client's automatic API retries; it does not change the proxy's
retry policy or provide strict exactly-once execution. Disabling retries reduces
automatic recovery from transient failures, so inspect the reported error before
retrying manually. Keep the variable in the SIWC-specific invocation; do not add
it to shell startup files or native Claude Code global settings. See Anthropic's
[retry settings](https://code.claude.com/docs/en/env-vars).
`CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` only disables fallback after a
mid-stream failure and does not disable the client's API retry layer.

The proxy never automatically replays inference POSTs. Identical failed request
bodies are blocked for 60 seconds in the same proxy process, and responses include
`x-should-retry: false`. This is a bounded retry guard, not durable idempotency.
Model-catalog GETs may use bounded backoff. A plan usage-limit error pauses new
inference until the proxy restarts; check [ChatGPT usage settings](https://chatgpt.com/settings/usage)
before retrying. Follow OpenAI's [error recovery guidance](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery).

## Credentials and local security

SIWC credentials live in `<configuration-root>/siwc/state.json`, independently of
all other providers. Each installation persists an opaque host ID before login.
OAuth uses PKCE, state/nonce checks, and signature/issuer/audience validation.
Refresh rotation is serialized across processes and saved atomically. An
uncertain refresh outcome requires reauthorization rather than reusing a possibly
consumed refresh token. Terminal refresh failure clears tokens while retaining
the registration. See the [official session lifecycle](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions).

The directory must have mode `0700` and its files `0600`; symlinks and hard-linked
credential files are rejected. This implementation requires Unix permissions
(macOS or Linux); Windows SIWC credential storage is not implemented. Files are
private plaintext, not encrypted at rest. Other processes running as your user
can access them and the unauthenticated local proxy. Browser-origin requests,
external hosts, and non-loopback peers are rejected.

SIWC disables traffic captures and persistent upstream error-body files, including
when general capture settings are enabled. Keep `auth status` output private: it
contains account and registration identifiers, though never tokens.

```sh
claude-code-proxy siwc auth logout
```

Logout attempts remote refresh-token revocation, clears local tokens, and retains
the host ID and client registration. If revocation cannot be confirmed, disconnect
the app in ChatGPT settings. Temporary network failures do not erase credentials.

## Validation scope

The automated tests use a local mock OAuth/Responses server, a public test signing
key, and a sanitized native Claude Code request fixture. They cover token rotation,
permission and identity isolation, routing boundaries, error redaction, large
headerless SSE frames, interrupted streams, and tool integrity. They do not log
in to a real account or establish live model/tool availability. Validate an actual
text turn and approved local tools with your own account before relying on a
particular Claude Code/model combination.
