---
title: Compatibility and limitations
description: Canonical security, account, protocol, model, context, multimodal, reasoning, tool, session, rate-limit, and deployment boundaries.
---

claude-code-proxy targets Claude Code's practical Anthropic API usage rather than complete protocol equivalence.

## Accounts and provider policy

- Provider subscriptions, allowed models, regions, quotas, and enforcement remain under each provider's control.
- OpenAI has publicly welcomed using Codex through other coding harnesses, but public statements do not guarantee future policy or account treatment.
- Kimi, Grok, and Cursor use unofficial client integrations. Review the terms and account risk for your use.
- Upstream rate limits are shared with other clients on the same account.

## Listener security

- Incoming clients are not authenticated.
- The default bind address is `127.0.0.1`.
- A non-loopback listener requires an external firewall or authenticating reverse proxy.
- The proxy stores subscription credentials and can consume account quota, so an exposed listener is equivalent to exposing that capability.

## Anthropic API scope

- Messages supports streaming and non-streaming responses for the fields exercised by Claude Code.
- `?beta=true` does not select a separate implementation.
- Token counts are local estimates, not exact upstream tokenizer or billing counts.
- Claude Code title generation and other structured background requests are forwarded and consume provider tokens.
- Anthropic-specific fields without a provider mapping can be dropped.
- Native OpenAI Responses passthrough is opt-in and applies to registered Codex models.
- Codex Images passthrough is separately opt-in, restricted to `gpt-image-2`, and supports generation plus JSON or multipart edits. Variations, masks, remote image URLs, and URL-formatted outputs are unsupported.

## OpenAI API scope

- `CCP_CODEX_RESPONSES_API=1` enables `/v1/chat/completions` and `/v1/responses` for Codex, Kimi, Grok, OpenCode Go, and Cursor models.
- Codex Responses requests use native passthrough. Requests for the other providers support their mapped subsets of text, reasoning, function tools, tool results, token limits, usage, streaming, aliases, and `[1m]` model hints; exact support depends on the selected provider and model.
- Unsupported non-null request fields return an error instead of being ignored.
- Grok search calls appear as Responses `web_search_call` items. Chat Completions returns the citations without a separate search item.
- Cursor tool bridging supports `Read`, `Write`, and `Bash`. It requires streaming and a stable session ID.
- Stored Responses operations, WebSocket ingress, multiple choices, audio, log probabilities, and arbitrary hosted tools are unsupported.

## Models and context

- Local registration does not guarantee account access to a model.
- Unknown model IDs have no implicit provider fallback.
- Anthropic-style aliases route only to the configured Codex or Kimi alias provider.
- `[1m]` is a Claude Code client hint and does not change upstream context.
- Provider context limits can be lower than Claude Code's local threshold.
- Switching provider or model can clear provider-specific continuation assumptions while Claude Code retains portable history.

## Codex

- Base64 user images and supported base64 tool-result images map to Responses images. Remote URLs and malformed or unsupported nested images remain text placeholders.
- Reasoning summaries can appear as thinking blocks. Codex decides whether a summary is emitted.
- Encrypted reasoning and compaction items remain provider continuation data and are not exposed as raw chain of thought.
- Hosted web search supports mapped domain filters, but the Anthropic `max_uses` value is not enforced because Codex exposes no equivalent limit.
- Strict JSON schema output is translated. Other Anthropic-only output settings can be omitted.
- `previous_response_id` and server compaction state are in memory and require stable session routing.
- Automatic transport fallback occurs only before an upstream request is sent, which avoids replaying possible side effects.

## Kimi

- The proxy exposes one Kimi Code wire model plus local aliases.
- Reasoning effort supports Kimi's low, medium, and high levels.
- Images in tool results use Kimi tool-message image parts.
- The persistent device ID is part of the login identity and must remain stable.

## Grok

- Model availability varies by account and region.
- Hosted general web search and X search are translated with citations and usage.
- Hosted web search omits `max_uses` because the Grok CLI endpoint exposes no
  equivalent cap. Non-null domain filters and user location are rejected because
  dropping them would weaken the caller's requested search scope.
