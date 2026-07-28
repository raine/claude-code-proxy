---
title: Cursor Agent
description: Configure Cursor authentication, dynamic models, plan and ask modes, effort variants, images, session continuation, tool bridging, and protobuf discovery.
---

Cursor uses Cursor Agent's HTTP/2 full-duplex Connect protocol at `https://api2.cursor.sh/agent.v1.AgentService/Run`.

## Account and authentication

Use a **Cursor account**:

```sh
claude-code-proxy cursor auth login
claude-code-proxy cursor auth status
```

The browser flow stores proxy-owned tokens. It does not read Cursor Agent's Keychain or `auth.json`. `CCP_CURSOR_AUTH_TOKEN` can supply a bearer token directly to the proxy process.

## Cursor Agent dependency

The provider speaks Cursor's protocol directly but loads generated protobuf classes from an installed Cursor Agent `index.js` bundle. Auto-detection checks common installations. Set `CCP_CURSOR_AGENT_BUNDLE` when the bundle cannot be found.

## Models and modes

Prefer prefixed IDs:

- `cursor:<model-id>` runs the named Cursor model in agent mode.
- `cursor-plan:<model-id>` uses plan mode.
- `cursor-ask:<model-id>` uses ask mode.

Legacy IDs such as `cursor`, `cursor-plan`, `cursor-ask`, `composer-2.5`, and `composer-2.5-fast` remain available. Prefixes avoid collisions, so `gpt-5.2` can route to Codex while `cursor:gpt-5.2` routes to Cursor.

The provider reads Cursor Agent's current model catalog at runtime. Use:

```sh
claude-code-proxy models --full
```

Unknown future IDs are accepted through `cursor:<raw-model>`.

## Effort mapping

Claude Code's `/effort` selects matching Cursor catalog variants when available. For example, `cursor:gpt-5.5` plus high effort can resolve to `gpt-5.5-high`. Explicit effort suffixes are preserved. Fast suffixes remain when the catalog supports them, and models without effort variants stay unchanged.

## Tools and multimodal input

- System prompts, messages, and tool definitions are rendered into the Cursor agent prompt.
- Base64 user images become Cursor selected images.
- Text, thinking, plan mode, ask mode, and usage stream back to Claude Code.
- The native tool bridge recognizes Cursor requests for `Read`, `Write`, and `Bash` when matching Claude tools are advertised and a Claude Code session ID is present. It pauses the stream, emits an Anthropic tool call, accepts the next tool result, and resumes stored events.
- Other Cursor workspace callbacks and native tool forms do not have a general Claude tool bridge.

## Session continuation

The proxy maps `x-claude-code-session-id` to a Cursor conversation ID in memory. Request metadata can resume a known Cursor chat through `cursor_chat_id`, `cursorChatId`, `cursor_resume`, or `cursorResume`. A proxy restart clears the in-memory mapping.

Metadata can also choose a mode per request:

```json
{
  "metadata": {
    "cursor_mode": "plan"
  }
}
```

## Configuration and troubleshooting

`CCP_CURSOR_BASE_URL`, `CCP_CURSOR_CLIENT_VERSION`, and `CCP_CURSOR_AGENT_BUNDLE` override protocol details. If authentication expires, Cursor requests return 401 and require another login. If startup or requests report protobuf discovery errors, run `cursor-agent --version` to confirm it is installed and set the bundle path explicitly.

See [Compatibility and limitations](/reference/compatibility-and-limitations/) for the supported tool boundary.
