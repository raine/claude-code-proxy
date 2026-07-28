---
title: Troubleshooting
description: Diagnose listener, authentication, model routing, streaming, context, Cursor bundle, rate-limit, logging, and traffic-capture problems.
---

## Proxy is unreachable

Check the process and liveness route:

```sh
curl http://127.0.0.1:18765/healthz
```

A healthy listener returns `{"ok":true}`. Confirm `ANTHROPIC_BASE_URL` uses the same address and port. `claude-code-proxy serve --port 11435` and `PORT=11435` change the listener port. `--port` wins for that command.

## Authentication error

Check the provider selected by the model, then inspect its credential:

```sh
claude-code-proxy codex auth status
claude-code-proxy kimi auth status
claude-code-proxy grok auth status
claude-code-proxy cursor auth status
```

Use that provider's login command when credentials are missing or expired. Codex requires ChatGPT subscription auth, not an OpenAI API key. Each provider uses proxy-owned credentials.

## Model returns HTTP 400

Run:

```sh
claude-code-proxy models
claude-code-proxy models --full
```

An unknown local ID returns a catalog in the error. A known ID can still be rejected upstream when the account, subscription, or region lacks access. Cursor's prefixed form, such as `cursor:gpt-5.5`, forces Cursor routing and avoids cross-provider collisions.

## Background requests fail

Set `ANTHROPIC_SMALL_FAST_MODEL` to a concrete routable ID. Claude Code sends title and small background tasks through that model independently of the main model.

## A tool runs twice

Set `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` for Claude Code. Retrying a partially completed stream as non-streaming can duplicate tool calls.

## Session reaches context limits

`[1m]` changes Claude Code's local compaction threshold and does not enlarge upstream context. Use a safe `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, remove `[1m]`, or compact earlier. For ChatGPT GPT-5.6 subscription models, `272000` is the documented setup value.

## Codex WebSocket fails

Use HTTP SSE to isolate transport behavior:

```sh
CCP_CODEX_TRANSPORT=http claude-code-proxy serve
```

`auto` falls back only when WebSocket setup fails before sending the request. It does not replay an in-flight request.

## Cursor bundle cannot be found

Cursor needs the installed Cursor Agent JavaScript bundle for protobuf schemas. Confirm Cursor Agent is installed, then set:

```sh
CCP_CURSOR_AGENT_BUNDLE=/path/to/cursor-agent/index.js \
  claude-code-proxy serve
```

## Rate limited

Upstream limits are shared with other clients on the same account. Codex limit responses and Kimi HTTP 429 responses surface as HTTP 429 with `retry-after`. Wait for the indicated interval or reduce concurrent traffic.

## Find the complete error

The monitor shows request detail. `proxy.log` contains structured JSONL events, and `errors/` stores complete redacted failed-response payloads. A `request_failed` event includes the error file path.

For a source checkout, run the isolated helper:

```sh
./scripts/debug-proxy
```

It prints a random local URL, a sourceable `client.env`, and artifact paths. Keep the terminal open, reproduce once, then press Ctrl-C. Captures contain sensitive prompt and tool content.

## Focused traffic capture

```sh
CCP_LOG_VERBOSE=1 \
CCP_TRAFFIC_LOG=1 \
  claude-code-proxy serve --no-monitor
```

Traffic capture writes the inbound request, translated upstream request, upstream headers and events, and downstream events in emission order. Known credentials are redacted, but prompts and tool content are preserved. Disable capture and delete artifacts after diagnosis.
