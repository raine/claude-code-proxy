---
title: Configuration
description: Canonical claude-code-proxy configuration keys, environment variables, defaults, precedence, boolean parsing, and example config.json.
---

Proxy settings come from environment variables or `config.json`. Precedence is **environment variable, config file, built-in default**. The `serve --port` option takes precedence over all port settings.

These settings configure the proxy process. Claude Code client settings such as `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, and `CLAUDE_CODE_*` are documented separately in [Configure Claude Code](/using/configure-claude-code/).

## Example `config.json`

```json
{
  "bindAddress": "127.0.0.1",
  "port": 18765,
  "aliasProvider": "codex",
  "codex": {
    "originator": "claude-code-proxy",
    "userAgent": "claude-code-proxy/0.1.24",
    "model": "gpt-5.6-sol",
    "effort": "high",
    "reasoningSummary": "auto",
    "serviceTier": "fast",
    "baseUrl": "https://chatgpt.com/backend-api/codex/responses",
    "transport": "websocket",
    "previousResponseId": false,
    "serverCompaction": false,
    "responsesApi": false
  },
  "kimi": {
    "userAgent": "KimiCLI/1.37.0",
    "oauthHost": "https://auth.kimi.com",
    "baseUrl": "https://api.kimi.com/coding/v1"
  },
  "grok": {
    "baseUrl": "https://cli-chat-proxy.grok.com/v1",
    "clientVersion": "0.2.93"
  },
  "cursor": {
    "baseUrl": "https://api2.cursor.sh",
    "clientVersion": "0.48.5",
    "agentBundle": "/path/to/cursor-agent/index.js"
  },
  "log": {
    "stderr": false,
    "verbose": false
  }
}
```

All keys are optional. An unreadable file, malformed JSON, or incompatible field type causes the file configuration to be ignored and built-in or environment values to apply.

## Core and diagnostics

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_BIND_ADDRESS` | `bindAddress` | `127.0.0.1` | Listener IP address. |
| `PORT` | `port` | `18765` | Listener port. |
| `CCP_CONFIG_DIR` | none | Platform config directory | Replaces the configuration and file-backed auth root. |
| `CCP_ALIAS_PROVIDER` | `aliasProvider` | `codex` | Routes recognized Anthropic-style aliases through `codex` or `kimi`. |
| `CCP_LOG_STDERR` | `log.stderr` | `false` | Mirrors logs to stderr when present in the environment, regardless of its value. |
| `CCP_LOG_VERBOSE` | `log.verbose` | `false` | Preserves full string fields in structured logs when present, regardless of its value. |
| `CCP_TRAFFIC_LOG` | none | `false` | Enables full request captures for `1`, `true`, or `yes`. |
| `XDG_STATE_HOME` | none | `~/.local/state` | State base on macOS and Linux. |

`CCP_CONFIG_DIR` affects `config.json` and file-backed provider auth. It does not relocate the state directory.

## Codex

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_CODEX_MODEL` | `codex.model` | unset | Forces every Codex request to one upstream model. |
| `CCP_CODEX_EFFORT` | `codex.effort` | unset | Forces `none`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `CCP_COMPACT_EFFORT` | none | `low` | Caps Codex reasoning effort for Claude Code summary compaction requests. `off` disables the cap and `none` removes reasoning. |
| `CCP_CODEX_REASONING_SUMMARY` | `codex.reasoningSummary` | unset | Overrides summary mode. `off` and `none` suppress summaries. |
| `CCP_CODEX_SERVICE_TIER` | `codex.serviceTier` | unset | Forces `fast` or `priority`, or `flex`. Fast is sent as `priority`. |
| `CCP_CODEX_BASE_URL` | `codex.baseUrl` | ChatGPT Codex Responses URL | Changes the Codex endpoint. |
| `CCP_CODEX_TRANSPORT` | `codex.transport` | `websocket` | Selects `websocket`, `http`, or `auto`. |
| `CCP_CODEX_PREVIOUS_RESPONSE_ID` | `codex.previousResponseId` | `false` | Enables append-only WebSocket continuation for `1`, `true`, or `yes`. |
| `CCP_CODEX_SERVER_COMPACTION` | `codex.serverCompaction` | `false` | Enables or disables native compaction for standard boolean words. |
| `CCP_CODEX_RESPONSES_API` | `codex.responsesApi` | `false` | Enables `/v1/responses` for `1`, `true`, or `yes`. |
| `CCP_CODEX_ORIGINATOR` | `codex.originator` | `claude-code-proxy` | Changes the Codex `originator` header. |
| `CCP_CODEX_USER_AGENT` | `codex.userAgent` | `claude-code-proxy/<version>` | Changes the Codex user-agent. |

`CLAUDE_CODE_PROXY_CODEX_BASE_URL` remains an accepted fallback for the Codex base URL. `CCP_CODEX_BASE_URL` takes precedence.

## Kimi

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_KIMI_OAUTH_HOST` | `kimi.oauthHost` | `https://auth.kimi.com` | Changes the OAuth host. |
| `CCP_KIMI_BASE_URL` | `kimi.baseUrl` | `https://api.kimi.com/coding/v1` | Changes the API base URL. |
| `CCP_KIMI_USER_AGENT` | `kimi.userAgent` | `KimiCLI/1.37.0` | Changes the Kimi user-agent. |

## Grok

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_GROK_BASE_URL` | `grok.baseUrl` | `https://cli-chat-proxy.grok.com/v1` | Changes the Responses API base URL. |
| `CCP_GROK_CLIENT_VERSION` | `grok.clientVersion` | `0.2.93` | Changes the Grok client version header. |
| `CCP_GROK_TOOL_IMAGE` | none | `omit` | Selects `omit`, `reattach`, `inline`, or `reject` image handling. |

## Cursor Agent

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_CURSOR_BASE_URL` | `cursor.baseUrl` | `https://api2.cursor.sh` | Changes the Cursor API base URL. |
| `CCP_CURSOR_CLIENT_VERSION` | `cursor.clientVersion` | `0.48.5` | Changes Cursor client version headers. |
| `CCP_CURSOR_AGENT_BUNDLE` | `cursor.agentBundle` | Auto-detected | Points to Cursor Agent's bundled `index.js` protobuf schemas. |
| `CCP_CURSOR_AUTH_TOKEN` | none | unset | Uses a bearer token instead of proxy-owned Cursor auth storage. |

## Shared compatibility fallbacks

`CCP_USER_AGENT` is the fallback when a Codex or Kimi provider-specific user-agent is absent. Prefer `CCP_CODEX_USER_AGENT` or `CCP_KIMI_USER_AGENT` in durable configuration.

Endpoint and client identity overrides can break provider compatibility. Use defaults unless a controlled integration or focused diagnostic requires an override.
