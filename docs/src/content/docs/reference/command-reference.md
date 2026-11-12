---
title: Command reference
description: Canonical claude-code-proxy command syntax for serving, monitoring, listing models, version output, and provider authentication.
---

Running `claude-code-proxy` without a subcommand is equivalent to `claude-code-proxy serve`.

## Global version commands

```sh
claude-code-proxy --version
claude-code-proxy -v
claude-code-proxy version
```

Each prints `claude-code-proxy <version>`.

## `serve`

```sh
claude-code-proxy serve [--port <PORT>] [--no-monitor]
```

Starts the local HTTP proxy and blocks until shutdown.

| Option | Behavior |
| --- | --- |
| `--port <PORT>` | Overrides `PORT`, `config.json`, and the default for this invocation. |
| `--no-monitor` | Uses plain output even when stdout is a terminal. |

The bind address comes from `CCP_BIND_ADDRESS` or `bindAddress`. Interactive stdout opens the monitor unless `--no-monitor` is present. Non-terminal stdout uses plain mode.

Plain mode continues collecting monitor history and supports separate dashboards. SIGTERM (Unix) and Ctrl-C request graceful service shutdown.

## `monitor`

```sh
claude-code-proxy monitor [--url <URL>]
```

Attach a read-only dashboard to a running proxy. The default URL is `http://127.0.0.1:<configured-port>`. No provider login is needed in the dashboard process. `q` and `Ctrl-C` detach without stopping the proxy; multiple dashboards are supported. After a connection failure, the dashboard retains its last snapshot and retries. The proxy accepts monitor reads only from loopback peers; use an SSH port forward for another machine.

## `demo`

```sh
claude-code-proxy demo
```

Opens the monitor with deterministic simulated traffic. It does not bind a network port or contact providers.

## `models`

```sh
claude-code-proxy models [--full]
```

Prints supported IDs grouped by provider. The default output compacts Cursor's runtime catalog. `--full` prints every Cursor alias.

## Provider authentication

The command shape is:

```text
claude-code-proxy <provider> auth <action>
```

| Provider | `login` | `device` | `status` | `logout` |
| --- | --- | --- | --- | --- |
| `codex` | Browser PKCE | Device code | Account, expiry, storage | Delete proxy credential |
| `kimi` | Device code | Unsupported | User, expiry, scope, storage | Delete proxy credential |
| `grok` | Browser PKCE | Device code | Expiry and storage | Delete proxy credential |
| `cursor` | Browser polling flow | Unsupported | Source, claims, expiry | Delete proxy credential |
| `antigravity` (experimental) | Browser PKCE; opt-in Unix `--manual` paste-back | Unsupported | Safe local readiness, optional `--json` | Delete only Antigravity credential |

Examples:

```sh
claude-code-proxy codex auth login
claude-code-proxy grok auth device
claude-code-proxy kimi auth status
claude-code-proxy cursor auth logout
```

For existing providers, a missing credential makes `auth status` exit with status 1. Other provider command failures exit with status 2. Successful commands exit with status 0.

Antigravity auth commands work even while inference is disabled:

```sh
claude-code-proxy antigravity auth status --json
claude-code-proxy antigravity auth login
claude-code-proxy antigravity auth login --manual
claude-code-proxy antigravity auth logout
```

Configure your explicit OAuth client ID before login; see
[Antigravity configuration](/reference/configuration/#experimental-antigravity).
Without `--manual`, browser opening and the ephemeral loopback callback are
unchanged. Experimental `--manual` is Unix-only: it requires terminal stdin and a
foreground controlling terminal, prints the authorization URL there, and opens
neither a browser nor a callback listener. The configured client must permit the
exact hosted redirect `https://antigravity.google/oauth-callback`.

Open the URL in your browser, select the intended account, then copy the **code
displayed by the completion page** into the visible local terminal prompt. The
code is sensitive: keep screens, scrollback, screenshots and recordings private.
The input row shows a character count; short codes fit in full, while `<` marks
that only the end of longer input is visible. All entered characters remain part
of the completion, up to the unchanged 8192-byte limit. A known terminal width of
at least 10 columns is required; changing width cancels rather than risking
incorrect wrapped editing. Output modes that translate carriage returns or letter
case are refused, not silently changed.
The page removes `code` from its address bar; copying that address afterward does not
supply the code. Cancel if the browser reports an error (such as
`redirect_uri_mismatch`) or displays no code. There is no automatic callback,
client or endpoint fallback. Never put a code in command arguments, chat or logs.

Input is limited to 8192 printable ASCII bytes and 180 seconds, with at most
three empty-entry reprompts under the same deadline. Backspace edits; Enter
submits; Ctrl-C/Ctrl-D cancel. Pipes and non-Unix manual mode are rejected before
URL emission. Bare codes rely on server-enforced fresh S256 PKCE, **not returned
state validation**; a complete redirect URL, if separately available with its
code intact, additionally requires an exact matching state and redirect target.
See configuration for security and terminal-restoration limitations.

`status --json` is Antigravity-only and emits schema version 1, `provider:
antigravity`, plus the safe provider descriptor documented under
[model discovery](/reference/http-api/#get-v1models). A successfully emitted
report exits 0 even for missing/invalid/unready credentials. Inspection performs
no network, refresh, directory creation or writes. CLI readiness describes this
CLI's configuration, not a different running proxy, and never proves live access.
Auth invocation failures exit nonzero. No existing provider gains a `--json` flag.

Logout removes the local proxy-owned credential. It does not call the provider to revoke a refresh token.

## Development commands

From a source checkout:

```sh
cargo run -- serve
cargo test --all
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
just check
just docs
```

`just docs` installs the locked documentation dependencies and starts the Astro development server on an available local port.
