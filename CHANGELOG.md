# Changelog

## Unreleased

- Keep Claude Code reasoning effort one-to-one under `co`, so `xhigh` remains
  wire-level `xhigh`, only an explicit `max` request uses `max`, and the
  obsolete `ultra` effort alias is rejected instead of being remapped.
- Persist Grok OAuth credentials in macOS Keychain with verified migration from
  existing mode-0600 files, while retaining file fallback and isolated
  `CCP_CONFIG_DIR` behavior.
- Remove the Kimi and Cursor providers, including their model routing,
  authentication commands, configuration, credentials helpers, dependencies,
  tests, and documentation; the proxy now contains only Codex and Grok
  backends.

## v0.1.25 (2026-07-24)

- Keep `co` and `cg` Claude Code transcripts, prompt history, session state,
  and resume targets isolated in dedicated GPT and Grok profiles while sharing
  an explicit first-use snapshot of non-session configuration.
- Add privacy-preserving diagnostic bundles for transferring bounded proxy
  logs and runtime metadata between Linux, WSL2, and the development machine
  without including credentials or captured prompt/tool payloads.
- Accept and ignore informational Codex response metadata events that do not
  change Anthropic output, preventing valid streams from failing closed when
  newer gateways emit them.

- Enforce Codex and Grok response tool policy in live and buffered paths:
  reject undeclared or forbidden function/hosted calls, a second call in
  serial mode, and successful responses that omit a required or named tool.
- Let opt-in Codex full-lane and Grok guidance cover independent repeated calls
  to one function tool, including named choices, while keeping `none` and
  `disable_parallel_tool_use` serial and free of tool-use guidance.
- Reconcile Grok's late `output_text.done` snapshot across hosted web/X tool
  boundaries, retaining exact-once matching and fail-closing on extensions or
  duplicates. Forced Grok function choices now match only exact declared names.
- Enforce hosted-search `max_uses` in Codex and Grok response policy, and close
  retry, reconnect, and transport-fallback windows as soon as an upstream
  hosted search starts so one logical request cannot replay the side effect.
- Give Codex hosted-search blocks collision-free downstream IDs, require an
  authoritative non-empty upstream ID, and preserve monotonic content-block
  ordering when a live response continues with function calls.
- Keep omitted-type and explicitly typed Grok functions named `WebSearch` or
  `XSearch` as functions; only explicit hosted types move execution upstream.
  Reject forced or otherwise unobservable hosted X-only responses, and reject
  request-level reasoning effort for Composer instead of dropping it.
- Reject GPT-5.6 Luna locally when a request requires the full Responses lane,
  preserving the caller's selected model instead of silently changing it to
  Sol. Reuse non-stream accumulator finish metadata instead of running the
  reducer again for continuation, and remove the unreachable buffered-stream
  production branch.

## v0.1.23 (2026-07-22)

- Accept Grok streams that open function calls before a matching late
  `response.output_text.done`, which previously failed closed as
  `invalid_event` after Claude Code had already received partial text or tools
  and then surfaced `Server error mid-response`. Log the concrete Grok reducer
  error behind `stream_reducer_error` instead of only the generic invalid-stream
  message.
- Put Grok parallel tool-calling policy at the front of translated instructions
  and strengthen it for Claude Code: prefer multiple dedicated tools over one
  mega-Bash script, and keep independent calls in the same response so the
  client can run them concurrently.
- Bound opt-in traffic captures with charged-byte and file-count admission
  quotas, both globally and per request. Every file costs at least one rounded
  4 KiB unit, existing regular files count after restart without following
  symlinks, and the initial scan runs off the async worker without holding the
  global registry lock. Reservations are concurrency-safe and quota exhaustion
  skips only new diagnostic artifacts; this does not prune or delete any
  existing capture.
- Reject missing or zero Anthropic `max_tokens` on `/v1/messages`, non-empty
  `stop_sequences`, and sampling controls before provider dispatch. Positive
  Codex output budgets stay accepted for Claude Code compatibility but are
  omitted because the private gateway rejects every supported Responses
  output-budget field.
- Let incoming `service_tier: "standard_only"` suppress local Codex priority,
  reject incompatible explicit strict schemas instead of silently downgrading
  them, and treat null `output_config`, effort, and format values as omitted.
