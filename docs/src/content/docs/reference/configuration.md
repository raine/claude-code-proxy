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
  "autoReviewModel": "gpt-5.6-terra",
  "codex": {
    "originator": "claude-code-proxy",
    "userAgent": "claude-code-proxy/0.1.24",
    "model": "gpt-6-sol",
    "effort": "high",
    "reasoningSummary": "auto",
    "serviceTier": "fast",
    "baseUrl": "https://chatgpt.com/backend-api/codex/responses",
    "transport": "websocket",
    "headerTimeoutMs": 300000,
    "previousResponseId": false,
    "serverCompaction": false,
    "responsesApi": false,
    "imagesApi": false,
    "imagesBaseUrl": "https://chatgpt.com/backend-api/codex"
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
  "opencode": {
    "apiKey": "YOUR_OPENCODE_GO_API_KEY",
    "baseUrl": "https://opencode.ai/zen/go/v1"
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

All keys are optional. For existing providers, an unreadable file, malformed JSON, or incompatible field type causes the file configuration to be ignored and built-in or environment values to apply. The experimental Antigravity adapter instead fails closed on invalid configuration; discovery reports bounded status enums without raw parse errors.

## Core and diagnostics

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_BIND_ADDRESS` | `bindAddress` | `127.0.0.1` | Listener IP address. |
| `PORT` | `port` | `18765` | Listener port. |
| `CCP_CONFIG_DIR` | none | Platform config directory | Replaces the configuration and file-backed auth root. |
| `CCP_ALIAS_PROVIDER` | `aliasProvider` | `codex` | Routes recognized Anthropic-style aliases through `codex` or `kimi`. |
| `CCP_AUTO_REVIEW_MODEL` | `autoReviewModel` | `gpt-6-luna` for Codex | Routes Claude Code's non-streaming, tool-free Bash security-review classifier through a registered model. |
| `CCP_LOG_STDERR` | `log.stderr` | `false` | Mirrors logs to stderr when present in the environment, regardless of its value. |
| `CCP_LOG_VERBOSE` | `log.verbose` | `false` | Preserves full string fields in structured logs when present, regardless of its value. |
| `CCP_TRAFFIC_LOG` | none | `false` | Enables full request captures for `1`, `true`, or `yes`. |
| `XDG_STATE_HOME` | none | `~/.local/state` | State base on macOS and Linux. |

`CCP_CONFIG_DIR` affects `config.json` and file-backed provider auth. It does not relocate the state directory.

Codex auto-review classifier requests use `gpt-6-luna` by default. Requests routed through other providers retain their requested model. `CCP_AUTO_REVIEW_MODEL` or `autoReviewModel` selects an explicit registered model for detected classifier requests without changing the session's provider affinity. **Exception:** explicit Antigravity requests ignore cross-provider auto-review overrides. Same-provider overrides still undergo exact replay/model validation; changing tiers with signed history is rejected. Normal messages, streaming requests, tool-using requests, and token counting retain their requested model.

## Outbound proxies

Outbound HTTP requests and Codex WebSocket setup inherit standard proxy environment variables when each provider client is created:

| Environment | Applies to | Purpose |
| --- | --- | --- |
| `HTTP_PROXY` / `http_proxy` | `http://` and `ws://` destinations | Routes plain HTTP and WebSocket Upgrade requests through the configured proxy. |
| `HTTPS_PROXY` / `https_proxy` | `https://` and `wss://` destinations | Routes TLS destinations through the configured proxy, normally with HTTP CONNECT. |
| `ALL_PROXY` / `all_proxy` | Any destination without a scheme-specific proxy | Provides the fallback proxy. |
| `NO_PROXY` / `no_proxy` | Matching destination hosts and IPs | Bypasses proxy routing. Supports comma-separated domains, subdomains, IP addresses, CIDR ranges, and `*`. |

On case-sensitive platforms, uppercase names are checked before lowercase names when both spellings exist. Windows treats environment names as case-insensitive, so each uppercase/lowercase pair identifies one variable. Set these variables before starting claude-code-proxy; changing them requires a restart because clients and pooled WebSocket connections retain their startup route. For CGI safety, proxy environment variables are ignored when `REQUEST_METHOD` is present. The variable name describes the **destination** scheme, so this default-WSS configuration is valid even though the local proxy URL uses `http://`:

| Variable | Value |
| --- | --- |
| `HTTP_PROXY` | `http://127.0.0.1:7890` |
| `HTTPS_PROXY` | `http://127.0.0.1:7890` |

After setting both variables through the operating system, service manager, or shell, start `claude-code-proxy serve` in the same environment.

Proxy URLs may use `http`, `https`, `socks4`, `socks4a`, `socks5`, or `socks5h`. HTTP proxy URLs can contain percent-encoded Basic credentials, for example `http://user:password@127.0.0.1:7890`; SOCKS5 and SOCKS5H URLs can contain username/password credentials for the SOCKS handshake. SOCKS4 and SOCKS4A are supported without URL credentials. Prefer a secret-management mechanism when available because environment variables may be visible to other local processes. WSS certificate verification uses both bundled public roots and the platform native root store, including locally installed enterprise proxy CAs. Malformed or unsupported proxy URLs fail provider startup rather than being ignored. Proxy failures do not silently retry with a direct connection; only `NO_PROXY` selects direct routing. OS proxy settings, PAC files, and WPAD are not read automatically.

## Codex

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_CODEX_MODEL` | `codex.model` | unset | Forces every Codex request to one upstream model. |
| `CCP_CODEX_EFFORT` | `codex.effort` | unset | Forces `none`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `CCP_COMPACT_EFFORT` | none | `low` | Caps Codex reasoning effort for Claude Code summary compaction requests. `off` disables the cap and `none` removes reasoning. |
| `CCP_CODEX_REASONING_SUMMARY` | `codex.reasoningSummary` | unset | Overrides summary mode. `off` and `none` suppress summaries. |
| `CCP_CODEX_SERVICE_TIER` | `codex.serviceTier` | unset | Forces `fast`, `priority`, `ultrafast`, or `flex` on `/v1/messages` requests. Fast is sent as `priority`. Ultrafast is sent as `priority` to models that do not offer it. |
| `CCP_CODEX_BASE_URL` | `codex.baseUrl` | ChatGPT Codex Responses URL | Changes the Codex endpoint. |
| `CCP_CODEX_TRANSPORT` | `codex.transport` | `websocket` | Selects `websocket`, `http`, or `auto`. |
| `CCP_CODEX_HEADER_TIMEOUT_MS` | `codex.headerTimeoutMs` | `300000` | Sets how long an HTTP-transport request waits for the Codex response headers. Values below `1000` are ignored. |
| `CCP_CODEX_PREVIOUS_RESPONSE_ID` | `codex.previousResponseId` | `false` | Enables append-only WebSocket continuation for `1`, `true`, or `yes`. |
| `CCP_CODEX_SERVER_COMPACTION` | `codex.serverCompaction` | `false` | Enables or disables native compaction for standard boolean words. |
| `CCP_CODEX_RESPONSES_API` | `codex.responsesApi` | `false` | Enables `/v1/responses` and `/v1/chat/completions` for every registered provider. Accepts `1`, `true`, or `yes`. |
| `CCP_CODEX_IMAGES_API` | `codex.imagesApi` | `false` | Enables `/v1/images/generations` and `/v1/images/edits` for `1`, `true`, or `yes`. |
| `CCP_CODEX_IMAGES_BASE_URL` | `codex.imagesBaseUrl` | `https://chatgpt.com/backend-api/codex` | Sets the trusted Codex Images API root; production use is restricted to HTTPS `chatgpt.com/backend-api/codex`. |
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
| `CCP_GROK_HOSTED_SEARCH` | none | off | Set to `1`, `on`, or `true` to let hosted search tools replace the caller's own search tools and force them on an explicit search turn. |
| `CCP_GROK_SEARCH_BLOCKS` | none | `text` | Selects how a hosted search is reported: `text` for a text block, `native` for `server_tool_use` plus a `*_tool_result` block. |

## OpenCode Go

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_OPENCODE_API_KEY` | `opencode.apiKey` | unset | OpenCode Go API key; takes precedence over `OPENCODE_API_KEY` and config. |
| `OPENCODE_API_KEY` | `opencode.apiKey` | unset | Fallback API-key variable accepted by the proxy when the CCP-specific variable is unset. |
| `CCP_OPENCODE_BASE_URL` | `opencode.baseUrl` | `https://opencode.ai/zen/go/v1` | Changes the OpenCode Go API base URL. |

## Cursor Agent

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_CURSOR_BASE_URL` | `cursor.baseUrl` | `https://api2.cursor.sh` | Changes the Cursor API base URL. |
| `CCP_CURSOR_CLIENT_VERSION` | `cursor.clientVersion` | `0.48.5` | Changes Cursor client version headers. |
| `CCP_CURSOR_AGENT_BUNDLE` | `cursor.agentBundle` | Auto-detected | Points to Cursor Agent's bundled `index.js` protobuf schemas. |
| `CCP_CURSOR_AUTH_TOKEN` | none | unset | Uses a bearer token instead of proxy-owned Cursor auth storage. |

## Experimental Antigravity

Disabled by default. This native Argon adapter is unofficial, not generic Gemini API
support and not a promise of Google account entitlement. Authentication commands
remain available while disabled. Do not enable it in a production session before
separate real-account OAuth/native smoke validation.

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_ANTIGRAVITY_ENABLED` | `antigravity.enabled` | `false` | Explicit opt-in. Environment accepts only `1`/`true` or `0`/`false`; file value is boolean. |
| `CCP_ANTIGRAVITY_CLIENT_ID` | `antigravity.clientId` | unset | Your public installed-app OAuth client ID; required for login and refresh. |
| `CCP_ANTIGRAVITY_CLIENT_SECRET` | `antigravity.clientSecret` | unset | Optional public-client application secret, when required by that registered application. Not a user bearer token. |
| `CCP_ANTIGRAVITY_PROJECT_ID` | `antigravity.projectId` | stored discovery result | Your authorized project; no borrowed project or automatic onboarding. |
| `CCP_ANTIGRAVITY_BASE_URL` | `antigravity.baseUrl` | `https://daily-cloudcode-pa.googleapis.com` | Consumer OAuth native API origin. Explicit trusted HTTPS origin override; no userinfo, query, fragment, or path. Never automatically falls back to another endpoint. |
| `CCP_ANTIGRAVITY_USER_AGENT` | `antigravity.userAgent` | `claude-code-proxy/<package version>` | Full HTTP User-Agent override for native generation and `loadCodeAssist` only. OAuth token exchange/refresh always retain the default proxy identity. |

The built-in daily origin serves both `loadCodeAssist` project discovery and
native generation for this adapter's supported consumer OAuth flow. Base URL
precedence remains environment, config file, then default. Explicit production or
custom trusted HTTPS origins remain unchanged; there is no hostname-based rewrite.
The adapter does not implement GCP/ADC/service-account authentication or account-tier
endpoint selection. This default is not a general Google Cloud routing policy; see
[endpoint evidence](/reference/compatibility-and-limitations/#consumer-endpoint-evidence).

Remote endpoints require HTTPS, redirects are never followed, and OAuth endpoints
are fixed to `https://accounts.google.com/o/oauth2/v2/auth` and
`https://oauth2.googleapis.com/token`. Plain loopback HTTP endpoints exist only as
injected test fixtures, not environment/config options. By default every HTTP
request sent by this adapter uses exactly `claude-code-proxy/<package version>`, with no
platform suffix. A provider-specific User-Agent override replaces that header only
on native generation (both streaming and nonstreaming frontend requests) and
`loadCodeAssist` project discovery, never OAuth code exchange or refresh. Incoming
client User-Agent, authorization and cookies are not forwarded. JSON product
selectors `userAgent: antigravity` and `requestType: agent` are unchanged.

Override precedence is `CCP_ANTIGRAVITY_USER_AGENT`, then `antigravity.userAgent`,
then the proxy default. Neither `CCP_USER_AGENT` nor Codex settings are inherited.
Presence selects the environment override even when empty; invalid values fail
configuration rather than falling back, including while the provider is disabled.
Invalid native configuration removes Antigravity readiness/advertisement; it does
not disable other providers or prevent the proxy listener from starting. Auth
status still exits successfully with a report of `configuration: invalid` and
`configuration_ready: false`; report success does not establish readiness.
The file field must be a string. The selected value must contain 1–4096 printable
ASCII bytes (`0x20`–`0x7e`), with no leading/trailing space, tabs, control bytes or
non-ASCII characters. Non-UTF8 environment values are rejected by the native
validator even though this dedicated setting is excluded from legacy UTF-8
snapshots. Unix key matching is exact; Windows covers ASCII case variants.
Unrelated non-Unicode environment entries remain unsupported and fail with a
fixed error, rather than being silently discarded. Valid bytes,
including interior spacing, are preserved exactly: no trimming, suffix appending,
version/changelist rewriting or platform substitution. Raw values are omitted from
status/errors and marked sensitive in HTTP diagnostics. Do not put credentials in
a User-Agent; it is sent to the configured trusted native API origin.

An explicit compatibility override may use this pinned historical value:

```text
antigravity/cli/1.3.5 (aidev_client; os_type=darwin; arch=arm64; cl=998863838; auth_method=consumer)
```

Set it as the full provider-specific environment or file value only when desired;
it is **not the default** and does not enable the provider. These literal platform
and consumer-profile tokens are caller-selected compatibility metadata, not a
claim about the host or a verified account classification. They are not normalized
to the host. The pinned public reference records CLI 1.3.5 generation on
Darwin/arm64 and uses the same formatter for project discovery. Exact CLI 1.4.2
identification, current official-client equivalence, live acceptance and provider
policy/terms compatibility remain unverified. No runtime `agy` probing, credential
import, device identity or attestation fabrication is performed. User-Agent is not
authentication or evidence of entitlement; see the
[reference evidence](/reference/compatibility-and-limitations/#evidence-and-remaining-gates).

Login requests explicit browser consent, offline access, PKCE S256 and independent
one-time state. By default it opens a browser and uses an ephemeral `127.0.0.1`
callback. The reference-protocol scopes
are `https://www.googleapis.com/auth/` followed by `cloud-platform`, `userinfo.email`,
`userinfo.profile`, `cclog`, and `experimentsandconfigs`. These are reference
compatibility scopes, not independently proven minimal scopes. No `agy`, Gemini,
Codex, keychain or other application's credentials are discovered or imported.

**Experimental manual completion:** `antigravity auth login --manual` prints the
authorization URL and waits for a visible terminal paste instead of opening a
browser/listener. It uses `https://antigravity.google/oauth-callback` in both the
authorization request and code exchange. Your explicitly configured client must
allow that exact hosted redirect; an installed-app/loopback-only registration may
not. No client ID/secret is bundled or substituted, scopes are unchanged, and no
Google OOB redirect, device flow or automatic fallback is attempted. If the browser
reports `redirect_uri_mismatch`, another error, or no code, cancel and correct the
registration; ordinary callback login remains a separately chosen alternative.

Choose the intended account in the browser, then copy the **displayed code**, not
the final address bar. A query-free public inspection on 2026-10-20 found that the
hosted page reads/URL-decodes `code`, removes it from browser history, and displays
and copies that same bare value without adding state or a signed envelope. This
confirms the page's display contract, not successful OAuth or current official-CLI
parser equivalence. Historical hosted-redirect evidence is
https://github.com/google-antigravity/antigravity-cli/issues/315; official
paste-back UX is documented at https://www.antigravity.google/docs/cli/install/.

Each attempt keeps a fresh verifier private and sends an independent state and
S256 challenge. A **bare code has no returned state to validate**: its binding to
this attempt relies on the authorization server enforcing S256 PKCE, including
rejecting a verifier-bearing exchange for a code issued without a challenge
(downgrade prevention). Those server properties and acceptance of the configured
client/redirect remain live-unverified. A complete redirect URL is also accepted
if obtained with its code intact; it must match the hosted HTTPS target and path,
have exactly one matching state and one valid code, and contain no userinfo,
fragment, conflicting error, duplicate security fields or malformed encoding.
A valid state-bound denial produces a fixed error. URL-looking malformed input
never falls back to bare-code acceptance. Bare codes are not percent/base64/JSON
decoded; surrounding spaces, controls, non-ASCII and URL/structured delimiters
are rejected rather than normalized. Slash/plus/dot/tilde/padding code bytes are
preserved and form-encoded once for the single token request.

Manual input is **Unix-only**, verified at runtime on Linux. It requires terminal
stdin and a foreground controlling terminal with a known width of at least
10 columns. A separately opened nonblocking `/dev/tty` owns input and bounded
prompt/URL/visible-input output. Kernel echo stays disabled: only validated
printable input is rendered by the application, never pasted escape sequences.
Ordinary pipes are unsupported. Input is not copied to stdout, stderr, argv,
logs or status; its intentional terminal display is **sensitive**, and may remain
in scrollback, screenshots or recordings even after editing or cancellation.
No scrollback erasure or exact current `agy` display equivalence is promised.

The input occupies one bounded row, showing `[character count]` and the code.
Short codes are fully visible. If longer input does not fit, `<` marks a visible
tail viewport; the undisplayed prefix remains part of the completion. Backspace
and Delete edit the complete logical input, not merely the displayed suffix.
The final terminal column is reserved to avoid wrapping. A detected width change
cancels without exchange rather than trying to repair terminal reflow. Output
postprocessing that translates carriage returns (`OCRNL`) or letter case
(`OLCUC`, where supported) is refused before displaying the authorization URL;
these flags are harmless when `OPOST` is off. Normal `OPOST`/`ONLCR` output and
other non-translating flags are preserved, and the saved mode is not normalized.

The input cap is 8192 bytes; the absolute 180-second input/output deadline also
covers visible redraws, blocked output, continuous input and partial pastes.
Only one bounded display frame is pending; intermediate edits may be coalesced,
not queued indefinitely. Three empty entries may redraw the empty prompt without
resetting that deadline; the fourth fails. Enter submits, and Ctrl-C/Ctrl-D,
Ctrl-Z/Ctrl-backslash or SIGINT/SIGTERM cancel, including during blocked redraws.
Exact bracketed-paste delimiters are recognized without enabling terminal modes;
malformed/nested escapes fail. Enter freezes the completion and ignores trailing
code; its final display frame/newline must be accepted by the terminal device
before cleanup and exchange. This cannot guarantee physical rendering by an
external terminal.

Before exchange, successful explicit cleanup discards **currently queued** terminal
input and restores the exact saved mode using immediate, non-draining termios
calls. If flushing fails, mode restoration is still attempted, but queued-input
disposal cannot be guaranteed; any cleanup error prevents exchange. Controlled
cancellation and unwinding also attempt restoration; SIGKILL, abort or unrecoverable
crashes cannot guarantee restoration. Input arriving after cleanup/process exit
cannot be disposed of by a finished process: stop pasting when cancelled. Never
paste credentials into chat,
shell commands or recorded transcripts. Login releases the auth lock during
consent, then rechecks its epoch before one exchange; logout/new login invalidates
stale consent. There is no token-exchange retry or partial-credential save. Once
credentials are durably saved, login is successful even if the bounded,
best-effort terminal confirmation cannot be shown or is interrupted.

Once a foreground terminal session has been acquired, failure diagnostics also
use that owned nonblocking terminal, with a separate absolute two-second budget
after runtime cleanup. A blocked/closed terminal may prevent the diagnostic from
being shown, but the command still exits nonzero without falling back to a
blocking stderr write. Early non-terminal/background/unsupported-platform
refusals retain ordinary CLI error reporting; their stderr is outside this
interactive-session deadline contract.

Only `CCP_CONFIG_DIR/antigravity/auth.json` is used. Atomic private writes use mode
0600 on Unix, under a mode-0700 provider directory. A stable sibling `auth.lock`
coordinates login/logout/refresh across processes; logout never removes that
lock. Missing, invalid and unreadable storage remain distinct. Local logout resolves
this exact store path independently of transport settings, so malformed JSON or
an invalid API URL or User-Agent does not prevent deleting this provider's credential. It does
not remove another provider's state.

Refresh preserves the profile ID and retains the old refresh token when rotation
is omitted. A bounded manager-owned, single-flight transaction acquires the stable
lock **before** contacting OAuth. Cancelling an inference waiter cannot discard a
completed rotation: credential maintenance may finish after disconnection, but
never starts or continues model generation. The 30-second network phase ends
before validated replacement persistence. Filesystem writes are synchronous and
are not claimed cancellable by that timer.

Refresh alone permits at most **two application HTTP send attempts**, separated
by 200 ms, under that same lock and original 30-second network deadline. The
second attempt requires at least 16 seconds remaining after backoff (the existing
15-second connection timeout plus a one-second response reserve). It is allowed
only when the pinned Reqwest/Hyper error graph proves the OAuth POST was not sent
and its first underlying I/O error is `ConnectionRefused` or `TimedOut`. The
complete, bounded graph includes wrapped I/O causes; TLS verification/protocol
errors, proxy authentication failures, unknown DNS errors and other unallowlisted
causes are not retried. Reqwest's private connector-deadline error is also not on
this I/O allowlist. A transport I/O timeout during TLS can qualify; there is no
blanket timeout or TLS-phase retry rule. Redirects and Reqwest's own retry policy
remain disabled. Hyper can internally reconnect an **unstarted** pooled request,
so two send attempts do not imply a maximum of two socket connections.

If every failed attempt is proven pre-send, credentials are retained without
claiming an unknown rotation. A 30-second cooldown, scoped to the same profile,
lock epoch and refresh token, prevents further foreground refreshes in this
manager; after it expires, the next foreground request may start another bounded
flight. No background retry is scheduled. Metadata-only edits do not reset the
cooldown; a changed scope or successful persistence clears it. This bound is
manager-local, not cross-process or crash-durable. Safe connection failures,
cooldown and shutdown before a retry return a fixed, redacted HTTP 503 `api_error`
without `Retry-After`, before streaming or nonstreaming generation headers. A
shutdown detected before retry prevents the second send.

An unresolved send, consumed request followed by connection loss, response/body
failure, or timeout while a request might have been sent remains ambiguous and
is **not retried**, including after a safe first failure. Those errors retain the
existing HTTP 401 `authentication_error` and obsolete-token protection. No model
generation or authorization-code exchange retries are added. These are this
proxy's source-audited guarantees, not a claim about the official `agy` client's
internal retry count or backoff.

If persistence fails, the manager retains the replacement in memory, scoped to
the profile, lock epoch and superseded refresh token, and reports a typed pending-
persistence error. Subsequent requests retry the local write, not the obsolete
token. Logout/new login or a different committed refresh token invalidates that
pending scope. An ambiguous network timeout/error requires explicit login rather
than automatically reusing a potentially consumed token. Graceful server shutdown
joins owned maintenance and attempts only pending local persistence; it performs
no new refresh or inference. Forced process termination/crashes between remote
rotation and durable storage are not covered by this in-memory recovery. Google's
immediate refresh-token invalidation behavior remains unverified.

A new login changes the profile and invalidates prior session replay.

## Shared compatibility fallbacks

`CCP_USER_AGENT` is the fallback when a Codex or Kimi provider-specific user-agent is absent. Prefer `CCP_CODEX_USER_AGENT` or `CCP_KIMI_USER_AGENT` in durable configuration.

Endpoint and client identity overrides can break provider compatibility. Use defaults unless a controlled integration or focused diagnostic requires an override.
