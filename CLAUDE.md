# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`claude-code-proxy` is a Rust 2024 CLI and Axum server that accepts the Anthropic Messages API shape used by Claude Code and routes model requests to Codex or Grok. The process also owns provider authentication, streaming translation, process-local session state, logging, traffic capture, and the terminal monitor.

## Development commands

The repository is a single crate. `just` tasks wrap `checkle`; the equivalent Cargo commands do not require either tool.

| Task | Command |
| --- | --- |
| Run the proxy | `cargo run -- serve` |
| Run without the monitor TUI | `cargo run -- serve --no-monitor` |
| Exercise the TUI with simulated traffic | `cargo run -- demo` |
| Build all targets | `cargo build --all` |
| Run the full test suite | `cargo test --all` |
| Check formatting | `cargo fmt --all --check` |
| Lint with warnings denied | `cargo clippy --all-targets -- -D warnings` |
| Run configured format, Clippy, build, and test checks | `just check` |
| Build and symlink the debug binary into `~/.cargo/bin` | `just install-dev` |
| Start an isolated verbose proxy with traffic capture | `scripts/debug-proxy` |

Run one integration-test target with `cargo test --test <target>`. Run one test within it by adding its name, for example:

```sh
cargo test --test cli version_aliases_print_expected_version
```

For an in-module unit test, use Cargo's name filter: `cargo test <test_name>`.

`just format`, `just clippy`, `just build`, and `just test` run the corresponding `checkle` tasks. The configured Clippy task does not add `-D warnings`; use the explicit Cargo command above for the strict lint used by the README.

## Parallel agent reliability

- For failure-sensitive parallel work, use individually named background Agent tasks instead of one large Dynamic Workflow. Claude Code reports each background Agent API failure independently, while a Workflow may aggregate failures only after its entire `parallel()` batch finishes.
- Limit each wave to at most four concurrent child agents. Record successful results by task or agent ID, and resume or retry only the failed lane. Never redeploy a completed lane because a sibling returned an API error.
- If a Workflow is unavoidable, launch the following command through the Bash tool with `run_in_background: true` immediately after the Workflow returns its transcript directory:

  ```sh
  python3 scripts/watch-claude-workflow-failures '<workflow-transcript-dir>'
  ```

  Exit status 42 with `CLAUDE_WORKFLOW_AGENT_FAILURE`, or 43 with `CLAUDE_WORKFLOW_TERMINAL_FAILURE`, is an interrupt signal: stop queuing later phases, preserve completed results, inspect the named agent/request, and retry only that failed lane. Do not wait for the whole Workflow merely to learn which child failed.
- A proxy request ID identifies one HTTP exchange, not an Agent tree. Do not infer parent/child ownership from the Claude session ID or cancel sibling requests at the proxy layer.

## Architecture

### Entry point and request lifecycle

- `src/main.rs` defines the `clap` CLI. No subcommand defaults to `serve`; serve mode starts the monitor TUI only when stdout is a terminal and `--no-monitor` is absent. Provider auth subcommands are delegated through the registry and each provider's CLI hook rather than implemented in the CLI layer.
- `src/lib.rs` exposes the reusable modules and core provider interfaces.
- `src/server.rs` owns the Axum surface and request lifecycle: parse an Anthropic `MessagesRequest`, remove the optional model suffix, look up existing session affinity, select a provider, record the session request, create monitoring/traffic context, and dispatch through `Provider`.
- The implemented HTTP routes are `GET /healthz`, `GET /version`, `GET /v1/models`, `POST /v1/messages`, and `POST /v1/messages/count_tokens`. Treat the router in `src/server.rs` as authoritative; model listing is available both over HTTP and through the CLI.
- Successful response bodies are monitored until the downstream client consumes or drops them. Do not eagerly collect a successful streaming body in the server layer, because completion versus abandonment accounting depends on body consumption. Failed responses are collected, redacted, and persisted separately.

### Model routing and process-local state