- Preserve Codex structured-output semantics with a request-local SchemaBridge:
  validate the original and projected schemas, reject unsafe references or
  ambiguous projections before dispatch, remove only bridge-generated optional
  nulls, and revalidate completed stream and non-stream JSON before success.
- Reuse Codex session WebSockets even when response continuation is disabled,
  reuse a fresh terminal ordering Pong for the immediately following tool turn,
  keep checked-out sockets under one owner, enable TCP_NODELAY, and switch
  statusless pre-dispatch Auto failures to HTTP without an artificial retry
  pause. Explicit WebSocket mode now fails before authentication or model
  dispatch when an environment proxy would require unsupported HTTP CONNECT.
- Keep GPT and Grok HTTP/2 connections warm for five-minute interactive pauses,
  with bounded TCP keepalives, a larger per-host idle pool, and adaptive receive
  windows for large Claude Code transcripts. Avoid a short HTTP/2 PING timeout
  that would override the providers' longer network-jitter tolerance.
- Let `cg` keep its high default through Claude Code settings instead of a
  hard-coded model alias, so `/effort medium` and `/effort low` now reduce Grok
  reasoning latency while `grok-4.5-high` remains an explicit locked profile;
  expose Composer Fast in the same model picker as an opt-in speed choice.
- Keep local GPT/Grok input-token estimation off the response hot path, run
  explicit `/count_tokens` work through Tokio's blocking pool, and share a
  two-slot admission gate with a 30-second overload queue. Use `o200k_base`
  text tokenization plus fixed compatibility estimates instead of a word-count
  heuristic, and avoid cloning complete message block arrays during request
  translation.
- Avoid whole-environment copies on Codex request configuration, path, and
  traffic-capture checks, cache the process log path, and skip project
  filesystem discovery when the monitor is disabled.
- Commit validated Codex HTTP/WebSocket and Grok terminal events before draining
  or probing the upstream connection, so a reset, truncated body, or delayed Pong
  after model completion cannot replace a complete Claude response with an
  incomplete-stream warning.
- Fail closed when Codex transport ends after complete-looking tool JSON but
  before authoritative item and response terminal events. The legacy synthetic
  `tool_use` terminal is available only behind the explicitly dangerous
  `unsafeSalvageToolCallOnClose` compatibility switch, and both outcomes are
  identified in terminal-resolution diagnostics.
- Send Codex and Grok downstream heartbeats every five seconds by default, with
  a configurable Codex interval and coverage before Codex's first generation
  event, so brief upstream or network stalls do not repeatedly trigger Claude
  Code's connection warning.
- Print the correct `co` or `cg` resume command when a Claude Code proxy
  session ends, preventing resumed GPT and Grok sessions from silently falling
  back to the first-party Claude runtime.
- Match Codex CLI 0.144.6 model metadata by using a 272K Claude Code context
  window for GPT-5.6 Sol, Terra, and Luna setup examples.
- Accept Claude Code's `ultra` effort without an API error, matching Codex CLI's
  wire behavior by sending `max` to Codex and capping it at `high` for Grok.
- Allow GPT-5.6 requests to opt out of Responses Lite so accounts that support
  the full request shape can use parallel tool calls.
- Preserve Claude tool-choice modes and single-tool constraints when translating
  requests to Codex Responses.
- Count Codex input with the `o200k_base` tokenizer, including accurate CJK text
  accounting for Claude Code compaction.
- Forward valid images nested in tool results as structured Responses content.
- Preserve encrypted Codex reasoning by default and distinguish filtered
  incomplete responses from output-token limits.
- Detect half-open Codex WebSockets with Ping/Pong probes, refresh stale pooled
  connections, and fail closed on malformed upstream events.
- Keep Codex `auto` transport on live WebSockets while healthy, then temporarily
  route later requests through HTTP after repeated transport health failures.
- Add an independent Codex HTTP first-byte timeout while preserving the existing
  body-idle timeout as the fallback when the new setting is not configured.
- Preserve the Codex recovery window across `response.created`, allowing a safe
  OAuth refresh before semantic output is committed.
- Classify model-request replay safety explicitly: retry pre-dispatch failures
  and upstream 429/500/502/503/504/529 responses, but never replay a POST or
  WebSocket request whose outcome is unknown.