- The implemented multimodal path does not claim general image or video compatibility.

## Cursor Agent

- An installed Cursor Agent JavaScript bundle supplies protobuf classes.
- The dynamic catalog reflects the installed Cursor Agent and can differ across machines.
- The native tool bridge covers recognized `Read`, `Write`, and `Bash` calls when matching tools and a session ID are present.
- Cursor workspace callbacks and arbitrary native tool forms do not have a general Claude tool bridge.
- Conversation and pending tool state are in memory. Restarts clear them.
- Cursor count-tokens uses a rough rendered-prompt estimate.

## Experimental native Antigravity Argon

**Default off and experimental.** Only the exact IDs
`antigravity/gemini-4-argon-low`, `antigravity/gemini-4-argon-medium`, and
`antigravity/gemini-4-argon-high` are registered after explicit opt-in. Aliases,
Codex/Kimi affinity, and other providers' normalization remain unchanged.
Unknown Antigravity names never fall back. Start a **fresh session** whenever
changing model, tier, profile/account or project. The conservative 200,000-token
client window is unmeasured, not a claim of a verified backend limit. Raw `[1m]`
selectors and the `context-1m-2025-08-07` beta are rejected before normalization
on both Messages and count-tokens.

### Supported client contract

- Text and ordered system instructions, external function definitions/calls/results,
  signed assistant replay, nonstreaming JSON and bounded buffered-turn SSE.
  The adapter never executes tools or spawns `agy` for inference.
- Absent thinking or `thinking: {"type":"adaptive"}` is accepted, including
  `display: "omitted"`. The exact model tier selects LOW/MEDIUM/HIGH. Omitted
  display produces an empty thinking carrier, not invented visible reasoning.
- Recognized `output_config.effort` values (`low`, `medium`, `high`, `max`) are
  advisory. They never change the pinned model/tier; the effective tier appears
  in the response header and monitor. Numeric thinking budgets, explicit
  disabled thinking, output-format promises, temperature/sampling options,
  unsupported media, hosted tools, forced tool choice and parallel-disable
  semantics are rejected rather than silently weakened.
- `tool_choice: {"type":"auto"}` is supported. `none` removes native declarations
  and rejects returned function calls. Function names are collision-checked and
  may be deterministically mapped to <=64-character native names; caller names
  remain exact.
- Absent context management, an empty edit list, or exact
  `clear_thinking_20251015` with `keep: "all"` is accepted. Other context edits
  are rejected. Recognized cache hints are frontend hints, not a native cache-TTL
  promise, and frontend Anthropic authorization/beta headers never reach Google.
- String/text-array `role: system` messages are extracted in message order after
  top-level system parts, including ordinary turns. Logical assistant-fragment
  boundaries are determined from the **original role sequence** before extraction:
  a system message never manufactures adjacency, and user text resembling a
  system reminder never gains system privilege. Adjacent fragments with one
  authoritative leading carrier are normalized and digest-checked; distinct
  complete turns keep distinct carriers. Empty array fragments carry no turn.
  Missing/duplicate/conflicting carriers still fail closed. System-only requests
  and non-text system blocks are rejected.
- After a tool-call turn, a complete unambiguous prefix of tool results must
  precede ordinary user text. Results are associated in original native call
  order; missing, duplicate, interleaved or text-before-results batches fail.
  Native result contents use `role: model`, without synthetic `[Continue]` text.
  Normal results wrap the original string or ordered text array as `output`;
  error results use `error` and `is_error: true`. Strings are never JSON-parsed
  into different result types.
- Generated local tool IDs are unique per turn/call, independent of native IDs.
  Native ID presence/absence and raw calls are preserved. Frozen v1 rejects
  repeated-name parallel calls with absent native IDs as ambiguous. Reused
  present native IDs remain distinct local calls, but their live native acceptance
  is not established by synthetic tests. A completed native call may omit `args`:
  its caller projection is `{}` and is checked against the original tool schema,
  while native replay retains the omission. Explicit null/non-object arguments
  and incremental-call fields remain errors. Argon acceptance of omitted args is
  reference-documented, not live-verified.