- `src/registry.rs` is the routing authority and public model catalog. Only Codex/Grok routes and their active Anthropic-compatible aliases appear in the CLI model list or `/v1/models`. The optional `[1m]` suffix is removed before routing.
- Anthropic-style aliases such as `haiku`, `sonnet`, `opus`, and `claude-*` route to Codex by default.
- `src/session.rs` tracks request sequence and alias affinity by `x-claude-code-session-id`. A concrete Codex or Grok request can pin the session, but only `claude-opus-5` currently follows Grok affinity; other aliases route to Codex. An alias request does not establish the pin. Entries expire after 30 idle minutes and the store is capped at 10,000 sessions.
- Session affinity, Codex continuation state, and the Codex WebSocket circuit are separate process-local stores. Deployments with multiple proxy processes need sticky routing for active features that depend on local state; do not assume all stores share the session store's TTL or capacity.
- Unknown models intentionally return an Anthropic-shaped HTTP 400. There is no implicit fallback provider.

### Provider and translation layer

- `src/provider.rs` defines the `Provider` trait, provider CLI hooks, and `RequestContext`. `src/providers/mod.rs` exposes the concrete backends. `Registry::new` registers the Codex and Grok handlers and model routes.
- `src/anthropic/` contains the inbound schema and Anthropic error/SSE shapes. `src/providers/translate_shared.rs` contains normalization shared across backends; keep provider-specific wire formats, retry behavior, token counting, error mapping, and auth behavior inside that provider.
- `src/providers/codex/` is the largest backend. Request/model translation is under `translate/`; HTTP and WebSocket transport live in `client.rs` and `websocket.rs`; append-only `previous_response_id` state lives in `continuation.rs`; live upstream events are reduced into Anthropic SSE incrementally.
- `src/providers/grok/` translates to a Responses-style API with an incremental stream reducer.
- Adding or reactivating a provider requires a `Provider` implementation plus deliberate registry/model wiring and public-catalog tests; each provider owns its authentication and token refresh manager.

### Configuration, authentication, and paths

- `src/config.rs` is the configuration API. Resolution is per setting with `environment > config.json > built-in default`; resolver functions reread current state rather than using one startup snapshot. Add settings through these resolvers instead of reading environment variables in provider code.
- `src/paths.rs` is the authoritative cross-platform location resolver. `CCP_CONFIG_DIR` overrides configuration and provider-auth storage, while logs, failed responses, and traffic captures use the state directory.
- `src/auth.rs` provides atomic file storage, permissions, macOS Keychain support, and legacy-path fallback. Provider-specific OAuth and refresh behavior remains under each provider's `auth/` modules.
- Incoming proxy requests are not authenticated. Any change to the default loopback bind behavior must account for that trust boundary.

### Observability and tests

- `src/monitor.rs` records lifecycle and usage state; `src/tui/` renders it. Providers report upstream start, stream progress, resolved models, and usage through the optional monitor in `RequestContext`.
- `src/logging.rs` writes redacted JSON-lines logs. `src/traffic.rs` writes opt-in, per-request protocol captures; credentials are redacted, but prompt and tool content are intentionally retained. `src/server.rs` separately persists redacted failed-response payloads.
- Integration tests under `tests/` are grouped by behavior: server/routing, CLI, Codex auth and WebSocket behavior, shared foundations, and smoke regressions. Fine-grained translation, continuation, configuration, retry, and reducer tests generally live beside their modules under `src/`.

## Streaming and continuation invariants

- An upstream request may be retried only before Anthropic output has been emitted. Once downstream output starts, replaying the request can duplicate tool execution; surface a stream error instead.
- Codex continuation is opt-in and valid only for append-only, structurally compatible translated requests with a matching prompt signature. On mismatch, missing state, connection loss, setup failure, or an ineligible terminal result, clear unsafe continuation state and send the full request on the safe path.
- Preserve provider terminal-event handling and traffic-capture finalization when changing reducers. A socket close is not automatically a successful stream completion.
