# claude-code-proxy

Claude Code, powered by **ChatGPT/Codex** or **Grok**.

[Quick start](#quick-start) · [Providers](#providers) ·
[How it works](#how-it-works) · [Configuration](#configuration) ·
[Switching models](#switching-models-and-backends) · [Limitations](#limitations)

## Why?

I feel Claude Code is still the best harness around, despite occasional
frustrations caused by updates. However, Anthropic keeps tightening the usage
limits, while OpenAI is still much more generous.

If you want to use OpenAI plans, your best options seem to be OpenCode and
Codex. I tried OpenCode, but the UX has many rough edges, especially around
skills feeling like a second-class feature. Fortunately it's open source and I
ended up forking it and applying some patches, but would much rather not do it.

## Quick start

### 1. Install

**Install script** (macOS and Linux):

```sh
curl -fsSL https://raw.githubusercontent.com/xiaodream551-a11y/claude-code-proxy/main/scripts/install.sh | bash
```

For a high-assurance install, first install and authenticate the GitHub CLI,
then require both the immutable-release attestation and the build-workflow
attestation:

```sh
curl -fsSL https://raw.githubusercontent.com/xiaodream551-a11y/claude-code-proxy/main/scripts/install.sh -o /tmp/ccproxy-install.sh
CLAUDE_CODE_PROXY_REQUIRE_ATTESTATION=1 bash /tmp/ccproxy-install.sh
```

**Manual:** download a prebuilt binary for your platform from the
[releases page](https://github.com/xiaodream551-a11y/claude-code-proxy/releases). Windows
artifacts are published as `claude-code-proxy-windows-amd64.zip` and
`claude-code-proxy-windows-arm64.zip`; extract the `.exe` somewhere on your
`PATH`.

### 2. Authenticate a routed provider

The public HTTP model registry supports two upstream providers: Codex and Grok.
Authenticate whichever route you intend to use; requests for that provider
return 401 until its token is stored.

**Codex (ChatGPT Plus/Pro):**

```sh
claude-code-proxy codex auth login     # browser OAuth (PKCE)
# or, on a headless machine:
claude-code-proxy codex auth device    # device-code flow
```

Sign in with your **ChatGPT Plus/Pro account**, not an OpenAI API account.

**Grok (grok.com):**

```sh
claude-code-proxy grok auth login      # browser OAuth (PKCE)
# or, on a headless machine:
claude-code-proxy grok auth device     # device-code flow (prints URL + code)
```

Sign in with your **grok.com account**. The proxy stores and refreshes its own
OAuth session and does not use the official Grok CLI credential file. On a
headless host, `grok auth device` prints a verification URL and code to enter on
any other device, then polls until authorization completes.

Codex and Grok prefer macOS Keychain and fall back to a mode-0600 provider file
when Keychain access is unavailable. Existing Grok file credentials are
promoted only after a verified Keychain write, so upgrading does not require
another login. Windows stores file credentials under
`%APPDATA%\claude-code-proxy\<provider>\auth.json`; Linux and macOS file
fallbacks use
`${XDG_CONFIG_HOME:-$HOME/.config}/claude-code-proxy/<provider>/auth.json`.

Verify:

```sh
claude-code-proxy codex auth status
claude-code-proxy grok auth status
```

### 3. Start the proxy

```sh
claude-code-proxy serve                # listens on 127.0.0.1:18765
PORT=11435 claude-code-proxy serve     # change the listen port
CCP_BIND_ADDRESS=0.0.0.0 claude-code-proxy serve --allow-remote-unauthenticated
claude-code-proxy serve --no-monitor   # plain logs instead of the monitor TUI
```

Binds to `127.0.0.1` by default. One `serve` process handles both routed
providers—the upstream for each request is chosen from `ANTHROPIC_MODEL`. When
stdout is a terminal, `serve` opens a monitor TUI with sessions, active requests,
recent requests, and error events. Use `--no-monitor` for plain terminal output.
The proxy does not authenticate incoming clients. Non-loopback binds are rejected
unless you explicitly pass `--allow-remote-unauthenticated` (or set the matching
config acknowledgement), and must be protected with a firewall or an
authenticating reverse proxy.

To explore every monitor pane and interaction without starting the proxy, launch
its deterministic simulated traffic:

```sh
claude-code-proxy demo
```

Resize the terminal to exercise the responsive request table. Press `?` for all
shortcuts, `Enter` for session and request details, and `b` for the setup overlay.

Installed via Homebrew, the proxy can also run as a background service that
starts at login and restarts if it exits:

```sh
brew services start claude-code-proxy
```

Service output goes to `~/.local/state/claude-code-proxy/service.log`,
alongside the proxy's own `proxy.log`. Provider logins are still a one-time
interactive step (e.g. `claude-code-proxy codex auth login`); the service
serves 401s until a token is stored.

When testing changes from a source checkout against that Homebrew service, use
`just deploy-homebrew`. It builds the release binary, backs up the active Cellar
binary, replaces it atomically, restarts the service, and verifies the new PID,
embedded Git SHA, clean-build flag, executable path, and runtime binary SHA. It
refuses a dirty worktree, recovers stale deployment locks, and verifies the old
running image by SHA even when that version predates `/version`. Failures and
termination signals restore and restart the backup. The default health URL uses
`PORT` (or 18765); an explicit `CCP_DEPLOY_HEALTH_URL` must end in `/healthz`.
`just install` targets Cargo's bin directory and does not replace the binary used
by the Homebrew launch service.

### 4. Point Claude Code at it

`ANTHROPIC_MODEL` selects the provider:

- `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.4`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`, `gpt-5.4-mini`, `gpt-5.2` → **codex**
- `grok-composer-2.5-fast`, `grok-4.5`, `grok-4.5-medium`, `grok-4.5-high` → **grok**

An unknown model returns a 400 listing the supported ids. There is no
implicit default provider.

`claude-code-proxy models` and `GET /v1/models` expose this same two-provider
catalog.

Anthropic-style aliases such as `haiku`, `sonnet`, `opus`, and `claude-*`
route to Codex by default. A concrete Grok request with an
`x-claude-code-session-id` establishes Grok affinity for that process-local
session; a later `claude-opus-5` request with the same ID follows that affinity
to Grok. Other generic aliases do not follow Grok affinity and continue to use
Codex.

Claude Code also issues background requests (session title generation and token
counts) against its small/fast model. Set `ANTHROPIC_DEFAULT_HAIKU_MODEL` to a
concrete active ID so those requests stay in the intended GPT or Grok family
instead of relying on generic-alias routing. Claude Code 2.1.212 still prefers
its deprecated `ANTHROPIC_SMALL_FAST_MODEL` variable when both are set, so
gateways supporting that version should set both to the same value.

For day-to-day GPT/Grok use, the proxy has launch profiles that keep the main
model, Claude model aliases used by subagents, and context window coherent. Pass
any remaining Claude Code arguments after `--`:

```sh
claude-code-proxy claude gpt -- --continue
claude-code-proxy claude grok -- --continue
```

| Profile | Main model | `haiku` | `sonnet` | `opus` | Context | Default effort |
| --- | --- | --- | --- | --- | ---: | --- |
| `gpt` / `co` | GPT-5.6 Sol | GPT-5.6 Luna | GPT-5.6 Terra | GPT-5.6 Sol | 272K | high |
| `grok` / `cg` | Grok 4.5 | Grok 4.5 Medium | Grok 4.5 High | Grok 4.5 High | 500K | high |

Each profile also supplies Claude Code with a concrete, same-family model
allowlist and clears any inherited fallback model. It injects complete,
session-scoped definitions for the configurable Explore, general-purpose, and
Plan agent types so their models and efforts do not depend on user or project
agent files:

| Agent type | `co` | `cg` |
| --- | --- | --- |
| built-in `claude` | GPT-5.6 Sol / inherited (`high` by default) | Grok 4.5 / inherited (`high` by default) |
| `Explore` | GPT-5.6 Luna / `medium` | Grok 4.5 Medium / `medium` |
| `general-purpose` | GPT-5.6 Sol / `high` | Grok 4.5 High / `high` |
| `Plan` | GPT-5.6 Terra / `high` | Grok 4.5 High / `high` |

The `co` profile also routes recognized automatic and manual compaction to
GPT-5.6 Terra. Codex compaction uses `medium` reasoning independently of the
main session effort. The session-scoped override replaces an inherited
compaction header, while `cg` keeps its existing main-loop compaction behavior.

The special built-in `claude` catch-all remains untouched because Claude Code
adds a private background-job and Agent View protocol that public `--agents`
definitions cannot preserve. It therefore inherits the current main-session
effort: the profile defaults still produce `high` under `co` and
`cg`, while an explicit `/effort` change also applies to this one
built-in agent. Explore and Plan use profile-managed read-only definitions;
general-purpose inherits the parent tool set.

The base `grok-4.5` id deliberately leaves reasoning effort under Claude Code's
control: `cg` still starts at `high`, while `/effort medium` or `/effort low`
now changes the wire request. Selecting `grok-4.5-high` explicitly remains a
way to lock high effort regardless of the UI setting; the model picker also
offers `grok-composer-2.5-fast` as an explicit speed-first option.

The `co` launcher defaults to Claude Code `high` effort with Ultracode off.
Ordinary `/effort` changes remain one-to-one for Codex, so standalone `xhigh`
stays wire-level `xhigh`. When Claude Code activates Ultracode, the proxy
recognizes its session marker and promotes only those `xhigh` requests to
wire-level `max`. The `cg` profile keeps its existing Grok effort behavior.

The generic Claude aliases are not broad family entries in the model picker.
`--model` and `--fallback-model` remain available for concrete models in the
active profile, while a cross-family model is rejected before Claude Code
starts. Options that can replace profile isolation (`--settings`,
`--managed-settings`, `--agents`, `--autocompact`, and `--advisor`) are also
rejected. Run plain `claude` for fully custom combinations. A future custom
agent outside the injected table that names a generic model alias inherits the
active parent model unless its definition uses a profile-allowed concrete id.

On Unix, symlinking the same binary as `co` and `cg` enables shorter forms. The
proxy selects the profile from the executable name and launches Claude Code
with the corresponding session-scoped settings:

```sh
ln -s /path/to/claude-code-proxy ~/.local/bin/co
ln -s /path/to/claude-code-proxy ~/.local/bin/cg
co
cg
```

The launchers keep Claude Code history separate as well as model routing. `co`
stores new transcripts and prompt history under `~/.claude-ccproxy/gpt`, while
`cg` uses `~/.claude-ccproxy/grok`; bare `--resume` and `--continue` therefore
only see sessions created by the same profile. Existing history under
`~/.claude` is left untouched and is not copied into either profile because old
transcripts cannot be classified reliably by launcher after the fact.

On first use, each profile receives a private snapshot of the current user
settings, permissions, plugin metadata, skills, hooks, and global state. The
snapshot keeps user-scoped MCP definitions such as Brave Search but removes
every prior `lastSessionId`; it never copies transcript, prompt-history, task,
or other session directories. Later profile configuration changes stay inside
the active profile. Installed plugin files can still be referenced from
Claude's user-level plugin cache when its metadata contains absolute paths;
this does not merge the profiles' transcript or prompt-history storage. Apply
a shared configuration change to both profiles explicitly when needed. Because
the launcher owns this boundary, an externally supplied `CLAUDE_CONFIG_DIR` is
rejected with an actionable error instead of being silently ignored.

Claude Code currently prints a generic `claude --resume <session-id>` command
when a session ends. That command does not restore the GPT or Grok proxy
environment. The profile launcher therefore prints a corrected, copyable
command after an interactive session exits:

```sh
co --resume <gpt-session-id>
cg --resume <grok-session-id>
```

The portable long forms are
`claude-code-proxy claude gpt -- --resume <session-id>` and
`claude-code-proxy claude grok -- --resume <session-id>`; launches that use a
long form print that same long form on exit. Print/redirected sessions and
detached `--background` or `--tmux` launches intentionally emit no extra resume
hint. If `--bare`, `--safe-mode`, `disableAllHooks`, or a hook-restricting
managed policy disables the capture hook, replace the leading `claude` in
Claude Code's own hint manually. `/login` only changes first-party Anthropic
authentication and does not restore the proxy profile.

Minimal manual environment examples are:

```sh
# Codex
ANTHROPIC_BASE_URL=http://localhost:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-5.6-sol \
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5.6-luna \
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna \
ANTHROPIC_CUSTOM_HEADERS='x-ccproxy-compaction-model: gpt-5.6-terra' \
CLAUDE_CODE_MAX_CONTEXT_TOKENS=272000 \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 \
CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude

# Grok
ANTHROPIC_BASE_URL=http://localhost:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=grok-4.5-high \
ANTHROPIC_DEFAULT_HAIKU_MODEL=grok-4.5-medium \
ANTHROPIC_SMALL_FAST_MODEL=grok-4.5-medium \
CLAUDE_CODE_MAX_CONTEXT_TOKENS=500000 \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=500000 \
CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90 \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude

```

Claude Code sends automatic and manual compaction with the main-loop model.
The `co` profile already routes recognized compaction to GPT-5.6 Terra. For a
manual Claude Code environment, or to choose a different proxy model, add a
custom header:

```sh
ANTHROPIC_CUSTOM_HEADERS='x-ccproxy-compaction-model: gpt-5.6-terra'
```

The header is ignored for ordinary prompts. An unknown override model returns
400 instead of silently falling back.

`CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` is recommended because the
proxy always talks to upstream providers with streaming requests, even when it
accumulates a non-streaming Anthropic response for Claude Code. Disabling Claude
Code's streaming-to-non-streaming fallback avoids retrying a partially completed
stream in a way that can duplicate tool calls.

If the proxy is your everyday default and you rarely need native Anthropic in
the same Claude config, put the env in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "gpt-5.6-sol",
    "ANTHROPIC_SMALL_FAST_MODEL": "gpt-5.6-luna",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "272000",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "272000",
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE": "90",
    "CLAUDE_CODE_MAX_RETRIES": "2",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": 1,
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": 1
  }
}
```

If you switch backends often, leave those proxy vars out of settings and use
one of the process-start patterns in
[Switching models and backends](#switching-models-and-backends).

### 5. Context window size

Claude Code decides auto-compaction based on the model's context window. For
custom gateway model ids, Claude Code 2.1.193 and later can use
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` as the raw model capacity and
`CLAUDE_CODE_AUTO_COMPACT_WINDOW` as the capacity used for compaction. Set
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` to trigger compaction at a percentage of that
capacity.

Codex CLI 0.144.6 corrected the bundled GPT-5.6 Sol, Terra, and Luna catalog to
a 272K raw context window. Match that behavior in Claude Code with:

```sh
CLAUDE_CODE_MAX_CONTEXT_TOKENS=272000
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000
CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90
```

Use the concrete custom model ids (`gpt-5.6-sol`, `gpt-5.6-terra`, and
`gpt-5.6-luna`) with these overrides. A `[1m]` suffix remains supported and is
stripped by the proxy, but it makes Claude Code report a 1M window rather than
the bundled catalog's 272K capacity.

Claude Code 2.1.212 reserves up to 20K tokens for output before applying the
configured compaction percentage. With the settings above, the current 90%
trigger is therefore approximately `(272K - 20K) * 90% = 226.8K`, rather than
the simpler `272K * 90%` estimate. The Grok launch profile uses a 500K raw and
auto-compact window, which triggers at approximately 432K under the same
reserve and percentage.

If you'd rather disable auto-compact completely, set
`DISABLE_AUTO_COMPACT=1` in your env or `~/.claude/settings.json`. Manual
`/compact` still works, but you risk hitting real upstream limits before
Claude Code can compact for you.

## Providers

### Codex (ChatGPT)

Upstream: `https://chatgpt.com/backend-api/codex/responses` (Responses API).

OpenAI's Thibault Sottiaux has publicly welcomed using Codex through other coding
harnesses:

> [Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't
> discriminate on the harness.](https://x.com/thsottiaux/status/2075830097488249060)

Set `ANTHROPIC_MODEL` to a model your ChatGPT subscription is allowed to use.
Append `-fast` to a Codex model name to request Codex fast mode for that request
without restarting the proxy. For example, `gpt-5.6-sol-fast` is sent upstream as
model `gpt-5.6-sol` with `service_tier: "priority"`. An explicit
`codex.serviceTier` / `CCP_CODEX_SERVICE_TIER` override normally takes precedence,
but an incoming `service_tier: "standard_only"` request suppresses every local
priority/fast override and omits the upstream priority tier.

The private Codex gateway rejects `max_output_tokens`, `max_tokens`, and
`max_completion_tokens`. `/v1/messages` therefore requires a positive Anthropic
`max_tokens` for protocol compatibility but deliberately omits all three fields
from the upstream request; the requested ceiling cannot be enforced by this
gateway. Missing `max_tokens` and `max_tokens: 0` (Anthropic's cache-only shape)
return HTTP 400 before any model request is dispatched. Sampling controls
(`temperature`, `top_p`, or `top_k`) and non-empty `stop_sequences` are likewise
rejected before dispatch instead of being silently ignored. An empty
`stop_sequences` array is accepted as a no-op.

Reasoning effort: Claude Code's `output_config.effort` value (the one you see in
the UI as `◐ medium · /effort`) is normally forwarded one-to-one as Codex
`reasoning.effort` (`low` / `medium` / `high` / `xhigh` / `max`). Standalone
`xhigh` therefore remains `xhigh`, and Claude Code's `ultrathink` keyword leaves
the request effort unchanged. Ultracode keeps its workflow orchestration
client-side, while the proxy recognizes Claude Code's generated Ultracode
session marker and sends that mode's `xhigh` turns as wire-level `max`. When
Claude Code omits effort for a Haiku request, the mapped `gpt-5.6-luna` model
defaults to `medium`. An explicit `codex.effort` / `CCP_CODEX_EFFORT` override
still takes precedence, and the global override can also force `none`.

Reasoning summaries: when a Codex request has reasoning effort, the proxy asks
Codex for `reasoning.summary: "auto"` and translates returned summary deltas
into Anthropic `thinking` content blocks. Codex decides when a summary is useful,
so simple prompts can emit no thinking block. Set `codex.reasoningSummary` /
`CCP_CODEX_REASONING_SUMMARY` to `off` or `none` to suppress summaries while
keeping `reasoning.effort` and encrypted continuation content.

Without `x-ccproxy-compaction-model`, Claude Code's automatic and manual
compaction requests keep the selected Codex model but cap reasoning effort at
`medium`. Compaction is a structured summary pass, and avoiding `max` reasoning
substantially reduces the wait without changing the effort used by normal
turns. An explicit global `codex.effort` / `CCP_CODEX_EFFORT` override still
takes precedence.

Claude Code's hosted `web_search_20250305` tool is translated to Codex's native
Responses `web_search` tool with live external web access and non-empty native
domain filters. Forced searches use Codex's required `allowed_tools` form so
structured filters remain active. Codex does not expose a wire-level
hosted-search limit, so the proxy supplies best-effort model guidance and also
enforces the Claude tool's `max_uses` while accepting the response. An upstream
search beyond the limit makes the response fail closed before that call is
emitted, but this cannot undo provider-side work already performed. Codex
hosted search calls are emitted back to Claude Code as Anthropic
`server_tool_use` and `web_search_tool_result` blocks with
`usage.server_tool_use.web_search_requests` so Claude Code can account for
completed searches.

Deferred `tool_reference` loading for Codex and Grok is intentionally bound to
Claude Code's built-in, non-deferred custom tool named `ToolSearch`. The generic
Anthropic wire does not mark arbitrary custom tools as search-capable, so a
differently named search tool is rejected rather than trusted as a
capability-control source. Source and referenced tools must permit the caller
represented by history, and failed tool results cannot activate deferred
schemas.

Confirmed working on **Plus**:

- `gpt-5.4`
- `gpt-5.3-codex`

Also verified:

- `gpt-5.2`
- `gpt-5.4-mini`

If the resolved model isn't supported by your account, upstream returns a 400
like
`"The 'gpt-4.1' model is not supported when using Codex with a ChatGPT account."`.
The proxy surfaces that verbatim.

Auth:

| Command             | What it does                               |
| ------------------- | ------------------------------------------ |
| `codex auth login`  | Browser OAuth (PKCE) via `auth.openai.com` |
| `codex auth device` | Device-code OAuth for headless machines    |
| `codex auth status` | Show account ID + token expiry             |
| `codex auth logout` | Delete stored credentials                  |

### Grok

Upstream: `https://cli-chat-proxy.grok.com/v1/responses` (Responses API).

Supported model ids are `grok-composer-2.5-fast`, `grok-4.5`, and the
`grok-4.5-medium` / `grok-4.5-high` profiles. The profiles send `grok-4.5`
upstream and force their named reasoning effort, regardless of the incoming
effort; the base id preserves the incoming `/effort` selection. Model access can
vary by account and region. The proxy translates Claude Code messages, function
tools, tool results, structured output, thinking, token
counts, and streaming events. Recognized compaction requests do not receive the
otherwise-default hosted search tool.
The CLI endpoint's plaintext reasoning summaries are suppressed because they are
not signed Anthropic thinking blocks and can contain draft answers; periodic
Anthropic `ping` events preserve downstream liveness while Grok reasons.
Failures delivered inside an HTTP 200 stream (`error`, `response.error`, or
`response.failed`) retain their upstream status, message, and `Retry-After`.
Retryable 429/500/502/503/504/529 failures rebuild the request only before semantic output;
after text or tool output begins, the proxy emits a typed Anthropic in-band
error instead of replaying a potentially duplicate tool call. Header timeouts,
body/stream failures, and explicit 204 or non-SSE success responses leave the
POST outcome ambiguous and therefore terminate without replaying the model
request.
Claude Code reasoning effort is forwarded as Responses `reasoning.effort` for
Grok 4.5; `xhigh` and `max` are capped at Grok's supported `high` level.
Composer has no reliable request-level effort mapping, so a request that
supplies `output_config.effort` for `grok-composer-2.5-fast` is rejected before
dispatch instead of silently dropping the control.
An explicit Anthropic `web_search_...` tool uses Grok's hosted general web
search, and explicit `type: "x_search"` maps to hosted X search. A tool named
`WebSearch` or `XSearch` with no `type`, or with `type: "function"`, remains a
client function with its schema intact. Because hosted `x_search` has no
observable Anthropic tool block, it cannot be forced or satisfy a required
X-only response; those cases fail closed instead of returning an empty
successful turn. Hosted `max_uses` is enforced while accepting the response;
as with Codex, this cannot undo provider-side work already performed. Function
tools explicitly enable parallel calls unless Claude disables them; Web/X
search intent keeps the other Read, Bash, MCP, and function tools available so
independent work can be emitted in the same turn.

Authentication uses browser OAuth with S256 PKCE through `auth.x.ai` and an
ephemeral loopback callback. Headless hosts can use the OAuth device-code flow
(`grok auth device`) instead, which prints a verification URL and user code and
polls the same issuer. The proxy stores its own access and refresh tokens,
refreshes them five minutes before expiry, and does not use `~/.grok/auth.json`.
On macOS, Grok prefers Keychain service `claude-code-proxy.grok` and falls back
to its mode-0600 provider file when Keychain access is unavailable. Setting
`CCP_CONFIG_DIR` keeps isolated runs file-only and never reads or mutates the
default Keychain entry.

| Command            | What it does                          |
| ------------------ | ------------------------------------- |
| `grok auth login`  | Browser OAuth with a local callback   |
| `grok auth device` | Device-code OAuth for headless hosts  |
| `grok auth status` | Show token expiry and storage path    |
| `grok auth logout` | Delete proxy-owned credentials        |

## How it works

```mermaid
sequenceDiagram
    autonumber
    participant CC as Claude Code
    participant P as claude-code-proxy
    participant AUTH as OAuth host / credential store
    participant U as Upstream API<br/>(Codex or Grok)

    Note over P,AUTH: One-time: PKCE / device OAuth<br/>tokens cached locally for reuse

    CC->>P: POST /v1/messages (Anthropic shape, stream: true)

    alt access token expiring
        P->>AUTH: POST /oauth/token (refresh_token)
        AUTH-->>P: new access (+ rotated refresh)
    end

    P->>P: translate request<br/>• strip Anthropic-only fields<br/>• system blocks → instructions / system message<br/>• tool_use / tool_result ↔ provider-specific shapes<br/>• prompt_cache_key = session id
    P->>U: POST upstream<br/>Bearer + provider-specific headers
    U-->>P: provider Responses stream
    P->>P: reducer: typed events<br/>(thinking / text / tool start/delta/stop, finish)
    P-->>CC: Anthropic SSE<br/>(message_start, content_block_*, message_delta, message_stop)
```

## Commands

| Command                                             | Description                 |
| --------------------------------------------------- | --------------------------- |
| [`serve`](#serve)                                   | Start the proxy and monitor |
| [`demo`](#demo)                                     | Open the TUI with mock data |
| [`diagnostics`](#diagnostics)                       | Collect or upload metadata-only diagnostics |
| `claude gpt -- <args>` / `claude grok -- <args>`    | Launch a coherent GPT or Grok Claude Code profile |
| `models`                                            | List active Codex and Grok HTTP model routes |
| `codex auth login` / `device` / `status` / `logout` | Codex OAuth management      |
| `grok auth login` / `device` / `status` / `logout`  | Grok OAuth management       |

---

### `serve`

Starts the HTTP proxy and blocks. Binds to `127.0.0.1` by default. Set
`CCP_BIND_ADDRESS` or the `bindAddress` config key to choose another IP address.
Non-loopback addresses also require `--allow-remote-unauthenticated`,
`CCP_ALLOW_REMOTE_UNAUTHENTICATED=1`, or
`server.allowRemoteUnauthenticated: true`; the proxy emits a security warning
because incoming requests are not authenticated.
When stdout is a terminal, `serve` opens a monitor TUI showing sessions, active
requests, recent requests, output throughput, and error events. Token throughput
uses the change in cumulative output usage over the matching observed generation
interval. For Codex, timing begins with the first upstream response event or body
chunk and ends with the terminal usage observation, so reasoning tokens and their
generation time have the same scope. Requests without both usage and timing are
excluded from the session rate. Session throughput combines the matched token and
duration samples retained by the monitor. Use `--no-monitor` to run with plain
terminal output.

Logs are written to the platform state directory and rotated at 20 MiB. Set
`CCP_LOG_STDERR=1` to mirror log lines to stderr while running without the
monitor.

```sh
claude-code-proxy serve
PORT=11435 claude-code-proxy serve
claude-code-proxy serve --no-monitor
CCP_LOG_STDERR=1 claude-code-proxy serve --no-monitor
```

The plain server banner prints the active Codex/Grok model-to-provider mapping
on startup. One `serve` process dispatches to either active provider based on
the `model` field in each request. Requests whose model is not in that public
registry are rejected with HTTP 400 listing the supported IDs.

---

### `demo`

Opens the real monitor TUI with deterministic simulated traffic and does not bind
a port or start the proxy server. Requests advance through each active lifecycle
status, token totals and throughput update continuously, completed requests enter
the recent and event panes, and new sessions appear over time. The baseline data
also covers successful and failed requests, sessions with and without project
names, both routed providers, throughput fallbacks, HTTP errors, and optional request
details.

```sh
claude-code-proxy demo
```

Use the normal monitor shortcuts and resize the terminal to inspect wide and
compact request layouts.

---

### `diagnostics`

Creates a small, metadata-only archive that can be sent from Linux or WSL2 to a
development machine when a failure is easier to reproduce there than locally.
Collection is local and upload is always a separate, explicit command.

```sh
# Read the normal platform state directory and write under diagnostics/ there.
claude-code-proxy diagnostics collect --machine-label wsl-laptop

# Narrow a report to the request-id response header shown for a failed request.
claude-code-proxy diagnostics collect --request-id REQUEST_ID --last-lines 10000

# Choose paths explicitly, useful for an isolated WSL installation.
claude-code-proxy diagnostics collect \
  --state-dir "$HOME/.local/state/claude-code-proxy" \
  --output /tmp/ccproxy-diagnostics.tar.gz
```

Every archive contains exactly `manifest.json`, `events.jsonl`, and
`environment.json`. The collector uses a fixed event and field allowlist. It
records lifecycle metadata such as provider/model routing, status and elapsed
time, retry or timeout classification, stream interruption, tool-block
start/completion/interruption, and whether a later client `tool_result` was
marked as an error. A completed tool block means that ccproxy emitted a
complete block into the downstream response; it does not prove that Claude
Code received or executed the tool.

The bundle excludes prompts, message content, tool arguments and results, raw
error text, session ids, credentials, configuration, `traffic/`, `errors/`, and
the original `proxy.log`. Dynamic upstream error text is reduced to a bounded
classification and fingerprint. There is deliberately no option to add raw
traffic or error captures to a diagnostic bundle.

ccproxy can observe API tool blocks and the `tool_result` that Claude Code later
sends back. It cannot see a Brave Search MCP process's own TCP connection,
timeout, stderr, or crash because that connection runs between Claude Code and
the MCP server. Diagnose that part with Claude Code/MCP logs separately; do not
attach those raw logs to this bundle without reviewing them yourself.

Upload uses the local OpenSSH `sftp` client. Put the development machine's port,
key, and host-key policy in `~/.ssh/config`; this works the same way inside WSL2,
whose SSH config is separate from Windows and must be configured in the Linux
home directory. The remote directory must already exist and be absolute.

```sshconfig
Host ccproxy-dev
  HostName 192.0.2.10
  User developer
  IdentityFile ~/.ssh/id_ed25519
```

```sh
# Prompts on a terminal before uploading.
claude-code-proxy diagnostics upload /tmp/ccproxy-diagnostics.tar.gz \
  --host ccproxy-dev \
  --remote-dir /srv/ccproxy-diagnostics

# Scripts and other non-interactive shells must opt in explicitly.
claude-code-proxy diagnostics upload /tmp/ccproxy-diagnostics.tar.gz \
  --host ccproxy-dev \
  --remote-dir /srv/ccproxy-diagnostics \
  --yes
```

Before transfer, the command validates the three members and rebuilds a fresh,
canonical ccproxy `metadata-only` archive, so unvalidated trailing bytes or
extra gzip data are never uploaded. It uploads to a temporary name and then
renames it over SFTP, so an interrupted connection does not leave a
final-looking partial file. No background or automatic upload is performed.

---

### Codex auth commands

#### `codex auth login`

Runs the PKCE browser flow against `auth.openai.com` using the Codex CLI's
client ID. Prints a URL, opens a local callback listener on port 1455, waits for
the browser to redirect back, and stores the resulting access / refresh tokens.
On macOS it prefers Keychain and safely falls back to a mode-0600 file when
non-interactive Keychain writes are unavailable. The process reports the actual
backend and exits automatically once the tokens are saved.

```sh
claude-code-proxy codex auth login
```

Sign in with your **ChatGPT Plus/Pro account**, not an OpenAI API account. The
token file includes the extracted `chatgpt_account_id` so the proxy can set the
`ChatGPT-Account-Id` header on every upstream call.

The proxy owns and rotates its Codex credentials independently. It does not read
or modify native Codex CLI credentials in `~/.codex` or native Codex credential
backends. If an earlier proxy version used your native Codex login implicitly,
run `claude-code-proxy codex auth login` or
`claude-code-proxy codex auth device` once after upgrading.

#### `codex auth device`

Same OAuth flow, but for headless machines. Prints a short user code and a URL;
you enter the code from any browser on any other device, and the CLI polls
`auth.openai.com` until you authorize, then stores the token.

```sh
claude-code-proxy codex auth device
```

Useful over SSH, inside a container, or on any host that can't open a browser.

#### `codex auth status`

Shows whether credentials are stored, the account ID, and how long until the
access token expires. Non-zero exit if no auth is present.

```sh
claude-code-proxy codex auth status
```

Example output:

```
Account: 79342a5e-57b7-44ea-bfdc-a83ba070dad6
Expires: 2026-04-28T16:46:04.827Z (in 863946s)
Storage: File fallback: /Users/alice/.config/claude-code-proxy/codex/auth.json (macOS Keychain write unavailable)
```

The proxy refreshes the access token 5 minutes before expiry with a
single-flight guard, so concurrent requests never trigger stampedes of refresh
calls.

#### `codex auth logout`

Removes stored auth credentials. On macOS this clears both the Keychain entry
and any file fallback. No server call is needed; the refresh token just becomes
dead.

```sh
claude-code-proxy codex auth logout
```

Run `codex auth login` again to re-authenticate.

---

### Endpoints

The proxy speaks enough of the Anthropic API for Claude Code:

- `POST /v1/messages`: the main turn endpoint (streaming and non-streaming)
- `POST /v1/messages?beta=true`: same (Claude Code always sends `?beta=true`)
- `POST /v1/messages/count_tokens`: local token count via `tiktoken-rs`
  (`o200k_base` text tokenization plus compatibility estimates for fixed
  request overhead and non-text content); used by Claude Code's compaction
  logic. GPT and Grok share a two-slot blocking admission gate, and the work
  runs through Tokio's blocking pool. A request waiting more than 30 seconds
  for an admission slot receives retryable HTTP 503 overload.
- `GET /v1/models`: the active Codex and Grok model catalog
- `GET /healthz`: liveness check
- `GET /version`: build SHA, binary SHA-256, PID, executable path, startup time,
  and a non-secret configuration fingerprint

## Configuration

Settings can come from either environment variables or a `config.json` file.
Precedence per setting: **env var > config file > built-in default**. The
config file is optional — env-var-only setups continue to work unchanged.

The file lives at `~/.config/claude-code-proxy/config.json` on macOS
(deliberately not `~/Library`), at `%APPDATA%\claude-code-proxy\config.json` on
Windows, and at
`${XDG_CONFIG_HOME:-$HOME/.config}/claude-code-proxy/config.json` on Linux. Set
`CCP_CONFIG_DIR` to use a separate config and auth directory for that process.
On macOS and Windows, if that platform's primary file does not exist, ccproxy
also checks the historical `XDG_CONFIG_HOME/claude-code-proxy/config.json`
location when `XDG_CONFIG_HOME` is set, otherwise the home-directory
`.config/claude-code-proxy/config.json` location. Older releases used this
generic path because of incorrect platform detection. This is a read-only
fallback: creating the primary file takes precedence, and ccproxy does not move
or delete the old file. `CCP_CONFIG_DIR` disables the fallback.

```json
{
  "bindAddress": "127.0.0.1",
  "port": 18765,
  "server": {
    "allowRemoteUnauthenticated": false,
    "maxRequestBodyBytes": 33554432,
    "maxBufferedRequestBytes": 268435456,
    "maxConcurrentRequests": 64,
    "maxConcurrentPerProvider": 48,
    "maxConcurrentPerSession": 24,
    "requestBodyIdleTimeoutMs": 5000,
    "requestBodyTotalTimeoutMs": 30000
  },
  "codex": {
    "originator": "claude-code-proxy",
    "userAgent": "claude-code-proxy/dev",
    "model": "gpt-5.4",
    "effort": "medium",
    "reasoningSummary": "auto",
    "serviceTier": "fast",
    "responsesLite": true,
    "parallelTools": false,
    "baseUrl": "https://chatgpt.com/backend-api/codex/responses",
    "transport": "auto",
    "connectTimeoutMs": 15000,
    "headerTimeoutMs": 60000,
    "httpFirstByteTimeoutMs": 30000,
    "bodyIdleTimeoutMs": 300000,
    "totalTimeoutMs": 540000,
    "streamHeartbeatMs": 5000,
    "websocketResponseStartTimeoutMs": 60000,
    "websocketIdleTimeoutMs": 300000,
    "maxIdleWebSockets": 128,
    "idleWebSocketTtlMs": 300000,
    "previousResponseId": false,
    "unsafeSalvageToolCallOnClose": false
  },
  "grok": {
    "baseUrl": "https://cli-chat-proxy.grok.com/v1",
    "clientVersion": "0.2.93",
    "connectTimeoutMs": 10000,
    "headerTimeoutMs": 60000,
    "firstByteTimeoutMs": 60000,
    "bodyIdleTimeoutMs": 300000,
    "totalTimeoutMs": 540000,
    "streamHeartbeatMs": 5000
  },
  "log": {
    "stderr": false,
    "verbose": false
  }
}
```

| Variable                         | Config key                 | Default                                           | Purpose                                                                                                                                                                           |
| -------------------------------- | -------------------------- | ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CCP_BIND_ADDRESS`               | `bindAddress`              | `127.0.0.1`                                       | Proxy listen IP address; use `0.0.0.0` only when remote access is required and protected                                                                                          |
| `CCP_ALLOW_REMOTE_UNAUTHENTICATED` | `server.allowRemoteUnauthenticated` | `false`                                | Explicit acknowledgement required for non-loopback binds; accepts `1`, `true`, or `yes`, and does not add authentication                                                          |
| `PORT`                           | `port`                     | `18765`                                           | Proxy listen port                                                                                                                                                                 |
| `CCP_CONFIG_DIR`                 | unset                      | platform config dir                               | Per-process configuration and provider-auth directory                                                                                                                             |
| `XDG_STATE_HOME`                 | —                          | `~/.local/state`                                  | Linux/macOS base dir for `proxy.log`                                                                                                                                              |
| `CCP_LOG_STDERR`                 | `log.stderr`               | unset                                             | Also mirror log lines to stderr; any env value enables it                                                                                                                         |
| `CCP_LOG_VERBOSE`                | `log.verbose`              | unset                                             | Preserve full string fields in `proxy.log`; any env value enables it                                                                                                              |
| `CCP_TRAFFIC_LOG`                | —                          | unset                                             | Write full per-request traffic captures under `traffic/` for session debugging (`1`, `true`, or `yes`)                                                                            |
| `CCP_TRAFFIC_MAX_TOTAL_BYTES`    | —                          | `536870912`                                       | Admission ceiling for charged bytes across the traffic root; every regular file costs at least one 4 KiB allocation unit, rounded up, so tiny files cannot bypass the disk budget  |
| `CCP_TRAFFIC_MAX_CAPTURE_BYTES`  | —                          | `67108864`                                        | Charged-byte ceiling for one request capture; exceeding it skips that artifact and the remainder of that capture without affecting the proxied request                             |
| `CCP_TRAFFIC_MAX_TOTAL_FILES`    | —                          | `65536`                                           | Admission ceiling for regular files across the traffic root; existing files count after restart and symbolic links are never followed                                             |
| `CCP_TRAFFIC_MAX_CAPTURE_FILES`  | —                          | `4096`                                            | Regular-file ceiling for one request capture, bounding inode use even when captured events are individually small                                                                  |
| `CCP_MAX_REQUEST_BODY_BYTES`     | `server.maxRequestBodyBytes` | `33554432`                                      | Hard cap for both Anthropic request endpoints; declared and streamed overflows return 413                                                                                         |
| `CCP_MAX_BUFFERED_REQUEST_BYTES` | `server.maxBufferedRequestBytes` | `268435456`                                  | Process-wide budget for buffered request bodies and initial replay material; long-lived Codex continuation capture has a separate bounded pool; saturation returns retryable 429   |
| `CCP_MAX_CONCURRENT_REQUESTS`    | `server.maxConcurrentRequests` | `64`                                            | Global in-flight request limit; saturated requests are shed immediately instead of entering an unbounded wait queue                                                               |
| `CCP_MAX_CONCURRENT_PER_PROVIDER` | `server.maxConcurrentPerProvider` | `48`                                         | Per-provider in-flight limit, preventing one upstream from consuming the entire process                                                                                           |
| `CCP_MAX_CONCURRENT_PER_SESSION` | `server.maxConcurrentPerSession` | `24`                                            | Per-session in-flight limit; checked before the global limit so one child-agent wave cannot occupy every global slot                                                              |
| `CCP_REQUEST_BODY_IDLE_TIMEOUT_MS` | `server.requestBodyIdleTimeoutMs` | `5000`                                       | Maximum pause while reading a local request body                                                                                                                                  |
| `CCP_REQUEST_BODY_TOTAL_TIMEOUT_MS` | `server.requestBodyTotalTimeoutMs` | `30000`                                    | Absolute request-body read budget; timeout returns 408 and releases all reservations                                                                                              |
| `CCP_CODEX_MODEL`                | `codex.model`              | unset                                             | Force all Codex requests to this model (`gpt-5.2`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.5`, `gpt-5.6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`) |
| `CCP_CODEX_EFFORT`               | `codex.effort`             | unset                                             | Force all Codex requests to this reasoning effort (`none`, `low`, `medium`, `high`, `xhigh`, `max`)                                                                                |
| `CCP_CODEX_REASONING_SUMMARY`    | `codex.reasoningSummary`   | unset                                             | Request Codex reasoning summaries when reasoning effort is enabled; `off` and `none` suppress summaries                                                                           |
| `CCP_CODEX_SERVICE_TIER`         | `codex.serviceTier`        | unset                                             | Force all Codex requests to this service tier (`fast`/`priority`, `flex`; `fast` is sent upstream as `priority`)                                                                  |
| `CCP_CODEX_RESPONSES_LITE`       | `codex.responsesLite`      | `true`                                            | Use the official GPT-5.6 Responses Lite request shape; set `false` to use the full Responses shape and permit parallel tool calls when the account supports it; Luna is Lite-only and is rejected rather than silently changed to another model |
| `CCP_CODEX_PARALLEL_TOOLS`       | `codex.parallelTools`      | `false`                                           | While Responses Lite is enabled, opt eligible GPT-5.6 Sol/Terra requests with at least one directly callable function tool into the full Responses shape, including repeated calls to one tool and named tool choices; Luna and internal or constrained requests stay on Lite |
| `CCP_CODEX_BASE_URL`             | `codex.baseUrl`            | `https://chatgpt.com/backend-api/codex/responses` | Override the Codex Responses endpoint                                                                                                                                             |
| `CCP_CODEX_TRANSPORT`            | `codex.transport`          | `auto` (resolves to `http` when a system proxy intercepts the upstream) | Codex transport: live `websocket`, incremental HTTP SSE with `http`, or WebSocket-first fallback plus an origin-wide breaker in `auto`                                         |
| `CCP_CODEX_CONNECT_TIMEOUT_MS`   | `codex.connectTimeoutMs`   | `15000`                                           | Maximum time to establish the Codex HTTP TCP/TLS connection, including an HTTP fallback selected by `auto`                                                                       |
| `CCP_CODEX_HEADER_TIMEOUT_MS`    | `codex.headerTimeoutMs`    | `60000`                                           | Maximum time to receive Codex HTTP response headers                                                                                                                               |
| `CCP_CODEX_HTTP_FIRST_BYTE_TIMEOUT_MS` | `codex.httpFirstByteTimeoutMs` | inherits `codex.bodyIdleTimeoutMs` (`300000`) | Maximum time to receive the first Codex HTTP response body byte; configure it independently to fail a header-only stall sooner, while the total request deadline remains authoritative |
| `CCP_CODEX_BODY_IDLE_TIMEOUT_MS` | `codex.bodyIdleTimeoutMs`  | `300000`                                          | Maximum gap between Codex HTTP response body chunks; this per-gap limit cannot extend the total request deadline                                                                 |
| `CCP_CODEX_TOTAL_TIMEOUT_MS`     | `codex.totalTimeoutMs`     | `540000`                                          | Total wall-clock budget across WebSocket retries, Auto HTTP fallback, and the remaining live stream; caps stragglers while remaining configurable                                   |
| `CCP_CODEX_STREAM_HEARTBEAT_MS`  | `codex.streamHeartbeatMs`  | `5000`                                            | Emit Anthropic `ping` events during Codex streaming; values are clamped to 1 through 60 seconds and do not extend request deadlines                                                  |
| `CCP_CODEX_WEBSOCKET_RESPONSE_START_TIMEOUT_MS` | `codex.websocketResponseStartTimeoutMs` | `60000` | Maximum wait for the first Codex response event; a timeout invalidates the connection and contributes to the breaker, but does not replay an already-sent request                    |
| `CCP_CODEX_WEBSOCKET_IDLE_TIMEOUT_MS` | `codex.websocketIdleTimeoutMs` | `300000` | Maximum gap between Codex response events; WebSocket control frames do not extend this business-event deadline                                                                    |
| `CCP_CODEX_MAX_IDLE_WEBSOCKETS` | `codex.maxIdleWebSockets` | `128` | Maximum session WebSockets retained while idle; values above 4096 are clamped                                                                                                      |
| `CCP_CODEX_IDLE_WEBSOCKET_TTL_MS` | `codex.idleWebSocketTtlMs` | `300000` | Time an idle session WebSocket remains pooled; values above 30 minutes are clamped and a background reaper closes expired entries                                                  |
| `CCP_CODEX_PREVIOUS_RESPONSE_ID` | `codex.previousResponseId` | `false`                                           | Enable payload continuation with `previous_response_id` when the request is append-only; session WebSocket connection reuse remains active when this is false                       |
| `CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE` | `codex.unsafeSalvageToolCallOnClose` | `false` | **Dangerous compatibility switch.** Literal `true` restores the legacy heuristic that treats complete-looking tool JSON as committed after upstream closes without authoritative terminal events; this can execute an uncommitted tool call |
| `CCP_CODEX_ORIGINATOR`           | `codex.originator`         | `claude-code-proxy`                               | Override the `originator` header sent to Codex                                                                                                                                    |
| `CCP_CODEX_USER_AGENT`           | `codex.userAgent`          | `claude-code-proxy/<version>`                     | Override the `User-Agent` header sent to Codex                                                                                                                                    |
| `CCP_GROK_BASE_URL`              | `grok.baseUrl`             | `https://cli-chat-proxy.grok.com/v1`              | Override the Grok Responses API base URL                                                                                                                                          |
| `CCP_GROK_CLIENT_VERSION`        | `grok.clientVersion`       | `0.2.93`                                          | Override the Grok client version header                                                                                                                                           |
| `CCP_GROK_CONNECT_TIMEOUT_MS`    | `grok.connectTimeoutMs`    | `10000`                                           | Maximum time to establish the Grok TCP/TLS connection                                                                                                                             |
| `CCP_GROK_HEADER_TIMEOUT_MS`     | `grok.headerTimeoutMs`     | `60000`                                           | Maximum time to receive Grok response headers; a timeout has an ambiguous POST outcome and is not replayed                                                                         |
| `CCP_GROK_FIRST_BYTE_TIMEOUT_MS` | `grok.firstByteTimeoutMs`  | `60000`                                           | Maximum time to receive the first Grok response body byte; a timeout terminates the request without replay                                                                         |
| `CCP_GROK_BODY_IDLE_TIMEOUT_MS`  | `grok.bodyIdleTimeoutMs`   | `300000`                                          | Maximum gap between Grok response body chunks; this per-gap limit cannot extend the total request deadline                                                                         |
| `CCP_GROK_TOTAL_TIMEOUT_MS`      | `grok.totalTimeoutMs`      | `540000`                                          | Total wall-clock budget across authentication, all physical attempts, backoff, stream rebuilds, and response-body streaming                                                        |
| `CCP_GROK_STREAM_HEARTBEAT_MS`   | `grok.streamHeartbeatMs`   | `5000`                                            | Emit Anthropic `ping` events during downstream silence; configured values are clamped to 1 through 60 seconds and do not extend the total deadline                                 |
| `CCP_ORIGINATOR`                 | —                          | `claude-code-proxy`                               | Fallback for `CCP_CODEX_ORIGINATOR`                                                                                                                                               |
| `CCP_USER_AGENT`                 | —                          | unset                                             | Fallback for `CCP_CODEX_USER_AGENT`                                                                                                                                               |

A malformed or unreadable `config.json` is reported on stderr. At startup it is
ignored and defaults are used; during hot reload ccproxy keeps the last valid
snapshot and generation until the file becomes valid again. Type errors reject
the new snapshot as a whole so a partially applied configuration cannot mix old
and new policy.

Codex uses `auto` transport by default. Without a system proxy, `auto` starts
with live WebSocket streaming while the connection is healthy. It falls back
to incremental HTTP only when the WebSocket attempt is known not to have
dispatched the model request, such as a handshake or pre-request pool failure.
When an HTTP(S) environment or operating-system proxy intercepts the upstream
URL, `auto` starts directly on
incremental HTTP SSE so Codex traffic follows that proxy instead of silently
bypassing it. Explicit `CCP_CODEX_TRANSPORT=websocket` remains a strict direct
connection because the WebSocket connector does not implement HTTP CONNECT. If
`HTTPS_PROXY`, `ALL_PROXY`, or another environment proxy applies to the Codex
upstream, explicit WebSocket mode now fails before authentication or model
dispatch with an actionable error. Use `auto` / `http`, or add the upstream
host to `NO_PROXY` when a direct WebSocket connection is intentional. An
operating-system-only proxy does not trigger this explicit-mode rejection.
The proxy decision is captured together with the Codex HTTP client at process
startup; restart the service after changing environment or operating-system
proxy settings.
Pool probes and busy pooled connections are also safe pre-request fallback
cases. After a request frame is sent, response-start or idle timeouts, failed
heartbeats, connection loss, malformed events, and a close without a terminal
event have an ambiguous outcome and are reported without replay. These failures
still count toward transport health. Three consecutive WebSocket transport
health failures for the same upstream origin open a 30-second circuit, temporarily
routing all sessions for that endpoint through HTTP. After the cooldown, one WebSocket request
probes recovery while concurrent requests keep using HTTP; a successful response
or any non-transport result resets the failure count.
Pooled WebSockets must answer a matching Ping/Pong probe before reuse. A
successful terminal ordering Pong supplies a one-shot, short-lived health
credit to the immediately following tool turn, avoiding a duplicate round trip;
older idle connections are probed normally. Active connections are also probed
after 30 seconds without a frame, so a half-open
socket left behind by a VPN or proxy-node switch is detected without waiting for
the business idle timeout. Only failures proven to occur before model dispatch,
plus explicit upstream retry responses 429/500/502/503/504/529, can consume the
current request's retry budget. A timeout or reset after an HTTP POST or
WebSocket request frame may mean the model already ran, so it is never replayed,
even when no Anthropic semantic output was emitted.
An upstream hosted-search added, in-progress, searching, completed, or done
event is also a replay barrier: after observing one, Codex will not retry,
reconnect, or switch transports even if no Anthropic content block has yet been
emitted.
Both Codex live transports emit a standard Anthropic `ping` after 5 seconds
without visible output, including while waiting for the first generation event.
HTTP SSE is decoded incrementally, so cancellation closes the upstream socket
instead of waiting for a buffered response. WebSocket retries,
HTTP retries, and an Auto HTTP fallback share one request-level dispatch budget:
at most four model HTTP/WebSocket dispatches, at most two OAuth token POSTs, and
at most six combined dispatches. OAuth is counted only immediately before a
token POST; reusing credentials already rotated by another request is free. A
model 401 triggers refresh only after the proxy reserves capacity for its replay.
After Auto falls back, that logical request remains on HTTP rather than probing
WebSocket again. Live WebSocket messages are capped at 1 MiB at the wire,
intermediary event channels hold two items each, and the final downstream queue
has a 2 MiB byte budget. Buffered HTTP success bodies are capped at 8 MiB, and
live translator input has the same cumulative ceiling. A downstream that remains
backpressured for 60 seconds is terminated and its upstream request is cancelled.
When Auto encounters a statusless pre-dispatch WebSocket fault, it starts the
HTTP fallback immediately because this is a transport switch rather than a
repeat attempt against the failed connection. Other first failures proven to
occur before model dispatch and carrying no `Retry-After` use a 100–300ms
full-jitter recovery delay. Explicit overload and rate-limit responses retain
the jittered exponential backoff capped at 30
seconds. Numeric and HTTP-date
`Retry-After` values are honored directly within that retry budget.
Codex WebSocket transport reuses a session connection whether continuation is
enabled or not; with continuation off, every turn still sends its complete
input. `CCP_CODEX_PREVIOUS_RESPONSE_ID=1` additionally enables opt-in payload
continuation for append-only turns. Continuation keeps in-memory state keyed by
Claude Code session id and sends
`previous_response_id` only when the translated request shape is unchanged and
the new input strictly extends the previous transcript. On mismatch, missing
state, an explicit `previous_response_not_found`, or a replay-safe pre-request
setup failure, the proxy clears unsafe continuation state and sends the full
request instead. A connection failure after the request frame is sent has an
ambiguous outcome and terminates without full-context replay.
Structured-output turns deliberately do not record continuation state: the
SchemaBridge can remove synthetic `null` fields before Claude Code persists the
assistant text, while the upstream response id still names the raw response.
The next turn therefore uses full context instead of combining inconsistent
histories. Tool arguments rewritten for Claude Code compatibility likewise make
that turn continuation-ineligible.
If that failure happens after complete-looking function arguments but before
authoritative `response.output_item.done` and response terminal events, the
default is fail-closed: the proxy emits an SSE error and never synthesizes a
successful `tool_use` `message_stop`. Setting
`CCP_CODEX_UNSAFE_SALVAGE_TOOL_CALL_ON_CLOSE=true` (or
`codex.unsafeSalvageToolCallOnClose: true`) restores the old heuristic only for
short-term compatibility. Valid JSON does not prove that the model committed
the call, so enabling it can execute a tool the upstream response had not
authoritatively completed. Both outcomes are recorded in
`terminal_resolution.authority` diagnostics.
The idle session WebSocket pool retains at most 128 sockets for five minutes by
default. Checkout removes a socket from the idle set, successful completion
returns it with a fresh idle timestamp, and a single background reaper closes
expired or excess idle entries even when no later request touches that session.
Continuation reduces repeated request upload size, but it does not increase the
upstream model context window. Multi-process or load-balanced deployments need
sticky sessions or shared state before enabling continuation.

The Codex and Grok model clients retain idle HTTP connections for five minutes,
send bounded TCP keepalives, and use adaptive HTTP/2 receive windows. This avoids
a fresh DNS/TCP/TLS setup after a long tool call or short interactive pause. The
client deliberately does not impose a shorter HTTP/2 PING acknowledgement
timeout over the providers' longer body-idle tolerance during network jitter.

Grok applies one 540-second wall-clock budget to authentication, connection and
header waits, every physical attempt, retry backoff, stream rebuilds, and the
remaining response body. Normal model requests use a constant-time wire-size
estimate only for fallback observability, so tokenizer work cannot delay ready
response headers. The exact local `o200k_base` work for `/count_tokens` runs on a
blocking worker outside the async runtime. The total deadline finishes before
Claude Code's common 600-second client timeout and prevents active reasoning
chunks from keeping a stuck request alive forever. During downstream silence the
proxy emits an Anthropic `ping` every 5 seconds; pings do not reset the total
deadline or close the pre-output recovery window. Connect failures proven to
occur before dispatch, explicit HTTP
429/500/502/503/504/529 responses, retryable upstream error events, and safe
authentication failures share one budget of up to three transient retries with
jittered backoff and `Retry-After` support. Header timeouts and response-body or
stream failures have an ambiguous POST outcome and terminate without rebuilding
the model request. Authentication is replayed only for idempotent OIDC
discovery, rate limits, or a token connection that failed before the refresh
request was sent. An ambiguous token response blocks later use of that same
credential generation across requests and process restarts. Refresh dispatches
use a cross-process file lock and a durable pre-dispatch marker. A successfully
rotated token remains usable in memory if persistence fails; the old durable
generation remains fail-closed until the new token is saved or credentials change,
and another rotation is refused while persistence is degraded. Browser/device
login and logout take the same mutation lock so they cannot be overwritten by an
in-flight refresh.
Grok `sequence_number` values are validated before reducer state changes:
identical repeated events within a bounded history are ignored, while gaps,
conflicts, backwards movement, or mixed numbered/legacy events terminate the
stream. This protocol check never deduplicates answer text or emoji. The
four-attempt wire cap applies to model `/responses` requests, while any authentication retry
consumes a transient slot and therefore reduces the remaining model retries. If
a Grok stream fails after any Anthropic text or tool event has been sent, the
proxy returns a stream error without replaying the request. Starting a hosted
web or X search closes the same replay window before downstream semantic output,
so reconnect cannot repeat the external search. Grok has no
`previous_response_id` continuation and never silently falls back to another
provider. The configurable total budget is capped at 24 hours; values at or above
the caller's own timeout lose the useful guarantee that the proxy fails first.
Grok streaming also uses a two-item/2 MiB downstream queue; a consumer stalled
for 60 seconds cancels the producer instead of pinning a connection until the
full model deadline. Raw stream bytes across rebuilds are capped at 8 MiB, which
also bounds reducer text and tool state.
Base64 image validation for Codex and Grok runs on blocking workers behind one
shared two-slot gate, so bounded full-image decoding cannot stall Tokio
heartbeats or multiply its peak decode memory across every admitted request. A
saturated gate fails after 30 seconds instead of building an unbounded decode
queue.

Claude Code retries API failures independently of the proxy. Its default retry
count can multiply a slow terminal failure, so the example configuration sets
`CLAUDE_CODE_MAX_RETRIES=2`; increase it only when you prefer availability over a
bounded user-visible wait.

The server aggregates at most 32 MiB per incoming request and 256 MiB across
active requests. Request bodies have five-second idle and 30-second total read
budgets. The byte budget is released after initial dispatch/replay material is
dropped and is not held for the duration of a downstream stream. Codex live
continuation capture uses a separate 20 MB process-wide budget and a 2 MB
per-session cap; when either cap is unavailable, continuation is disabled for
that turn without delaying the response. Rejected
response bodies are capped at 64 KiB with five-second idle and 10-second total
budgets. Global, provider, and session bulkheads shed excess work with 429 instead
of allowing a child-agent wave to exhaust the process. Concurrency permits and
bounded provider error responses remain held through response EOF or body drop,
not merely until response headers arrive.

Request lifecycle logs distinguish response headers from a finished stream:
`response_started` records the status returned by the provider,
`request_completed` is written only after response-body EOF; successful SSE
responses must first contain a complete Anthropic `message_stop`. A clean
channel EOF without that terminal event is surfaced as a body error and recorded
as `request_failed`. HTTP and in-band SSE errors are also recorded as failed,
while `request_abandoned` records downstream cancellation before completion.

### Files

On Unix, credential and diagnostic files are created as `0600` inside `0700`
directories. Sensitive traffic/error capture fails closed if that boundary
cannot be established; ordinary redacted logging reports a permission warning
but remains available. Windows relies on the profile directories' ACLs.

- `proxy.log` — JSON-lines log, rotated at 20 MiB. It lives at
  `$XDG_STATE_HOME/claude-code-proxy/proxy.log` on macOS/Linux and at
  `%LOCALAPPDATA%\claude-code-proxy\proxy.log` on Windows (falling back to
  `%USERPROFILE%\AppData\Local`). Secrets (`authorization`, `access`,
  `refresh`, `id_token`, `ChatGPT-Account-Id`, …) are redacted before write.
- `errors/` - redacted provider 5xx responses captured as JSON files under the
  state directory. A single background writer keeps at most 128 files; local
  validation/load-shed failures and provider 4xx responses are log-only so an
  error storm cannot turn this diagnostic path into disk exhaustion.
- `traffic/` — per-request captures written when `CCP_TRAFFIC_LOG=1` is set.
  Captures live under the state directory, grouped by Claude Code session and
  request sequence. They include inbound Anthropic requests, translated upstream
  requests, upstream headers, upstream events, and downstream events. Stream
  events are written under each request's `events/` directory with monotonic
  sequence numbers so sorted filenames preserve emission order. Token and
  account headers are redacted, but prompt and tool content are intentionally
  preserved for debugging.
  New traffic writes use quota admission: the process scans existing regular
  files once per traffic root (without following symbolic links), then reserves
  charged bytes and one file slot before each artifact write. A file is charged
  at least 4 KiB and larger files are rounded up to the next 4 KiB unit, avoiding
  a tiny-file/inode bypass of a logical-byte-only limit. The defaults are 512 MiB
  and 65,536 files per traffic root, plus 64 MiB and 4,096 files per request. The
  first recursive scan runs on a blocking worker and does not hold the global
  quota registry lock. A full quota disables the new capture or its remaining
  artifacts and emits one metadata-only warning; the model request continues.
  This is not TTL pruning: ccproxy never deletes an existing capture
  automatically, and files left by an earlier process restart count against the
  next process's totals. Overrides must be positive counts; the process clamps
  bytes to 64 GiB total and 4 GiB per request, files to 1,048,576 total and
  65,536 per request, and each effective per-request value to its configured
  total.
  For the most complete debugging run, use `CCP_LOG_STDERR=1 CCP_LOG_VERBOSE=1
CCP_TRAFFIC_LOG=1`.
- `config.json` — optional configuration file (see table above). It lives at
  `~/.config/claude-code-proxy/config.json` on macOS,
  `${XDG_CONFIG_HOME:-$HOME/.config}/claude-code-proxy/config.json` on Linux,
  and `%APPDATA%\claude-code-proxy\config.json` on Windows. `CCP_CONFIG_DIR`
  replaces the platform config directory for the current process. The macOS
  and Windows legacy fallback described above is disabled for explicit config
  directories.
- Codex tokens — macOS prefers Keychain service `claude-code-proxy.codex`; its
  file fallback writes `~/.config/claude-code-proxy/codex/auth.json` and reports
  that backend. Linux uses
  `${XDG_CONFIG_HOME:-$HOME/.config}/claude-code-proxy/codex/auth.json`;
  Windows uses `%APPDATA%\claude-code-proxy\codex\auth.json`.
- Grok tokens — macOS prefers Keychain service `claude-code-proxy.grok`; its
  file fallback writes `~/.config/claude-code-proxy/grok/auth.json`. Linux uses
  `${XDG_CONFIG_HOME:-$HOME/.config}/claude-code-proxy/grok/auth.json`, and
  Windows uses `%APPDATA%\claude-code-proxy\grok\auth.json`.
  On macOS and Windows, provider auth also reads the historical generic path
  described above when it differs from the primary path; new writes stay on
  the primary path.

## Switching models and backends

Claude Code binds the API base URL and auth when the process starts. Switching
**backends** (proxy vs direct Anthropic, or a different Anthropic-compatible
host) is a launch-time concern. The proxy can route many model ids in one
`serve` process, but its public registry contains only GPT/Codex and Grok
models. A Claude Code client process has one context window and one set of model
aliases. In-session `/model` changes are therefore safe only within a compatible
profile family. Exit and relaunch with `co` or `cg` when switching between GPT
and Grok.

For the common GPT and Grok paths, use the built-in `claude gpt` and
`claude grok` launch profiles (or the `co`/`cg` executable-name shortcuts).
Use process env, a shell wrapper, or Claude Code's own `/model` for custom
GPT/Grok combinations.

### Choose a pattern

| Goal | Pattern |
| --- | --- |
| Always use the proxy | Put proxy env in `~/.claude/settings.json` (see [Quick start](#4-point-claude-code-at-it)) |
| Keep GPT/Grok aliases and context windows isolated | `co` / `cg`, or `claude-code-proxy claude gpt` / `grok` |
| Try one model once | Prefix `claude` with env vars, or use a shell alias |
| Flip proxy on/off without editing JSON | Flag-file wrapper + toggle script |
| Change models inside one profile family | `/model` or `--model` within the GPT models under `co`, or Grok models under `cg` |

### One-shot launch and aliases

Any of the `ANTHROPIC_BASE_URL=... claude` examples in
[Quick start](#4-point-claude-code-at-it) are one-shots: they do not rewrite
settings. Prefer the complete built-in profiles for daily use:

```sh
co
cg
```

### Sticky proxy on/off (recommended multi-backend setup)

If you still use an Anthropic subscription sometimes, leave proxy env out of
`~/.claude/settings.json` and inject it only when a flag file exists. Put a
wrapper ahead of the real `claude` on `PATH`, and point `exec` at the real
binary (not back at the wrapper). For GPT/Grok, prefer `co`/`cg`; unlike this
generic wrapper, the built-in profiles also keep every Claude model alias,
effort level, retry setting, and subagent routing coherent.

Example wrapper:

```bash
#!/usr/bin/env bash
# Optionally route Claude Code through claude-code-proxy.
# Active when ~/.claude/claude-code-proxy-enabled exists.
set -euo pipefail

# Path to the real Claude Code binary. Do not point this at this script.
real_claude="${REAL_CLAUDE:-$HOME/.local/bin/claude}"

if [ -f "$HOME/.claude/claude-code-proxy-enabled" ]; then
  model_file="$HOME/.claude/claude-code-proxy-model"
  main_model="gpt-5.6-sol"
  small_model="gpt-5.6-luna"

  if [ -f "$model_file" ]; then
    main_model="$(tr -d '[:space:]' <"$model_file")"
    case "$main_model" in
      gpt-5.6-sol|gpt-5.6-sol\[1m\])
        main_model="gpt-5.6-sol"
        small_model="gpt-5.6-luna"
        ;;
      gpt-5.5|gpt-5.5\[1m\])
        main_model="gpt-5.5[1m]"
        small_model="gpt-5.4-mini[1m]"
        ;;
      grok|grok-4.5|grok-4.5-high)
        main_model="grok-4.5"
        small_model="grok-4.5-medium"
        ;;
      grok-4.5-medium)
        main_model="grok-4.5-medium"
        small_model="grok-4.5-medium"
        ;;
      grok-composer-2.5-fast)
        main_model="grok-composer-2.5-fast"
        small_model="grok-composer-2.5-fast"
        ;;
      *)
        small_model="$main_model"
        ;;
    esac
  fi

  export ANTHROPIC_BASE_URL="http://localhost:18765"
  export ANTHROPIC_AUTH_TOKEN="unused"
  export ANTHROPIC_MODEL="$main_model"
  export ANTHROPIC_SMALL_FAST_MODEL="$small_model"
  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
  export CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1

  case "$main_model" in
    gpt-5.6-sol|gpt-5.6-luna|gpt-5.6-terra)
      export CLAUDE_CODE_MAX_CONTEXT_TOKENS="${CLAUDE_CODE_MAX_CONTEXT_TOKENS:-272000}"
      export CLAUDE_CODE_AUTO_COMPACT_WINDOW="${CLAUDE_CODE_AUTO_COMPACT_WINDOW:-272000}"
      export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE="${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE:-90}"
      ;;
    grok-4.5|grok-4.5-high|grok-4.5-medium)
      export CLAUDE_CODE_MAX_CONTEXT_TOKENS="${CLAUDE_CODE_MAX_CONTEXT_TOKENS:-500000}"
      export CLAUDE_CODE_AUTO_COMPACT_WINDOW="${CLAUDE_CODE_AUTO_COMPACT_WINDOW:-500000}"
      export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE="${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE:-90}"
      ;;
  esac
fi

exec "$real_claude" "$@"
```

If the wrapper itself is installed as `~/.local/bin/claude`, set
`REAL_CLAUDE` to the underlying binary under
`~/.local/share/claude/versions/` (or another path that is not this script).

Toggle script (`claude-proxy-toggle` on your `PATH`):

```bash
#!/usr/bin/env bash
# Toggle claude-code-proxy routing for the Claude wrapper.
set -euo pipefail

flag="$HOME/.claude/claude-code-proxy-enabled"

if [ -f "$flag" ]; then
  rm "$flag"
  echo "proxy: off"
else
  mkdir -p "$(dirname "$flag")"
  touch "$flag"
  echo "proxy: on"
fi
```

Optional sticky default proxy model:

```bash
#!/usr/bin/env bash
# Usage: claude-proxy-model [model-id]
# With no args, print the current sticky proxy model.
set -euo pipefail

model_file="$HOME/.claude/claude-code-proxy-model"
default_model="gpt-5.6-sol"

if [ "${1:-}" = "" ]; then
  if [ -f "$model_file" ]; then
    tr -d '[:space:]' <"$model_file"
    printf '\n'
  else
    printf '%s\n' "$default_model"
  fi
  exit 0
fi

mkdir -p "$(dirname "$model_file")"
printf '%s\n' "$1" >"$model_file"
echo "proxy model: $1"
```

Examples:

```sh
claude-proxy-toggle                 # proxy on/off for new sessions
claude-proxy-model grok-4.5-high    # sticky default while proxy is on
claude-proxy-model gpt-5.6-sol
```

New Claude sessions pick up flag/model changes. A session that is already
running keeps the env it started with.

### Switch models while staying on the proxy

While `ANTHROPIC_BASE_URL` already points at this proxy, a generic Claude Code
process can change the request model without restarting the proxy server:

- start a session with a different `ANTHROPIC_MODEL`
- pass `claude --model <id>`
- use Claude Code's `/model` command in-session

The proxy chooses Codex or Grok from the model ID on each request, so
`gpt-5.6-sol` and `grok-4.5` can share one `serve` process. The client process
still has only one context window and one set of model aliases. With `co` and
`cg`, use `/model` or `--model` only within the models offered by that profile;
exit and relaunch with the other command to switch between GPT and Grok.

To populate Claude Code's model picker from the proxy's `/v1/models` catalog,
enable gateway discovery when launching:

```sh
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
ANTHROPIC_BASE_URL=http://localhost:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-5.6-sol \
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna \
CLAUDE_CODE_MAX_CONTEXT_TOKENS=272000 \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 \
CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

Do not combine gateway discovery with
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`; the latter suppresses model
discovery. The `/v1/models` response populates model choices but does not tell
Claude Code the context size of arbitrary gateway models, so keep the context
variables matched to the selected model family.

`/model` only changes the model id. It does not move a session from the proxy
to direct Anthropic or the other way around. For that, start a new process with
different env (or flip the flag-file wrapper and open a new session).

### What this project does not manage

- multi-provider profile GUIs or account managers
- rewriting `~/.claude/settings.json` for each switch
- mid-session base URL / auth changes
- IDE or Desktop launch wiring outside process env

If you need a cross-app profile switcher, use a dedicated tool for that. For
Claude Code + this proxy, use process-start profiles and keep `/model` changes
within the active profile family.

## Limitations

- **Terms of service:** OpenAI has [publicly welcomed using Codex through other
  coding harnesses](https://x.com/thsottiaux/status/2075830097488249060), though
  this does not guarantee future policy or account enforcement.
- **Rate limits:** shared across all clients of your upstream account. Codex and
  Grok upstream rate-limit responses are surfaced with their retry metadata.
- **Codex — image inputs in tool results:** valid URL and base64 image blocks are
  forwarded as structured Responses `input_image` content. Malformed or unsupported
  blocks are replaced with an explicit omission marker.
- **Codex — reasoning blocks:** visible reasoning summaries are forwarded as
  Anthropic `thinking` blocks. Encrypted reasoning is carried in a namespaced
  thinking signature and replayed on later turns; the proxy never decrypts it.
- **Codex — Responses Lite parallel tools:** GPT-5.6 Lite models follow the native
  Codex behavior and disable parallel tool emission. Existing parallel tool history
  still replays correctly. Set `codex.parallelTools` to `true` to move only eligible
  GPT-5.6 Sol/Terra requests with at least one directly callable function tool to
  the full Responses shape. This includes independent repeated calls to the same
  tool and named tool choices. Compaction, structured-output, tool-choice-none,
  parallel-disabled, function-tool-free, and Luna requests remain on Lite. Set
  `codex.responsesLite` to `false` to force the full Responses shape for GPT-5.6
  requests. Luna is Lite-only, so a Luna or Haiku request that requires the full
  lane is rejected locally with an actionable error rather than being silently
  changed to Sol. Other full-lane GPT-5.6 availability is account-dependent, so
  restore the default if the upstream reports model not found.
- **Codex — HTTP transport:** live requests are translated incrementally and
  support prompt cancellation. Non-streaming Anthropic requests still accumulate
  the upstream SSE response before returning one JSON document.
- **Session title generation:** Claude Code's parallel title-gen request is
  forwarded upstream like any other structured-output request. This costs a
  handful of tokens per session rather than being stubbed.
- **Codex — `output_config.format`:** translated to Responses API `text.format`
  (`json_schema` with `strict: true`) through a request-local SchemaBridge. The
  bridge keeps both the caller's original schema and the stricter upstream
  projection, rejects external/recursive references, every schema-position
  `allOf`/`anyOf`/`oneOf` (including otherwise unambiguous forms), and nested
  schema resource identifiers (`$id`, `$anchor`, and `$dynamicAnchor`; root
  identifiers remain allowed). It removes only optional `null` values introduced
  by that projection, then validates the completed JSON against the original
  schema. Structured stream text is held until an authoritative terminal event.
  A validation failure before the first downstream chunk is returned as an HTTP
  error; after a heartbeat has committed the stream it is emitted as an SSE
  error. Neither failure path emits a successful `message_stop`. Ordinary text
  streams remain incremental. `output_config: null` and null `effort`/`format`
  values are treated as omitted.
- **Codex — strict function tools:** an explicit `strict: true` tool whose input
  schema cannot be represented without weakening the contract returns HTTP 400
  before model dispatch instead of silently becoming non-strict.
- **Codex — response tool policy:** live and buffered responses reject undeclared
  function or hosted calls, calls forbidden by `tool_choice`, and successful
  terminals that omit a required or named tool. They also reject a second tool
  call when parallel use is disabled and hosted calls beyond `max_uses`.
  Hosted-search IDs must be non-empty and are mapped without collisions across
  function and hosted blocks; live mixed hosted/function content keeps monotonic
  block indexes. Incomplete and failed terminals keep their upstream failure
  semantics without flushing never-emitted deferred tool blocks.
- **Grok — response tool policy:** the same allowlist, `tool_choice`, required
  terminal, serial-call, and per-kind hosted `max_uses` limits are enforced after
  translation, including tools removed by deferred-loading or direct-caller
  rules. Function and hosted calls share one downstream tool-use ID namespace.
- **GPT/Grok — request controls:** unknown fields, malformed compatibility
  metadata, fixed thinking budgets, disabled reasoning, sampling controls, and
  non-empty stop sequences fail before dispatch. Claude Code's current adaptive
  thinking plus null or empty `context_management.edits` remain accepted. Its
  exact `clear_thinking_20251015` marker with `keep: "all"` is also accepted
  because it removes no history. Context edits that can modify conversation
  history fail before dispatch because the alternate providers cannot apply or
  report Anthropic context editing faithfully. Grok also rejects `service_tier:
  "standard_only"` because it cannot guarantee that capacity constraint; `auto`
  remains supported.
- **Grok — `output_config.format`:** translated to Responses API `text.format`
  for structured background requests such as session title generation.

## Development

```sh
cargo run -- serve                         # run locally (routes Codex and Grok)
cargo test --all                           # run tests
cargo fmt --all --check                    # check formatting
cargo clippy --all-targets -- -D warnings  # lint
just check                                 # run the full project check
```

## Related projects

- [claude-history](https://github.com/raine/claude-history): search Claude Code
  conversation history from the terminal
- [git-surgeon](https://github.com/raine/git-surgeon): non-interactive
  hunk-level git staging for AI agents
- [workmux](https://github.com/raine/workmux): manage parallel AI coding tasks
  in separate git worktrees with tmux
- [consult-llm](https://github.com/raine/consult-llm): Consult other AI models
  from your agent workflow