### Schemas and replay

Original source schemas are validated using pinned `jsonschema` 0.59.0 with
network/file resolution disabled. Accepted dialects are Draft 2020-12
(`https://json-schema.org/draft/2020-12/schema`, the default when omitted) and
Draft 7 (`http://json-schema.org/draft-07/schema`, optional trailing `#`). References,
definitions/dynamic references, unknown dialects/vocabularies, `prefixItems` and
`unevaluatedProperties` are rejected before compilation.

Gemini receives a separate projection containing supported fields and uppercase
types. Integer exclusive bounds are projected exactly; non-native constraints
remain enforced against the **original** schema and are identified in bounded
hints, including all non-string enum values. Unsupported/freeform leaves use an
explicit JSON-string codec, decoded before validation, with a complete deterministic
rendering of the original subschema (names, types, required fields and constraints)
in its guidance. If that guidance cannot fit 16 KiB, the request is rejected before
inference rather than truncated. A freeform root object uses one `json` string
wrapper with no extra keys; a closed no-argument object omits native parameters.
Required names without native property declarations use this faithful codec when
additional properties are allowed. When a closed object forbids a required
undeclared property, it is rejected as an unsatisfiable tool contract, not falsely
classified as invalid JSON Schema syntax. Ordinary structured tools stay structured.
Invalid JSON, unsafe numbers or arguments
violating the original schema produce a redacted upstream-output error before
any tool-use emission, never tool execution or hidden repair/retry.

The single leading `ccp:antigravity:v1:` thinking signature contains original
native contents/signatures plus exact profile/project/model/session binding,
recorded display mode and tool/name/codec mappings. Replay is restart-safe with
no process-local correctness cache. Local validation detects missing/malformed
carriers, changed client projections and scope/session/mapping inconsistencies.
The SHA-256 digest is **consistency checking, not authentication**: it does not
bind the authenticity of opaque native contents/signatures. Corruption that
leaves the client projection unchanged is validated by the native upstream in
one attempt; its rejection is surfaced without signature repair, generation
retry, fallback or tool emission. Raw native signatures/carriers are redacted
from traffic and error diagnostics.

### Transport, errors and bounds

Native HTTP headers are awaited before downstream 200. Initial HTTP errors retain
HTTP status and the Anthropic error envelope/type, including when SSE was requested.
The generic message appends only fixed, allowlisted diagnostics in this order:
`google_status`, `reason`, `domain`, `quota_failure`, `retry_hint`, `retry_s`.
Unknown status/reason/domain values become `unclassified`; arbitrary upstream strings,
messages, numeric error codes, metadata, project/account identifiers and replay data
are never reflected or logged. Exact Google `ErrorInfo` reasons require an allowlisted
domain; multiple ErrorInfo details are unclassified even if identical. A valid
`QuotaFailure` is only `reported`, not proof that account subscription quota is
exhausted. HTTP 429 and `RESOURCE_EXHAUSTED` alone cannot distinguish quota, rate or
capacity limits. These supported literals are not a claim of live Argon acceptance.

Diagnostic body collection requires complete EOF within 8 KiB and one absolute
1-second deadline, further limited by the remaining 30-second header-request budget.
JSON rejects duplicate keys, depth above 64, and more than 16 error details.
Malformed, oversized, incomplete or timed-out bodies become unclassified without
changing HTTP status. Unknown detail types are ignored; malformed detail structure
invalidates body diagnostics. No diagnostic reader survives request cancellation.

