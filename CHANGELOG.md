---
title: Changelog
description: Release notes for claude-code-proxy.
---

## Unreleased

- Optional Codex Images API routes reuse the existing ChatGPT OAuth session for
  `gpt-image-2` generation and JSON or multipart image edits, with strict size,
  concurrency, upstream-host, header, and diagnostic privacy boundaries.

## v0.1.27 (2026-07-29)

- Grok streams remain reliable during long responses, keepalive events, and
  output-token truncation instead of failing after partial output.
- Forced Codex web searches keep the selected model, so Luna searches no longer
  switch to Sol, while preserving domain filters and search usage reporting.
  ([#53](https://github.com/raine/claude-code-proxy/pull/53))
- Codex WebSocket connections honor `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
  and `NO_PROXY`, restoring standard non-TUN HTTP proxy support.
  ([#83](https://github.com/raine/claude-code-proxy/pull/83))

## v0.1.26 (2026-07-28)

- Standard OpenAI clients can use Codex through the optional
  `POST /v1/chat/completions` endpoint, with streaming, reasoning effort, and
  structured output support.
- Cursor Agent works with current client versions and restores text, thinking,
  usage, model mode, and fast-mode handling.
- Claude Code's `claude-opus-5` model name routes correctly through Codex and
  Kimi.
- Claude Code's automatic security-review classifier can use a dedicated model
  configured with `CCP_AUTO_REVIEW_MODEL` or `autoReviewModel`.
  ([#72](https://github.com/raine/claude-code-proxy/pull/72))
- Codex retries empty successful completions and returns a clear error if no
  usable response arrives. ([#70](https://github.com/raine/claude-code-proxy/pull/70),
  [#71](https://github.com/raine/claude-code-proxy/pull/71))
- Grok handles images in user messages and tool results without failing requests.
  Images are omitted by default, with opt-in vision through
  `CCP_GROK_TOOL_IMAGE`. Traffic captures redact image payloads.
  ([#69](https://github.com/raine/claude-code-proxy/pull/69))
- Nix builds use vendored dependencies for reproducible sandboxed builds.
  ([#80](https://github.com/raine/claude-code-proxy/pull/80))

## v0.1.25 (2026-07-24)

- Kimi users can select Kimi K3 with the `kimi-k3` or `k3` model name, including
  its one-million-token context window and `max` reasoning effort.
  ([#79](https://github.com/raine/claude-code-proxy/pull/79))
- The monitor shows active native Codex compaction requests with a dedicated
  `compacting` status.
- The new [documentation site](https://claude-code-proxy.raine.dev) provides
  setup guides, provider details, configuration references, troubleshooting,
  and an `llms.txt` version for coding agents.

## v0.1.24 (2026-07-23)

- Codex optionally preserves conversation continuity across Claude Code
  compaction boundaries with native encrypted compaction artifacts. Enable it
  with `codex.serverCompaction` or `CCP_CODEX_SERVER_COMPACTION`.
- Native OpenAI Responses clients can use `POST /v1/responses` with existing
  Codex authentication, including JSON responses, SSE streaming, and automatic
  token refresh. Enable the endpoint with `codex.responsesApi` or
  `CCP_CODEX_RESPONSES_API=1`; it is disabled by default.

## v0.1.23 (2026-07-22)

- Codex WebSocket streaming handles pooled connections and HTTP fallback more
  reliably, preventing concurrent requests from blocking each other or sending
  the same request twice.
- Codex errors preserve upstream status codes and optional retry timing, with
  clearer permission failures and safer WebSocket handshake diagnostics.
- Codex streaming limits oversized events and error responses, preventing
  malformed or stalled upstream responses from consuming unbounded memory.

## v0.1.22 (2026-07-20)

- Grok accepts request metadata and tool-result references sent by current Claude
  Code versions while continuing to reject malformed and unknown fields.
  ([#56](https://github.com/raine/claude-code-proxy/pull/56))
- Gateway model discovery lists every configured provider model and Claude-style
  alias through `GET /v1/models`, making supported aliases available in Claude
  Code's model picker. ([#60](https://github.com/raine/claude-code-proxy/issues/60),
  [#61](https://github.com/raine/claude-code-proxy/pull/61))
- Codex preserves supported base64 images in tool results while keeping mixed
  text, image, error, and fallback content in its original order.
  ([#59](https://github.com/raine/claude-code-proxy/pull/59))
- Codex requests continue after the included usage limit when account credits
  remain available. ([#68](https://github.com/raine/claude-code-proxy/pull/68))
- Codex compaction requests cap reasoning effort at `low` to reduce latency and
  reasoning-token usage. `CCP_COMPACT_EFFORT` can choose a different cap or
  disable the behavior. ([#67](https://github.com/raine/claude-code-proxy/pull/67))

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
