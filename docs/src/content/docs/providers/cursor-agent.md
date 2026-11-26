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

Some Cursor list ids are not wire ids. `cursor:claude-sonnet-5-5-medium` is sent as family id `claude-sonnet-5-5` with `context=300k` and `reasoning_effort=medium`. The proxy does this for the families whose catalog stores effort in parameters (`reasoning_effort`, `effort`, or `reasoning`) and includes `context` only when that family's non-max variant has one. `fast` is sent only when the family lists it. Sending `fast` on Claude Sonnet 5.5 makes Cursor return `ERROR_BAD_MODEL_NAME`.

Composer and Grok stay a family id plus the `fast` flag. A trailing `-fast` on Opus is kept. An unknown `cursor:<id>` is forwarded as that id with `fast`. A bare family id uses that family's default effort. This is the non-max parameter set, not the 1m max-mode variant.

## Tools and multimodal input

- System prompts, messages, and tool definitions are rendered into the Cursor agent prompt.
- Base64 user images become Cursor selected images.
- Text, thinking, plan mode, ask mode, and usage stream back to Claude Code.
- The native tool bridge recognizes Cursor requests for `Read`, `Write`, and `Bash` when matching Claude tools are advertised and a Claude Code session ID is present. It pauses the stream, emits an Anthropic tool call, accepts the next tool result, and resumes stored events.
- Other Cursor workspace callbacks and native tool forms do not have a general Claude tool bridge.

## OpenAI-compatible APIs

When the OpenAI routes are enabled, Cursor models work with both `POST /v1/chat/completions` and `POST /v1/responses`. To use the `Read`, `Write`, or `Bash` tool bridge, send `stream: true`, define the matching function tools, and include a stable session header. The proxy accepts `x-claude-code-session-id`, `session_id`, or `x-client-request-id`. Other function names are not supported by the Cursor bridge.

## Session continuation

For tool-bridged requests, the proxy keeps the Cursor conversation of each Claude Code session, model, system prompt and tool set in memory: the conversation ID, the last checkpoint and the agent-store blobs. When the next request extends the transcript that conversation saw, the run sends the checkpoint back with only the new messages, so Cursor's prompt cache reuses the earlier turns instead of writing the whole context again. A changed transcript (an edit, a compaction) or tool list starts a new conversation with the full prompt. Conversations idle for 30 minutes are dropped, and a proxy restart clears them all. Set `CCP_CURSOR_CONTINUATION=off` to send the full transcript on every request.

Each run logs `cursor_turn_usage` with Cursor's own input, output, cache-read and cache-write token counts and whether it continued a conversation.

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