- Recover Grok requests from definite pre-dispatch failures and explicit
  429/500/502/503/504/529 responses while preserving the no-replay boundary for
  ambiguous transport failures and emitted text or tools.
- Parse Codex protocol SSE as strict UTF-8 and avoid repeated buffer compaction
  in its incremental decoder.
- Serialize Codex HTTP and Grok model request bodies once per logical request,
  reusing reference-counted bytes across safe retries and rebuilds.
- Cache parsed file configuration and use one coherent logging snapshot per
  record instead of rereading configuration for every redacted string.
- Release Grok replay payloads as soon as semantic output makes rebuilding
  ineligible.
- Honor numeric and HTTP-date `Retry-After` values and add bounded jitter to
  exponential retry delays.
- Forward Claude Code reasoning effort to Grok 4.5 instead of relying on the
  upstream model's default effort.

## v0.1.21 (2026-07-15)

- The monitor shows session token activity trends at common terminal widths,
  making throughput history visible without an extra-wide window.

## v0.1.20 (2026-07-15)

- The monitor reliably shows project names for Claude Code sessions and keeps
  them visible as requests are sequenced.
- Keyboard navigation scrolls session and recent-request tables to keep the
  selected row visible.
- Pressing `q` asks for confirmation before gracefully shutting down the proxy.
- Compact monitor layouts show more project, provider, model, effort, and token
  details without requiring a wider terminal.

## v0.1.19 (2026-07-15)

- The monitor shows project and session context at more terminal widths while
  preserving key request details in narrower layouts.

## v0.1.18 (2026-07-15)