`Retry-After` is forwarded only after validating a single header as 1–5 decimal digits
(0–86400 seconds, leading zeros normalized) or an exact canonical IMF-fixdate with
valid calendar/weekday/GMT. Past dates imply zero wait; future dates beyond 24 hours
are rejected, never shortened. RFC850/asctime dates, duplicate, signed, fractional,
overlong or arbitrary header values are deliberately no longer forwarded.
Google `RetryInfo.retryDelay` accepts a canonical nonnegative seconds string with up
to nine fractional digits, rounded **up** using integer arithmetic, bounded to 86400
seconds. It never synthesizes a `Retry-After` header. `retry_hint` is
`valid`, `absent`, `invalid` or `conflicting`; `retry_s` is a bounded integer only for
valid hints, otherwise `none`. Invalid hints take precedence; multiple RetryInfo
objects conflict even when identical. Different valid header/body delta-seconds
conflict; a valid HTTP-date plus valid body delay uses the larger remaining wait.
A valid header is still forwarded if body hints conflict or are invalid. An unusable
body cannot establish absent retry information, but a validated independent header
remains usable. Retry hints are advisory only: there is no automatic sleep or retry.

After native 2xx the response body owns collection; no successful text/tool turn is published
until terminal state, arguments and replay metadata are validated. Late errors
emit an SSE error without `message_stop` and record one failed monitor outcome.
Dropping the body cancels owned inference work. Loopback socket tests cover
pre-header, nonstream and streaming cancellation; this is not a guarantee of
upstream quota refunds. Separately owned credential-refresh maintenance may finish
and persist a rotation after the inference waiter disappears, but cannot continue
generation. See [credential ownership and recovery](/reference/configuration/#experimental-antigravity).
There are no automatic generation retries or endpoint/account fallback.

| Limit | v1 bound |
| --- | --- |
| Collected native turn / decoded carrier | 8 MiB each |
| Encoded carrier payload | 12 MiB |
| Native SSE line / frame | 1 MiB each |
| JSON depth | 64 |
| Native response contents / parts / calls | 512 / 4096 / 128 |
| Logical assistant projection / blocks | 8 MiB semantic bytes / 4097; cumulative bounds checked before growth |
| Original-schema codec guidance | 16 KiB per complete description; no essential-constraint truncation |
| Incoming messages / prepared native history contents | 512 / 1024 |
| Caller tools / source schema bytes / schema nodes / schema depth | 128 / 256 KiB per schema / 8192 / 32 |
| Raw codec JSON / decoded argument bytes | 1 MiB each |
| Codec paths per call / path bytes / root-key bytes | 64 / 1024 / 256 |
| Requested output tokens | 1..65536 (8192 when omitted) |
| Native header / body idle / whole collection deadline | 30s / 30s / 180s |
| Auth lock acquisition / token-project request / browser callback | 10s / 30s / 180s |
| Manual terminal input, prompt and visible redraw output / input bytes / empty reprompts | 180s absolute / 8192 bytes / 3; fourth empty entry fails |
| Manual completion confirmation output | 2s absolute, best effort after durable success |
| Established manual-session failure diagnostic | 2s absolute after runtime cleanup; nonzero exit even if output is blocked |
| Callback headers / each preconnect read | 8 KiB / 2s |
| Stored credential / OAuth-project response | 64 KiB each |

All limits apply together. Exceeding a request bound returns an invalid-request
error before inference where detectable; upstream output bounds return a redacted
upstream error, never a partial successful turn. Usage fields are cumulative,
not summed across frames. Cache reads are capped by prompt usage. Candidate plus
thought output is used only with consistent native totals; ambiguous totals,
decreasing counters, refusals, unknown termination and truncated streams are
explicit errors rather than invented billing numbers. Token counting reconstructs
validated native history and estimates its normalized full semantic projection,
including retained hidden thoughts and recorded caller-name/argument codecs. Full
versus omitted display, supported assistant fragmentation and opaque signature
size cannot change that estimate. Signatures/carrier duplication are excluded;
this remains a local estimate, not verified Google tokenizer/context accounting.
Adjacent text coalescing appends in place without repeated accumulated-prefix
copies. Overrides into Antigravity receive the same strict original-byte checks,
including duplicate/per-message fields, raw context hints and every beta header.
Cross-provider overrides out of explicit Antigravity routes remain blocked.

### Consumer endpoint evidence

The 2026-10-23–24 investigation observed an `agy` 1.4.6 consumer request using
`https://daily-cloudcode-pa.googleapis.com`; static analysis also found that its
consumer endpoint selector chooses daily. A controlled native comparison held the
profile, model and request settings fixed and changed only the native API origin:
production returned HTTP 429, daily returned HTTP 200 with a complete Anthropic
text SSE turn, expected marker and valid usage, then production returned HTTP 429
again. This supports the daily default for this adapter's supported consumer OAuth
flow in that profile/time window, not a claim that every account or Google Cloud
tier must use daily. HTTP 429 alone does not establish exhausted subscription quota.

Both project discovery and generation use the selected native origin. Explicit
`CCP_ANTIGRAVITY_BASE_URL` / `antigravity.baseUrl` overrides remain authoritative,
including production and custom trusted HTTPS origins. GCP/ADC/service-account
flows and account-tier endpoint selection are not implemented. No automatic
endpoint fallback, hostname rewrite, OAuth endpoint change or replay-format change
is introduced. Synthetic tests cover the complete nonstream assistant-content
carrier and tool-result round trip; neither those tests nor the live low-tier text
result establish all-tier or live tool/replay acceptance.

### Evidence and remaining gates

Synthetic Rust HTTP/OAuth tests and an isolated real-client replay fixture do not
prove Google acceptance, entitlement, billing or native model limits. Browser
OAuth with the configured client, project readiness, the selected HTTP identity,
native text/reasoning/tool round trips and **all three tiers** still require
real-account smoke validation before activation. Low has public request-shape
evidence; Medium/High encodings are catalog-inferred. Catalog evidence indicates
Google AI Ultra, but actual access responses remain authoritative. Automatic
long-context compaction and native compaction acceptance are unverified. A
manual real-client compact/resume fixture does not establish general compaction
support.

Opt-in `antigravity auth login --manual` uses the hosted redirect
`https://antigravity.google/oauth-callback`, not Google OOB or a loopback listener.
The hosted page displays/copies the decoded authorization code and removes it
from its URL; copy the displayed code into the visible local terminal, not chat or
shell arguments. Input is sensitive and may remain in screen/scrollback recordings;
application-managed visibility does not log it or promise erasure. Short codes
are fully displayed; longer input shows its end with a `<` marker and total
character count, not all 8192 bytes simultaneously. Known width of at least
10 columns and non-translating output modes are required; detected width changes
cancel without exchange. This is not verified current `agy` display parity.
Client registration/redirect acceptance, token exchange, server
S256 PKCE enforcement and rejection of PKCE downgrade remain live-validation
gates. Bare-code input does not validate returned state; complete redirect-URL
input does. No official-CLI parser or credential import is implied. Manual input
is Unix-only (Linux runtime tests), refuses non-TTY/background use, and restores
terminal mode on controlled exits but cannot guarantee restoration after SIGKILL,
abort or a crash. See the [manual completion contract](/reference/configuration/#experimental-antigravity).

The default HTTP User-Agent is exactly `claude-code-proxy/<package version>` on
all Antigravity operations. An explicit `CCP_ANTIGRAVITY_USER_AGENT` or
`antigravity.userAgent` override is verbatim compatibility metadata applied only to
native generation and `loadCodeAssist`; OAuth exchange and refresh keep proxy
identification. The historical CLI 1.3.5 / changelist 998863838 example comes from
this pinned public generation fixture:
https://github.com/cortexkit/antigravity-auth/blob/f5a836df058a99cee6099ad308b9675e4ef11026/test-fixtures/agy-cli-1.3.5/native-argon-low.json.
Project-discovery use is reference-adapter behavior, not a new official capture.
The override is not a host-platform assertion, account verification or
attestation. Exact CLI 1.4.2 matching and provider policy/terms compatibility are
unverified. There is no runtime CLI detection, automatic profile upgrade, device
identity fabrication or change to OAuth clients/scopes. User-Agent is not
authentication and cannot establish access. See
[override validation and precedence](/reference/configuration/#experimental-antigravity).

### Optional offline client fixture

`tests/antigravity_compatibility/run_container.py` is developer test tooling, not
an inference backend or installer. Supply **both** an already-local image and a
single installed native Claude executable; there are no machine-specific defaults.
For example, replace these paths and image tag with your own trusted local inputs:

```sh
python3 -B tests/antigravity_compatibility/run_container.py \
  --image your-fixture-image:local \
  --claude-binary /absolute/path/to/native-claude \
  --out /absolute/existing/private-parent/antigravity-compatibility-new minimal
```

The supported setup is a nonroot Linux caller with equal positive real/effective
UIDs and GIDs, trusted **local rootful Docker Engine** without user-namespace
remapping, and a compatible Linux image containing `/usr/bin/env`, `python3` on
`/usr/bin:/bin`, and the CLI's runtime libraries. The CLI must be a readable,
executable 64-bit little-endian ELF (amd64 or arm64), matching the inspected image
architecture. Symlinked native installations resolve to one file; npm/JavaScript
wrappers and parent-directory mounts are unsupported. ARM compatibility is
validation-tested, not an actual-client runtime claim. Rootless Docker, Podman,
Docker Desktop, remote daemons, macOS and Windows are not supported by this launcher.

Pure input validation precedes read-only Docker context/engine/image checks;
output is created only afterward. Docker's context-versus-host precedence is
honored, then the selected Unix endpoint and immutable image ID are pinned.
A Unix endpoint is not proof against a proxy: trusting the daemon, client config,
image and local filesystem remains a prerequisite. No pull, build, download,
credential import, privilege repair or permission change is performed.

The new output root and its `control/` and `work/` children are private (0700).
Only `work/` is container-writable; host-only `control/` retains command, image,
binary identity, inspection and log evidence. Inputs are read-only, the rootfs is
read-only, network is disabled, and HOME is an identity-owned private tmpfs.
Image VOLUMEs are rejected, inherited environment keys and client-config proxy
variables are explicitly blanked, healthchecks are disabled, and the entrypoint
remains `env -i` running the Python fake-endpoint harness, not the CLI directly.
The launcher never executes the CLI on the host or mounts host auth/config or a
Docker socket. Paths containing commas, double quotes or ASCII controls are
rejected. The output needs an existing nonsymlink parent and cannot overlap inputs.

Binary device/inode/size/hash are checked again immediately before launch. This
is not race-proof against a malicious host swapping files after validation. A
single 600-second Docker lifecycle budget reserves 30 seconds for cleanup;
cleanup targets only an inspected invocation-owned full container ID, never a
name/filter guess. Interrupted or ambiguous ownership reports a possible orphan
for manual investigation rather than expanding cleanup authority. Keep generated
artifacts private; they are not contribution files.

Historical actual-client evidence used Claude Code **2.1.270** with synthetic
loopback responses. The portability correction was checked with mocked processes
and the existing codec/HTTP unit suite in an offline nonroot container, with
**zero actual Claude executions**. It does not establish a new client-version,
image/architecture or live-provider compatibility claim. Case validation uses the
existing harness catalog; `catalog-default` must run separately.

Protocol references were independently translated from MIT-licensed
`cortexkit/antigravity-auth`, revision
`f5a836df058a99cee6099ad308b9675e4ef11026`:
https://github.com/cortexkit/antigravity-auth/tree/f5a836df058a99cee6099ad308b9675e4ef11026.
Copyright (c) 2025 Jens; the reference license is retained in
`meta/antigravity-reference-LICENSE`. No full captured CLI system prompts,
proprietary descriptions, binaries, device/session fingerprint fabrication or signature-bypass
code are included. Test vectors contain synthetic/minimized data only.

## Diagnostics and privacy

- Structured logs redact known credential keys, but user-provided strings can still contain secrets.
- Error captures contain complete redacted failed responses.
- Traffic captures intentionally preserve prompts and tool content for message/Responses diagnostics.
- Image generation and edit routes never create traffic captures or persist prompts, uploads, data URLs, generated base64, or upstream error bodies.
- Verbose logging and traffic capture should be scoped to a focused local investigation.

For provider-specific behavior, use the [provider pages](/providers/choosing-a-provider/). For released changes, see the [Changelog](/reference/changelog/).