- Codex preserves encrypted reasoning across turns, improving continuity when
  conversation history is replayed. ([#52](https://github.com/raine/claude-code-proxy/pull/52))
- The new `demo` command opens the interactive monitor with simulated traffic,
  without starting a proxy server or requiring provider credentials.
- Session rows show project names and output-token activity over time, making
  concurrent sessions and usage bursts easier to identify.
- Monitor tables adapt more consistently across terminal sizes and keep important
  request details readable in compact layouts.
- The monitor stays visible during graceful shutdown and shows progress until the
  proxy finishes draining connections.

## v0.1.17 (2026-07-14)

- The proxy can listen on a configurable IP address through `CCP_BIND_ADDRESS`
  or `bindAddress`, enabling protected access from containers and remote hosts.
  ([#48](https://github.com/raine/claude-code-proxy/pull/48))
- Model names with context-window hints such as `[1m]` route correctly across
  providers. ([#50](https://github.com/raine/claude-code-proxy/pull/50))
- The monitor reports more accurate output rates by measuring generation time
  and excluding requests without complete usage and timing data.

## v0.1.16 (2026-07-13)

- GPT-5.6 Luna requests work without a custom User-Agent instead of failing with
  a model unavailable error.
  ([#45](https://github.com/raine/claude-code-proxy/issues/45))
- Canceled or replaced Codex prompts cannot interrupt later turns with stale
  continuation state.
- GPT-5.6 setup examples use a 272K compaction window to stay within the current
  ChatGPT context limit.
- Homebrew installations can run the proxy at login as a background service with
  `brew services start claude-code-proxy`.
  ([#44](https://github.com/raine/claude-code-proxy/pull/44))

## v0.1.15 (2026-07-12)

- Codex function tools preserve optional parameters, preventing unintended tool
  arguments and incorrect agent isolation choices.
  ([#43](https://github.com/raine/claude-code-proxy/issues/43))
- Forced Codex web searches return live results while preserving allowed and
  blocked domain filters.
  ([#26](https://github.com/raine/claude-code-proxy/issues/26))
- Codex credentials are stored and refreshed independently from the native Codex
  CLI, preventing either application from invalidating the other's login. Users
  who relied on the native Codex login must sign in to the proxy once after
  upgrading.
- [Expanded guidance](https://github.com/raine/claude-code-proxy/#switching-models-and-backends)
  explains how to switch models within the proxy and how to switch between the
  proxy and direct Anthropic.

## v0.1.14 (2026-07-12)

- Codex hosted web searches work when Claude Code routes them through the Luna
  small model. ([#26](https://github.com/raine/claude-code-proxy/issues/26),
  [#35](https://github.com/raine/claude-code-proxy/pull/35))
- Codex context-window errors trigger Claude Code's compaction flow instead of
  ending the request. ([#29](https://github.com/raine/claude-code-proxy/pull/29))
- Codex requests fall back to HTTP after WebSocket handshake failures while
  preserving live streaming for established connections.
  ([#39](https://github.com/raine/claude-code-proxy/pull/39))
- Codex HTTP and WebSocket failures retain upstream status codes and error
  details, making failures clearer and more actionable.
  ([#40](https://github.com/raine/claude-code-proxy/pull/40))

## v0.1.13 (2026-07-12)

- Grok users can sign in on headless hosts with `grok auth device`.
  ([#38](https://github.com/raine/claude-code-proxy/pull/38))
- Grok tool calls accept Claude Code's prompt-cache markers, preventing errors
  when switching to Grok during a tool-using session.
  ([#37](https://github.com/raine/claude-code-proxy/pull/37))
- Codex hosted web searches return their result links and citations to Claude
  Code instead of appearing to produce zero results.
  ([#10](https://github.com/raine/claude-code-proxy/issues/10))
- Codex authentication refresh is coordinated across concurrent requests and
  automatically recovers live WebSocket requests after credentials expire.
- Codex requests recover more reliably from temporary upstream failures,
  connection resets, overloads, and long-running responses.

## v0.1.12 (2026-07-12)

- Codex hosted web searches work with GPT-5.6 models instead of failing with an
  unsupported tool error. ([#26](https://github.com/raine/claude-code-proxy/issues/26),
  [#35](https://github.com/raine/claude-code-proxy/pull/35))
- Codex WebSocket connection timeouts are retried automatically, reducing
  interrupted requests.

## v0.1.11 (2026-07-11)

- Grok subscriptions can power Claude Code through browser login, with support for
  Grok 4.5 and Composer 2.5 Fast, streaming, thinking, tools, and token counts.
- Codex WebSocket requests recover from handshake failures and stay marked active
  until the full response body finishes streaming.
- The monitor shows local timestamps, clearer request status and detail indicators,
  more compact columns, arrow-key pane navigation, and an uncluttered display.
- Forward Claude Code's `max` effort as Codex `reasoning.effort: "max"` so
  GPT-5.6 can use its highest supported reasoning level instead of silently
  receiving `xhigh`. ([#28](https://github.com/raine/claude-code-proxy/pull/28))

## v0.1.10 (2026-07-10)

- Claude Code requests using Opus 4.8, Sonnet 5, and Fable 5 model names can
  route through Codex

## v0.1.9 (2026-07-10)

- Claude model aliases use the matching GPT-5.6 tier through Codex: Haiku uses
  Luna, Sonnet uses Terra, and Opus uses Sol.
- GPT-5.6 Codex requests preserve reasoning context and support system guidance
  and tools through the Responses Lite API.
- The dashboard shows requested effort and resolved upstream models, making
  routing decisions easier to inspect.

## v0.1.8 (2026-07-09)

- Codex requests can use `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`,
  including `-fast` variants.
- The default Codex setup uses `gpt-5.6-sol` with `gpt-5.6-luna` as the small
  fast model and a 372K compaction window.

## v0.1.7 (2026-07-06)

- Codex `Read` tool calls get clearer offset guidance and recover from clearly
  invalid large offsets, reducing stalled sessions caused by mistaken
  line-number reads.
- The monitor keeps request lists accurate when a client disconnects or abandons
  a request.

## v0.1.5 (2026-07-03)

- Claude Code's `xhigh` and `max` effort settings now work with Codex and Kimi
  requests instead of being rejected or downgraded unexpectedly.
  ([#20](https://github.com/raine/claude-code-proxy/pull/20))
- Codex receives clearer `Read` tool guidance for line offsets, reducing
  incorrect follow-up reads on large files.
  ([#22](https://github.com/raine/claude-code-proxy/pull/22))

## v0.1.4 (2026-07-01)

- Codex WebSocket streams recover when a pooled continuation connection closes
  before the final response, retrying the turn with full context instead of
  failing the session.

## v0.1.3 (2026-07-01)

- Codex WebSocket streams deliver live text and reasoning progress while reusing
  pooled session continuations to reduce repeated upstream input.
- Codex stream recovery handles retryable startup failures, context-window
  errors, stale continuations, completed tool-call disconnects, stalled `Read`
  arguments, quiet upstream turns, and completed-turn stop reasons.
- Codex gateway requests and tool result translation use accepted payload shapes
  and preserve omitted-block markers for malformed text and image result
  content.

## v0.1.2 (2026-06-30)

- Codex WebSocket continuations recover from streams that only deliver rate
  limit or control events, preventing Claude Code sessions from waiting
  indefinitely on a stalled upstream response.

## v0.1.1 (2026-06-30)

- Codex reasoning summaries are now surfaced as thinking blocks in the response
  stream, so you can see the model's reasoning in your Claude Code session
  when reasoning effort is enabled. Set `codex.reasoningSummary` or
  `CCP_CODEX_REASONING_SUMMARY` to `off` or `none` to suppress summary display
  while keeping reasoning effort active. (Thanks @samot-gc!)
- Codex transport errors (WebSocket connection failures, etc.) now show the
  actual error message instead of a generic "Upstream error", making
  connection issues easier to diagnose.

## v0.1.0 (2026-06-30)

- Ships the native Rust implementation as the release binary.
- Adds the default monitor TUI for `serve`.
- Improves diagnostics with failed-response captures and clearer monitor
  request details.

## v0.0.22 (2026-06-24)

- Codex requests now retry more transient stream and overload failures, making temporary upstream errors less likely to interrupt Claude Code sessions. ([#15](https://github.com/raine/claude-code-proxy/issues/15))
- Codex can now recover stalled `Read` tool calls that previously left Claude Code waiting on incomplete streamed arguments.
- Cursor tool calls are recovered more reliably when Cursor returns XML-style tool use, improving compatibility with Claude Code tools.
- Cursor auth can now be isolated with `CCP_CONFIG_DIR`, so separate proxy configs can keep separate Cursor logins.
- Cursor `composer-2.5` requests now stay in non-fast mode unless fast mode is explicitly requested. ([#17](https://github.com/raine/claude-code-proxy/issues/17), [#18](https://github.com/raine/claude-code-proxy/pull/18))

## v0.0.21 (2026-06-15)

- Forced Codex web search requests now use hosted web search correctly, fixing repeated upstream `Tool choice 'function' not found in 'tools' parameter.` errors. ([#10](https://github.com/raine/claude-code-proxy/issues/10))

## v0.0.20 (2026-06-15)

- Cursor's generic `cursor`, `cursor-agent`, `cursor-plan`, and `cursor-ask` aliases now use Cursor default model selection instead of forcing Composer 2.5 fast mode.

## v0.0.19 (2026-06-14)

- Codex now supports Claude Code hosted web search through Codex's native web search, including domain filters and search usage accounting. ([#10](https://github.com/raine/claude-code-proxy/issues/10))

## v0.0.18 (2026-06-09)

- Cursor sessions now stop heartbeat traffic after streams close, reducing stray connection errors.
- Codex now treats runtime system messages as developer guidance instead of assistant output, preventing Claude Code reminders from being repeated.

## v0.0.17 (2026-06-08)

- Added Cursor Agent as a provider, including login, model selection, ask mode, plan mode, and session continuation.
- Cursor users can select models from the Cursor catalog with `cursor:<model-id>`, `cursor-plan:<model-id>`, and `cursor-ask:<model-id>` aliases.

## v0.0.16 (2026-06-02)

- Codex now uses WebSocket transport by default
- Codex sessions can opt in to append-only continuation with `previous_response_id`, reducing repeated upload size on compatible turns.
- `CCP_TRAFFIC_LOG=1` writes redacted per-request traffic captures to help debug sessions.
- Codex request logging now includes size summaries and image warnings to make compaction and large requests easier to diagnose.
- README guidance for Codex context limits and `[1m]` model suffixes is clearer.

## v0.0.15 (2026-05-30)

- Anthropic requests that omit `stream` now receive JSON responses, fixing Claude Code `/model` validation through the proxy.

## v0.0.14 (2026-05-30)

- Codex streaming now stays responsive during long `Read` tool calls by sending keepalive pings while tool arguments are buffered.
- Truncated Codex streams now return a clear error instead of appearing to finish successfully with incomplete tool calls.
- Stalled Codex requests now time out and retry when response headers never arrive, with clearer diagnostics for slow upstream responses.

## v0.0.13 (2026-05-14)

- Windows users can now download prebuilt `windows-amd64` and `windows-arm64` release archives.

## v0.0.12 (2026-05-12)

- Codex requests can now use `gpt-5.3-codex-spark` as a supported model. ([#14](https://github.com/raine/claude-code-proxy/pull/14))

## v0.0.11 (2026-05-12)

- Claude-style aliases such as `haiku`, `sonnet`, and `opus` now default to Codex while still following the provider already active in the current Claude Code session.
- Mixed Codex and Kimi sessions now keep background alias and token-count requests on the right provider instead of unexpectedly switching providers.
- Tool results with images, errors, or unsupported blocks are handled more safely, reducing malformed upstream requests.

## v0.0.10 (2026-05-06)

- Codex requests can now use `codex.serviceTier` or `CCP_CODEX_SERVICE_TIER` to request a service tier; `fast` is sent upstream as `priority`.
- Codex model names can now include `-fast`, such as `gpt-5.4-fast[1m]`, to request fast mode per request without restarting the proxy.
- Codex's upstream endpoint can now be overridden with `codex.baseUrl` or `CCP_CODEX_BASE_URL`.

## v0.0.9 (2026-05-03)

- Kimi debugging overrides now use `CCP_KIMI_OAUTH_HOST` and `CCP_KIMI_BASE_URL`, matching the proxy's `CCP_` environment variable naming.

## v0.0.8 (2026-04-30)

- Added exponential backoff retry on upstream 429 errors, respecting
  `Retry-After` headers when present
- Added `config.json` as an alternative to environment variables (read from
  `~/.config/claude-code-proxy/config.json` on macOS, XDG-compliant on Linux)
- Made the `originator` and `User-Agent` headers configurable via new env vars
  (`CCP_CODEX_ORIGINATOR`, `CCP_CODEX_USER_AGENT`, `CCP_KIMI_USER_AGENT`,
  `CCP_ORIGINATOR`, `CCP_USER_AGENT`) and the config file
- Codex now sends a default `User-Agent: claude-code-proxy/<version>` header

## v0.0.7 (2026-04-25)

- Some security hardening inspired by [#5](https://github.com/raine/claude-code-proxy/pull/5)

## v0.0.6 (2026-04-25)

- Added support for `gpt-5.5`, and `opus`/`claude-opus-4-7` aliases now map to
  `gpt-5.5` instead of `gpt-5.4`
- Model names with a `[1m]` context suffix (e.g. `gpt-5.4[1m]`) are now
  accepted and stripped before routing, so Claude Code's larger-context model
  variants work without errors
- Documented how to switch between the proxy and direct Anthropic in the README

## v0.0.5 (2026-04-22)

- Added `CCP_CODEX_MODEL` and `CCP_CODEX_EFFORT` environment variables to
  override the model and reasoning effort for Codex requests
  ([#2](https://github.com/raine/claude-code-proxy/pull/2))
- Added `claude-sonnet-4-6` and additional model aliases so more Claude-style
  model names resolve correctly
- Improved request logging with usage summaries, time-to-first-byte metrics, and
  stream completion details for easier debugging
- Client disconnections during streaming are now handled gracefully

## v0.0.4 (2026-04-20)

- Kimi: reasoning content is now preserved across turns as Anthropic thinking
  blocks, so Claude Code sees the model's thinking and multi-turn reasoning
  stays coherent
- Kimi: thinking is always enabled

## v0.0.3 (2026-04-20)

- Renamed to `claude-code-proxy` to reflect multi-provider support
- Added Kimi (kimi.com) as a provider, with device-code login via the install
  script and support for Kimi's chat models
- Requests are now routed to providers based on the requested model, so a single
  proxy can serve both Codex and Kimi models simultaneously
- Improved token counting accuracy and fixed cached token usage reporting
- Added MIT license

## v0.0.2 (2026-04-19)

- Accept Claude-style model aliases (`haiku`, `sonnet`, `opus`, and `claude-*`
  names), resolving them to the appropriate upstream model so portable configs
  and subagents work without edits
- Fix malformed streamed Read tool arguments that Claude Code would reject when
  upstream emitted an empty `pages` field

## v0.0.1 (2026-04-19)

Initial release.
